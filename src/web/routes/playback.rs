use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use serenity::all::GuildId;

use crate::voice::{PlayerError, PlayerRegistry};
use crate::web::WebState;
use crate::web::response::{
    TrackJson, error_response, parse_guild_id, player_error_status, track_json,
};

const SNAPSHOT_PUSH_INTERVAL: Duration = Duration::from_millis(1500);

pub fn routes() -> Router<WebState> {
    Router::new()
        .route("/api/guilds/{guild_id}/now-playing", get(now_playing))
        .route("/api/guilds/{guild_id}/ws", get(now_playing_ws))
        .route("/api/guilds/{guild_id}/toggle-pause", post(toggle_pause))
        .route("/api/guilds/{guild_id}/skip", post(skip))
        .route("/api/guilds/{guild_id}/stop", post(stop))
        .route("/api/guilds/{guild_id}/shuffle", post(shuffle))
        .route("/api/guilds/{guild_id}/toggle-radio", post(toggle_radio))
        .route("/api/guilds/{guild_id}/volume", post(set_volume))
}

#[derive(Serialize)]
struct SnapshotJson {
    state: &'static str,
    track: Option<TrackJson>,
    position_ms: Option<u64>,
    volume: u8,
    radio_enabled: bool,
    upcoming: Vec<TrackJson>,
}

async fn build_snapshot(player: &PlayerRegistry, guild_id: GuildId) -> SnapshotJson {
    let snapshot = player.queue_snapshot(guild_id).await;
    let paused = player.is_paused(guild_id).await;
    let volume = player.get_volume(guild_id).await;
    let radio_enabled = player.is_radio_enabled(guild_id).await;

    let (state, track) = if let Some(queued) = &snapshot.now_playing {
        let state = if paused == Some(true) {
            "paused"
        } else {
            "playing"
        };
        (state, Some(track_json(queued)))
    } else if let Some(queued) = &snapshot.loading {
        ("buffering", Some(track_json(queued)))
    } else if let Some(queued) = &snapshot.last_played {
        ("queue_finished", Some(track_json(queued)))
    } else {
        ("empty", None)
    };

    let upcoming: Vec<TrackJson> = snapshot.upcoming.iter().map(track_json).collect();

    let position_ms = if matches!(state, "playing" | "paused") {
        player
            .track_position(guild_id)
            .await
            .map(|position| u64::try_from(position.as_millis()).unwrap_or(u64::MAX))
    } else {
        None
    };

    SnapshotJson {
        state,
        track,
        position_ms,
        volume,
        radio_enabled,
        upcoming,
    }
}

async fn snapshot_response(player: &PlayerRegistry, guild_id: GuildId) -> Response {
    Json(build_snapshot(player, guild_id).await).into_response()
}

fn player_error_response(err: PlayerError) -> Response {
    error_response(player_error_status(&err), err.to_string())
}

pub async fn respond_after(
    player: &PlayerRegistry,
    guild_id: GuildId,
    result: Result<(), PlayerError>,
) -> Response {
    match result {
        Ok(()) => snapshot_response(player, guild_id).await,
        Err(err) => player_error_response(err),
    }
}

async fn now_playing(State(state): State<WebState>, Path(guild_id): Path<String>) -> Response {
    let Some(guild_id) = parse_guild_id(&guild_id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid guild id");
    };
    snapshot_response(&state.player, guild_id).await
}

async fn now_playing_ws(
    State(state): State<WebState>,
    Path(guild_id): Path<String>,
    ws: WebSocketUpgrade,
) -> Response {
    let Some(guild_id) = parse_guild_id(&guild_id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid guild id");
    };
    ws.on_upgrade(move |socket| stream_now_playing(socket, state, guild_id))
}

async fn stream_now_playing(mut socket: WebSocket, state: WebState, guild_id: GuildId) {
    let mut interval = tokio::time::interval(SNAPSHOT_PUSH_INTERVAL);
    loop {
        tokio::select! {
            _ = interval.tick() => {
                let snapshot = build_snapshot(&state.player, guild_id).await;
                let Ok(text) = serde_json::to_string(&snapshot) else {
                    continue;
                };
                if socket.send(Message::Text(text.into())).await.is_err() {
                    return;
                }
            }
            message = socket.recv() => {
                match message {
                    Some(Ok(Message::Close(_))) | None => return,
                    Some(Err(_)) => return,
                    _ => {}
                }
            }
        }
    }
}

async fn toggle_pause(State(state): State<WebState>, Path(guild_id): Path<String>) -> Response {
    let Some(guild_id) = parse_guild_id(&guild_id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid guild id");
    };
    let result = match state.player.is_paused(guild_id).await {
        Some(true) => state.player.resume(guild_id).await,
        Some(false) => state.player.pause(guild_id).await,
        None => Err(PlayerError::NothingPlaying),
    };
    respond_after(&state.player, guild_id, result).await
}

async fn skip(State(state): State<WebState>, Path(guild_id): Path<String>) -> Response {
    let Some(guild_id) = parse_guild_id(&guild_id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid guild id");
    };
    let result = state.player.skip(guild_id).await;
    respond_after(&state.player, guild_id, result).await
}

async fn stop(State(state): State<WebState>, Path(guild_id): Path<String>) -> Response {
    let Some(guild_id) = parse_guild_id(&guild_id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid guild id");
    };
    let result = state.player.stop(guild_id).await;
    respond_after(&state.player, guild_id, result).await
}

async fn shuffle(State(state): State<WebState>, Path(guild_id): Path<String>) -> Response {
    let Some(guild_id) = parse_guild_id(&guild_id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid guild id");
    };
    let result = state.player.shuffle(guild_id).await;
    respond_after(&state.player, guild_id, result).await
}

async fn toggle_radio(State(state): State<WebState>, Path(guild_id): Path<String>) -> Response {
    let Some(guild_id) = parse_guild_id(&guild_id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid guild id");
    };
    state.player.toggle_radio(guild_id).await;
    snapshot_response(&state.player, guild_id).await
}

#[derive(Deserialize)]
struct VolumeRequest {
    level: u8,
}

async fn set_volume(
    State(state): State<WebState>,
    Path(guild_id): Path<String>,
    Json(body): Json<VolumeRequest>,
) -> Response {
    let Some(guild_id) = parse_guild_id(&guild_id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid guild id");
    };
    let result = state.player.set_volume(guild_id, body.level).await;
    respond_after(&state.player, guild_id, result).await
}
