use std::sync::Arc;
use std::time::Duration;

use serenity::all::{ChannelId, GuildId};
use uuid::Uuid;

use crate::model::Track;
use crate::voice::resolve::PlaybackError;

pub struct AudioSource {
    pub(crate) video_id: String,
    pub(crate) url: String,
    pub(crate) headers: Vec<(String, String)>,
}

pub struct TrackStatus {
    pub position: Duration,
    pub paused: bool,
}

#[async_trait::async_trait]
pub trait VoiceEvents: Send + Sync + 'static {
    async fn track_finished(&self, guild_id: GuildId, track_id: Uuid);

    async fn connection_lost(&self, guild_id: GuildId);
}

#[async_trait::async_trait]
pub trait VoiceBackend: Send + Sync + 'static {
    async fn join(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        events: Arc<dyn VoiceEvents>,
    ) -> Result<(), String>;

    async fn remove(&self, guild_id: GuildId) -> Result<(), String>;

    fn call(&self, guild_id: GuildId) -> Option<Arc<dyn VoiceCall>>;

    /// Returns the voice channel currently connected (or connecting) to for
    /// this guild, if any. Must be a cheap, local check — no IPC round trip
    /// — since it's used to decide whether a `join()` call is a no-op.
    async fn current_channel(&self, guild_id: GuildId) -> Option<ChannelId>;

    async fn buffered_source(&self, track: &Track) -> Result<AudioSource, PlaybackError>;
}

#[async_trait::async_trait]
pub trait VoiceCall: Send + Sync {
    async fn play(&self, source: AudioSource) -> Result<Arc<dyn VoiceTrack>, String>;
}

#[async_trait::async_trait]
pub trait VoiceTrack: Send + Sync {
    fn uuid(&self) -> Uuid;
    async fn set_volume(&self, multiplier: f32) -> Result<(), String>;
    async fn stop(&self) -> Result<(), String>;
    async fn pause(&self) -> Result<(), String>;
    async fn resume(&self) -> Result<(), String>;
    fn notify_when_finished(&self, guild_id: GuildId, events: Arc<dyn VoiceEvents>);
    async fn status(&self) -> Option<TrackStatus>;
}
