use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionInfoDto {
    pub guild_id: u64,
    pub channel_id: u64,
    pub endpoint: String,
    pub session_id: String,
    pub token: String,
    pub user_id: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct TrackStatusDto {
    pub position_ms: u64,
    pub paused: bool,
}
