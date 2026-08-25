//! Per-guild playback state: an in-memory queue driven by songbird's
//! track-end event, plus idle auto-disconnect.
//!
//! [`PlayerRegistry`] is the single shared entry point commands use — it
//! owns the songbird manager handle and all per-guild queues, so every
//! command (`/play`, `/skip`, `/queue`, ...) goes through the same state
//! rather than each reaching into songbird directly.
//!
//! Constructed once in `main.rs` and shared via `Data::player`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use poise::serenity_prelude as serenity;
use rand::seq::SliceRandom;
use serenity::{ChannelId, GuildId, MessageId, UserId};
use songbird::input::Input;
use songbird::tracks::{PlayMode, TrackHandle};
use songbird::{
    Call, CoreEvent, Event, EventContext, EventHandler as SongbirdEventHandler, Songbird,
    TrackEvent,
};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::db;
use crate::voice::radio;
use crate::voice::resolve::{self, PlaybackError};
use crate::youtube::api::{Track, YouTubeClient};

/// A background pre-buffer for the track that's up next in the queue,
/// started as soon as the current track begins playing so its download
/// finishes (or is well underway) by the time it's actually needed. See
/// `cached_track_input`.
type Prefetch = JoinHandle<Result<Input, PlaybackError>>;

/// How long an empty, drained queue waits before the bot leaves the voice
/// channel on its own. Re-checked when the timer fires (not just scheduled
/// once) so a track queued in the meantime cancels the disconnect.
const IDLE_DISCONNECT: Duration = Duration::from_secs(150);

/// Highest accepted `/volume` percentage. Songbird's volume is an unbounded
/// gain multiplier, so anything above 100% is amplification (and clipping),
/// not "louder".
const MAX_VOLUME: u8 = 100;

/// Converts a 0-100 volume percentage into songbird's gain multiplier,
/// clamping out-of-range input rather than trusting it.
fn volume_multiplier(volume: u8) -> f32 {
    f32::from(volume.min(MAX_VOLUME)) / 100.0
}

/// Picks the more informative of a playback resolution failure and a
/// [`resolve::preflight_check`] classification of the same video: an opaque
/// `Other` yields to a named cause, anything already named wins.
fn better_playback_error(
    original: PlaybackError,
    preflight: Option<PlaybackError>,
) -> PlaybackError {
    if !matches!(original, PlaybackError::Other(_)) {
        return original;
    }
    match preflight {
        Some(classified) if !matches!(classified, PlaybackError::Other(_)) => classified,
        _ => original,
    }
}

/// Why a panel edit failed, reduced to the only distinction its callers act
/// on. Carrying this instead of `serenity::Error` also keeps that (large)
/// type out of a `Result` the hot paths return.
enum PanelEditFailure {
    /// The message is genuinely gone — deleted out from under the stored
    /// pointer. The only case that justifies forgetting the panel.
    Gone,
    /// Anything else: a network blip, a 5xx, a rate-limit give-up. The
    /// panel presumably still exists, so the pointer is kept.
    Transient(String),
}

impl PanelEditFailure {
    /// Classifies a failed `edit_message`. Treating *every* failure as
    /// "gone" (the old behaviour) makes a single transient blip forget a
    /// panel that's still on screen, so the next `/player` posts a duplicate
    /// one next to it.
    fn classify(err: &serenity::Error) -> Self {
        /// Discord's JSON error code for "Unknown Message".
        const UNKNOWN_MESSAGE: isize = 10008;

        let serenity::Error::Http(serenity::HttpError::UnsuccessfulRequest(response)) = err else {
            return Self::Transient(err.to_string());
        };
        if response.status_code == serenity::StatusCode::NOT_FOUND
            || response.error.code == UNKNOWN_MESSAGE
        {
            Self::Gone
        } else {
            Self::Transient(err.to_string())
        }
    }
}

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
    /// `current_handle`'s track uuid, captured at the same time it's set.
    /// [`TrackEndHandler`] carries the uuid of the track it was registered
    /// for and `advance` checks it against this before touching state — a
    /// track that was stopped/skipped/replaced still has an End/Error event
    /// in flight from songbird's mixer thread, and without this check that
    /// stale event would land after a *new* track has already started,
    /// clearing its handle out from under it or layering another track on
    /// top of it (two tracks audibly playing at once).
    current_track_id: Option<Uuid>,
    /// Background pre-buffer for `queue`'s front entry, started right after
    /// the current track begins playing. Always corresponds to whatever is
    /// at the front of `queue` — `enqueue`/`advance`/`stop` are the only
    /// places that mutate the queue, and each keeps this in sync.
    prefetch: Option<Prefetch>,
    /// The live `/player` panel message for this guild, if one has been
    /// posted — kept current by [`PlayerRegistry::refresh_panel`] after
    /// every state-changing mutation.
    panel: Option<(ChannelId, MessageId)>,
    /// Set while a `/player` invocation holds the exclusive right to post
    /// this guild's panel (via [`GuildState::claim_panel`]) but hasn't
    /// posted it yet — closes the race where two concurrent `/player`s both
    /// see no panel and both post one. See [`GuildState::claim_panel`].
    panel_reserved: bool,
    /// Whether radio mode is on for this guild — a pure toggle, not tied to
    /// any particular seed. See `radio_seed`.
    radio_enabled: bool,
    /// Video id of the most recently *started* track (playlist, one-off
    /// `/play`, search result, or a radio-fetched track alike — every path
    /// through `start_playback` updates this), used to look up the next
    /// radio track when the queue runs dry. `None` until something has
    /// played this session.
    radio_seed: Option<String>,
    /// Requester of the track `radio_seed` points at — attributed to
    /// radio-fetched tracks too, since nothing new requested them.
    radio_requested_by: Option<UserId>,
    /// Video ids already surfaced by radio mode this session, so it doesn't
    /// immediately repeat itself.
    radio_played: HashSet<String>,
    /// Set when a radio refill found the seed's Mix exhausted (every entry
    /// already in `radio_played`). Short-circuits further refills, which
    /// would otherwise re-run the whole yt-dlp mix listing plus hydration on
    /// every queue drain only to come up empty again, forever. Cleared when
    /// something changes the picture: radio being toggled, or a track being
    /// queued by hand (see `enqueue`/`enqueue_many`/`toggle_radio`).
    radio_exhausted: bool,
    /// Bumped by [`PlayerRegistry::stop`]. Background work that leaves the
    /// guild lock and comes back later (radio's refill task) captures this
    /// first and refuses to commit if it changed meanwhile — a `/stop` that
    /// lands mid-refill must not have a track pushed into the queue behind
    /// it, which would break the `now_playing.is_none()` ⇔ `queue.is_empty()`
    /// invariant `is_idle` (and the idle disconnect) rely on.
    epoch: u64,
}

