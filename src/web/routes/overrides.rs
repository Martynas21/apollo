use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;

use crate::db;
use crate::web::WebState;
use crate::web::response::{
    TrackOverrideJson, error_response, parse_guild_id, track_override_json,
};

pub fn routes() -> Router<WebState> {
    Router::new()
        .route("/api/guilds/{guild_id}/overrides", get(list_overrides))
        .route(
            "/api/guilds/{guild_id}/overrides/{video_id}/replace",
            post(change_override),
        )
        .route(
            "/api/guilds/{guild_id}/overrides/{video_id}/remove",
            post(remove_override),
        )
}

async fn overrides_response(state: &WebState, guild_id: &str) -> Response {
    match db::list_track_overrides(&state.db, guild_id).await {
        Ok(overrides) => {
            let overrides: Vec<TrackOverrideJson> =
                overrides.iter().map(track_override_json).collect();
            Json(overrides).into_response()
        }
        Err(err) => {
            tracing::warn!(%guild_id, %err, "dashboard failed to list track overrides");
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to load overrides",
            )
        }
    }
}

async fn list_overrides(State(state): State<WebState>, Path(guild_id): Path<String>) -> Response {
    let Some(guild_id) = parse_guild_id(&guild_id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid guild id");
    };
    overrides_response(&state, &guild_id.to_string()).await
}

#[derive(Deserialize)]
struct ChangeOverrideRequest {
    video_id: String,
}

/// Points an existing override at another track. Nothing is queued: the
/// change only affects what plays the next time the original comes up.
async fn change_override(
    State(state): State<WebState>,
    Path((guild_id, original)): Path<(String, String)>,
    Json(body): Json<ChangeOverrideRequest>,
) -> Response {
    let Some(guild_id) = parse_guild_id(&guild_id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid guild id");
    };
    if body.video_id == original {
        return error_response(
            StatusCode::BAD_REQUEST,
            "pick a different track to replace this one with",
        );
    }
    let guild_id = guild_id.to_string();
    let existing = match db::list_track_overrides(&state.db, &guild_id).await {
        Ok(overrides) => overrides
            .into_iter()
            .find(|mapping| mapping.original.video_id == original),
        Err(err) => {
            tracing::warn!(%guild_id, %err, "dashboard failed to look up a track override");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to load overrides",
            );
        }
    };
    let Some(existing) = existing else {
        return error_response(StatusCode::NOT_FOUND, "override not found");
    };
    let track = match state.youtube.get_video(&body.video_id).await {
        Ok(track) => track,
        Err(err) => return error_response(StatusCode::BAD_REQUEST, err.to_string()),
    };
    if let Err(err) =
        db::save_track_override(&state.db, &guild_id, &existing.original, &track).await
    {
        tracing::warn!(%guild_id, %err, "dashboard failed to change a track override");
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, "failed to save override");
    }
    overrides_response(&state, &guild_id).await
}

async fn remove_override(
    State(state): State<WebState>,
    Path((guild_id, original)): Path<(String, String)>,
) -> Response {
    let Some(guild_id) = parse_guild_id(&guild_id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid guild id");
    };
    let guild_id = guild_id.to_string();
    match db::delete_track_override(&state.db, &guild_id, &original).await {
        Ok(true) => overrides_response(&state, &guild_id).await,
        Ok(false) => error_response(StatusCode::NOT_FOUND, "override not found"),
        Err(err) => {
            tracing::warn!(%guild_id, %err, "dashboard failed to remove a track override");
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to remove override",
            )
        }
    }
}
