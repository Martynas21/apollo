use std::time::Duration;

use axum::Json;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use poise::serenity_prelude as serenity;
use serde::{Deserialize, Serialize};
use serenity::GuildId;

use crate::voice::player::{PlayerError, PlayerRegistry};
use crate::web::WebState;
use crate::web::auth;

const SNAPSHOT_PUSH_INTERVAL: Duration = Duration::from_millis(1500);

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

fn error_response(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(ErrorBody {
            error: message.into(),
        }),
    )
        .into_response()
}

fn parse_guild_id(raw: &str) -> Option<GuildId> {
    raw.parse::<u64>().ok().map(GuildId::new)
}

fn player_error_status(err: &PlayerError) -> StatusCode {
    match err {
        PlayerError::NothingPlaying | PlayerError::NothingToShuffle | PlayerError::QueueEmpty => {
            StatusCode::CONFLICT
        }
        PlayerError::NotConnected | PlayerError::Join(_) | PlayerError::InvalidQueueIndex => {
            StatusCode::BAD_REQUEST
        }
        PlayerError::Playback(_) | PlayerError::Storage(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

#[derive(Serialize)]
struct GuildJson {
    id: String,
    name: String,
}

#[derive(Serialize)]
struct TrackJson {
    title: String,
    channel: String,
    video_id: String,
    duration_secs: Option<u64>,
}

#[derive(Serialize)]
struct SnapshotJson {
    state: &'static str,
    track: Option<TrackJson>,
    volume: u8,
    radio_enabled: bool,
    upcoming: Vec<TrackJson>,
}

fn track_json(queued: &crate::voice::QueuedTrack) -> TrackJson {
    TrackJson {
        title: queued.track.title.clone(),
        channel: queued.track.channel.clone(),
        video_id: queued.track.video_id.clone(),
        duration_secs: queued.track.duration.map(|d| d.as_secs()),
    }
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

    SnapshotJson {
        state,
        track,
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

async fn respond_after(
    player: &PlayerRegistry,
    guild_id: GuildId,
    result: Result<(), PlayerError>,
) -> Response {
    match result {
        Ok(()) => snapshot_response(player, guild_id).await,
        Err(err) => player_error_response(err),
    }
}

#[derive(Deserialize)]
pub struct LoginRequest {
    username: String,
    password: String,
}

#[derive(Serialize)]
pub struct LoginResponse {
    token: String,
}

pub async fn login(State(state): State<WebState>, Json(body): Json<LoginRequest>) -> Response {
    match auth::verify_login(&state.db, &body.username, &body.password).await {
        Ok(true) => {
            let token = auth::issue_session(&state.sessions);
            Json(LoginResponse { token }).into_response()
        }
        Ok(false) => error_response(StatusCode::UNAUTHORIZED, "invalid username or password"),
        Err(err) => {
            tracing::warn!(%err, "dashboard login failed to check credentials");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "login failed")
        }
    }
}

pub async fn list_guilds(State(state): State<WebState>) -> Response {
    let guilds: Vec<GuildJson> = state
        .cache
        .guilds()
        .into_iter()
        .filter_map(|id| {
            state.cache.guild(id).map(|guild| GuildJson {
                id: id.to_string(),
                name: guild.name.clone(),
            })
        })
        .collect();
    Json(guilds).into_response()
}

pub async fn now_playing(State(state): State<WebState>, Path(guild_id): Path<String>) -> Response {
    let Some(guild_id) = parse_guild_id(&guild_id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid guild id");
    };
    snapshot_response(&state.player, guild_id).await
}

pub async fn now_playing_ws(
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

pub async fn toggle_pause(State(state): State<WebState>, Path(guild_id): Path<String>) -> Response {
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

pub async fn skip(State(state): State<WebState>, Path(guild_id): Path<String>) -> Response {
    let Some(guild_id) = parse_guild_id(&guild_id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid guild id");
    };
    let result = state.player.skip(guild_id).await;
    respond_after(&state.player, guild_id, result).await
}

pub async fn stop(State(state): State<WebState>, Path(guild_id): Path<String>) -> Response {
    let Some(guild_id) = parse_guild_id(&guild_id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid guild id");
    };
    let result = state.player.stop(guild_id).await;
    respond_after(&state.player, guild_id, result).await
}

pub async fn shuffle(State(state): State<WebState>, Path(guild_id): Path<String>) -> Response {
    let Some(guild_id) = parse_guild_id(&guild_id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid guild id");
    };
    let result = state.player.shuffle(guild_id).await;
    respond_after(&state.player, guild_id, result).await
}

pub async fn toggle_radio(State(state): State<WebState>, Path(guild_id): Path<String>) -> Response {
    let Some(guild_id) = parse_guild_id(&guild_id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid guild id");
    };
    state.player.toggle_radio(guild_id).await;
    snapshot_response(&state.player, guild_id).await
}

#[derive(Deserialize)]
pub struct VolumeRequest {
    level: u8,
}

pub async fn set_volume(
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

pub async fn remove_queue_track(
    State(state): State<WebState>,
    Path((guild_id, index)): Path<(String, usize)>,
) -> Response {
    let Some(guild_id) = parse_guild_id(&guild_id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid guild id");
    };
    let result = state.player.remove_queue_track(guild_id, index).await;
    respond_after(&state.player, guild_id, result).await
}

#[derive(Deserialize)]
pub struct MoveQueueTrackRequest {
    to: usize,
}

pub async fn move_queue_track(
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

pub async fn clear_queue(State(state): State<WebState>, Path(guild_id): Path<String>) -> Response {
    let Some(guild_id) = parse_guild_id(&guild_id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid guild id");
    };
    let result = state.player.clear_queue(guild_id).await;
    respond_after(&state.player, guild_id, result).await
}
