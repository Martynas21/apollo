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

mod auth;
mod response;
mod routes;

use std::sync::Arc;

use axum::Router;
use axum::middleware::{from_fn, from_fn_with_state};
use axum::routing::get;
use serenity::all as serenity;

pub use auth::bootstrap_user_if_needed;

const DASHBOARD_HTML: &str = include_str!("../../assets/dashboard.html");

#[derive(Clone)]
pub struct WebState {
    pub player: crate::voice::PlayerRegistry,
    pub youtube: crate::youtube::api::YouTubeClient,
    pub db: sqlx::SqlitePool,
    pub cache: Arc<serenity::Cache>,
    pub sessions: auth::SessionStore,
}

impl WebState {
    pub fn new(
        player: crate::voice::PlayerRegistry,
        youtube: crate::youtube::api::YouTubeClient,
        db: sqlx::SqlitePool,
        cache: Arc<serenity::Cache>,
    ) -> Self {
        Self {
            player,
            youtube,
            db,
            cache,
            sessions: auth::SessionStore::default(),
        }
    }
}

/// Builds the fully-wired dashboard `Router`, with every middleware layer
/// applied but no listener bound yet — the seam `serve()` runs on, and the
/// one integration tests drive directly via `tower::ServiceExt::oneshot`.
pub fn router(state: WebState) -> Router {
    // `require_admin` needs `require_session` to have already resolved the
    // caller's identity into the request's extensions, so it's layered onto
    // `admin_routes()` alone before that merges into `protected` — the
    // outer `route_layer(require_session)` below then wraps the whole
    // merged router, running first on every request.
    let admin_only = routes::users::admin_routes().route_layer(from_fn(auth::require_admin));

    let protected = routes::guilds::routes()
        .merge(routes::playback::routes())
        .merge(routes::queue::routes())
        .merge(routes::search::routes())
        .merge(routes::playlists::routes())
        .merge(routes::favourites::routes())
        .merge(routes::users::routes())
        .merge(admin_only)
        .route_layer(from_fn_with_state(state.clone(), auth::require_session));

    let public = Router::new()
        .route("/", get(|| async { axum::response::Html(DASHBOARD_HTML) }))
        .merge(routes::auth::routes());

    public.merge(protected).with_state(state)
}

pub async fn serve(bind_addr: &str, state: WebState) -> anyhow::Result<()> {
    let app = router(state);

    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    tracing::info!(%bind_addr, "dashboard listening");
    axum::serve(listener, app).await?;
    Ok(())
}
