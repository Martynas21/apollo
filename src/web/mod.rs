//! The web dashboard: a small HTTP+WebSocket surface exposing "Now Playing"
//! state and full playback control for whichever of the bot's guilds the
//! dashboard's server switcher has selected. It's the only control surface —
//! Discord slash commands and the old `/player` panel have been removed.
//!
//! Covers: now-playing state and transport controls
//! (pause/resume/skip/stop/shuffle/radio/volume), queue management
//! (play/remove/reorder/clear), YouTube search/add-to-queue, saved-playlist
//! management (import/play/refresh/remove), and per-guild play-count
//! favourites.

mod api;
mod auth;
mod users;

use std::sync::Arc;

use axum::Router;
use axum::middleware::{from_fn, from_fn_with_state};
use axum::routing::{delete, get, post};
use serenity::all as serenity;

pub use auth::bootstrap_user_if_needed;

const DASHBOARD_HTML: &str = include_str!("dashboard.html");

#[derive(Clone)]
pub struct WebState {
    pub player: crate::voice::PlayerRegistry,
    pub db: sqlx::SqlitePool,
    pub cache: Arc<serenity::Cache>,
    pub sessions: auth::SessionStore,
}

impl WebState {
    pub fn new(
        player: crate::voice::PlayerRegistry,
        db: sqlx::SqlitePool,
        cache: Arc<serenity::Cache>,
    ) -> Self {
        Self {
            player,
            db,
            cache,
            sessions: auth::SessionStore::default(),
        }
    }
}

fn playback_routes() -> Router<WebState> {
    Router::new()
        .route("/api/guilds", get(api::list_guilds))
        .route(
            "/api/guilds/{guild_id}/voice-channels",
            get(api::list_voice_channels),
        )
        .route("/api/guilds/{guild_id}/join", post(api::join_voice_channel))
        .route("/api/guilds/{guild_id}/now-playing", get(api::now_playing))
        .route("/api/guilds/{guild_id}/ws", get(api::now_playing_ws))
        .route(
            "/api/guilds/{guild_id}/toggle-pause",
            post(api::toggle_pause),
        )
        .route("/api/guilds/{guild_id}/skip", post(api::skip))
        .route("/api/guilds/{guild_id}/stop", post(api::stop))
        .route("/api/guilds/{guild_id}/shuffle", post(api::shuffle))
        .route(
            "/api/guilds/{guild_id}/toggle-radio",
            post(api::toggle_radio),
        )
        .route("/api/guilds/{guild_id}/volume", post(api::set_volume))
        .route(
            "/api/guilds/{guild_id}/queue/{index}/remove",
            post(api::remove_queue_track),
        )
        .route(
            "/api/guilds/{guild_id}/queue/{index}/play",
            post(api::play_queue_track),
        )
        .route(
            "/api/guilds/{guild_id}/queue/{index}/move",
            post(api::move_queue_track),
        )
        .route("/api/guilds/{guild_id}/queue/clear", post(api::clear_queue))
        .route("/api/guilds/{guild_id}/search", get(api::search))
        .route("/api/guilds/{guild_id}/queue/add", post(api::add_to_queue))
        .route("/api/guilds/{guild_id}/favourites", get(api::favourites))
}

fn playlist_routes() -> Router<WebState> {
    Router::new()
        .route("/api/guilds/{guild_id}/playlists", get(api::list_playlists))
        .route(
            "/api/guilds/{guild_id}/playlists/import",
            post(api::import_playlist),
        )
        .route(
            "/api/guilds/{guild_id}/playlists/{playlist_id}/play",
            post(api::play_playlist),
        )
        .route(
            "/api/guilds/{guild_id}/playlists/{playlist_id}/refresh",
            post(api::refresh_playlist),
        )
        .route(
            "/api/guilds/{guild_id}/playlists/{playlist_id}/remove",
            post(api::remove_playlist),
        )
}

fn me_routes() -> Router<WebState> {
    Router::new().route("/api/me", get(users::me))
}

fn admin_routes() -> Router<WebState> {
    Router::new()
        .route(
            "/api/users",
            get(users::list_users).post(users::create_user),
        )
        .route("/api/users/{username}", delete(users::delete_user))
        .route("/api/users/{username}/password", post(users::set_password))
        .route("/api/users/{username}/username", post(users::set_username))
}

pub async fn serve(bind_addr: &str, state: WebState) -> anyhow::Result<()> {
    // `require_admin` needs `require_session` to have already resolved the
    // caller's identity into the request's extensions, so it's layered onto
    // `admin_routes()` alone before that merges into `protected` — the
    // outer `route_layer(require_session)` below then wraps the whole
    // merged router, running first on every request.
    let admin_only = admin_routes().route_layer(from_fn(auth::require_admin));

    let protected = playback_routes()
        .merge(playlist_routes())
        .merge(me_routes())
        .merge(admin_only)
        .route_layer(from_fn_with_state(state.clone(), auth::require_session));

    let public = Router::new()
        .route("/", get(|| async { axum::response::Html(DASHBOARD_HTML) }))
        .route("/api/login", post(api::login));

    let app = public.merge(protected).with_state(state);

    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    tracing::info!(%bind_addr, "dashboard listening");
    axum::serve(listener, app).await?;
    Ok(())
}
