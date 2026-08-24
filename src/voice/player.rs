//! Per-guild playback state: an in-memory queue driven by songbird's
//! track-end event, plus idle auto-disconnect.
//!
//! [`PlayerRegistry`] is the single shared entry point commands use — it
//! owns the songbird manager handle and all per-guild queues, so every
//! command (`/play`, `/skip`, `/queue`, ...) goes through the same state
//! rather than each reaching into songbird directly.
//!
//! Constructed once in `main.rs` and shared via `Data::player`.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, GuildId, UserId};
use songbird::tracks::TrackHandle;
use songbird::{Call, Event, EventContext, EventHandler as SongbirdEventHandler, Songbird, TrackEvent};
use tokio::sync::Mutex;

use crate::voice::resolve;
use crate::youtube::api::Track;

/// How long an empty, drained queue waits before the bot leaves the voice
/// channel on its own. Re-checked when the timer fires (not just scheduled
/// once) so a track queued in the meantime cancels the disconnect.
const IDLE_DISCONNECT: Duration = Duration::from_secs(5 * 60);

/// A track paired with who queued it.
#[derive(Debug, Clone)]
pub struct QueuedTrack {
    pub track: Track,
    /// Not displayed by any command yet — Phase 7's now-playing embeds
    /// ("title, thumbnail, requester, progress") are what read this.
    #[allow(dead_code)]
    pub requested_by: UserId,
}

#[derive(Debug)]
pub enum PlayerError {
    /// No active voice connection for this guild (caller should `/join` first).
    NotConnected,
    /// `/skip`, `/pause`, `/resume`, or `/stop` with nothing currently playing.
    NothingPlaying,
    Join(String),
    Playback(String),
}

impl std::fmt::Display for PlayerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlayerError::NotConnected => write!(f, "not connected to a voice channel"),
            PlayerError::NothingPlaying => write!(f, "nothing is playing"),
            PlayerError::Join(message) => write!(f, "failed to join voice channel: {message}"),
            PlayerError::Playback(message) => write!(f, "playback error: {message}"),
        }
    }
}

impl std::error::Error for PlayerError {}

#[derive(Default)]
struct GuildState {
    queue: VecDeque<QueuedTrack>,
    now_playing: Option<QueuedTrack>,
    current_handle: Option<TrackHandle>,
}

/// A point-in-time view of a guild's queue, for `/queue` and `/nowplaying`.
pub struct QueueSnapshot {
    pub now_playing: Option<QueuedTrack>,
    pub upcoming: Vec<QueuedTrack>,
}

/// Shared, per-process registry of per-guild playback state. Cheap to
/// clone (every field is `Arc`-backed) — lives in [`crate::commands::Data`]
/// so every command shares the same songbird manager and queues.
#[derive(Clone)]
pub struct PlayerRegistry {
    songbird: Arc<Songbird>,
    http: oauth2::reqwest::Client,
    cookies_file: Option<String>,
    guilds: Arc<Mutex<HashMap<GuildId, GuildState>>>,
}

impl PlayerRegistry {
    pub fn new(
        songbird: Arc<Songbird>,
        http: oauth2::reqwest::Client,
        cookies_file: Option<String>,
    ) -> Self {
        Self {
            songbird,
            http,
            cookies_file,
            guilds: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn join(
        &self,
        guild_id: GuildId,
        voice_channel_id: ChannelId,
    ) -> Result<(), PlayerError> {
        self.songbird
            .join(guild_id, voice_channel_id)
            .await
            .map_err(|e| PlayerError::Join(e.to_string()))?;
        Ok(())
    }

    /// Leaves voice and drops all queued/now-playing state for the guild.
    pub async fn leave(&self, guild_id: GuildId) -> Result<(), PlayerError> {
        self.songbird
            .leave(guild_id)
            .await
            .map_err(|e| PlayerError::Join(e.to_string()))?;
        self.guilds.lock().await.remove(&guild_id);
        Ok(())
    }

    pub fn is_connected(&self, guild_id: GuildId) -> bool {
        self.songbird.get(guild_id).is_some()
    }

    /// Enqueues a track. If nothing is currently playing, starts it
    /// immediately instead of leaving it queued.
    pub async fn enqueue(
        &self,
        guild_id: GuildId,
        queued: QueuedTrack,
    ) -> Result<(), PlayerError> {
        let call = self
            .songbird
            .get(guild_id)
            .ok_or(PlayerError::NotConnected)?;

        let should_start = {
            let mut guilds = self.guilds.lock().await;
            let state = guilds.entry(guild_id).or_default();
            let should_start = state.now_playing.is_none();
            if should_start {
                state.now_playing = Some(queued.clone());
            } else {
                state.queue.push_back(queued.clone());
            }
            should_start
        };

        if should_start {
            self.start_playback(guild_id, call, queued).await?;
        }

        Ok(())
    }

    async fn start_playback(
        &self,
        guild_id: GuildId,
        call: Arc<Mutex<Call>>,
        queued: QueuedTrack,
    ) -> Result<(), PlayerError> {
        let input = resolve::track_input(
            self.http.clone(),
            &queued.track.video_id,
            self.cookies_file.as_deref(),
        );

        let handle = {
            let mut call = call.lock().await;
            call.play_input(input)
        };

        // Best-effort: if registering the end-of-track hook itself fails,
        // the track still plays, it just won't auto-advance the queue —
        // surfacing that as a playback failure would be misleading.
        if let Err(err) = handle.add_event(
            Event::Track(TrackEvent::End),
            TrackEndHandler {
                registry: self.clone(),
                guild_id,
            },
        ) {
            tracing::warn!(%err, "failed to register track-end handler");
        }

        let mut guilds = self.guilds.lock().await;
        if let Some(state) = guilds.get_mut(&guild_id) {
            state.current_handle = Some(handle);
        }

        Ok(())
    }

    /// Advances to the next queued track, or — if the queue is empty —
    /// schedules an idle-timeout disconnect. Called from [`TrackEndHandler`]
    /// whenever a track ends, whether naturally or via `/skip`/`/stop`.
    async fn advance(&self, guild_id: GuildId) {
        let next = {
            let mut guilds = self.guilds.lock().await;
            let Some(state) = guilds.get_mut(&guild_id) else {
                return;
            };
            state.current_handle = None;
            state.now_playing = state.queue.pop_front();
            state.now_playing.clone()
        };

        match next {
            Some(queued) => {
                if let Some(call) = self.songbird.get(guild_id)
                    && let Err(err) = self.start_playback(guild_id, call, queued).await
                {
                    tracing::warn!(%err, "failed to start next queued track");
                }
            }
            None => self.schedule_idle_disconnect(guild_id),
        }
    }

    fn schedule_idle_disconnect(&self, guild_id: GuildId) {
        let registry = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(IDLE_DISCONNECT).await;

            // Re-check rather than disconnecting unconditionally: something
            // may have been queued (or `/leave` already ran) while we slept.
            let still_idle = {
                let guilds = registry.guilds.lock().await;
                guilds
                    .get(&guild_id)
                    .is_none_or(|state| state.now_playing.is_none())
            };

            if still_idle {
                let _ = registry.leave(guild_id).await;
            }
        });
    }

