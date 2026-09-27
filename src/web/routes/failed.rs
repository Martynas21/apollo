use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::post;
use serde::Deserialize;

use crate::db;
use crate::model::{QueuedTrack, Track};
use crate::web::WebState;
use crate::web::response::{error_response, parse_guild_id};
use crate::web::routes::playback::{respond_after, snapshot_response};

pub fn routes() -> Router<WebState> {
    Router::new()
        .route(
            "/api/guilds/{guild_id}/failed/{video_id}/dismiss",
            post(dismiss_failed),
        )
        .route(
            "/api/guilds/{guild_id}/failed/{video_id}/replace",
            post(replace_failed),
        )
}

/// Skips the failed track for good: it is saved as a skip override, so a
/// playlist play leaves it out from now on, and it comes off the list.
async fn dismiss_failed(
    State(state): State<WebState>,
    Path((guild_id, video_id)): Path<(String, String)>,
) -> Response {
    let Some(guild_id) = parse_guild_id(&guild_id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid guild id");
    };
    let guild_id_str = guild_id.to_string();
    let skipped = replaced_track(&state, &guild_id_str, &video_id).await;
    if let Err(err) = db::save_skip_override(&state.db, &guild_id_str, &skipped).await {
        tracing::warn!(%guild_id, %err, "dashboard failed to save a skip override");
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, "failed to skip track");
    }
    if let Err(err) = db::delete_failed_track(&state.db, &guild_id_str, &video_id).await {
        tracing::warn!(%guild_id, %err, "dashboard failed to dismiss a failed track");
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, "failed to skip track");
    }
    snapshot_response(&state.player, guild_id).await
}

/// The failed track as it was recorded, kept as the override's fallback
/// name for when no saved playlist holds the original; a track no longer
/// listed is saved unnamed.
async fn replaced_track(state: &WebState, guild_id: &str, video_id: &str) -> Track {
    let recorded = match db::failed_tracks(&state.db, guild_id).await {
        Ok(failed) => failed
            .into_iter()
            .find(|failed| failed.track.video_id == video_id)
            .map(|failed| failed.track),
        Err(err) => {
            tracing::warn!(%guild_id, %err, "dashboard failed to look up the replaced track");
            None
        }
    };
    recorded.unwrap_or_else(|| Track {
        video_id: video_id.to_string(),
        title: String::new(),
        channel: String::new(),
        duration: None,
    })
}

#[derive(Deserialize)]
struct ReplaceFailedRequest {
    video_id: String,
}

/// Saves the chosen track as the guild's stand-in for the failed one and
/// queues it next. The override is stored before the queueing, so a retry
/// after joining voice only has to queue.
async fn replace_failed(
    State(state): State<WebState>,
    Path((guild_id, original)): Path<(String, String)>,
    Json(body): Json<ReplaceFailedRequest>,
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
    let track = match state.youtube.get_video(&body.video_id).await {
        Ok(track) => track,
        Err(err) => return error_response(StatusCode::BAD_REQUEST, err.to_string()),
    };

    let guild_id_str = guild_id.to_string();
    let replaced = replaced_track(&state, &guild_id_str, &original).await;
    if let Err(err) = db::save_track_override(&state.db, &guild_id_str, &replaced, &track).await {
        tracing::warn!(%guild_id, %err, "dashboard failed to save a track replacement");
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to save replacement",
        );
    }
    if let Err(err) = db::delete_failed_track(&state.db, &guild_id_str, &original).await {
        tracing::warn!(%guild_id, %err, "dashboard failed to clear the replaced track");
    }

    let queued = QueuedTrack {
        track,
        requested_by: state.cache.current_user().id,
    };
    let result = state.player.enqueue_next(guild_id, queued).await;
    respond_after(&state.player, guild_id, result).await
}
