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
use rand::seq::SliceRandom;
use serenity::{ChannelId, GuildId, MessageId, UserId};
use songbird::input::Input;
use songbird::tracks::{PlayMode, TrackHandle};
use songbird::{
    Call, Event, EventContext, EventHandler as SongbirdEventHandler, Songbird, TrackEvent,
};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::db;
use crate::voice::resolve::{self, PlaybackError};
use crate::youtube::api::Track;

/// A background pre-buffer for the track that's up next in the queue,
/// started as soon as the current track begins playing so its download
/// finishes (or is well underway) by the time it's actually needed. See
/// `cached_track_input`.
type Prefetch = JoinHandle<Result<Input, PlaybackError>>;

/// How long an empty, drained queue waits before the bot leaves the voice
/// channel on its own. Re-checked when the timer fires (not just scheduled
/// once) so a track queued in the meantime cancels the disconnect.
const IDLE_DISCONNECT: Duration = Duration::from_mins(5);

/// A track paired with who queued it.
#[derive(Debug, Clone)]
pub struct QueuedTrack {
    pub track: Track,
    pub requested_by: UserId,
}

#[derive(Debug)]
pub enum PlayerError {
    /// No active voice connection for this guild (caller should `/play` first).
    NotConnected,
    /// `/skip`, `/pause`, `/resume`, or `/stop` with nothing currently playing.
    NothingPlaying,
    /// `/shuffle` with fewer than two upcoming tracks to shuffle.
    NothingToShuffle,
    /// A queue-jump selection that's out of range, e.g. because the queue
    /// changed between the panel being rendered and the click landing.
    InvalidSelection,
    Join(String),
    Playback(String),
    /// Persisting a `/volume` change to the database failed.
    Storage(String),
}

impl std::fmt::Display for PlayerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConnected => write!(f, "not connected to a voice channel"),
            Self::NothingPlaying => write!(f, "nothing is playing"),
            Self::NothingToShuffle => write!(f, "not enough upcoming tracks to shuffle"),
            Self::InvalidSelection => {
                write!(
                    f,
                    "that queue selection is no longer valid — the queue may have changed"
                )
            }
            Self::Join(message) => write!(f, "failed to join voice channel: {message}"),
            Self::Playback(message) => write!(f, "playback error: {message}"),
            Self::Storage(message) => write!(f, "failed to save setting: {message}"),
        }
    }
}

impl std::error::Error for PlayerError {}

#[derive(Default)]
struct GuildState {
    queue: VecDeque<QueuedTrack>,
    now_playing: Option<QueuedTrack>,
    current_handle: Option<TrackHandle>,
    /// Background pre-buffer for `queue`'s front entry, started right after
    /// the current track begins playing. Always corresponds to whatever is
    /// at the front of `queue` — `enqueue`/`advance`/`stop` are the only
    /// places that mutate the queue, and each keeps this in sync.
    prefetch: Option<Prefetch>,
    /// The live `/player` panel message for this guild, if one has been
    /// posted — kept current by [`PlayerRegistry::refresh_panel`] after
    /// every state-changing mutation.
    panel: Option<(ChannelId, MessageId)>,
}

/// A point-in-time view of a guild's queue, for `/queue` and `/now_playing`.
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
    /// Used only to edit/delete the live `/player` panel message from
    /// contexts that aren't already handling a Discord interaction (e.g.
    /// [`TrackEndHandler`], or a `/skip` slash command refreshing a panel
    /// message it didn't itself respond to). Unrelated to `http` above,
    /// which is for yt-dlp/YouTube stream resolution.
    discord_http: Arc<serenity::Http>,
    cookies_file: Option<String>,
    db: sqlx::SqlitePool,
    guilds: Arc<Mutex<HashMap<GuildId, GuildState>>>,
}

