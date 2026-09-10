//! User management: listing, creating, deleting and scoping dashboard accounts.
//! Every route here except `/api/me` and `/api/logout` is admin-only (see
//! `require_admin` in `auth.rs` and the route wiring in `mod.rs`) — this is
//! not a playback surface, so it stays out of `api.rs`.

use axum::Json;
use axum::Router;
use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use serde::{Deserialize, Serialize};

use crate::db;
use crate::web::WebState;
use crate::web::auth::{self, CurrentUser, SessionToken};
use crate::web::response::{error_response, parse_guild_id};

pub fn routes() -> Router<WebState> {
    Router::new()
        .route("/api/me", get(me))
        .route("/api/logout", post(logout))
}

pub fn admin_routes() -> Router<WebState> {
    Router::new()
        .route("/api/users", get(list_users).post(create_user))
        .route("/api/users/{username}", delete(delete_user))
        .route("/api/users/{username}/password", post(set_password))
        .route("/api/users/{username}/username", post(set_username))
        .route("/api/users/{username}/guild", post(set_guild))
}

#[derive(Serialize)]
struct UserJson {
    username: String,
    is_admin: bool,
    is_root: bool,
    guild_id: Option<String>,
}

async fn me(Extension(user): Extension<CurrentUser>) -> Response {
    Json(UserJson {
        username: user.username,
        is_admin: user.is_admin,
        is_root: user.is_root,
        guild_id: user.guild_id,
    })
    .into_response()
}

/// Revokes just the calling session, so signing out of one browser tab
/// doesn't touch any other session open for the same account.
async fn logout(
    State(state): State<WebState>,
    Extension(token): Extension<SessionToken>,
) -> Response {
    state.sessions.revoke_token(&token.0);
    StatusCode::NO_CONTENT.into_response()
}

async fn list_users(State(state): State<WebState>) -> Response {
    match db::list_users(&state.db).await {
        Ok(users) => Json(
            users
                .into_iter()
                .map(|u| UserJson {
                    username: u.username,
                    is_admin: u.is_admin,
                    is_root: u.is_root,
                    guild_id: u.guild_id,
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(err) => {
            tracing::warn!(%err, "failed to list users");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "failed to list users")
        }
    }
}

#[derive(Deserialize)]
struct CreateUserRequest {
    username: String,
    password: String,
    #[serde(default)]
    is_admin: bool,
    #[serde(default)]
    guild_id: Option<String>,
}

async fn create_user(
    State(state): State<WebState>,
    Json(body): Json<CreateUserRequest>,
) -> Response {
    let username = body.username.trim();
    if username.is_empty() || body.password.is_empty() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "username and password are required",
        );
    }

    match db::user_exists(&state.db, username).await {
        Ok(true) => {
            return error_response(
                StatusCode::CONFLICT,
                "a user with that username already exists",
            );
        }
        Ok(false) => {}
        Err(err) => {
            tracing::warn!(%err, "failed to check for an existing user");
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "failed to create user");
        }
    }

    let hash = match auth::hash_password(&body.password) {
        Ok(hash) => hash,
        Err(err) => {
            tracing::warn!(%err, "failed to hash password for a new user");
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "failed to create user");
        }
    };

    // `is_root` is never accepted from the request body — it's exclusive to
    // the env-bootstrapped account (see `auth::bootstrap_user_if_needed`),
    // so there's no API path that can grant it.
    let guild_id = body.guild_id.as_deref().filter(|id| !id.trim().is_empty());
    match db::insert_user(&state.db, username, &hash, body.is_admin, false, guild_id).await {
        Ok(()) => Json(UserJson {
            username: username.to_string(),
            is_admin: body.is_admin,
            is_root: false,
            guild_id: guild_id.map(str::to_string),
        })
        .into_response(),
        Err(err) => {
            tracing::warn!(%err, "failed to insert new user");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "failed to create user")
        }
    }
}

async fn delete_user(
    State(state): State<WebState>,
    Extension(current): Extension<CurrentUser>,
    Path(username): Path<String>,
) -> Response {
    if username == current.username {
        return error_response(StatusCode::BAD_REQUEST, "you can't delete your own account");
    }

    let privileges = match db::user_privileges(&state.db, &username).await {
        Ok(Some(privileges)) => privileges,
        Ok(None) => return error_response(StatusCode::NOT_FOUND, "no such user"),
        Err(err) => {
            tracing::warn!(%err, "failed to look up user before deleting");
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "failed to delete user");
        }
    };

    // There is exactly one root account (the env-bootstrapped one) and no
    // way to grant that status again short of DB surgery — losing it would
    // permanently strip away the ability to reset anyone else's password.
    if privileges.is_root {
        return error_response(StatusCode::BAD_REQUEST, "can't delete the root account");
    }

    if privileges.is_admin {
        match db::admin_count(&state.db).await {
            Ok(count) if count <= 1 => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    "can't delete the last remaining admin",
                );
            }
            Ok(_) => {}
            Err(err) => {
                tracing::warn!(%err, "failed to count admins before deleting a user");
                return error_response(StatusCode::INTERNAL_SERVER_ERROR, "failed to delete user");
            }
        }
    }

    match db::delete_user(&state.db, &username).await {
        Ok(()) => {
            state.sessions.revoke_user(&username);
            StatusCode::NO_CONTENT.into_response()
        }
        Err(err) => {
            tracing::warn!(%err, "failed to delete user");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "failed to delete user")
        }
    }
}

