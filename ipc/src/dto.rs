//! Plain-data mirrors of the songbird types that cross the IPC boundary.
//!
//! `apollo-audio-worker` deliberately does not depend on `apollo`'s songbird
//! feature set (it only needs songbird's `driver` feature, not
//! `gateway`/`serenity`), so these are hand-written duals rather than
//! `#[derive(Serialize)]` on songbird's own types.

use serde::{Deserialize, Serialize};

/// Mirrors `songbird::ConnectionInfo` field-for-field. IDs travel as `u64`
/// (songbird's `GuildId`/`ChannelId`/`UserId` newtypes wrap `NonZeroU64`,
/// reconstructed on the worker side via `NonZeroU64::new`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionInfoDto {
    pub guild_id: u64,
    pub channel_id: u64,
    pub endpoint: String,
    pub session_id: String,
    pub token: String,
    pub user_id: u64,
}

/// Mirrors `songbird::tracks::TrackHandle::get_info()`'s relevant fields —
/// see `SongbirdTrack::status` in `apollo`'s `voice/player.rs`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct TrackStatusDto {
    pub position_ms: u64,
    pub paused: bool,
}