impl PlayerRegistry {
    pub fn new(
        songbird: Arc<Songbird>,
        http: oauth2::reqwest::Client,
        discord_http: Arc<serenity::Http>,
        cookies_file: Option<String>,
        db: sqlx::SqlitePool,
    ) -> Self {
        Self {
            songbird,
            http,
            discord_http,
            cookies_file,
            db,
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

    /// Leaves voice and drops all queued/now-playing state for the guild. If
    /// a `/player` panel is live, it gets one last edit to reflect that
    /// nothing is playing anymore — otherwise it would freeze showing
    /// whatever was playing right before disconnect (notably including the
    /// common case of the idle-timeout auto-disconnect in
    /// [`Self::schedule_idle_disconnect`]).
    pub async fn leave(&self, guild_id: GuildId) -> Result<(), PlayerError> {
        self.songbird
            .leave(guild_id)
            .await
            .map_err(|e| PlayerError::Join(e.to_string()))?;

        let panel = self
            .guilds
            .lock()
            .await
            .remove(&guild_id)
            .and_then(|state| state.panel);
        if let Some((channel_id, message_id)) = panel {
            let _ = self.edit_panel(guild_id, channel_id, message_id).await;
        }
        Ok(())
    }

    pub fn is_connected(&self, guild_id: GuildId) -> bool {
        self.songbird.get(guild_id).is_some()
    }

    /// Enqueues a track. If nothing is currently playing, starts it
    /// immediately instead of leaving it queued.
    pub async fn enqueue(&self, guild_id: GuildId, queued: QueuedTrack) -> Result<(), PlayerError> {
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

        if should_start && let Err(err) = self.start_playback(guild_id, call, queued, None).await {
            // `state.now_playing` was set speculatively above, before the
            // resolve/play attempt above was known to succeed. Roll it back
            // on failure — otherwise it's left pointing at a track with no
            // `current_handle` and no `TrackEndHandler` ever registered to
            // advance past it, permanently orphaning every track queued
            // behind it (they just see `now_playing.is_some()` and pile up
            // in `state.queue` instead of ever being tried).
            let mut guilds = self.guilds.lock().await;
            if let Some(state) = guilds.get_mut(&guild_id) {
                state.now_playing = None;
            }
            return Err(err);
        }

        self.refresh_panel(guild_id).await;
        Ok(())
    }

    /// Resolves a prefetch handle into a playable input, falling back to
    /// building fresh (rather than propagating a stale prefetch failure) if
    /// the background download errored.
    async fn resolve_prefetched(&self, queued: &QueuedTrack, prefetched: Prefetch) -> Input {
        match prefetched.await {
            Ok(Ok(input)) => return input,
            Ok(Err(err)) => tracing::warn!(%err, "prefetch failed, resolving fresh instead"),
            Err(err) => tracing::warn!(%err, "prefetch task panicked, resolving fresh instead"),
        }
        self.cached_input(queued).await.unwrap_or_else(|_| {
            // `cached_track_input`'s only fallible steps are the same ones
            // that already failed above; fall back to the plain live-stream
            // input so playback can still be attempted rather than giving up.
            resolve::track_input(
                self.http.clone(),
                &queued.track.video_id,
                self.cookies_file.as_deref(),
            )
        })
    }

    async fn cached_input(&self, queued: &QueuedTrack) -> Result<Input, PlaybackError> {
        resolve::cached_track_input(
            self.http.clone(),
            &queued.track.video_id,
            queued.track.duration,
            self.cookies_file.as_deref(),
        )
        .await
    }

    async fn start_playback(
        &self,
        guild_id: GuildId,
        call: Arc<Mutex<Call>>,
        queued: QueuedTrack,
        prefetched: Option<Prefetch>,
    ) -> Result<(), PlayerError> {
        let input = match prefetched {
            Some(handle) => self.resolve_prefetched(&queued, handle).await,
            None => self
                .cached_input(&queued)
                .await
                .map_err(|e| PlayerError::Playback(e.to_string()))?,
        };

        let handle = {
            let mut call = call.lock().await;
            call.play_input(input)
        };

        // Best-effort: a missing/unreadable volume setting shouldn't block
        // playback — fall back to songbird's own default (100%) rather than
        // erroring the whole track out.
        let volume = db::get_guild_volume(&self.db, &guild_id.to_string())
            .await
            .unwrap_or(db::DEFAULT_VOLUME);
        if let Err(err) = handle.set_volume(f32::from(volume) / 100.0) {
            tracing::warn!(%err, "failed to apply saved volume to new track");
        }

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

            // Start pre-buffering whatever's next in the queue now, so its
            // download runs in the background while this track plays.
            if let Some(next) = state.queue.front().cloned() {
                let registry = self.clone();
                state.prefetch = Some(tokio::spawn(
                    async move { registry.cached_input(&next).await },
                ));
            }
        }

        Ok(())
    }

    /// Advances to the next queued track, or — if the queue is empty —
    /// schedules an idle-timeout disconnect. Called from [`TrackEndHandler`]
    /// whenever a track ends, whether naturally or via `/skip`/`/stop`.
    async fn advance(&self, guild_id: GuildId) {
        let (next, prefetch) = {
            let mut guilds = self.guilds.lock().await;
            let Some(state) = guilds.get_mut(&guild_id) else {
                return;
            };
            state.current_handle = None;
            state.now_playing = state.queue.pop_front();
            (state.now_playing.clone(), state.prefetch.take())
        };

        match next {
            Some(queued) => {
                if let Some(call) = self.songbird.get(guild_id)
                    && let Err(err) = self.start_playback(guild_id, call, queued, prefetch).await
                {
                    tracing::warn!(%err, "failed to start next queued track");
                }
            }
            None => self.schedule_idle_disconnect(guild_id),
        }

        self.refresh_panel(guild_id).await;
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
            if let Some(prefetch) = state.prefetch.take() {
                prefetch.abort();
            }
            state.current_handle.take()
        };

        let Some(handle) = handle else {
            return Err(PlayerError::NothingPlaying);
        };

        // Stopping fires a Track::End event, but `now_playing`/`current_handle`
        // are already cleared above, so `advance()` will see an empty queue
        // and just (re-)schedule the idle timer rather than double-advance.
        let result = handle
            .stop()
            .map_err(|e| PlayerError::Playback(e.to_string()));
        if result.is_ok() {
            self.refresh_panel(guild_id).await;
        }
        result
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
        let result = handle
            .pause()
            .map_err(|e| PlayerError::Playback(e.to_string()));
        if result.is_ok() {
            self.refresh_panel(guild_id).await;
        }
        result
    }