#[derive(Deserialize)]
struct SetPasswordRequest {
    password: String,
}

/// Anyone (admin-only routes, so always some admin) can change their own
/// password; only root can change someone else's — see the field doc on
/// `auth::CurrentUser::is_root`.
async fn set_password(
    State(state): State<WebState>,
    Extension(current): Extension<CurrentUser>,
    Path(username): Path<String>,
    Json(body): Json<SetPasswordRequest>,
) -> Response {
    if body.password.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "password is required");
    }
    if username != current.username && !current.is_root {
        return error_response(
            StatusCode::FORBIDDEN,
            "only the root admin can change another user's password",
        );
    }

    match db::user_exists(&state.db, &username).await {
        Ok(true) => {}
        Ok(false) => return error_response(StatusCode::NOT_FOUND, "no such user"),
        Err(err) => {
            tracing::warn!(%err, "failed to check for an existing user before changing password");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to change password",
            );
        }
    }

    let hash = match auth::hash_password(&body.password) {
        Ok(hash) => hash,
        Err(err) => {
            tracing::warn!(%err, "failed to hash password");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to change password",
            );
        }
    };

    match db::set_user_password(&state.db, &username, &hash).await {
        Ok(()) => {
            state.sessions.revoke_user(&username);
            StatusCode::NO_CONTENT.into_response()
        }
        Err(err) => {
            tracing::warn!(%err, "failed to update password");
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to change password",
            )
        }
    }
}

#[derive(Deserialize)]
struct SetUsernameRequest {
    new_username: String,
}

/// Same authorization rule as `set_password`: anyone can rename themselves,
/// only root can rename someone else.
async fn set_username(
    State(state): State<WebState>,
    Extension(current): Extension<CurrentUser>,
    Path(username): Path<String>,
    Json(body): Json<SetUsernameRequest>,
) -> Response {
    let new_username = body.new_username.trim();
    if new_username.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "username is required");
    }
    if username != current.username && !current.is_root {
        return error_response(
            StatusCode::FORBIDDEN,
            "only the root admin can rename another user",
        );
    }

    match db::user_exists(&state.db, &username).await {
        Ok(true) => {}
        Ok(false) => return error_response(StatusCode::NOT_FOUND, "no such user"),
        Err(err) => {
            tracing::warn!(%err, "failed to check for an existing user before renaming");
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "failed to rename user");
        }
    }

    if new_username == username {
        return StatusCode::NO_CONTENT.into_response();
    }

    match db::user_exists(&state.db, new_username).await {
        Ok(true) => {
            return error_response(
                StatusCode::CONFLICT,
                "a user with that username already exists",
            );
        }
        Ok(false) => {}
        Err(err) => {
            tracing::warn!(%err, "failed to check for a username conflict before renaming");
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "failed to rename user");
        }
    }

    match db::rename_user(&state.db, &username, new_username).await {
        Ok(()) => {
            state.sessions.rename(&username, new_username);
            StatusCode::NO_CONTENT.into_response()
        }
        Err(err) => {
            tracing::warn!(%err, "failed to rename user");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "failed to rename user")
        }
    }
}

#[derive(Deserialize)]
struct SetGuildRequest {
    /// Absent or null clears the pin, letting the account reach every guild
    /// the bot is in again.
    #[serde(default)]
    guild_id: Option<String>,
}

/// Pins an account to a single guild, or clears that pin. Admin-only, and
/// deliberately without the self-service path `set_password`/`set_username`
/// have: an account changing its own guild would lift the only restriction
/// holding it. Any existing session for the account is revoked, since the
/// guild it may reach is resolved at login and cached there.
async fn set_guild(
    State(state): State<WebState>,
    Path(username): Path<String>,
    Json(body): Json<SetGuildRequest>,
) -> Response {
    let guild_id = body
        .guild_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty());
    if let Some(id) = guild_id
        && parse_guild_id(id).is_none()
    {
        return error_response(StatusCode::BAD_REQUEST, "invalid guild id");
    }

    match db::user_exists(&state.db, &username).await {
        Ok(true) => {}
        Ok(false) => return error_response(StatusCode::NOT_FOUND, "no such user"),
        Err(err) => {
            tracing::warn!(%err, "failed to check for an existing user before setting its guild");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to set the user's guild",
            );
        }
    }

    match db::set_user_guild(&state.db, &username, guild_id).await {
        Ok(()) => {
            state.sessions.revoke_user(&username);
            StatusCode::NO_CONTENT.into_response()
        }
        Err(err) => {
            tracing::warn!(%err, "failed to set the user's guild");
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to set the user's guild",
            )
        }
    }
}
