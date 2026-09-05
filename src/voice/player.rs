use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use poise::serenity_prelude as serenity;
use rand::seq::SliceRandom;
use serenity::{ChannelId, GuildId, MessageId, UserId};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::db;
use crate::voice::radio;
use crate::voice::resolve::{self, PlaybackError};
use crate::youtube::api::{Track, YouTubeClient};

type Prefetch = JoinHandle<Result<AudioSource, PlaybackError>>;

const IDLE_DISCONNECT: Duration = Duration::from_secs(150);

const MAX_VOLUME: u8 = 100;

const RADIO_HISTORY_CAP: usize = 5;

const RADIO_REFILL_BATCH: usize = 3;

fn discard_prefetch(prefetch: Prefetch) {
    prefetch.abort();
    tokio::spawn(async move {
        if let Ok(Ok(source)) = prefetch.await {
            let _ = tokio::fs::remove_file(&source.path).await;
        }
    });
}

fn volume_multiplier(volume: u8) -> f32 {
    f32::from(volume.min(MAX_VOLUME)) / 100.0
}

fn pick_radio_seed<'a>(history: &'a [String], rng: &mut impl rand::Rng) -> Option<&'a str> {
    use rand::seq::IndexedRandom;

    history
        .iter()
        .enumerate()
        .collect::<Vec<_>>()
        .choose_weighted(rng, |(i, _)| (i + 1) as u32)
        .ok()
        .map(|(_, id)| id.as_str())
}

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

enum PanelEditFailure {
    Gone,
    Transient(String),
}

impl PanelEditFailure {
    fn classify(err: &serenity::Error) -> Self {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedTrack {
    pub track: Track,
    pub requested_by: UserId,
}

#[derive(Debug)]
pub enum PlayerError {
    NotConnected,
    NothingPlaying,
    NothingToShuffle,
    QueueEmpty,
    Join(String),
    Playback(String),
    Storage(String),
}

impl std::fmt::Display for PlayerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConnected => write!(f, "not connected to a voice channel"),
            Self::NothingPlaying => write!(f, "nothing is playing"),
            Self::NothingToShuffle => write!(f, "not enough upcoming tracks to shuffle"),
            Self::QueueEmpty => write!(f, "the queue is already empty"),
            Self::Join(message) => write!(f, "failed to join voice channel: {message}"),
            Self::Playback(message) => write!(f, "playback error: {message}"),
            Self::Storage(message) => write!(f, "failed to save setting: {message}"),
        }
    }
}

impl std::error::Error for PlayerError {}

pub struct AudioSource {
    pub(crate) video_id: String,
    pub(crate) path: std::path::PathBuf,
}

pub struct TrackStatus {
    #[allow(dead_code)]
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

#[derive(Default)]
struct GuildState {
    now_playing: Option<QueuedTrack>,
    last_played: Option<QueuedTrack>,
    current_handle: Option<Arc<dyn VoiceTrack>>,
    current_track_id: Option<Uuid>,
    prefetch: Option<Prefetch>,
    panel: Option<(ChannelId, MessageId)>,
    panel_reserved: bool,
    radio_enabled: bool,
    radio_history: VecDeque<String>,
    radio_requested_by: Option<UserId>,
    radio_played: HashSet<String>,
    radio_exhausted: bool,
    epoch: u64,
}

impl GuildState {
    fn set_panel(&mut self, channel_id: ChannelId, message_id: MessageId) {
        self.panel = Some((channel_id, message_id));
        self.panel_reserved = false;
    }

    fn release_panel(&mut self) {
        self.panel_reserved = false;
    }

    fn begin_panel_repost(&mut self) -> PanelRepost {
        if self.panel_reserved {
            return PanelRepost::InProgress;
        }
        self.panel_reserved = true;
        PanelRepost::Reserved
    }
}

pub enum PanelRepost {
    Reserved,
    InProgress,
}

pub struct QueueSnapshot {
    pub now_playing: Option<QueuedTrack>,
    pub loading: Option<QueuedTrack>,
    pub last_played: Option<QueuedTrack>,
    pub upcoming: Vec<QueuedTrack>,
}

enum StartOutcome {
    Committed,
    Stale,
    Failed(PlayerError),
}

struct SessionSnapshot {
    now_playing: Option<QueuedTrack>,
    last_played: Option<QueuedTrack>,
    radio_enabled: bool,
    radio_requested_by: Option<UserId>,
    radio_history: Vec<String>,
}

impl From<&GuildState> for SessionSnapshot {
    fn from(state: &GuildState) -> Self {
        Self {
            now_playing: state.now_playing.clone(),
            last_played: state.last_played.clone(),
            radio_enabled: state.radio_enabled,
            radio_requested_by: state.radio_requested_by,
            radio_history: Vec::from(state.radio_history.clone()),
        }
    }
}

fn persisted_to_queued_track(pair: Option<(Track, String)>) -> Option<QueuedTrack> {
    pair.and_then(|(track, requested_by)| {
        requested_by.parse().ok().map(|id| QueuedTrack {
            track,
            requested_by: UserId::new(id),
        })
    })
}

#[derive(Clone)]
pub struct PlayerRegistry {
    voice: Arc<dyn VoiceBackend>,
    discord_http: Arc<serenity::Http>,
    cookies_file: Option<String>,
    db: sqlx::SqlitePool,
    youtube: YouTubeClient,
    guilds: Arc<Mutex<HashMap<GuildId, GuildState>>>,
}

#[async_trait::async_trait]
impl VoiceEvents for PlayerRegistry {
    async fn track_finished(&self, guild_id: GuildId, track_id: Uuid) {
        self.advance(guild_id, track_id).await;
    }

