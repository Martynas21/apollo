//! The wire protocol between `apollo` and `apollo-audio-worker`: one
//! full-duplex framed stream carrying request/response pairs (correlated by
//! `id`) and server-pushed events, all wrapped in [`Envelope`].

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::dto::{ConnectionInfoDto, TrackStatusDto};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Envelope {
    Request { id: u64, body: Request },
    Response { id: u64, body: Result<Response, String> },
    Event(Event),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    Join {
        guild_id: u64,
        info: ConnectionInfoDto,
    },
    Leave {
        guild_id: u64,
    },
    Play {
        guild_id: u64,
        track_id: Uuid,
        audio_path: String,
    },
    Pause {
        guild_id: u64,
        track_id: Uuid,
    },
    Resume {
        guild_id: u64,
        track_id: Uuid,
    },
    Stop {
        guild_id: u64,
        track_id: Uuid,
    },
    SetVolume {
        guild_id: u64,
        track_id: Uuid,
        multiplier: f32,
    },
    Status {
        guild_id: u64,
        track_id: Uuid,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    Ok,
    Status(TrackStatusDto),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    TrackFinished { guild_id: u64, track_id: Uuid },
    TrackErrored { guild_id: u64, track_id: Uuid, error: String },
    ConnectionLost { guild_id: u64 },
}
