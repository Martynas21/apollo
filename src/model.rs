//! Core domain types shared across the database, the voice/IPC layer and the
//! web dashboard. None of these are tied to yt-dlp — the yt-dlp adapter
//! (`youtube::api`) maps its own wire format into `Track`.

use std::time::Duration;

use serenity::all::UserId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    pub video_id: String,
    pub title: String,
    pub channel: String,
    pub duration: Option<Duration>,
}

pub struct PlaylistListing {
    pub title: Option<String>,
    pub tracks: Vec<Track>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedTrack {
    pub track: Track,
    pub requested_by: UserId,
}
