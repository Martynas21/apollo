//! Shared response-shaping helpers used across every route module: the JSON
//! error envelope, guild/channel id parsing from path segments, and the
//! `Track`-to-wire-format mapping.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serenity::all::{ChannelId, GuildId};

use crate::model::{QueuedTrack, Track};
use crate::voice::PlayerError;

#[derive(Serialize)]
pub struct ErrorBody {
    error: String,
}

pub fn error_response(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(ErrorBody {
            error: message.into(),
        }),
    )
        .into_response()
}

pub fn parse_guild_id(raw: &str) -> Option<GuildId> {
    raw.parse::<u64>().ok().map(GuildId::new)
}

pub fn parse_channel_id(raw: &str) -> Option<ChannelId> {
    raw.parse::<u64>().ok().map(ChannelId::new)
}

pub fn player_error_status(err: &PlayerError) -> StatusCode {
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
pub struct TrackJson {
    title: String,
    channel: String,
    video_id: String,
    duration_secs: Option<u64>,
}

pub fn track_json(queued: &QueuedTrack) -> TrackJson {
    TrackJson {
        title: queued.track.title.clone(),
        channel: queued.track.channel.clone(),
        video_id: queued.track.video_id.clone(),
        duration_secs: queued.track.duration.map(|d| d.as_secs()),
    }
}

pub fn track_json_from_track(track: &Track) -> TrackJson {
    TrackJson {
        title: track.title.clone(),
        channel: track.channel.clone(),
        video_id: track.video_id.clone(),
        duration_secs: track.duration.map(|d| d.as_secs()),
    }
}