    async fn connection_lost(&self, guild_id: GuildId) {
        if let Err(err) = self.leave(guild_id).await {
            tracing::debug!(%guild_id, %err, "cleanup after voice disconnect");
        }
    }
}

impl PlayerRegistry {
    pub fn new(
        voice: Arc<dyn VoiceBackend>,
        discord_http: Arc<serenity::Http>,
        cookies_file: Option<String>,
        db: sqlx::SqlitePool,
        youtube: YouTubeClient,
    ) -> Self {
        Self {
            voice,
            discord_http,
            cookies_file,
            db,
            youtube,
            guilds: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn events(&self) -> Arc<dyn VoiceEvents> {
        Arc::new(self.clone())
    }

    #[cfg(test)]
    fn new_for_test(voice: Arc<dyn VoiceBackend>, db: sqlx::SqlitePool) -> Self {
        Self::new(
            voice,
            Arc::new(serenity::Http::new("test-token")),
            None,
            db,
            YouTubeClient::default(),
        )
    }

    #[cfg(test)]
    async fn settle_playback_start(&self, guild_id: GuildId) {
        for _ in 0..500 {
            let settled = {
                let guilds = self.guilds.lock().await;
                match guilds.get(&guild_id) {
                    Some(state) => state.current_track_id.is_some() || state.now_playing.is_none(),
                    None => true,
                }
            };
            if settled {
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        panic!("background playback start did not settle for guild {guild_id}");
    }

    pub async fn join(
        &self,
        guild_id: GuildId,
        voice_channel_id: ChannelId,
    ) -> Result<(), PlayerError> {
        self.voice
            .join(guild_id, voice_channel_id, self.events())
            .await
            .map_err(PlayerError::Join)?;

        if let Some(call) = self.voice.call(guild_id) {
            self.restore_session_if_new(guild_id, call).await;
        }

        Ok(())
    }

    async fn restore_session_if_new(&self, guild_id: GuildId, call: Arc<dyn VoiceCall>) {
        if self.guild_state_exists(guild_id).await {
            return;
        }

        let guild_id_str = guild_id.to_string();
        let Some(session) = self.load_persisted_session(guild_id, &guild_id_str).await else {
            return;
        };

        let Some((candidate, epoch)) = self
            .apply_restored_session(guild_id, &guild_id_str, session)
            .await
        else {
            return;
        };

        match candidate {
            Some(queued) => self.spawn_start_sequence(guild_id, call, queued, epoch, None),
            None => self.schedule_idle_disconnect(guild_id),
        }

        self.refresh_panel(guild_id).await;
    }

    async fn guild_state_exists(&self, guild_id: GuildId) -> bool {
        let guilds = self.guilds.lock().await;
        guilds.contains_key(&guild_id)
    }

    async fn load_persisted_session(
        &self,
        guild_id: GuildId,
        guild_id_str: &str,
    ) -> Option<db::PersistedSession> {
        match db::load_guild_session(&self.db, guild_id_str).await {
            Ok(Some(session)) => Some(session),
            Ok(None) => None,
            Err(err) => {
                tracing::warn!(%guild_id, %err, "failed to load persisted session");
                None
            }
        }
    }

    async fn apply_restored_session(
        &self,
        guild_id: GuildId,
        guild_id_str: &str,
        session: db::PersistedSession,
    ) -> Option<(Option<QueuedTrack>, u64)> {
        let mut guilds = self.guilds.lock().await;
        let state = guilds.entry(guild_id).or_default();
        if state.now_playing.is_some() {
            return None;
        }
        state.radio_enabled = session.radio_enabled;
        state.radio_requested_by = session
            .radio_requested_by
            .as_deref()
            .and_then(|id| id.parse().ok())
            .map(UserId::new);
        state.radio_history = session.radio_history.into();
        state.now_playing = persisted_to_queued_track(session.now_playing);
        state.last_played = persisted_to_queued_track(session.last_played);
        if state.now_playing.is_none() {
            let next = match db::queue_pop_front(&self.db, guild_id_str).await {
                Ok(next) => next,
                Err(err) => {
                    tracing::warn!(%guild_id, %err, "failed to pop the next queued track");
                    None
                }
            };
            state.now_playing = next.clone();
        }
        Some((state.now_playing.clone(), state.epoch))
    }

    async fn persist_session(&self, guild_id: GuildId) {
        let snapshot = {
            let guilds = self.guilds.lock().await;
            let Some(state) = guilds.get(&guild_id) else {
                tracing::debug!(%guild_id, "skipping session persist for a guild with no state");
                return;
            };
            SessionSnapshot::from(state)
        };
        self.save_session_snapshot(guild_id, snapshot).await;
    }

    async fn save_session_snapshot(&self, guild_id: GuildId, snapshot: SessionSnapshot) {
        let guild_id_str = guild_id.to_string();
        let radio_requested_by = snapshot.radio_requested_by.map(|id| id.to_string());
        let now_playing_requested_by = snapshot
            .now_playing
            .as_ref()
            .map(|q| q.requested_by.to_string());
        let now_playing_arg = snapshot
            .now_playing
            .as_ref()
            .zip(now_playing_requested_by.as_deref())
            .map(|(q, rb)| (&q.track, rb));
        let last_played_requested_by = snapshot
            .last_played
            .as_ref()
            .map(|q| q.requested_by.to_string());
        let last_played_arg = snapshot
            .last_played
            .as_ref()
            .zip(last_played_requested_by.as_deref())
            .map(|(q, rb)| (&q.track, rb));
        let result = db::save_guild_session_meta(
            &self.db,
            &guild_id_str,
            snapshot.radio_enabled,
            radio_requested_by.as_deref(),
            &snapshot.radio_history,
            now_playing_arg,
            last_played_arg,
        )
        .await;
        if let Err(err) = result {
            tracing::warn!(%guild_id, %err, "failed to persist guild session");
        }
    }

    pub async fn leave(&self, guild_id: GuildId) -> Result<(), PlayerError> {
        let (snapshot, panel) = {
            let mut guilds = self.guilds.lock().await;
            let snapshot = guilds.get(&guild_id).map(SessionSnapshot::from);
            let panel = Self::take_guild_state(guild_id, &mut guilds);
            (snapshot, panel)
        };
        if let Some(snapshot) = snapshot {
            self.save_session_snapshot(guild_id, snapshot).await;
        }

        let result = self.voice.remove(guild_id).await.map_err(PlayerError::Join);

        if let Some((channel_id, message_id)) = panel {
            let _ = self.edit_panel(guild_id, channel_id, message_id).await;
        }
        result
    }

    fn take_guild_state(
        guild_id: GuildId,
        guilds: &mut HashMap<GuildId, GuildState>,
    ) -> Option<(ChannelId, MessageId)> {
        let mut removed = guilds.remove(&guild_id);
        if let Some(prefetch) = removed.as_mut().and_then(|state| state.prefetch.take()) {
            discard_prefetch(prefetch);
        }
        removed.and_then(|state| state.panel)
    }

    async fn leave_if_idle(&self, guild_id: GuildId) {
        let (snapshot, panel) = {
            let mut guilds = self.guilds.lock().await;
            let now_playing_set = guilds
                .get(&guild_id)
                .is_some_and(|state| state.now_playing.is_some());
            let handle = guilds
                .get(&guild_id)
                .and_then(|state| state.current_handle.clone());

            let still_idle = if now_playing_set {
                match handle {
                    Some(handle) => handle.status().await.is_some_and(|status| status.paused),
                    None => false,
                }
            } else {
                db::queue_len(&self.db, &guild_id.to_string())
                    .await
                    .unwrap_or(0)
                    == 0
            };
            if !still_idle {
                return;
            }

            let snapshot = guilds.get(&guild_id).map(SessionSnapshot::from);
            let panel = Self::take_guild_state(guild_id, &mut guilds);
            (snapshot, panel)
        };

        if let Some(snapshot) = snapshot {
            self.save_session_snapshot(guild_id, snapshot).await;
        }

        let result = self.voice.remove(guild_id).await.map_err(PlayerError::Join);

        if let Err(err) = result {
            tracing::warn!(%guild_id, %err, "idle disconnect failed to leave voice");
        }
        if let Some((channel_id, message_id)) = panel {
            let _ = self.edit_panel(guild_id, channel_id, message_id).await;
        }
    }

    pub fn is_connected(&self, guild_id: GuildId) -> bool {
        self.voice.call(guild_id).is_some()
    }

    pub async fn enqueue(&self, guild_id: GuildId, queued: QueuedTrack) -> Result<(), PlayerError> {
        let (call, start) = {
            let mut guilds = self.guilds.lock().await;
            let call = self.voice.call(guild_id).ok_or(PlayerError::NotConnected)?;
            let state = guilds.entry(guild_id).or_default();
            state.radio_exhausted = false;
            let should_start = state.now_playing.is_none();
            if should_start {
                state.now_playing = Some(queued.clone());
            } else if let Err(err) =
                db::queue_push_back(&self.db, &guild_id.to_string(), &queued).await
            {
                return Err(PlayerError::Storage(err.to_string()));
            }
            (call, should_start.then_some(state.epoch))
        };

        if let Some(epoch) = start {
            self.spawn_start_sequence(guild_id, call, queued, epoch, None);
        }

        self.refresh_panel(guild_id).await;
        self.persist_session(guild_id).await;
        Ok(())
    }

    pub async fn enqueue_many(
        &self,
        guild_id: GuildId,
        tracks: Vec<QueuedTrack>,
    ) -> Result<(), PlayerError> {
        if tracks.is_empty() {
            return Ok(());
        }
        let guild_id_str = guild_id.to_string();
        let mut tracks = tracks;
        let first = tracks.first().cloned();

        let (call, start) = {
            let mut guilds = self.guilds.lock().await;
            let call = self.voice.call(guild_id).ok_or(PlayerError::NotConnected)?;
            let state = guilds.entry(guild_id).or_default();
            state.radio_exhausted = false;
            let needs_start = state.now_playing.is_none();
            if needs_start {
                state.now_playing = first.clone();
            }

            let rest: Vec<QueuedTrack> = if needs_start {
                tracks.split_off(1)
            } else {
                std::mem::take(&mut tracks)
            };
            if !rest.is_empty()
                && let Err(err) = db::queue_push_many(&self.db, &guild_id_str, &rest).await
            {
                if needs_start {
                    state.now_playing = None;
                }
                return Err(PlayerError::Storage(err.to_string()));
            }

            let start = match (needs_start, &first) {
                (true, Some(track)) => Some((track.clone(), state.epoch)),
                _ => None,
            };
            (call, start)
        };

        if let Some((first, epoch)) = start {
            self.spawn_start_sequence(guild_id, call, first, epoch, None);
        }

        self.refresh_panel(guild_id).await;
        self.persist_session(guild_id).await;
        Ok(())
    }

    async fn resolve_prefetched(
        &self,
        queued: &QueuedTrack,
        prefetched: Prefetch,
    ) -> Result<AudioSource, PlaybackError> {
        match prefetched.await {
            Ok(Ok(source)) => return Ok(source),
            Ok(Err(err)) => tracing::warn!(%err, "prefetch failed, resolving fresh instead"),
            Err(err) => tracing::warn!(%err, "prefetch task panicked, resolving fresh instead"),
        }
        self.voice.buffered_source(&queued.track).await
    }

    async fn cached_input(&self, queued: &QueuedTrack) -> Result<AudioSource, PlaybackError> {
        self.voice.buffered_source(&queued.track).await
    }

    async fn classify_playback_failure(&self, video_id: &str, err: PlaybackError) -> PlayerError {
        if !matches!(err, PlaybackError::Other(_)) {
            return PlayerError::Playback(err.to_string());
        }

        let preflight = resolve::preflight_check(video_id, self.cookies_file.as_deref())
            .await
            .err();
        PlayerError::Playback(better_playback_error(err, preflight).to_string())
    }

    async fn promote_next(&self, guild_id: GuildId) -> Option<(QueuedTrack, u64)> {
        let mut guilds = self.guilds.lock().await;
        if !guilds.contains_key(&guild_id) {
            return None;
        }
        let next = match db::queue_pop_front(&self.db, &guild_id.to_string()).await {
            Ok(next) => next,
            Err(err) => {
                tracing::warn!(%guild_id, %err, "failed to pop the next queued track");
                None
            }
        };
        let state = guilds.get_mut(&guild_id)?;
        state.now_playing = next.clone();
        next.map(|queued| (queued, state.epoch))
    }

    fn spawn_start_sequence(
        &self,
        guild_id: GuildId,
        call: Arc<dyn VoiceCall>,
        first: QueuedTrack,
        epoch: u64,
        prefetch: Option<Prefetch>,
    ) {
        let registry = self.clone();
        tokio::spawn(async move {
            registry
                .run_start_sequence(guild_id, call, first, epoch, prefetch)
                .await;
        });
    }

    async fn run_start_sequence(
        &self,
        guild_id: GuildId,
        call: Arc<dyn VoiceCall>,
        first: QueuedTrack,
        first_epoch: u64,
        prefetch: Option<Prefetch>,
    ) {
        let mut candidate = Some((first, first_epoch));
        let mut prefetch = prefetch;
        let mut started = false;
        while let Some((queued, epoch)) = candidate.take() {
            match self
                .try_start_playback(guild_id, call.clone(), queued, prefetch.take(), epoch)
                .await
            {
                StartOutcome::Committed => {
                    started = true;
                    break;
                }
                StartOutcome::Stale => return,
                StartOutcome::Failed(err) => {
                    tracing::warn!(%err, "failed to start queued track, trying the next one");
                    candidate = self.promote_next(guild_id).await;
                }
            }
        }

        if !started {
            self.schedule_idle_disconnect(guild_id);
        }

        self.refresh_panel(guild_id).await;
        self.persist_session(guild_id).await;
    }

    async fn try_start_playback(
        &self,
        guild_id: GuildId,
        call: Arc<dyn VoiceCall>,
        queued: QueuedTrack,
        prefetched: Option<Prefetch>,
        expected_epoch: u64,
    ) -> StartOutcome {
        let source = match self
            .resolve_for_start(guild_id, &queued, prefetched, expected_epoch)
            .await
        {
            Ok(source) => source,
            Err(outcome) => return outcome,
        };

        let handle = match call.play(source).await {
            Ok(handle) => handle,
            Err(err) => return StartOutcome::Failed(PlayerError::Playback(err)),
        };

        self.commit_started_track(guild_id, queued, handle, expected_epoch)
            .await
    }

    async fn resolve_for_start(
        &self,
        guild_id: GuildId,
        queued: &QueuedTrack,
        prefetched: Option<Prefetch>,
        expected_epoch: u64,
    ) -> Result<AudioSource, StartOutcome> {
        let still_current = {
            let guilds = self.guilds.lock().await;
            guilds
                .get(&guild_id)
                .is_some_and(|state| state.epoch == expected_epoch)
        };
        if !still_current {
            if let Some(prefetch) = prefetched {
                discard_prefetch(prefetch);
            }
            return Err(StartOutcome::Stale);
        }

        let resolved = match prefetched {
            Some(handle) => self.resolve_prefetched(queued, handle).await,
            None => self.voice.buffered_source(&queued.track).await,
        };
        match resolved {
            Ok(source) => Ok(source),
            Err(err) => Err(StartOutcome::Failed(
                self.classify_playback_failure(&queued.track.video_id, err)
                    .await,
            )),
        }
    }

    async fn commit_started_track(
        &self,
        guild_id: GuildId,
        queued: QueuedTrack,
        handle: Arc<dyn VoiceTrack>,
        expected_epoch: u64,
    ) -> StartOutcome {
        let track_id = handle.uuid();

        let volume = db::get_guild_volume(&self.db, &guild_id.to_string())
            .await
            .unwrap_or(db::DEFAULT_VOLUME);
        if let Err(err) = handle.set_volume(volume_multiplier(volume)).await {
            tracing::warn!(%err, "failed to apply saved volume to new track");
        }

        let mut guilds = self.guilds.lock().await;
        let matches_epoch = guilds
            .get(&guild_id)
            .is_some_and(|state| state.epoch == expected_epoch);
        if !matches_epoch {
            drop(guilds);
            let _ = handle.stop().await;
            return StartOutcome::Stale;
        }

        let mut needs_radio_refill = false;
        if let Some(state) = guilds.get_mut(&guild_id) {
            handle.notify_when_finished(guild_id, self.events());
            state.current_handle = Some(handle);
            state.current_track_id = Some(track_id);
            state.last_played = Some(queued.clone());
            state.radio_history.push_back(queued.track.video_id.clone());
            if state.radio_history.len() > RADIO_HISTORY_CAP {
                state.radio_history.pop_front();
            }
            state.radio_requested_by = Some(queued.requested_by);

            let next = db::queue_peek_front(&self.db, &guild_id.to_string())
                .await
                .ok()
                .flatten();
            if let Some(next) = next {
                let registry = self.clone();
                state.prefetch = Some(tokio::spawn(
                    async move { registry.cached_input(&next).await },
                ));
            } else {
                needs_radio_refill = true;
            }
        }
        drop(guilds);

        if needs_radio_refill {
            self.maybe_spawn_radio_refill(guild_id);
        }

        StartOutcome::Committed
    }

    async fn advance(&self, guild_id: GuildId, track_id: Uuid) {
        let (next, epoch, prefetch) = {
            let mut guilds = self.guilds.lock().await;
            let Some(state) = guilds.get_mut(&guild_id) else {
                return;
            };
            if state.current_track_id != Some(track_id) {
                return;
            }
            state.current_handle = None;
            state.current_track_id = None;
            let next = match db::queue_pop_front(&self.db, &guild_id.to_string()).await {
                Ok(next) => next,
                Err(err) => {
                    tracing::warn!(%guild_id, %err, "failed to pop the next queued track");
                    None
                }
            };
            state.now_playing = next.clone();
            (next, state.epoch, state.prefetch.take())
        };

        match (next, self.voice.call(guild_id)) {
            (Some(next), Some(call)) => {
                self.spawn_start_sequence(guild_id, call, next, epoch, prefetch)
            }
            _ => {
                if let Some(prefetch) = prefetch {
                    discard_prefetch(prefetch);
                }
                self.schedule_idle_disconnect(guild_id);
            }
        }

        self.refresh_panel(guild_id).await;
        self.persist_session(guild_id).await;
    }

    fn schedule_idle_disconnect(&self, guild_id: GuildId) {
        let registry = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(IDLE_DISCONNECT).await;

            registry.leave_if_idle(guild_id).await;
        });
    }

    pub async fn stop(&self, guild_id: GuildId) -> Result<(), PlayerError> {
        let handle = {
            let mut guilds = self.guilds.lock().await;
            let Some(state) = guilds.get_mut(&guild_id) else {
                return Err(PlayerError::NothingPlaying);
            };
            if state.now_playing.is_none() {
                return Err(PlayerError::NothingPlaying);
            }
            state.now_playing = None;
            state.last_played = None;
            state.current_track_id = None;
            state.epoch = state.epoch.wrapping_add(1);
            state.radio_enabled = false;
            state.radio_history.clear();
            state.radio_requested_by = None;
            state.radio_played.clear();
            state.radio_exhausted = false;
            if let Some(prefetch) = state.prefetch.take() {
                discard_prefetch(prefetch);
            }
            let handle = state.current_handle.take();
            if let Err(err) = db::queue_clear(&self.db, &guild_id.to_string()).await {
                tracing::warn!(%guild_id, %err, "failed to clear the persisted queue on stop");
            }
            handle
        };

        if let Some(handle) = handle
            && let Err(err) = handle.stop().await
        {
            return Err(PlayerError::Playback(err));
        }

        self.schedule_idle_disconnect(guild_id);
        self.refresh_panel(guild_id).await;
        if let Err(err) = db::clear_guild_session(&self.db, &guild_id.to_string()).await {
            tracing::warn!(%guild_id, %err, "failed to clear the persisted session on stop");
        }
        Ok(())
    }

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
            .await
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
            .await
            .map_err(|e| PlayerError::Playback(e.to_string()));
        if result.is_ok() {
            self.schedule_idle_disconnect(guild_id);
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
            .resume()
            .await
            .map_err(|e| PlayerError::Playback(e.to_string()));
        if result.is_ok() {
            self.refresh_panel(guild_id).await;
        }
        result
    }

    pub async fn shuffle(&self, guild_id: GuildId) -> Result<(), PlayerError> {
        let guild_id_str = guild_id.to_string();
        let mut guilds = self.guilds.lock().await;
        let mut items = db::queue_all(&self.db, &guild_id_str)
            .await
            .map_err(|e| PlayerError::Storage(e.to_string()))?;
        if items.len() < 2 {
            return Err(PlayerError::NothingToShuffle);
        }

        items.shuffle(&mut rand::rng());
        db::queue_replace_all(&self.db, &guild_id_str, &items)
            .await
            .map_err(|e| PlayerError::Storage(e.to_string()))?;
        if let Some(state) = guilds.get_mut(&guild_id) {
            self.restart_prefetch(state, items.first().cloned());
        }
        drop(guilds);

        self.refresh_panel(guild_id).await;
        self.persist_session(guild_id).await;
        Ok(())
    }

    pub async fn clear_queue(&self, guild_id: GuildId) -> Result<(), PlayerError> {
        let guild_id_str = guild_id.to_string();
        let needs_refill = {
            let mut guilds = self.guilds.lock().await;
            let len = db::queue_len(&self.db, &guild_id_str)
                .await
                .map_err(|e| PlayerError::Storage(e.to_string()))?;
            if len == 0 {
                return Err(PlayerError::QueueEmpty);
            }
            db::queue_clear(&self.db, &guild_id_str)
                .await
                .map_err(|e| PlayerError::Storage(e.to_string()))?;
            match guilds.get_mut(&guild_id) {
                Some(state) => {
                    if let Some(prefetch) = state.prefetch.take() {
                        discard_prefetch(prefetch);
                    }
                    state.radio_enabled
                }
                None => false,
            }
        };

        if needs_refill {
            self.maybe_spawn_radio_refill(guild_id);
        }

        self.refresh_panel(guild_id).await;
        self.persist_session(guild_id).await;
        Ok(())
    }

    fn restart_prefetch(&self, state: &mut GuildState, next: Option<QueuedTrack>) {
        if let Some(old) = state.prefetch.take() {
            discard_prefetch(old);
        }
        if let Some(next) = next {
            let registry = self.clone();
            state.prefetch = Some(tokio::spawn(
                async move { registry.cached_input(&next).await },
            ));
        }
    }

    pub async fn is_paused(&self, guild_id: GuildId) -> Option<bool> {
        let handle = {
            let guilds = self.guilds.lock().await;
            guilds
                .get(&guild_id)
                .and_then(|state| state.current_handle.clone())
        }?;
        handle.status().await.map(|status| status.paused)
    }

    pub async fn get_volume(&self, guild_id: GuildId) -> u8 {
        db::get_guild_volume(&self.db, &guild_id.to_string())
            .await
            .unwrap_or(db::DEFAULT_VOLUME)
    }

    pub async fn set_volume(&self, guild_id: GuildId, volume: u8) -> Result<(), PlayerError> {
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
            && let Err(err) = handle.set_volume(volume_multiplier(volume)).await
        {
            tracing::warn!(%err, "failed to apply volume change to current track");
        }

        self.refresh_panel(guild_id).await;
        Ok(())
    }

    pub async fn toggle_radio(&self, guild_id: GuildId) -> bool {
        let (enabled, needs_refill) = {
            let mut guilds = self.guilds.lock().await;
            let state = guilds.entry(guild_id).or_default();
            state.radio_enabled = !state.radio_enabled;
            state.radio_exhausted = false;
            let enabled = state.radio_enabled;
            let needs_refill = if enabled {
                db::queue_len(&self.db, &guild_id.to_string())
                    .await
                    .unwrap_or(0)
                    == 0
            } else {
                false
            };
            (enabled, needs_refill)
        };

        if enabled && needs_refill {
            self.maybe_spawn_radio_refill(guild_id);
        }

        self.refresh_panel(guild_id).await;
        self.persist_session(guild_id).await;
        enabled
    }

    pub async fn is_radio_enabled(&self, guild_id: GuildId) -> bool {
        let guilds = self.guilds.lock().await;
        guilds
            .get(&guild_id)
            .is_some_and(|state| state.radio_enabled)
    }

    fn maybe_spawn_radio_refill(&self, guild_id: GuildId) {
        let registry = self.clone();
        tokio::spawn(async move { registry.run_radio_refill(guild_id).await });
    }

    async fn run_radio_refill(&self, guild_id: GuildId) {
        let Some((history, requested_by, played, epoch)) =
            self.radio_refill_snapshot(guild_id).await
        else {
            return;
        };
        let Some(seed) = pick_radio_seed(&history, &mut rand::rng()) else {
            return;
        };

        let mix_ids = match radio::list_mix_video_ids(seed, self.cookies_file.as_deref()).await {
            Ok(ids) => ids,
            Err(err) => {
                tracing::warn!(%guild_id, %seed, %err, "radio refill: failed to list mix");
                return;
            }
        };

        let Some(candidates) = self
            .radio_refill_candidates(guild_id, seed, epoch, &mix_ids, &history, &played)
            .await
        else {
            return;
        };

        let candidate_refs: Vec<&str> = candidates.iter().map(String::as_str).collect();
        let hydrated = match self.youtube.hydrate_videos(&candidate_refs).await {
            Ok(tracks) => tracks,
            Err(err) => {
                tracing::warn!(%guild_id, %err, "radio refill: failed to hydrate mix candidates");
                return;
            }
        };
        if hydrated.is_empty() {
            tracing::warn!(%guild_id, %seed, "radio refill: hydration returned no tracks");
            return;
        }

        let pushed = self
            .push_radio_refill(guild_id, epoch, requested_by, hydrated)
            .await;

        if pushed > 0 {
            self.kick_off_if_idle(guild_id).await;
            self.refresh_panel(guild_id).await;
            self.persist_session(guild_id).await;
        }
    }

    async fn kick_off_if_idle(&self, guild_id: GuildId) {
        let idle = {
            let guilds = self.guilds.lock().await;
            guilds
                .get(&guild_id)
                .is_some_and(|state| state.now_playing.is_none())
        };
        if !idle {
            return;
        }
        let Some(call) = self.voice.call(guild_id) else {
            return;
        };
        match self.promote_next(guild_id).await {
            Some((queued, epoch)) => self.spawn_start_sequence(guild_id, call, queued, epoch, None),
            None => self.schedule_idle_disconnect(guild_id),
        }
    }

    async fn radio_refill_snapshot(
        &self,
        guild_id: GuildId,
    ) -> Option<(Vec<String>, UserId, HashSet<String>, u64)> {
        let guilds = self.guilds.lock().await;
        let state = guilds.get(&guild_id)?;
        if state.radio_exhausted {
            return None;
        }
        let (true, Some(requested_by)) = (state.radio_enabled, state.radio_requested_by) else {
            return None;
        };
        Some((
            Vec::from(state.radio_history.clone()),
            requested_by,
            state.radio_played.clone(),
            state.epoch,
        ))
    }

    async fn radio_refill_candidates(
        &self,
        guild_id: GuildId,
        seed: &str,
        epoch: u64,
        mix_ids: &[String],
        history: &[String],
        played: &HashSet<String>,
    ) -> Option<Vec<String>> {
        let mut candidates: Vec<String> = mix_ids
            .iter()
            .filter(|id| !history.iter().any(|played_id| played_id == *id) && !played.contains(*id))
            .take(10)
            .cloned()
            .collect();
        if candidates.is_empty() {
            tracing::warn!(
                %guild_id, %seed,
                "radio refill: mix exhausted, no unplayed candidates left"
            );
            let mut guilds = self.guilds.lock().await;
            if let Some(state) = guilds.get_mut(&guild_id)
                && state.epoch == epoch
            {
                state.radio_exhausted = true;
            }
            return None;
        }
        candidates.shuffle(&mut rand::rng());
        candidates.truncate(RADIO_REFILL_BATCH);
        Some(candidates)
    }

    async fn push_radio_refill(
        &self,
        guild_id: GuildId,
        epoch: u64,
        requested_by: UserId,
        hydrated: Vec<Track>,
    ) -> usize {
        let mut guilds = self.guilds.lock().await;
        match guilds.get_mut(&guild_id) {
            Some(state) if state.radio_enabled && state.epoch == epoch => {
                let to_queue: Vec<QueuedTrack> = hydrated
                    .into_iter()
                    .map(|track| {
                        state.radio_played.insert(track.video_id.clone());
                        QueuedTrack {
                            track,
                            requested_by,
                        }
                    })
                    .collect();
                let count = to_queue.len();
                match db::queue_push_many(&self.db, &guild_id.to_string(), &to_queue).await {
                    Ok(()) => count,
                    Err(err) => {
                        tracing::warn!(%guild_id, %err, "radio refill: failed to persist refilled tracks");
                        0
                    }
                }
            }
            _ => 0,
        }
    }

    pub async fn queue_snapshot(&self, guild_id: GuildId) -> QueueSnapshot {
        let (now_playing, loading, last_played, has_entry) = {
            let guilds = self.guilds.lock().await;
            match guilds.get(&guild_id) {
                Some(s) => {
                    let playing = s.current_track_id.is_some();
                    let now_playing = playing.then(|| s.now_playing.clone()).flatten();
                    let loading = (!playing).then(|| s.now_playing.clone()).flatten();
                    (now_playing, loading, s.last_played.clone(), true)
                }
                None => (None, None, None, false),
            }
        };
        let last_played = if has_entry {
            last_played
        } else {
            self.load_last_played_from_db(guild_id).await
        };
        let upcoming = db::queue_all(&self.db, &guild_id.to_string())
            .await
            .unwrap_or_else(|err| {
                tracing::warn!(%guild_id, %err, "failed to load queue snapshot");
                Vec::new()
            });
        QueueSnapshot {
            now_playing,
            loading,
            last_played,
            upcoming,
        }
    }

    async fn load_last_played_from_db(&self, guild_id: GuildId) -> Option<QueuedTrack> {
        match db::load_guild_session(&self.db, &guild_id.to_string()).await {
            Ok(Some(session)) => persisted_to_queued_track(session.last_played),
            Ok(None) => None,
            Err(err) => {
                tracing::warn!(%guild_id, %err, "failed to load last-played from persisted session");
                None
            }
        }
    }

    pub async fn begin_panel_repost(&self, guild_id: GuildId) -> PanelRepost {
        let mut guilds = self.guilds.lock().await;
        guilds.entry(guild_id).or_default().begin_panel_repost()
    }

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

    pub async fn release_panel_slot(&self, guild_id: GuildId) {
        let mut guilds = self.guilds.lock().await;
        if let Some(state) = guilds.get_mut(&guild_id) {
            state.release_panel();
        }
    }

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

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;

    use tokio::sync::Notify;

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
    fn volume_is_clamped_to_full_scale() {
        assert!((volume_multiplier(0) - 0.0).abs() < f32::EPSILON);
        assert!((volume_multiplier(50) - 0.5).abs() < f32::EPSILON);
        assert!((volume_multiplier(100) - 1.0).abs() < f32::EPSILON);
        assert!((volume_multiplier(255) - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn empty_history_yields_no_seed() {
        assert_eq!(pick_radio_seed(&[], &mut rand::rng()), None);
    }

    #[test]
    fn single_entry_history_always_picks_it() {
        let history = vec!["only".to_string()];
        assert_eq!(pick_radio_seed(&history, &mut rand::rng()), Some("only"));
    }

    #[test]
    fn seed_is_always_one_of_the_history_entries() {
        let history: Vec<String> = ["a", "b", "c", "d"].iter().map(|s| s.to_string()).collect();
        for _ in 0..200 {
            let seed = pick_radio_seed(&history, &mut rand::rng()).expect("non-empty history");
            assert!(history.iter().any(|id| id == seed));
        }
    }

    #[test]
    fn weighting_favors_the_most_recent_entry_over_many_draws() {
        use rand::SeedableRng;
        let history: Vec<String> = ["oldest", "b", "c", "newest"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        let mut newest_count = 0;
        let mut oldest_count = 0;
        for _ in 0..1000 {
            match pick_radio_seed(&history, &mut rng) {
                Some("newest") => newest_count += 1,
                Some("oldest") => oldest_count += 1,
                _ => {}
            }
        }
        assert!(newest_count > oldest_count);
        assert!(oldest_count > 0);
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

    fn sample_panel() -> (ChannelId, MessageId) {
        (ChannelId::new(111), MessageId::new(222))
    }

    #[test]
    fn set_panel_fulfils_a_reservation_and_records_the_message() {
        let mut state = GuildState::default();
        assert!(matches!(state.begin_panel_repost(), PanelRepost::Reserved));

        let (channel_id, message_id) = sample_panel();
        state.set_panel(channel_id, message_id);

        assert_eq!(state.panel, Some((channel_id, message_id)));
        assert!(!state.panel_reserved);
    }

    #[test]
    fn release_panel_clears_the_reservation_without_touching_panel() {
        let mut state = GuildState::default();
        assert!(matches!(state.begin_panel_repost(), PanelRepost::Reserved));

        state.release_panel();

        assert!(!state.panel_reserved);
        assert!(state.panel.is_none());
        assert!(matches!(state.begin_panel_repost(), PanelRepost::Reserved));
    }

    #[test]
    fn begin_panel_repost_reserves_even_when_a_panel_already_exists() {
        let mut state = GuildState {
            panel: Some(sample_panel()),
            ..GuildState::default()
        };

        assert!(matches!(state.begin_panel_repost(), PanelRepost::Reserved));
        assert!(state.panel_reserved);
    }

    #[test]
    fn begin_panel_repost_reports_in_progress_while_already_reserved() {
        let mut state = GuildState::default();
        assert!(matches!(state.begin_panel_repost(), PanelRepost::Reserved));
        assert!(matches!(
            state.begin_panel_repost(),
            PanelRepost::InProgress
        ));
    }

    #[derive(Default)]
    struct FakeTrackState {
        stopped: bool,
        paused: bool,
        volume: Option<f32>,
        stop_error: Option<String>,
        registered: Option<(GuildId, Arc<dyn VoiceEvents>)>,
    }

    struct FakeTrack {
        uuid: Uuid,
        state: StdMutex<FakeTrackState>,
    }

    impl FakeTrack {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                uuid: Uuid::new_v4(),
                state: StdMutex::new(FakeTrackState::default()),
            })
        }

        fn was_stopped(&self) -> bool {
            self.state.lock().unwrap().stopped
        }

        fn is_paused(&self) -> bool {
            self.state.lock().unwrap().paused
        }

        fn volume(&self) -> Option<f32> {
            self.state.lock().unwrap().volume
        }

        fn registered(&self) -> Option<(GuildId, Arc<dyn VoiceEvents>)> {
            self.state.lock().unwrap().registered.clone()
        }
    }

    #[async_trait::async_trait]
    impl VoiceTrack for FakeTrack {
        fn uuid(&self) -> Uuid {
            self.uuid
        }

        async fn set_volume(&self, multiplier: f32) -> Result<(), String> {
            self.state.lock().unwrap().volume = Some(multiplier);
            Ok(())
        }

        async fn stop(&self) -> Result<(), String> {
            let mut state = self.state.lock().unwrap();
            if let Some(message) = state.stop_error.take() {
                return Err(message);
            }
            state.stopped = true;
            Ok(())
        }

        async fn pause(&self) -> Result<(), String> {
            self.state.lock().unwrap().paused = true;
            Ok(())
        }

        async fn resume(&self) -> Result<(), String> {
            self.state.lock().unwrap().paused = false;
            Ok(())
        }

        fn notify_when_finished(&self, guild_id: GuildId, events: Arc<dyn VoiceEvents>) {
            self.state.lock().unwrap().registered = Some((guild_id, events));
        }

        async fn status(&self) -> Option<TrackStatus> {
            let state = self.state.lock().unwrap();
            Some(TrackStatus {
                position: Duration::ZERO,
                paused: state.paused,
            })
        }
    }

    struct PlayedTrack {
        video_id: String,
        handle: Arc<FakeTrack>,
    }

    #[derive(Default)]
    struct FakeCall {
        played: StdMutex<Vec<PlayedTrack>>,
    }

    impl FakeCall {
        fn played_video_ids(&self) -> Vec<String> {
            self.played
                .lock()
                .unwrap()
                .iter()
                .map(|p| p.video_id.clone())
                .collect()
        }

        fn last_track(&self) -> Arc<FakeTrack> {
            self.played
                .lock()
                .unwrap()
                .last()
                .expect("play() was never called")
                .handle
                .clone()
        }

        fn track(&self, track_id: Uuid) -> Option<Arc<FakeTrack>> {
            self.played
                .lock()
                .unwrap()
                .iter()
                .find(|played| played.handle.uuid == track_id)
                .map(|played| played.handle.clone())
        }
    }

    #[async_trait::async_trait]
    impl VoiceCall for FakeCall {
        async fn play(&self, source: AudioSource) -> Result<Arc<dyn VoiceTrack>, String> {
            let track = FakeTrack::new();
            self.played.lock().unwrap().push(PlayedTrack {
                video_id: source.video_id,
                handle: track.clone(),
            });
            Ok(track)
        }
    }

    #[derive(Default)]
    struct FakeBackendState {
        calls: HashMap<GuildId, Arc<FakeCall>>,
        events: HashMap<GuildId, Arc<dyn VoiceEvents>>,
        next_join_failure: Option<String>,
        download_gate: Option<Arc<Notify>>,
        failing_video_ids: HashMap<String, String>,
    }

    #[derive(Default)]
    struct FakeBackend {
        state: StdMutex<FakeBackendState>,
    }

    impl FakeBackend {
        fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }

        fn fail_next_join(&self, message: &str) {
            self.state.lock().unwrap().next_join_failure = Some(message.to_string());
        }

        fn hold_downloads(&self) -> Arc<Notify> {
            let notify = Arc::new(Notify::new());
            self.state.lock().unwrap().download_gate = Some(notify.clone());
            notify
        }

        fn fail_buffered_source_for(&self, video_id: &str, message: &str) {
            self.state
                .lock()
                .unwrap()
                .failing_video_ids
                .insert(video_id.to_string(), message.to_string());
        }

        fn call_for(&self, guild_id: GuildId) -> Option<Arc<FakeCall>> {
            self.state.lock().unwrap().calls.get(&guild_id).cloned()
        }

        async fn finish_track(&self, guild_id: GuildId, track_id: Uuid) {
            let registered = self
                .call_for(guild_id)
                .and_then(|call| call.track(track_id))
                .and_then(|track| track.registered());
            if let Some((guild_id, events)) = registered {
                events.track_finished(guild_id, track_id).await;
            }
        }

        async fn disconnect(&self, guild_id: GuildId) {
            let events = self.state.lock().unwrap().events.get(&guild_id).cloned();
            if let Some(events) = events {
                events.connection_lost(guild_id).await;
            }
        }
    }

    #[async_trait::async_trait]
    impl VoiceBackend for FakeBackend {
        async fn join(
            &self,
            guild_id: GuildId,
            _channel_id: ChannelId,
            events: Arc<dyn VoiceEvents>,
        ) -> Result<(), String> {
            let mut state = self.state.lock().unwrap();
            if let Some(message) = state.next_join_failure.take() {
                return Err(message);
            }
            state.calls.insert(guild_id, Arc::new(FakeCall::default()));
            state.events.insert(guild_id, events);
            Ok(())
        }

        async fn remove(&self, guild_id: GuildId) -> Result<(), String> {
            let mut state = self.state.lock().unwrap();
            state.calls.remove(&guild_id);
            state.events.remove(&guild_id);
            Ok(())
        }

        fn call(&self, guild_id: GuildId) -> Option<Arc<dyn VoiceCall>> {
            self.state
                .lock()
                .unwrap()
                .calls
                .get(&guild_id)
                .cloned()
                .map(|call| call as Arc<dyn VoiceCall>)
        }

        async fn buffered_source(&self, track: &Track) -> Result<AudioSource, PlaybackError> {
            let gate = self.state.lock().unwrap().download_gate.clone();
            if let Some(gate) = gate {
                gate.notified().await;
            }
            let failure = self
                .state
                .lock()
                .unwrap()
                .failing_video_ids
                .get(&track.video_id)
                .cloned();
            if let Some(message) = failure {
                return Err(PlaybackError::Other(message));
            }
            Ok(empty_source(&track.video_id))
        }
    }

    async fn wait_until(mut predicate: impl FnMut() -> bool) {
        for _ in 0..500 {
            if predicate() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        panic!("condition was not met in time");
    }

    fn empty_source(video_id: &str) -> AudioSource {
        AudioSource {
            video_id: video_id.to_string(),
            path: std::path::PathBuf::from("/dev/null"),
        }
    }

    async fn new_registry() -> (PlayerRegistry, Arc<FakeBackend>, GuildId) {
        let db = db::connect("sqlite::memory:").await.expect("in-memory db");
        let backend = FakeBackend::new();
        let registry = PlayerRegistry::new_for_test(backend.clone(), db);
        (registry, backend, GuildId::new(1))
    }

    async fn joined_registry() -> (PlayerRegistry, Arc<FakeBackend>, GuildId) {
        let (registry, backend, guild_id) = new_registry().await;
        registry
            .join(guild_id, ChannelId::new(2))
            .await
            .expect("fake join always succeeds unless primed to fail");
        (registry, backend, guild_id)
    }

    async fn current_track_id(registry: &PlayerRegistry, guild_id: GuildId) -> Uuid {
        registry
            .guilds
            .lock()
            .await
            .get(&guild_id)
            .and_then(|state| state.current_track_id)
            .expect("a track should be current")
    }

    fn upcoming_ids(snapshot: &QueueSnapshot) -> Vec<String> {
        snapshot
            .upcoming
            .iter()
            .map(|t| t.track.video_id.clone())
            .collect()
    }

    #[tokio::test]
    async fn enqueue_starts_playback_immediately_on_an_empty_queue() {
        let (registry, backend, guild_id) = joined_registry().await;

        registry.enqueue(guild_id, queued("a")).await.unwrap();
        registry.settle_playback_start(guild_id).await;

        let call = backend.call_for(guild_id).unwrap();
        assert_eq!(call.played_video_ids(), vec!["a"]);
        let snapshot = registry.queue_snapshot(guild_id).await;
        assert_eq!(snapshot.now_playing.unwrap().track.video_id, "a");
        assert!(snapshot.upcoming.is_empty());
    }

    #[tokio::test]
    async fn enqueue_appends_behind_a_track_already_playing() {
        let (registry, backend, guild_id) = joined_registry().await;

        registry.enqueue(guild_id, queued("a")).await.unwrap();
        registry.enqueue(guild_id, queued("b")).await.unwrap();
        registry.settle_playback_start(guild_id).await;

        let call = backend.call_for(guild_id).unwrap();
        assert_eq!(call.played_video_ids(), vec!["a"]);
        let snapshot = registry.queue_snapshot(guild_id).await;
        assert_eq!(snapshot.now_playing.unwrap().track.video_id, "a");
        assert_eq!(snapshot.upcoming.len(), 1);
        assert_eq!(snapshot.upcoming[0].track.video_id, "b");
    }

    #[tokio::test]
    async fn enqueue_many_starts_the_first_track_on_an_empty_queue() {
        let (registry, backend, guild_id) = joined_registry().await;

        registry
            .enqueue_many(guild_id, vec![queued("a"), queued("b"), queued("c")])
            .await
            .unwrap();
        registry.settle_playback_start(guild_id).await;

        let call = backend.call_for(guild_id).unwrap();
        assert_eq!(call.played_video_ids(), vec!["a"]);
        let snapshot = registry.queue_snapshot(guild_id).await;
        assert_eq!(upcoming_ids(&snapshot), vec!["b", "c"]);
        assert_eq!(snapshot.now_playing.unwrap().track.video_id, "a");
    }

    #[tokio::test]
    async fn enqueue_many_only_appends_when_something_is_already_playing() {
        let (registry, backend, guild_id) = joined_registry().await;
        registry.enqueue(guild_id, queued("a")).await.unwrap();

        registry
            .enqueue_many(guild_id, vec![queued("b"), queued("c")])
            .await
            .unwrap();
        registry.settle_playback_start(guild_id).await;

        let call = backend.call_for(guild_id).unwrap();
        assert_eq!(call.played_video_ids(), vec!["a"]);
        let snapshot = registry.queue_snapshot(guild_id).await;
        assert_eq!(upcoming_ids(&snapshot), vec!["b", "c"]);
        assert_eq!(snapshot.now_playing.unwrap().track.video_id, "a");
    }

    #[tokio::test]
    async fn advance_starts_the_next_queued_track_when_one_ends() {
        let (registry, backend, guild_id) = joined_registry().await;
        registry.enqueue(guild_id, queued("a")).await.unwrap();
        registry.enqueue(guild_id, queued("b")).await.unwrap();
        registry.settle_playback_start(guild_id).await;
        let a_id = current_track_id(&registry, guild_id).await;

        backend.finish_track(guild_id, a_id).await;
        registry.settle_playback_start(guild_id).await;

        let call = backend.call_for(guild_id).unwrap();
        assert_eq!(call.played_video_ids(), vec!["a", "b"]);
        let snapshot = registry.queue_snapshot(guild_id).await;
        assert_eq!(snapshot.now_playing.unwrap().track.video_id, "b");
        assert!(snapshot.upcoming.is_empty());
    }

    #[tokio::test]
    async fn advance_goes_idle_when_the_queue_is_empty() {
        let (registry, backend, guild_id) = joined_registry().await;
        registry.enqueue(guild_id, queued("a")).await.unwrap();
        registry.settle_playback_start(guild_id).await;
        let a_id = current_track_id(&registry, guild_id).await;

        backend.finish_track(guild_id, a_id).await;

        let snapshot = registry.queue_snapshot(guild_id).await;
        assert!(snapshot.now_playing.is_none());
        assert!(snapshot.upcoming.is_empty());
    }

    #[tokio::test]
    async fn advance_into_an_empty_queue_preserves_radio_history_in_the_db() {
        let (registry, backend, guild_id) = joined_registry().await;
        registry.enqueue(guild_id, queued("a")).await.unwrap();
        registry.settle_playback_start(guild_id).await;
        let a_id = current_track_id(&registry, guild_id).await;

        backend.finish_track(guild_id, a_id).await;

        let session = db::load_guild_session(&registry.db, &guild_id.to_string())
            .await
            .unwrap()
            .expect("radio history should survive the queue draining");
        assert_eq!(session.radio_history, vec!["a".to_string()]);
        assert!(session.now_playing.is_none());
    }

    #[tokio::test]
    async fn advance_into_an_empty_queue_keeps_the_finished_track_as_last_played() {
        let (registry, backend, guild_id) = joined_registry().await;
        registry.enqueue(guild_id, queued("a")).await.unwrap();
        registry.settle_playback_start(guild_id).await;
        let a_id = current_track_id(&registry, guild_id).await;

        backend.finish_track(guild_id, a_id).await;

        let snapshot = registry.queue_snapshot(guild_id).await;
        assert!(snapshot.now_playing.is_none());
        assert_eq!(
            snapshot.last_played.map(|q| q.track.video_id),
            Some("a".to_string())
        );
    }

    #[tokio::test]
    async fn stop_clears_the_last_played_track() {
        let (registry, _backend, guild_id) = joined_registry().await;
        registry.enqueue(guild_id, queued("a")).await.unwrap();

        registry.stop(guild_id).await.unwrap();

        let snapshot = registry.queue_snapshot(guild_id).await;
        assert!(snapshot.last_played.is_none());
    }

    #[tokio::test]
    async fn kick_off_if_idle_starts_a_freshly_queued_track_once_the_previous_one_drained() {
        let (registry, backend, guild_id) = joined_registry().await;
        registry.enqueue(guild_id, queued("a")).await.unwrap();
        registry.settle_playback_start(guild_id).await;
        let a_id = current_track_id(&registry, guild_id).await;
        backend.finish_track(guild_id, a_id).await;

        db::queue_push_back(&registry.db, &guild_id.to_string(), &queued("b"))
            .await
            .unwrap();
        registry.kick_off_if_idle(guild_id).await;
        registry.settle_playback_start(guild_id).await;

        let call = backend.call_for(guild_id).unwrap();
        assert_eq!(call.played_video_ids(), vec!["a", "b"]);
        let snapshot = registry.queue_snapshot(guild_id).await;
        assert_eq!(snapshot.now_playing.unwrap().track.video_id, "b");
    }

    #[tokio::test]
    async fn advance_ignores_a_stale_track_finished_event() {
        let (registry, backend, guild_id) = joined_registry().await;
        registry.enqueue(guild_id, queued("a")).await.unwrap();
        registry.enqueue(guild_id, queued("b")).await.unwrap();
        registry.settle_playback_start(guild_id).await;
        let a_id = current_track_id(&registry, guild_id).await;

        backend.finish_track(guild_id, a_id).await;
        backend.finish_track(guild_id, a_id).await;
        registry.settle_playback_start(guild_id).await;

        let call = backend.call_for(guild_id).unwrap();
        assert_eq!(call.played_video_ids(), vec!["a", "b"]);
        let snapshot = registry.queue_snapshot(guild_id).await;
        assert_eq!(snapshot.now_playing.unwrap().track.video_id, "b");
    }

    #[tokio::test]
    async fn stop_clears_queue_state_stops_playback_and_bumps_the_epoch() {
        let (registry, backend, guild_id) = joined_registry().await;
        registry.enqueue(guild_id, queued("a")).await.unwrap();
        registry.enqueue(guild_id, queued("b")).await.unwrap();
        registry.settle_playback_start(guild_id).await;
        let track = backend.call_for(guild_id).unwrap().last_track();
        let epoch_before = registry.guilds.lock().await.get(&guild_id).unwrap().epoch;

        registry.stop(guild_id).await.unwrap();

        assert!(track.was_stopped());
        assert_eq!(
            db::queue_len(&registry.db, &guild_id.to_string())
                .await
                .unwrap(),
            0
        );
        let guilds = registry.guilds.lock().await;
        let state = guilds.get(&guild_id).unwrap();
        assert!(state.now_playing.is_none());
        assert!(state.current_track_id.is_none());
        assert!(state.current_handle.is_none());
        assert_eq!(state.epoch, epoch_before.wrapping_add(1));
    }

    #[tokio::test]
    async fn stop_with_nothing_playing_reports_nothing_playing() {
        let (registry, _backend, guild_id) = joined_registry().await;

        let err = registry.stop(guild_id).await.unwrap_err();

        assert!(matches!(err, PlayerError::NothingPlaying));
    }

    #[tokio::test]
    async fn clear_queue_drops_upcoming_but_leaves_now_playing_alone() {
        let (registry, backend, guild_id) = joined_registry().await;
        registry.enqueue(guild_id, queued("a")).await.unwrap();
        registry
            .enqueue_many(guild_id, vec![queued("b"), queued("c")])
            .await
            .unwrap();
        registry.settle_playback_start(guild_id).await;
        let a_track = backend.call_for(guild_id).unwrap().last_track();

        registry.clear_queue(guild_id).await.unwrap();

        assert!(!a_track.was_stopped());
        let snapshot = registry.queue_snapshot(guild_id).await;
        assert_eq!(snapshot.now_playing.unwrap().track.video_id, "a");
        assert!(snapshot.upcoming.is_empty());
    }

    #[tokio::test]
    async fn clear_queue_with_nothing_queued_reports_queue_empty() {
        let (registry, _backend, guild_id) = joined_registry().await;
        registry.enqueue(guild_id, queued("a")).await.unwrap();

        let err = registry.clear_queue(guild_id).await.unwrap_err();

        assert!(matches!(err, PlayerError::QueueEmpty));
    }

    #[tokio::test]
    async fn clear_queue_with_no_guild_state_reports_queue_empty() {
        let (registry, _backend, guild_id) = new_registry().await;

        let err = registry.clear_queue(guild_id).await.unwrap_err();

        assert!(matches!(err, PlayerError::QueueEmpty));
    }

    #[tokio::test]
    async fn leave_if_idle_leaves_voice_when_the_guild_is_idle() {
        let (registry, backend, guild_id) = joined_registry().await;

        registry.leave_if_idle(guild_id).await;

        assert!(backend.call_for(guild_id).is_none());
    }

    #[tokio::test]
    async fn leave_if_idle_does_nothing_if_a_track_is_playing() {
        let (registry, backend, guild_id) = joined_registry().await;
        registry.enqueue(guild_id, queued("a")).await.unwrap();
        registry.settle_playback_start(guild_id).await;

        registry.leave_if_idle(guild_id).await;

        assert!(backend.call_for(guild_id).is_some());
        let snapshot = registry.queue_snapshot(guild_id).await;
        assert_eq!(snapshot.now_playing.unwrap().track.video_id, "a");
    }

    #[tokio::test]
    async fn leave_if_idle_does_nothing_if_only_the_queue_is_non_empty() {
        let (registry, backend, guild_id) = joined_registry().await;
        db::queue_push_back(&registry.db, &guild_id.to_string(), &queued("a"))
            .await
            .unwrap();

        registry.leave_if_idle(guild_id).await;

        assert!(backend.call_for(guild_id).is_some());
    }

    #[tokio::test]
    async fn join_failure_is_reported_and_leaves_no_call_registered() {
        let (registry, backend, guild_id) = new_registry().await;
        backend.fail_next_join("no permission to join");

        let err = registry
            .join(guild_id, ChannelId::new(2))
            .await
            .unwrap_err();

        assert!(matches!(err, PlayerError::Join(message) if message == "no permission to join"));
        assert!(backend.call_for(guild_id).is_none());
    }

    #[tokio::test]
    async fn connection_lost_tears_down_playback_state() {
        let (registry, backend, guild_id) = joined_registry().await;
        registry.enqueue(guild_id, queued("a")).await.unwrap();

        backend.disconnect(guild_id).await;

        assert!(backend.call_for(guild_id).is_none());
        let guilds = registry.guilds.lock().await;
        assert!(guilds.get(&guild_id).is_none());
    }

    #[tokio::test]
    async fn pause_and_resume_toggle_the_current_track() {
        let (registry, backend, guild_id) = joined_registry().await;
        registry.enqueue(guild_id, queued("a")).await.unwrap();
        registry.settle_playback_start(guild_id).await;
        let track = backend.call_for(guild_id).unwrap().last_track();

        registry.pause(guild_id).await.unwrap();
        assert!(track.is_paused());

        registry.resume(guild_id).await.unwrap();
        assert!(!track.is_paused());
    }

    #[tokio::test]
    async fn leave_if_idle_disconnects_a_track_that_is_still_paused() {
        let (registry, backend, guild_id) = joined_registry().await;
        registry.enqueue(guild_id, queued("a")).await.unwrap();
        registry.settle_playback_start(guild_id).await;
        registry.pause(guild_id).await.unwrap();

        registry.leave_if_idle(guild_id).await;

        assert!(backend.call_for(guild_id).is_none());
        let session = db::load_guild_session(&registry.db, &guild_id.to_string())
            .await
            .unwrap()
            .expect("the paused track should be preserved for a later resume");
        assert_eq!(
            session.now_playing.map(|(track, _)| track.video_id),
            Some("a".to_string())
        );
    }

    #[tokio::test]
    async fn leave_if_idle_does_nothing_if_the_track_was_resumed() {
        let (registry, backend, guild_id) = joined_registry().await;
        registry.enqueue(guild_id, queued("a")).await.unwrap();
        registry.settle_playback_start(guild_id).await;
        registry.pause(guild_id).await.unwrap();
        registry.resume(guild_id).await.unwrap();

        registry.leave_if_idle(guild_id).await;

        assert!(backend.call_for(guild_id).is_some());
    }

    #[tokio::test]
    async fn advance_into_an_empty_queue_then_idle_disconnect_still_shows_last_played() {
        let (registry, backend, guild_id) = joined_registry().await;
        registry.enqueue(guild_id, queued("a")).await.unwrap();
        registry.settle_playback_start(guild_id).await;
        let a_id = current_track_id(&registry, guild_id).await;
        backend.finish_track(guild_id, a_id).await;

        registry.leave_if_idle(guild_id).await;

        assert!(backend.call_for(guild_id).is_none());
        let snapshot = registry.queue_snapshot(guild_id).await;
        assert_eq!(
            snapshot.last_played.map(|q| q.track.video_id),
            Some("a".to_string())
        );
    }

    #[tokio::test]
    async fn leave_preserves_last_played_for_the_next_queue_snapshot() {
        let (registry, backend, guild_id) = joined_registry().await;
        registry.enqueue(guild_id, queued("a")).await.unwrap();
        registry.settle_playback_start(guild_id).await;
        let a_id = current_track_id(&registry, guild_id).await;
        backend.finish_track(guild_id, a_id).await;

        registry.leave(guild_id).await.unwrap();

        assert!(backend.call_for(guild_id).is_none());
        let snapshot = registry.queue_snapshot(guild_id).await;
        assert_eq!(
            snapshot.last_played.map(|q| q.track.video_id),
            Some("a".to_string())
        );
    }

    #[tokio::test]
    async fn set_volume_applies_to_the_current_track() {
        let (registry, backend, guild_id) = joined_registry().await;
        registry.enqueue(guild_id, queued("a")).await.unwrap();
        registry.settle_playback_start(guild_id).await;
        let call = backend.call_for(guild_id).unwrap();
        assert_eq!(call.played_video_ids().len(), 1);
        let track = call.last_track();

        registry.set_volume(guild_id, 50).await.unwrap();

        assert_eq!(track.volume(), Some(0.5));
    }

    #[tokio::test]
    async fn enqueue_persists_a_resumable_session() {
        let (registry, _backend, guild_id) = joined_registry().await;

        registry.enqueue(guild_id, queued("a")).await.unwrap();
        registry.enqueue(guild_id, queued("b")).await.unwrap();

        let session = db::load_guild_session(&registry.db, &guild_id.to_string())
            .await
            .unwrap()
            .expect("a session should have been persisted");
        assert_eq!(
            session.now_playing.map(|(track, _)| track.video_id),
            Some("a".to_string())
        );
        assert_eq!(
            db::queue_all(&registry.db, &guild_id.to_string())
                .await
                .unwrap()
                .iter()
                .map(|q| q.track.video_id.as_str())
                .collect::<Vec<_>>(),
            vec!["b"]
        );
    }

    #[tokio::test]
    async fn stop_clears_the_persisted_session() {
        let (registry, _backend, guild_id) = joined_registry().await;
        registry.enqueue(guild_id, queued("a")).await.unwrap();

        registry.stop(guild_id).await.unwrap();

        assert_eq!(
            db::load_guild_session(&registry.db, &guild_id.to_string())
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn stop_prevents_a_later_idle_disconnect_from_resurrecting_radio_history() {
        let (registry, _backend, guild_id) = joined_registry().await;
        registry.enqueue(guild_id, queued("a")).await.unwrap();

        registry.stop(guild_id).await.unwrap();
        registry.leave_if_idle(guild_id).await;

        let session = db::load_guild_session(&registry.db, &guild_id.to_string())
            .await
            .unwrap();
        let history = session.map(|s| s.radio_history).unwrap_or_default();
        assert!(history.is_empty());
    }

    #[tokio::test]
    async fn leave_preserves_the_persisted_session_for_later_resume() {
        let (registry, _backend, guild_id) = joined_registry().await;
        registry.enqueue(guild_id, queued("a")).await.unwrap();

        registry.leave(guild_id).await.unwrap();

        let session = db::load_guild_session(&registry.db, &guild_id.to_string())
            .await
            .unwrap()
            .expect("an involuntary disconnect should not wipe the persisted session");
        assert_eq!(
            session.now_playing.map(|(track, _)| track.video_id),
            Some("a".to_string())
        );
    }

    #[tokio::test]
    async fn join_restores_a_persisted_session_and_starts_its_now_playing() {
        let (registry, backend, guild_id) = new_registry().await;
        db::save_guild_session_meta(
            &registry.db,
            &guild_id.to_string(),
            true,
            Some("1"),
            &["a".to_string()],
            Some((&queued("a").track, "1")),
            None,
        )
        .await
        .unwrap();
        db::queue_push_back(&registry.db, &guild_id.to_string(), &queued("b"))
            .await
            .unwrap();

        registry.join(guild_id, ChannelId::new(2)).await.unwrap();
        registry.settle_playback_start(guild_id).await;

        let call = backend.call_for(guild_id).unwrap();
        assert_eq!(call.played_video_ids(), vec!["a"]);
        let snapshot = registry.queue_snapshot(guild_id).await;
        assert_eq!(upcoming_ids(&snapshot), vec!["b"]);
        assert_eq!(snapshot.now_playing.unwrap().track.video_id, "a");
        assert!(registry.is_radio_enabled(guild_id).await);
    }

    #[tokio::test]
    async fn enqueue_after_restore_appends_behind_the_resumed_queue() {
        let (registry, backend, guild_id) = new_registry().await;
        db::save_guild_session_meta(
            &registry.db,
            &guild_id.to_string(),
            false,
            None,
            &[],
            Some((&queued("a").track, "1")),
            None,
        )
        .await
        .unwrap();

        registry.join(guild_id, ChannelId::new(2)).await.unwrap();
        registry.enqueue(guild_id, queued("new")).await.unwrap();
        registry.settle_playback_start(guild_id).await;

        let call = backend.call_for(guild_id).unwrap();
        assert_eq!(call.played_video_ids(), vec!["a"]);
        let snapshot = registry.queue_snapshot(guild_id).await;
        assert_eq!(upcoming_ids(&snapshot), vec!["new"]);
        assert_eq!(snapshot.now_playing.unwrap().track.video_id, "a");
    }

    #[tokio::test]
    async fn join_with_no_persisted_session_starts_with_an_empty_queue() {
        let (registry, backend, guild_id) = new_registry().await;

        registry.join(guild_id, ChannelId::new(2)).await.unwrap();

        assert!(
            backend
                .call_for(guild_id)
                .unwrap()
                .played_video_ids()
                .is_empty()
        );
        let snapshot = registry.queue_snapshot(guild_id).await;
        assert!(snapshot.now_playing.is_none());
    }

    #[tokio::test]
    async fn enqueue_shows_a_loading_track_before_the_background_start_completes() {
        let (registry, backend, guild_id) = joined_registry().await;
        let gate = backend.hold_downloads();

        registry.enqueue(guild_id, queued("a")).await.unwrap();
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        let snapshot = registry.queue_snapshot(guild_id).await;
        assert_eq!(
            snapshot.loading.map(|q| q.track.video_id),
            Some("a".to_string())
        );
        assert!(snapshot.now_playing.is_none());
        assert!(
            backend
                .call_for(guild_id)
                .unwrap()
                .played_video_ids()
                .is_empty()
        );

        gate.notify_one();
        registry.settle_playback_start(guild_id).await;

        let snapshot = registry.queue_snapshot(guild_id).await;
        assert_eq!(
            snapshot.now_playing.map(|q| q.track.video_id),
            Some("a".to_string())
        );
        assert!(snapshot.loading.is_none());
        assert_eq!(
            backend.call_for(guild_id).unwrap().played_video_ids(),
            vec!["a"]
        );
    }

    #[tokio::test]
    async fn enqueue_retries_the_next_candidate_when_the_first_track_fails_to_start() {
        let (registry, backend, guild_id) = joined_registry().await;
        backend.fail_buffered_source_for("a", "boom");

        registry.enqueue(guild_id, queued("a")).await.unwrap();
        registry.enqueue(guild_id, queued("b")).await.unwrap();
        registry.settle_playback_start(guild_id).await;

        let call = backend.call_for(guild_id).unwrap();
        assert_eq!(call.played_video_ids(), vec!["b"]);
        let snapshot = registry.queue_snapshot(guild_id).await;
        assert_eq!(snapshot.now_playing.unwrap().track.video_id, "b");
    }

    #[tokio::test]
    async fn a_concurrent_stop_discards_a_still_loading_track_and_stops_the_handle_it_creates() {
        let (registry, backend, guild_id) = joined_registry().await;
        let gate = backend.hold_downloads();

        registry.enqueue(guild_id, queued("a")).await.unwrap();
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert!(registry.queue_snapshot(guild_id).await.loading.is_some());

        registry.stop(guild_id).await.unwrap();
        gate.notify_one();

        let call = backend.call_for(guild_id).unwrap();
        wait_until(|| !call.played_video_ids().is_empty() && call.last_track().was_stopped()).await;

        assert!(call.last_track().was_stopped());
        assert!(
            registry
                .queue_snapshot(guild_id)
                .await
                .now_playing
                .is_none()
        );
    }
}
