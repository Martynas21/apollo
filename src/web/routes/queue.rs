use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::post;
use serde::Deserialize;

use crate::model::QueuedTrack;
use crate::web::WebState;
use crate::web::response::{error_response, parse_guild_id};
use crate::web::routes::playback::respond_after;

pub fn routes() -> Router<WebState> {
    Router::new()
        .route(
            "/api/guilds/{guild_id}/queue/{index}/remove",
            post(remove_queue_track),
        )
        .route(
            "/api/guilds/{guild_id}/queue/{index}/play",
            post(play_queue_track),
        )
        .route(
            "/api/guilds/{guild_id}/queue/{index}/move",
            post(move_queue_track),
        )
        .route("/api/guilds/{guild_id}/queue/clear", post(clear_queue))
        .route("/api/guilds/{guild_id}/queue/add", post(add_to_queue))
}

async fn remove_queue_track(
    State(state): State<WebState>,
    Path((guild_id, index)): Path<(String, usize)>,
) -> Response {
    let Some(guild_id) = parse_guild_id(&guild_id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid guild id");
    };
    let result = state.player.remove_queue_track(guild_id, index).await;
    respond_after(&state.player, guild_id, result).await
}

async fn play_queue_track(
    State(state): State<WebState>,
    Path((guild_id, index)): Path<(String, usize)>,
) -> Response {
    let Some(guild_id) = parse_guild_id(&guild_id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid guild id");
    };
    let result = state.player.play_queue_track(guild_id, index).await;
    respond_after(&state.player, guild_id, result).await
}

#[derive(Deserialize)]
struct MoveQueueTrackRequest {
    to: usize,
}

async fn move_queue_track(
    State(state): State<WebState>,
    Path((guild_id, index)): Path<(String, usize)>,
    Json(body): Json<MoveQueueTrackRequest>,
) -> Response {
    let Some(guild_id) = parse_guild_id(&guild_id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid guild id");
    };
    let result = state
        .player
        .move_queue_track(guild_id, index, body.to)
        .await;
    respond_after(&state.player, guild_id, result).await
}

async fn clear_queue(State(state): State<WebState>, Path(guild_id): Path<String>) -> Response {
    let Some(guild_id) = parse_guild_id(&guild_id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid guild id");
    };
    let result = state.player.clear_queue(guild_id).await;
    respond_after(&state.player, guild_id, result).await
}

#[derive(Deserialize)]
struct AddToQueueRequest {
    video_id: String,
}

async fn add_to_queue(
    State(state): State<WebState>,
    Path(guild_id): Path<String>,
    Json(body): Json<AddToQueueRequest>,
) -> Response {
    let Some(guild_id) = parse_guild_id(&guild_id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid guild id");
    };

    let track = match state.youtube.get_video(&body.video_id).await {
        Ok(track) => track,
        Err(err) => return error_response(StatusCode::BAD_REQUEST, err.to_string()),
    };

    let queued = QueuedTrack {
        track,
        requested_by: state.cache.current_user().id,
    };
    let result = state.player.enqueue_next(guild_id, queued).await;
    respond_after(&state.player, guild_id, result).await
}