    pub async fn resume(&self, guild_id: GuildId) -> Result<(), PlayerError> {
        let handle = {
            let guilds = self.guilds.lock().await;
            guilds
                .get(&guild_id)
                .and_then(|state| state.current_handle.clone())
        };
        let handle = handle.ok_or(PlayerError::NothingPlaying)?;
        let result = handle
            .play()
            .map_err(|e| PlayerError::Playback(e.to_string()));
        if result.is_ok() {
            self.refresh_panel(guild_id).await;
        }
        result
    }

    /// Shuffles the upcoming queue in place. Leaves `now_playing` where it
    /// is — shuffling shouldn't restart or skip the current track.
    pub async fn shuffle(&self, guild_id: GuildId) -> Result<(), PlayerError> {
        let mut guilds = self.guilds.lock().await;
        let Some(state) = guilds.get_mut(&guild_id) else {
            return Err(PlayerError::NothingToShuffle);
        };
        if state.queue.len() < 2 {
            return Err(PlayerError::NothingToShuffle);
        }

        let mut items: Vec<QueuedTrack> = state.queue.drain(..).collect();
        items.shuffle(&mut rand::rng());
        state.queue = items.into();
        drop(guilds);

        self.refresh_panel(guild_id).await;
        Ok(())
    }

    /// Skips directly to the upcoming track at `index` (0-based, matching
    /// [`QueueSnapshot::upcoming`]), discarding every track ahead of it.
    /// Reuses the current track's stop path — [`TrackEndHandler`] advances
    /// to the new front of the queue exactly as it would on a natural
    /// track end or a `/skip`.
    pub async fn jump_to(&self, guild_id: GuildId, index: usize) -> Result<(), PlayerError> {
        let handle = {
            let mut guilds = self.guilds.lock().await;
            let state = guilds
                .get_mut(&guild_id)
                .ok_or(PlayerError::InvalidSelection)?;
            if index >= state.queue.len() {
                return Err(PlayerError::InvalidSelection);
            }
            state.queue.drain(..index);
            state.current_handle.clone()
        };
        let handle = handle.ok_or(PlayerError::NothingPlaying)?;
        handle
            .stop()
            .map_err(|e| PlayerError::Playback(e.to_string()))
    }

    /// Whether the current track is paused. `None` if nothing is playing.
    pub async fn is_paused(&self, guild_id: GuildId) -> Option<bool> {
        let handle = {
            let guilds = self.guilds.lock().await;
            guilds
                .get(&guild_id)
                .and_then(|state| state.current_handle.clone())
        }?;
        handle
            .get_info()
            .await
            .ok()
            .map(|state| matches!(state.playing, PlayMode::Pause))
    }