    /// Stops the current track and clears the queue (does not leave voice).
    pub async fn stop(&self, guild_id: GuildId) -> Result<(), PlayerError> {
        let handle = {
            let mut guilds = self.guilds.lock().await;
            let Some(state) = guilds.get_mut(&guild_id) else {
                return Err(PlayerError::NothingPlaying);
            };
            state.queue.clear();
            state.now_playing = None;
            state.current_handle.take()
        };

        let Some(handle) = handle else {
            return Err(PlayerError::NothingPlaying);
        };

        // Stopping fires a Track::End event, but `now_playing`/`current_handle`
        // are already cleared above, so `advance()` will see an empty queue
        // and just (re-)schedule the idle timer rather than double-advance.
        handle
            .stop()
            .map_err(|e| PlayerError::Playback(e.to_string()))
    }

    /// Stops the current track, which triggers the queue to auto-advance to
    /// the next one via the existing track-end handler.
    pub async fn skip(&self, guild_id: GuildId) -> Result<(), PlayerError> {
        let handle = {
            let guilds = self.guilds.lock().await;
            guilds
                .get(&guild_id)
                .and_then(|state| state.current_handle.clone())
        };
        let handle = handle.ok_or(PlayerError::NothingPlaying)?;
        handle
            .stop()
            .map_err(|e| PlayerError::Playback(e.to_string()))
    }

    pub async fn pause(&self, guild_id: GuildId) -> Result<(), PlayerError> {
        let handle = {
            let guilds = self.guilds.lock().await;
            guilds
                .get(&guild_id)
                .and_then(|state| state.current_handle.clone())
        };
        let handle = handle.ok_or(PlayerError::NothingPlaying)?;
        handle
            .pause()
            .map_err(|e| PlayerError::Playback(e.to_string()))
    }

    pub async fn resume(&self, guild_id: GuildId) -> Result<(), PlayerError> {
        let handle = {
            let guilds = self.guilds.lock().await;
            guilds
                .get(&guild_id)
                .and_then(|state| state.current_handle.clone())
        };
        let handle = handle.ok_or(PlayerError::NothingPlaying)?;
        handle
            .play()
            .map_err(|e| PlayerError::Playback(e.to_string()))
    }

    pub async fn queue_snapshot(&self, guild_id: GuildId) -> QueueSnapshot {
        let guilds = self.guilds.lock().await;
        match guilds.get(&guild_id) {
            Some(state) => QueueSnapshot {
                now_playing: state.now_playing.clone(),
                upcoming: state.queue.iter().cloned().collect(),
            },
            None => QueueSnapshot {
                now_playing: None,
                upcoming: Vec::new(),
            },
        }
    }
}

struct TrackEndHandler {
    registry: PlayerRegistry,
    guild_id: GuildId,
}

#[async_trait::async_trait]
impl SongbirdEventHandler for TrackEndHandler {
    async fn act(&self, _ctx: &EventContext<'_>) -> Option<Event> {
        self.registry.advance(self.guild_id).await;
        None
    }
}