impl GuildState {
    /// Whether this guild has nothing playing *and* nothing queued — the one
    /// definition of "idle" shared by the idle-disconnect timer
    /// ([`PlayerRegistry::schedule_idle_disconnect`]) and the leave path it
    /// triggers ([`PlayerRegistry::leave_if_idle`]).
    ///
    /// Checking the queue too, not just `now_playing`, is deliberate: a
    /// queue entry with nothing playing is a state that shouldn't happen,
    /// but if it ever does (a background refill racing a `/stop`, say),
    /// disconnecting would silently destroy it.
    fn is_idle(&self) -> bool {
        self.now_playing.is_none() && self.queue.is_empty()
    }

    /// Atomically checks for a live `/player` panel and, if there isn't one,
    /// reserves this guild's panel slot — the check-and-reserve
    /// [`PlayerRegistry::claim_panel_slot`] performs under the guild lock so
    /// two `/player` invocations landing together can't both see no panel
    /// and both post one (the old `existing_panel`-then-`replace_panel`
    /// pair left exactly that gap open across two `.await` points).
    ///
    /// A [`PanelClaim::Reserved`] caller holds the exclusive right to post a
    /// panel and must follow up with [`Self::set_panel`] once it has (or
    /// [`Self::release_panel`] if it gives up before posting, so this guild
    /// isn't wedged with a reservation nobody will ever fulfil).
    fn claim_panel(&mut self) -> PanelClaim {
        if let Some(panel) = self.panel {
            return PanelClaim::Existing(panel);
        }
        if self.panel_reserved {
            return PanelClaim::InProgress;
        }
        self.panel_reserved = true;
        PanelClaim::Reserved
    }

    /// Records a freshly posted `/player` panel and clears the reservation
    /// [`Self::claim_panel`] made for it.
    fn set_panel(&mut self, channel_id: ChannelId, message_id: MessageId) {
        self.panel = Some((channel_id, message_id));
        self.panel_reserved = false;
    }

    /// Releases a reservation from [`Self::claim_panel`] without posting a
    /// panel — e.g. the send itself failed. Leaves any actual `panel`
    /// pointer alone; this only ever clears the in-flight flag.
    fn release_panel(&mut self) {
        self.panel_reserved = false;
    }
}

/// The result of [`PlayerRegistry::claim_panel_slot`] (see
/// [`GuildState::claim_panel`] for the invariant it maintains).
pub enum PanelClaim {
    /// A panel is already live at this location — point back at it instead
    /// of posting a new one.
    Existing((ChannelId, MessageId)),
    /// No panel exists yet, and the caller now holds the exclusive right to
    /// post one. Must be followed by [`PlayerRegistry::set_panel`] on
    /// success, or [`PlayerRegistry::release_panel_slot`] if the caller
    /// gives up before posting.
    Reserved,
    /// No panel exists yet, but another `/player` invocation is already in
    /// the middle of posting one. Nothing to do here; a `/player` shortly
    /// after will see it via [`PanelClaim::Existing`].
    InProgress,
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
    http: reqwest::Client,
    /// Used only to edit/delete the live `/player` panel message from
    /// contexts that aren't already handling a Discord interaction (e.g.
    /// [`TrackEndHandler`], or a `/skip` slash command refreshing a panel
    /// message it didn't itself respond to). Unrelated to `http` above,
    /// which is for yt-dlp/YouTube stream resolution.
    discord_http: Arc<serenity::Http>,
    cookies_file: Option<String>,
    db: sqlx::SqlitePool,
    /// `yt-dlp`-backed client, used by radio mode's background refill to
    /// hydrate bare video ids (from `crate::voice::radio::list_mix_video_ids`)
    /// into full `Track`s — see `maybe_spawn_radio_refill`.
    youtube: YouTubeClient,
    guilds: Arc<Mutex<HashMap<GuildId, GuildState>>>,
}