    /// This guild's persisted playback volume (0-100), for display — the
    /// same lookup [`Self::start_playback`] uses to apply it to a new track.
    pub async fn get_volume(&self, guild_id: GuildId) -> u8 {
        db::get_guild_volume(&self.db, &guild_id.to_string())
            .await
            .unwrap_or(db::DEFAULT_VOLUME)
    }

    /// Sets and persists this guild's playback volume (0-100), applying it
    /// immediately to whatever's currently playing.
    pub async fn set_volume(&self, guild_id: GuildId, volume: u8) -> Result<(), PlayerError> {
        db::set_guild_volume(&self.db, &guild_id.to_string(), volume)
            .await
            .map_err(|e| PlayerError::Storage(e.to_string()))?;

        let handle = {
            let guilds = self.guilds.lock().await;
            guilds
                .get(&guild_id)
                .and_then(|state| state.current_handle.clone())
        };
        if let Some(handle) = handle
            && let Err(err) = handle.set_volume(f32::from(volume) / 100.0)
        {
            tracing::warn!(%err, "failed to apply volume change to current track");
        }

        self.refresh_panel(guild_id).await;
        Ok(())
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

    /// Live playback position of the current track, for a `/now_playing`
    /// progress display. `None` if nothing is playing or songbird couldn't
    /// report a position (e.g. the track just ended in a race with this
    /// call) — a missing position isn't worth surfacing as an error.
    pub async fn now_playing_position(&self, guild_id: GuildId) -> Option<Duration> {
        let handle = {
            let guilds = self.guilds.lock().await;
            guilds
                .get(&guild_id)
                .and_then(|state| state.current_handle.clone())
        }?;
        handle.get_info().await.ok().map(|state| state.position)
    }

    /// Points this guild's live `/player` panel at a new message, best-effort
    /// deleting whatever panel message preceded it (ignored if it's already
    /// gone — e.g. a user deleted it themselves).
    pub async fn replace_panel(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        message_id: MessageId,
    ) {
        let old = {
            let mut guilds = self.guilds.lock().await;
            guilds
                .entry(guild_id)
                .or_default()
                .panel
                .replace((channel_id, message_id))
        };

        if let Some((old_channel, old_message)) = old {
            let _ = old_channel
                .delete_message(self.discord_http.clone(), old_message)
                .await;
        }
    }

    /// Renders and pushes the panel's current appearance to an already-known
    /// panel message. Shared by [`Self::refresh_panel`] (looks up the
    /// pointer itself and self-heals on failure) and [`Self::leave`] (which
    /// already has the pointer in hand, from the `GuildState` it just
    /// removed).
    async fn edit_panel(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        message_id: MessageId,
    ) -> Result<(), ()> {
        let (content, embed, components) = crate::voice::panel::render(self, guild_id).await;
        let mut edit = serenity::EditMessage::new()
            .content(content)
            .components(components);
        edit = match embed {
            Some(embed) => edit.embed(embed),
            None => edit.embeds(Vec::new()),
        };

        channel_id
            .edit_message(self.discord_http.clone(), message_id, edit)
            .await
            .map(|_| ())
            .map_err(|_| ())
    }

    /// Edits this guild's live `/player` panel message (if any) with fresh
    /// content — called after every state-changing mutation so the panel
    /// stays current regardless of what triggered the change (its own
    /// buttons, a slash command, or a track ending on its own).
    ///
    /// Self-heals on edit failure (e.g. the message was deleted out from
    /// under it) by forgetting the panel, so later mutations don't keep
    /// retrying a dead pointer.
    async fn refresh_panel(&self, guild_id: GuildId) {
        let panel = {
            let guilds = self.guilds.lock().await;
            guilds.get(&guild_id).and_then(|state| state.panel)
        };
        let Some((channel_id, message_id)) = panel else {
            return;
        };

        if self
            .edit_panel(guild_id, channel_id, message_id)
            .await
            .is_err()
        {
            let mut guilds = self.guilds.lock().await;
            if let Some(state) = guilds.get_mut(&guild_id) {
                state.panel = None;
            }
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