impl PlayerRegistry {
    pub fn new(
        songbird: Arc<Songbird>,
        http: reqwest::Client,
        discord_http: Arc<serenity::Http>,
        cookies_file: Option<String>,
        db: sqlx::SqlitePool,
        youtube: YouTubeClient,
    ) -> Self {
        Self {
            songbird,
            http,
            discord_http,
            cookies_file,
            db,
            youtube,
            guilds: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn join(
        &self,
        guild_id: GuildId,
        voice_channel_id: ChannelId,
    ) -> Result<(), PlayerError> {
        let call = self
            .songbird
            .join(guild_id, voice_channel_id)
            .await
            .map_err(|e| PlayerError::Join(e.to_string()))?;

        // Losing the voice connection (kicked or disconnected by a
        // moderator, or a reconnect that exhausted its retries) is reported
        // *only* through this core event — songbird fires no track End/Error
        // for the track that was playing at the time. Without this handler
        // that track's state would sit in `now_playing`/`current_handle`
        // forever: nothing to advance the queue, nothing to time out (the
        // idle check would keep seeing an occupied `now_playing`), and a
        // `/player` panel frozen mid-progress-bar.
        {
            let mut call = call.lock().await;
            // A rejoin reuses the same `Call`, so clear first rather than
            // stacking a second handler onto the same connection.
            call.remove_all_global_events();
            call.add_global_event(
                Event::Core(CoreEvent::DriverDisconnect),
                DriverDisconnectHandler {
                    registry: self.clone(),
                    guild_id,
                },
            );
        }

        Ok(())
    }

    /// Leaves voice and drops all queued/now-playing state for the guild. If
    /// a `/player` panel is live, it gets one last edit to reflect that
    /// nothing is playing anymore — otherwise it would freeze showing
    /// whatever was playing right before disconnect (notably including the
    /// common case of the idle-timeout auto-disconnect in
    /// [`Self::schedule_idle_disconnect`]).
    pub async fn leave(&self, guild_id: GuildId) -> Result<(), PlayerError> {
        let (result, panel) = {
            let mut guilds = self.guilds.lock().await;
            self.leave_under_lock(guild_id, &mut guilds).await
        };

        if let Some((channel_id, message_id)) = panel {
            let _ = self.edit_panel(guild_id, channel_id, message_id).await;
        }
        result
    }

    /// Leaves voice and drops the guild's state, with the caller's guild-map
    /// lock held throughout so the decision to leave and the teardown can't
    /// be interleaved with an `enqueue` (see [`Self::leave_if_idle`]).
    ///
    /// Returns the panel pointer (if any) instead of refreshing it here: the
    /// panel renderer reads this same map, so it must only run once the
    /// caller has dropped the lock.
    async fn leave_under_lock(
        &self,
        guild_id: GuildId,
        guilds: &mut HashMap<GuildId, GuildState>,
    ) -> (Result<(), PlayerError>, Option<(ChannelId, MessageId)>) {
        let mut removed = guilds.remove(&guild_id);
        if let Some(prefetch) = removed.as_mut().and_then(|state| state.prefetch.take()) {
            prefetch.abort();
        }

        // `remove`, not `leave` — `Songbird::leave` only clears the voice
        // connection, leaving the (now-disconnected) `Call` registered in
        // songbird's manager map. `is_connected` would then keep reporting
        // this guild as connected, so `/play` would skip rejoining voice
        // and play into a `Call` with nothing on the other end.
        let result = self
            .songbird
            .remove(guild_id)
            .await
            .map_err(|e| PlayerError::Join(e.to_string()));

        (result, removed.and_then(|state| state.panel))
    }

    /// Leaves voice, but only if the guild is still idle — the action half
    /// of the idle-disconnect timer.
    ///
    /// The idle check and the teardown happen under a single lock
    /// acquisition on purpose. Checking, dropping the lock, and *then*
    /// leaving loses to a concurrent `/play`: `enqueue` claims
    /// `now_playing` before its network-bound resolve step, so a leave
    /// decided a moment earlier would tear the guild down around a track
    /// that's already mid-start — and that track's eventual End/Error event
    /// would find no state to advance, leaving the bot silently out of voice
    /// with a queue nobody drains. `enqueue`/`enqueue_many` look up the
    /// `Call` under this same lock, so they either win the race (and this
    /// sees a non-idle guild and bails) or lose it (and see no `Call`,
    /// reporting `NotConnected` rather than playing into a dead one).
    async fn leave_if_idle(&self, guild_id: GuildId) {
        let (result, panel) = {
            let mut guilds = self.guilds.lock().await;
            match guilds.get(&guild_id) {
                Some(state) if !state.is_idle() => return,
                _ => {}
            }
            self.leave_under_lock(guild_id, &mut guilds).await
        };

        if let Err(err) = result {
            tracing::warn!(%guild_id, %err, "idle disconnect failed to leave voice");
        }
        if let Some((channel_id, message_id)) = panel {
            let _ = self.edit_panel(guild_id, channel_id, message_id).await;
        }
    }

    pub fn is_connected(&self, guild_id: GuildId) -> bool {
        self.songbird.get(guild_id).is_some()
    }

    /// Enqueues a track. If nothing is currently playing, starts it
    /// immediately instead of leaving it queued.
    pub async fn enqueue(&self, guild_id: GuildId, queued: QueuedTrack) -> Result<(), PlayerError> {
        let (call, should_start) = {
            let mut guilds = self.guilds.lock().await;
            // Looked up under the guild lock, not before it: that's what
            // makes this mutually exclusive with `leave_if_idle`'s teardown
            // — see its doc comment.
            let call = self
                .songbird
                .get(guild_id)
                .ok_or(PlayerError::NotConnected)?;
            let state = guilds.entry(guild_id).or_default();
            // A hand-queued track means the queue is no longer running on
            // radio fumes; give a previously exhausted mix another chance.
            state.radio_exhausted = false;
            let should_start = state.now_playing.is_none();
            if should_start {
                state.now_playing = Some(queued.clone());
            } else {
                state.queue.push_back(queued.clone());
            }
            (call, should_start)
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

    /// Enqueues many tracks at once — for `/playlist_play`, queuing a whole
    /// saved playlist in one go. Equivalent to calling [`Self::enqueue`]
    /// once per track, but under a single lock and with a single panel
    /// refresh at the end, instead of one Discord API call *per track*:
    /// looping the single-track `enqueue` over a playlist of hundreds (or
    /// thousands) of tracks turns "queue a playlist" into that many
    /// sequential, rate-limited HTTP round-trips — slow enough to look
    /// hung. Returns how many tracks ended up queued (fewer than
    /// `tracks.len()` only if the tracks tried as the starting track all
    /// failed to play).
    pub async fn enqueue_many(
        &self,
        guild_id: GuildId,
        tracks: Vec<QueuedTrack>,
    ) -> Result<usize, PlayerError> {
        if tracks.is_empty() {
            return Ok(0);
        }
        let total = tracks.len();

        let (call, needs_start) = {
            let mut guilds = self.guilds.lock().await;
            // Under the lock, as in `enqueue` — see `leave_if_idle`.
            let call = self
                .songbird
                .get(guild_id)
                .ok_or(PlayerError::NotConnected)?;
            let state = guilds.entry(guild_id).or_default();
            state.radio_exhausted = false;
            let needs_start = state.now_playing.is_none();
            state.queue.extend(tracks);
            if needs_start {
                state.now_playing = state.queue.pop_front();
            }
            (call, needs_start)
        };

        let mut failed = 0;
        if needs_start {
            // Keep trying front-of-queue tracks until one starts or the
            // queue runs dry — same rollback-and-retry `enqueue` relies on
            // to avoid orphaning everything behind a track that fails to
            // start, just looped here since the whole playlist is already
            // sitting in the queue rather than trickling in one call at a
            // time.
            let mut candidate = {
                let guilds = self.guilds.lock().await;
                guilds.get(&guild_id).and_then(|s| s.now_playing.clone())
            };
            while let Some(queued) = candidate {
                match self
                    .start_playback(guild_id, call.clone(), queued, None)
                    .await
                {
                    Ok(()) => break,
                    Err(err) => {
                        tracing::warn!(%err, "failed to start a playlist track, trying the next one");
                        failed += 1;
                        candidate = self.promote_next(guild_id).await;
                    }
                }
            }
        }

        self.refresh_panel(guild_id).await;
        Ok(total - failed)
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

    /// Turns a raw resolution failure into the clearest user-facing error
    /// available.
    ///
    /// The playback path resolves through songbird's `YoutubeDl` source,
    /// whose failures surface as an opaque `PlaybackError::Other` — a
    /// truncated yt-dlp/IO message that tells a user nothing about the
    /// common, actionable cases. So on failure, re-ask yt-dlp directly via
    /// [`resolve::preflight_check`], whose stderr classification names them
    /// ("video is age-restricted", "not available in this region", "private
    /// or deleted"), and report that instead when it has an opinion. Only
    /// costs the extra subprocess on the failure path.
    async fn classify_playback_failure(&self, video_id: &str, err: PlaybackError) -> PlayerError {
        if !matches!(err, PlaybackError::Other(_)) {
            return PlayerError::Playback(err.to_string());
        }

        let preflight = resolve::preflight_check(video_id, self.cookies_file.as_deref())
            .await
            .err();
        PlayerError::Playback(better_playback_error(err, preflight).to_string())
    }

    /// Moves the front of the queue into `now_playing` and returns it (or
    /// `None` if the queue is empty, or the guild's state is gone). Used to
    /// walk past tracks that fail to start rather than stalling on them.
    async fn promote_next(&self, guild_id: GuildId) -> Option<QueuedTrack> {
        let mut guilds = self.guilds.lock().await;
        let state = guilds.get_mut(&guild_id)?;
        state.now_playing = state.queue.pop_front();
        state.now_playing.clone()
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
            None => match self.cached_input(&queued).await {
                Ok(input) => input,
                Err(err) => {
                    return Err(self
                        .classify_playback_failure(&queued.track.video_id, err)
                        .await);
                }
            },
        };

        let handle = {
            let mut call = call.lock().await;
            call.play_input(input)
        };
        let track_id = handle.uuid();

        // Best-effort: a missing/unreadable volume setting shouldn't block
        // playback — fall back to songbird's own default (100%) rather than
        // erroring the whole track out.
        let volume = db::get_guild_volume(&self.db, &guild_id.to_string())
            .await
            .unwrap_or(db::DEFAULT_VOLUME);
        // Clamped here rather than trusting callers/storage: songbird takes
        // an unbounded multiplier, so a stray >100 value (a hand-edited row,
        // a future caller that forgets to validate) would blow out the
        // amplitude and clip.
        if let Err(err) = handle.set_volume(volume_multiplier(volume)) {
            tracing::warn!(%err, "failed to apply saved volume to new track");
        }

        // Best-effort: if registering these hooks itself fails, the track
        // still plays, it just won't auto-advance the queue — surfacing
        // that as a playback failure would be misleading.
        //
        // `Error` (not just `End`) is required: a track whose lazy input
        // fails to resolve/stream/decode after it starts (network drop,
        // yt-dlp hiccup, a bad remote stream) goes to songbird's
        // `PlayMode::Errored` without ever firing `End` — see
        // `driver::tasks::mixer`'s handling of `InputReadyingError`/
        // `MixStatus::Errored` in songbird 0.6. Without this handler too,
        // such a track leaves `now_playing` stuck forever: no auto-advance,
        // no idle-disconnect (since `now_playing` looks occupied), and no
        // error ever surfaced to Discord — playback just silently stalls.
        if let Err(err) = handle.add_event(
            Event::Track(TrackEvent::End),
            TrackEndHandler {
                registry: self.clone(),
                guild_id,
                track_id,
            },
        ) {
            tracing::warn!(%err, "failed to register track-end handler");
        }
        if let Err(err) = handle.add_event(
            Event::Track(TrackEvent::Error),
            TrackEndHandler {
                registry: self.clone(),
                guild_id,
                track_id,
            },
        ) {
            tracing::warn!(%err, "failed to register track-error handler");
        }

        let mut guilds = self.guilds.lock().await;
        let mut needs_radio_refill = false;
        if let Some(state) = guilds.get_mut(&guild_id) {
            state.current_handle = Some(handle);
            state.current_track_id = Some(track_id);
            // Every track start updates the radio seed, regardless of how
            // the track got here (playlist, one-off `/play`, search, or a
            // radio-fetched track itself) — see `maybe_spawn_radio_refill`.
            state.radio_seed = Some(queued.track.video_id.clone());
            state.radio_requested_by = Some(queued.requested_by);

            // Start pre-buffering whatever's next in the queue now, so its
            // download runs in the background while this track plays.
            if let Some(next) = state.queue.front().cloned() {
                let registry = self.clone();
                state.prefetch = Some(tokio::spawn(
                    async move { registry.cached_input(&next).await },
                ));
            } else {
                // Nothing queued behind this track — if radio mode is on,
                // top the queue up in the background so a track is already
                // waiting by the time this one ends. Deferred until after
                // `guilds` is dropped below, since `maybe_spawn_radio_refill`
                // does its own locking.
                needs_radio_refill = true;
            }
        }
        drop(guilds);

        if needs_radio_refill {
            self.maybe_spawn_radio_refill(guild_id);
        }

        Ok(())
    }

    /// Advances to the next queued track, or — if the queue is empty —
    /// schedules an idle-timeout disconnect. Called from [`TrackEndHandler`]
    /// whenever a track ends, whether naturally or via `/skip`/`/stop`.
    ///
    /// `track_id` is the uuid of the track whose End/Error event triggered
    /// this call. It's checked against the guild's `current_track_id` and
    /// the call is dropped if they don't match — an End/Error event fired by
    /// songbird's mixer thread for a track that's already been superseded
    /// (stopped, skipped, or replaced by a fresh `/play` before this event
    /// landed). Acting on it anyway would advance past whatever's actually
    /// playing now, or start a second track on top of it.
    async fn advance(&self, guild_id: GuildId, track_id: Uuid) {
        let (mut next, mut prefetch) = {
            let mut guilds = self.guilds.lock().await;
            let Some(state) = guilds.get_mut(&guild_id) else {
                return;
            };
            if state.current_track_id != Some(track_id) {
                return;
            }
            state.current_handle = None;
            state.current_track_id = None;
            state.now_playing = state.queue.pop_front();
            (state.now_playing.clone(), state.prefetch.take())
        };

        // Walk past tracks that fail to start instead of stalling on the
        // first one: a failed start leaves `now_playing` set with no handle
        // and no end-event handler, so without this the queue behind it
        // would never be tried (the same trap `enqueue`/`enqueue_many`
        // already guard against on their own start paths).
        let mut started = false;
        while let Some(queued) = next {
            let Some(call) = self.songbird.get(guild_id) else {
                break;
            };
            match self
                .start_playback(guild_id, call, queued, prefetch.take())
                .await
            {
                Ok(()) => {
                    started = true;
                    break;
                }
                Err(err) => {
                    tracing::warn!(%err, "failed to start next queued track, trying the one after");
                    next = self.promote_next(guild_id).await;
                }
            }
        }

        if !started {
            self.schedule_idle_disconnect(guild_id);
        }

        self.refresh_panel(guild_id).await;
    }

    fn schedule_idle_disconnect(&self, guild_id: GuildId) {
        let registry = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(IDLE_DISCONNECT).await;

            // Re-checked rather than disconnecting unconditionally —
            // something may have been queued (or `/leave` already run) while
            // we slept — and re-checked *inside* the leave itself, holding
            // the guild lock across both halves, so a `/play` landing in
            // between can't be torn down mid-start. See `leave_if_idle`.
            registry.leave_if_idle(guild_id).await;
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
            state.current_track_id = None;
            // Invalidates any in-flight background refill: radio's mix
            // lookup can take arbitrarily long, and a track pushed into the
            // queue after a `/stop` would either sit inert forever or be
            // silently destroyed by the idle disconnect this schedules.
            state.epoch = state.epoch.wrapping_add(1);
            if let Some(prefetch) = state.prefetch.take() {
                prefetch.abort();
            }
            state.current_handle.take()
        };

        let Some(handle) = handle else {
            return Err(PlayerError::NothingPlaying);
        };

        // Stopping fires a Track::End event, but `current_track_id` is
        // already cleared above, so `advance()` will recognize it as stale
        // (belonging to a track that's no longer current) and ignore it
        // rather than double-advance — schedule the idle timer ourselves
        // instead of relying on that event to do it.
        let result = handle
            .stop()
            .map_err(|e| PlayerError::Playback(e.to_string()));
        if result.is_ok() {
            self.schedule_idle_disconnect(guild_id);
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
        self.restart_prefetch(state);
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
        let mut guilds = self.guilds.lock().await;
        let state = guilds
            .get_mut(&guild_id)
            .ok_or(PlayerError::InvalidSelection)?;
        if index >= state.queue.len() {
            return Err(PlayerError::InvalidSelection);
        }
        let handle = state
            .current_handle
            .clone()
            .ok_or(PlayerError::NothingPlaying)?;

        // Stop first, and keep the guild lock held while dropping the
        // skipped-over entries: `TrackHandle::stop` is a non-blocking send
        // to the mixer, so a failure here means nothing was stopped and the
        // queue mutation must not be committed. Holding the lock across both
        // also keeps the resulting End event's `advance` (which needs this
        // same lock) from popping the old front before it's drained.
        handle
            .stop()
            .map_err(|e| PlayerError::Playback(e.to_string()))?;

        state.queue.drain(..index);
        if index > 0 {
            self.restart_prefetch(state);
        }
        Ok(())
    }

    /// Cancels any in-flight prefetch and starts a fresh one for the new
    /// front of `state.queue`, if there is one. `prefetch` is only ever kept
    /// in sync automatically when the queue is mutated by `push_back`/
    /// `pop_front` (see the field's doc comment) — `shuffle` and `jump_to`
    /// instead reorder or drop entries out from under an in-flight prefetch,
    /// which would otherwise hand the *next* track a download meant for
    /// whatever used to be at the front (wrong audio playing under the
    /// right track's title). Must be called with `state`'s guild lock held.
    fn restart_prefetch(&self, state: &mut GuildState) {
        if let Some(old) = state.prefetch.take() {
            old.abort();
        }
        if let Some(next) = state.queue.front().cloned() {
            let registry = self.clone();
            state.prefetch = Some(tokio::spawn(
                async move { registry.cached_input(&next).await },
            ));
        }
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
        // Clamped rather than trusting the caller to have validated — see
        // `start_playback`.
        let volume = volume.min(MAX_VOLUME);
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
            && let Err(err) = handle.set_volume(volume_multiplier(volume))
        {
            tracing::warn!(%err, "failed to apply volume change to current track");
        }

        self.refresh_panel(guild_id).await;
        Ok(())
    }

    /// Flips radio mode for the guild and returns the new state. If it just
    /// turned on and nothing is currently queued behind whatever's playing
    /// (or nothing is playing at all), kicks off a refill in the background
    /// rather than waiting for the next natural track-end.
    pub async fn toggle_radio(&self, guild_id: GuildId) -> bool {
        let (enabled, needs_refill) = {
            let mut guilds = self.guilds.lock().await;
            let state = guilds.entry(guild_id).or_default();
            state.radio_enabled = !state.radio_enabled;
            // Toggling is the user's "try again" for a mix that previously
            // came up empty.
            state.radio_exhausted = false;
            (state.radio_enabled, state.queue.is_empty())
        };

        if enabled && needs_refill {
            self.maybe_spawn_radio_refill(guild_id);
        }

        self.refresh_panel(guild_id).await;
        enabled
    }

    /// Whether radio mode is on for this guild — for the `/player` panel's
    /// toggle button.
    pub async fn is_radio_enabled(&self, guild_id: GuildId) -> bool {
        let guilds = self.guilds.lock().await;
        guilds
            .get(&guild_id)
            .is_some_and(|state| state.radio_enabled)
    }

    /// Spawns a background task that tops up `guild_id`'s queue with one
    /// radio-mix track, if radio mode is on and something has already
    /// played this session (`radio_seed`/`radio_requested_by` set). No-op
    /// otherwise — called from `start_playback` whenever the queue goes
    /// empty as a track starts, and from `toggle_radio` when radio is
    /// switched on mid-session against an already-empty queue. Calling it
    /// from both places can race (e.g. toggled on the instant a track
    /// starts with an empty queue) — worst case two tracks land instead of
    /// one, which is harmless and not worth guarding against.
    ///
    /// On any failure (yt-dlp/mix-listing failure, hydration failure, or an
    /// exhausted mix with no unplayed candidates left), logs a warning and
    /// does nothing further — `advance()`'s existing idle-disconnect path
    /// remains the safety net if the queue stays empty.
    fn maybe_spawn_radio_refill(&self, guild_id: GuildId) {
        let registry = self.clone();
        tokio::spawn(async move {
            let (enabled, seed, requested_by, played, epoch) = {
                let guilds = registry.guilds.lock().await;
                let Some(state) = guilds.get(&guild_id) else {
                    return;
                };
                if state.radio_exhausted {
                    // Already established there's nothing left to pull from
                    // this mix — don't re-run the whole yt-dlp listing plus
                    // hydration on every queue drain just to find that out
                    // again. Cleared by a radio toggle or a hand-queued track.
                    return;
                }
                (
                    state.radio_enabled,
                    state.radio_seed.clone(),
                    state.radio_requested_by,
                    state.radio_played.clone(),
                    state.epoch,
                )
            };
            let (true, Some(seed), Some(requested_by)) = (enabled, seed, requested_by) else {
                return;
            };

            let mix_ids =
                match radio::list_mix_video_ids(&seed, registry.cookies_file.as_deref()).await {
                    Ok(ids) => ids,
                    Err(err) => {
                        tracing::warn!(%guild_id, %seed, %err, "radio refill: failed to list mix");
                        return;
                    }
                };

            let candidates: Vec<&str> = mix_ids
                .iter()
                .map(String::as_str)
                .filter(|id| *id != seed.as_str() && !played.contains(*id))
                .take(5)
                .collect();
            if candidates.is_empty() {
                tracing::warn!(
                    %guild_id, %seed,
                    "radio refill: mix exhausted, no unplayed candidates left"
                );
                let mut guilds = registry.guilds.lock().await;
                if let Some(state) = guilds.get_mut(&guild_id)
                    && state.epoch == epoch
                {
                    state.radio_exhausted = true;
                }
                return;
            }

            let hydrated = match registry.youtube.hydrate_videos(&candidates).await {
                Ok(tracks) => tracks,
                Err(err) => {
                    tracing::warn!(%guild_id, %err, "radio refill: failed to hydrate mix candidates");
                    return;
                }
            };
            let Some(chosen) = hydrated.into_iter().next() else {
                tracing::warn!(%guild_id, %seed, "radio refill: hydration returned no tracks");
                return;
            };

            let pushed = {
                let mut guilds = registry.guilds.lock().await;
                // `state.epoch == epoch` rejects a refill that a `/stop`
                // overtook while the (unbounded-latency) mix listing and
                // hydration above were in flight: pushing now would
                // resurrect a queue behind a stopped player, leaving an
                // entry that either sits inert or gets quietly eaten by the
                // idle disconnect `stop` already scheduled.
                match guilds.get_mut(&guild_id) {
                    Some(state) if state.radio_enabled && state.epoch == epoch => {
                        state.radio_played.insert(chosen.video_id.clone());
                        state.queue.push_back(QueuedTrack {
                            track: chosen,
                            requested_by,
                        });
                        true
                    }
                    _ => false,
                }
            };

            if pushed {
                registry.refresh_panel(guild_id).await;
            }
        });
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

    /// Atomic check-and-reserve for the `/player` command: if this guild
    /// already has a live panel, refreshes and returns it (the same
    /// self-heal an older `existing_panel` used to do standalone); otherwise
    /// reserves the right to post one so a second, concurrent `/player`
    /// doesn't also post one. See [`GuildState::claim_panel`] and
    /// [`PanelClaim`].
    ///
    /// The caller MUST follow a [`PanelClaim::Reserved`] result with
    /// [`Self::set_panel`] once it has posted, or [`Self::release_panel_slot`]
    /// if it gives up before posting.
    pub async fn claim_panel_slot(&self, guild_id: GuildId) -> PanelClaim {
        let panel = {
            let mut guilds = self.guilds.lock().await;
            let state = guilds.entry(guild_id).or_default();
            match state.claim_panel() {
                PanelClaim::Existing(panel) => panel,
                other => return other,
            }
        };
        let (channel_id, message_id) = panel;

        // Refresh the existing panel in place, and only give up the slot
        // (letting the caller post a fresh panel) if the message is
        // genuinely gone rather than a transient edit failure.
        match self.edit_panel(guild_id, channel_id, message_id).await {
            Ok(()) => PanelClaim::Existing(panel),
            Err(failure) => {
                let gone = self.forget_panel_if_gone(guild_id, panel, &failure).await;
                if gone {
                    // `forget_panel_if_gone` only clears `panel`, not a
                    // reservation — there wasn't one here (we're in the
                    // `Existing` arm), so it's safe to immediately reserve.
                    let mut guilds = self.guilds.lock().await;
                    let state = guilds.entry(guild_id).or_default();
                    state.claim_panel()
                } else {
                    PanelClaim::Existing(panel)
                }
            }
        }
    }

    /// Records a freshly posted `/player` panel from a
    /// [`PanelClaim::Reserved`] claim, best-effort deleting whatever panel
    /// message preceded it (ignored if it's already gone — e.g. a user
    /// deleted it themselves).
    pub async fn set_panel(&self, guild_id: GuildId, channel_id: ChannelId, message_id: MessageId) {
        let old = {
            let mut guilds = self.guilds.lock().await;
            let state = guilds.entry(guild_id).or_default();
            let old = state.panel;
            state.set_panel(channel_id, message_id);
            old
        };

        if let Some((old_channel, old_message)) = old {
            let _ = old_channel
                .delete_message(self.discord_http.clone(), old_message)
                .await;
        }
    }

    /// Releases a [`PanelClaim::Reserved`] claim without posting a panel —
    /// e.g. the send itself failed — so a later `/player` isn't wedged
    /// forever believing one is already being posted.
    pub async fn release_panel_slot(&self, guild_id: GuildId) {
        let mut guilds = self.guilds.lock().await;
        if let Some(state) = guilds.get_mut(&guild_id) {
            state.release_panel();
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
    ) -> Result<(), PanelEditFailure> {
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
            .map_err(|err| PanelEditFailure::classify(&err))
    }

    /// Compare-and-clear for a failed panel edit: forgets the guild's panel
    /// pointer only if the failure means the message is really gone *and*
    /// the stored pointer is still the one that failed. Returns whether the
    /// panel was considered gone.
    ///
    /// The comparison matters because the edit happens with the guild lock
    /// released: two `/player` invocations landing together can have the
    /// second one delete the first's message and install its own before the
    /// first's edit fails. Clearing unconditionally would then wipe out the
    /// pointer to the panel that's actually live, and the next `/player`
    /// would post a duplicate.
    async fn forget_panel_if_gone(
        &self,
        guild_id: GuildId,
        panel: (ChannelId, MessageId),
        failure: &PanelEditFailure,
    ) -> bool {
        if let PanelEditFailure::Transient(message) = failure {
            tracing::warn!(%guild_id, message, "failed to refresh the /player panel; keeping it");
            return false;
        }

        let mut guilds = self.guilds.lock().await;
        if let Some(state) = guilds.get_mut(&guild_id)
            && state.panel == Some(panel)
        {
            state.panel = None;
        }
        true
    }

    /// Edits this guild's live `/player` panel message (if any) with fresh
    /// content — called after every state-changing mutation so the panel
    /// stays current regardless of what triggered the change (its own
    /// buttons, a slash command, or a track ending on its own).
    ///
    /// Self-heals when the message turns out to have been deleted out from
    /// under it, by forgetting the panel so later mutations don't keep
    /// retrying a dead pointer — see [`Self::forget_panel_if_gone`] for why
    /// that's narrower than "any edit failure".
    async fn refresh_panel(&self, guild_id: GuildId) {
        let panel = {
            let guilds = self.guilds.lock().await;
            guilds.get(&guild_id).and_then(|state| state.panel)
        };
        let Some(panel) = panel else {
            return;
        };
        let (channel_id, message_id) = panel;

        if let Err(failure) = self.edit_panel(guild_id, channel_id, message_id).await {
            self.forget_panel_if_gone(guild_id, panel, &failure).await;
        }
    }
}

struct TrackEndHandler {
    registry: PlayerRegistry,
    guild_id: GuildId,
    /// Uuid of the track this handler was registered for — see `advance`'s
    /// doc comment for why this is needed to ignore stale events.
    track_id: Uuid,
}

#[async_trait::async_trait]
impl SongbirdEventHandler for TrackEndHandler {
    async fn act(&self, _ctx: &EventContext<'_>) -> Option<Event> {
        self.registry.advance(self.guild_id, self.track_id).await;
        None
    }
}

/// Reacts to the voice connection going away underneath us — the bot being
/// disconnected or kicked by a moderator, or a reconnect that ran out of
/// retries. Registered as a *global* event on the `Call` in
/// [`PlayerRegistry::join`], because songbird reports this only through
/// [`CoreEvent::DriverDisconnect`]: the track that was playing fires no
/// `End` and no `Error`, so nothing in the track-level path ever runs.
///
/// Songbird reports a deliberate local `leave` with the same event (it can't
/// be told apart from an admin kick — both land as
/// `DisconnectReason::Requested` via `Call::leave_local`), so the teardown
/// below has to be idempotent. It is: `leave` on an already-torn-down guild
/// finds no state, no panel, and no call to remove.
struct DriverDisconnectHandler {
    registry: PlayerRegistry,
    guild_id: GuildId,
}

#[async_trait::async_trait]
impl SongbirdEventHandler for DriverDisconnectHandler {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        if let EventContext::DriverDisconnect(data) = ctx {
            tracing::warn!(
                guild_id = %self.guild_id,
                kind = ?data.kind,
                reason = ?data.reason,
                "voice connection lost; clearing playback state"
            );
        }

        // Spawned rather than awaited inline: this runs on songbird's event
        // task, and the teardown takes the `Call` lock (via
        // `Songbird::remove`) plus does Discord HTTP for the panel edit —
        // none of which the event loop should be blocked on.
        let registry = self.registry.clone();
        let guild_id = self.guild_id;
        tokio::spawn(async move {
            // Drops the queue and now-playing state, refreshes the `/player`
            // panel so it stops showing a live progress bar for a track that
            // isn't playing, and removes the dead `Call` so the next `/play`
            // rejoins voice instead of playing into nothing.
            if let Err(err) = registry.leave(guild_id).await {
                tracing::debug!(%guild_id, %err, "cleanup after voice disconnect");
            }
        });

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queued(video_id: &str) -> QueuedTrack {
        QueuedTrack {
            track: Track {
                video_id: video_id.to_string(),
                title: "t".to_string(),
                channel: "c".to_string(),
                duration: Some(Duration::from_secs(10)),
            },
            requested_by: UserId::new(1),
        }
    }

    #[test]
    fn fresh_state_is_idle() {
        assert!(GuildState::default().is_idle());
    }

    #[test]
    fn state_with_a_current_track_is_not_idle() {
        let state = GuildState {
            now_playing: Some(queued("abc")),
            ..GuildState::default()
        };
        assert!(!state.is_idle());
    }

    /// The case a `now_playing`-only idle check missed: a queue entry with
    /// nothing playing (e.g. a radio refill that landed just after a
    /// `/stop`) must not be silently disconnected out from under.
    #[test]
    fn state_with_only_a_queued_track_is_not_idle() {
        let mut state = GuildState::default();
        state.queue.push_back(queued("abc"));
        assert!(!state.is_idle());
    }

    #[test]
    fn volume_is_clamped_to_full_scale() {
        assert!((volume_multiplier(0) - 0.0).abs() < f32::EPSILON);
        assert!((volume_multiplier(50) - 0.5).abs() < f32::EPSILON);
        assert!((volume_multiplier(100) - 1.0).abs() < f32::EPSILON);
        assert!((volume_multiplier(255) - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn preflight_classification_replaces_an_opaque_failure() {
        let original = PlaybackError::Other("some opaque yt-dlp noise".to_string());
        let better = better_playback_error(original, Some(PlaybackError::AgeRestricted));
        assert!(matches!(better, PlaybackError::AgeRestricted));
    }

    #[test]
    fn an_already_classified_failure_is_kept() {
        let original = PlaybackError::Unavailable;
        let better = better_playback_error(original, Some(PlaybackError::AgeRestricted));
        assert!(matches!(better, PlaybackError::Unavailable));
    }

    #[test]
    fn an_opaque_failure_survives_an_unhelpful_preflight() {
        for preflight in [None, Some(PlaybackError::Other("noise".to_string()))] {
            let original = PlaybackError::Other("the real message".to_string());
            match better_playback_error(original, preflight) {
                PlaybackError::Other(message) => assert_eq!(message, "the real message"),
                other => panic!("expected Other, got {other:?}"),
            }
        }
    }

    /// Non-HTTP failures are never "the message was deleted", so they must
    /// not clear the panel pointer. The 404/`Unknown Message` side can't be
    /// unit-tested: serenity's `DiscordJsonError` is `#[non_exhaustive]`, so
    /// an `ErrorResponse` can't be constructed outside serenity.
    #[test]
    fn transient_failures_do_not_count_as_a_deleted_panel() {
        for err in [
            serenity::Error::Other("timed out"),
            serenity::Error::Url("bad url".to_string()),
        ] {
            assert!(matches!(
                PanelEditFailure::classify(&err),
                PanelEditFailure::Transient(_)
            ));
        }
    }

    // ---- GuildState::claim_panel / set_panel / release_panel ----

    fn sample_panel() -> (ChannelId, MessageId) {
        (ChannelId::new(111), MessageId::new(222))
    }

    #[test]
    fn claim_panel_reserves_a_fresh_slot() {
        let mut state = GuildState::default();
        assert!(matches!(state.claim_panel(), PanelClaim::Reserved));
        assert!(state.panel_reserved);
        assert!(state.panel.is_none());
    }

    #[test]
    fn claim_panel_returns_existing_without_reserving() {
        let mut state = GuildState::default();
        let panel = sample_panel();
        state.panel = Some(panel);

        match state.claim_panel() {
            PanelClaim::Existing(got) => assert_eq!(got, panel),
            PanelClaim::Reserved => panic!("expected Existing, got Reserved"),
            PanelClaim::InProgress => panic!("expected Existing, got InProgress"),
        }
        // Seeing an existing panel must not also flip the reservation flag.
        assert!(!state.panel_reserved);
    }

    /// The race this whole mechanism exists to close: a second claim while
    /// the first reservation is still outstanding (nothing posted yet) must
    /// not also get `Reserved` — that would let both callers post a panel.
    #[test]
    fn a_second_claim_while_reserved_does_not_also_reserve() {
        let mut state = GuildState::default();
        assert!(matches!(state.claim_panel(), PanelClaim::Reserved));
        assert!(matches!(state.claim_panel(), PanelClaim::InProgress));
    }

    #[test]
    fn set_panel_fulfils_a_reservation_and_records_the_message() {
        let mut state = GuildState::default();
        assert!(matches!(state.claim_panel(), PanelClaim::Reserved));

        let (channel_id, message_id) = sample_panel();
        state.set_panel(channel_id, message_id);

        assert_eq!(state.panel, Some((channel_id, message_id)));
        assert!(!state.panel_reserved);
    }

    #[test]
    fn release_panel_clears_the_reservation_without_touching_panel() {
        let mut state = GuildState::default();
        assert!(matches!(state.claim_panel(), PanelClaim::Reserved));

        state.release_panel();

        assert!(!state.panel_reserved);
        assert!(state.panel.is_none());
        // The slot is claimable again after a released reservation.
        assert!(matches!(state.claim_panel(), PanelClaim::Reserved));
    }
}
