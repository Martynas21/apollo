use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use serenity::all::{ChannelId, GuildId, UserId};
use tokio::sync::Notify;
use uuid::Uuid;

use crate::db;
use crate::model::{QueuedTrack, Track};
use crate::voice::backend::{
    AudioSource, TrackStatus, VoiceBackend, VoiceCall, VoiceEvents, VoiceTrack,
};
use crate::voice::registry::PlayerRegistry;
use crate::voice::resolve::PlaybackError;
use crate::voice::state::QueueSnapshot;

pub(super) fn queued(video_id: &str) -> QueuedTrack {
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

#[derive(Default)]
struct FakeTrackState {
    stopped: bool,
    paused: bool,
    position: Duration,
    volume: Option<f32>,
    stop_error: Option<String>,
    registered: Option<(GuildId, Arc<dyn VoiceEvents>)>,
}

pub(super) struct FakeTrack {
    uuid: Uuid,
    state: StdMutex<FakeTrackState>,
    shared: Arc<FakeShared>,
}

/// Behaviour switches shared by the backend, its calls and their tracks.
#[derive(Default)]
struct FakeShared {
    /// Whether a stop is followed by the track-finished report a real
    /// worker sends once the track has ended.
    confirm_stops: AtomicBool,
    /// Holds the next `set_volume` call, which is where a start sits
    /// between the worker accepting the track and the commit landing.
    volume_gate: StdMutex<Option<Arc<Notify>>>,
}

impl FakeTrack {
    fn new(shared: Arc<FakeShared>) -> Arc<Self> {
        Arc::new(Self {
            uuid: Uuid::new_v4(),
            state: StdMutex::new(FakeTrackState::default()),
            shared,
        })
    }

    pub(super) fn fail_next_stop(&self, message: &str) {
        self.state.lock().unwrap().stop_error = Some(message.to_string());
    }

    pub(super) fn was_stopped(&self) -> bool {
        self.state.lock().unwrap().stopped
    }

    pub(super) fn is_paused(&self) -> bool {
        self.state.lock().unwrap().paused
    }

    pub(super) fn volume(&self) -> Option<f32> {
        self.state.lock().unwrap().volume
    }

    pub(super) fn set_position(&self, position: Duration) {
        self.state.lock().unwrap().position = position;
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
        let gate = self.shared.volume_gate.lock().unwrap().take();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        self.state.lock().unwrap().volume = Some(multiplier);
        Ok(())
    }

    async fn stop(&self) -> Result<(), String> {
        let registered = {
            let mut state = self.state.lock().unwrap();
            if let Some(message) = state.stop_error.take() {
                return Err(message);
            }
            state.stopped = true;
            state.registered.clone()
        };
        if self.shared.confirm_stops.load(Ordering::Relaxed)
            && let Some((guild_id, events)) = registered
        {
            events.track_finished(guild_id, self.uuid).await;
        }
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
            position: state.position,
            paused: state.paused,
        })
    }
}

struct PlayedTrack {
    video_id: String,
    handle: Arc<FakeTrack>,
}

#[derive(Default)]
pub(super) struct FakeCall {
    played: StdMutex<Vec<PlayedTrack>>,
    shared: Arc<FakeShared>,
}

impl FakeCall {
    fn new(shared: Arc<FakeShared>) -> Arc<Self> {
        Arc::new(Self {
            played: StdMutex::new(Vec::new()),
            shared,
        })
    }

    pub(super) fn played_video_ids(&self) -> Vec<String> {
        self.played
            .lock()
            .unwrap()
            .iter()
            .map(|p| p.video_id.clone())
            .collect()
    }

    pub(super) fn last_track(&self) -> Arc<FakeTrack> {
        self.played
            .lock()
            .unwrap()
            .last()
            .expect("play() was never called")
            .handle
            .clone()
    }

    pub(super) fn track(&self, track_id: Uuid) -> Option<Arc<FakeTrack>> {
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
        let track = FakeTrack::new(self.shared.clone());
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
    /// Calls replaced by a rejoin or dropped by a leave, so a test can
    /// count plays across a rebuilt session.
    retired_calls: Vec<(GuildId, Arc<FakeCall>)>,
    shared: Arc<FakeShared>,
    events: HashMap<GuildId, Arc<dyn VoiceEvents>>,
    current_channel: HashMap<GuildId, ChannelId>,
    join_call_count: HashMap<GuildId, u32>,
    simulate_eviction_disconnect: bool,
    next_join_failure: Option<String>,
    download_gate: Option<Arc<Notify>>,
    /// Video ids whose resolve fails: `Some(message)` as an unclassified
    /// error, `None` as an already-classified one, which skips the preflight
    /// (a real yt-dlp call) on the way to the next track.
    failing_video_ids: HashMap<String, Option<String>>,
    buffered_source_calls: Vec<String>,
}

#[derive(Default)]
pub(super) struct FakeBackend {
    state: StdMutex<FakeBackendState>,
}

impl FakeBackend {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub(super) fn fail_next_join(&self, message: &str) {
        self.state.lock().unwrap().next_join_failure = Some(message.to_string());
    }

    pub(super) fn hold_downloads(&self) -> Arc<Notify> {
        let notify = Arc::new(Notify::new());
        self.state.lock().unwrap().download_gate = Some(notify.clone());
        notify
    }

    pub(super) fn fail_buffered_source_for(&self, video_id: &str, message: &str) {
        self.state
            .lock()
            .unwrap()
            .failing_video_ids
            .insert(video_id.to_string(), Some(message.to_string()));
    }

    pub(super) fn make_unplayable(&self, video_id: &str) {
        self.state
            .lock()
            .unwrap()
            .failing_video_ids
            .insert(video_id.to_string(), None);
    }

    pub(super) fn buffered_source_calls(&self) -> Vec<String> {
        self.state.lock().unwrap().buffered_source_calls.clone()
    }

    pub(super) fn clear_buffered_source_calls(&self) {
        self.state.lock().unwrap().buffered_source_calls.clear();
    }

    pub(super) fn call_for(&self, guild_id: GuildId) -> Option<Arc<FakeCall>> {
        self.state.lock().unwrap().calls.get(&guild_id).cloned()
    }

    /// Every video id played for the guild, across retired calls and the
    /// current one, in start order.
    pub(super) fn all_played_video_ids(&self, guild_id: GuildId) -> Vec<String> {
        let state = self.state.lock().unwrap();
        state
            .retired_calls
            .iter()
            .filter(|(id, _)| *id == guild_id)
            .map(|(_, call)| call)
            .chain(state.calls.get(&guild_id))
            .flat_map(|call| call.played_video_ids())
            .collect()
    }

    pub(super) fn set_stop_confirmation(&self, confirm: bool) {
        self.state
            .lock()
            .unwrap()
            .shared
            .confirm_stops
            .store(confirm, Ordering::Relaxed);
    }

    /// Holds the next track start just before its commit; the returned
    /// notify lets it through.
    pub(super) fn hold_volume(&self) -> Arc<Notify> {
        let notify = Arc::new(Notify::new());
        *self
            .state
            .lock()
            .unwrap()
            .shared
            .volume_gate
            .lock()
            .unwrap() = Some(notify.clone());
        notify
    }

    pub(super) fn join_call_count(&self, guild_id: GuildId) -> u32 {
        self.state
            .lock()
            .unwrap()
            .join_call_count
            .get(&guild_id)
            .copied()
            .unwrap_or(0)
    }

    /// Reproduces the pre-fix audio-worker behavior: rejoining a guild
    /// evicts whatever session was there and fires a spurious
    /// `connection_lost` for it, as `Sessions::join` used to before the
    /// `retired`-flag fix. Used to write a regression test proving the
    /// same-channel short-circuit in `PlayerRegistry::join` prevents
    /// `FakeBackend::join` from ever being reached in that case.
    pub(super) fn enable_eviction_simulation(&self) {
        self.state.lock().unwrap().simulate_eviction_disconnect = true;
    }

    fn registered_events(
        &self,
        guild_id: GuildId,
        track_id: Uuid,
    ) -> Option<(GuildId, Arc<dyn VoiceEvents>)> {
        self.call_for(guild_id)
            .and_then(|call| call.track(track_id))
            .and_then(|track| track.registered())
    }

    pub(super) async fn finish_track(&self, guild_id: GuildId, track_id: Uuid) {
        if let Some((guild_id, events)) = self.registered_events(guild_id, track_id) {
            events.track_finished(guild_id, track_id).await;
        }
    }

    /// Reports the track as having errored after playing for `position`,
    /// the way the audio worker does for a refused or broken stream.
    pub(super) async fn fail_track(&self, guild_id: GuildId, track_id: Uuid, position: Duration) {
        if let Some((guild_id, events)) = self.registered_events(guild_id, track_id) {
            events
                .track_errored(guild_id, track_id, position, "stream refused".to_string())
                .await;
        }
    }

    pub(super) async fn disconnect(&self, guild_id: GuildId) {
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
        channel_id: ChannelId,
        events: Arc<dyn VoiceEvents>,
    ) -> Result<(), String> {
        let (old_events, simulate) = {
            let mut state = self.state.lock().unwrap();
            if let Some(message) = state.next_join_failure.take() {
                return Err(message);
            }
            *state.join_call_count.entry(guild_id).or_default() += 1;
            let old_events = state.events.get(&guild_id).cloned();
            let simulate = state.simulate_eviction_disconnect;
            let call = FakeCall::new(state.shared.clone());
            if let Some(old_call) = state.calls.insert(guild_id, call) {
                state.retired_calls.push((guild_id, old_call));
            }
            state.events.insert(guild_id, events);
            state.current_channel.insert(guild_id, channel_id);
            (old_events, simulate)
        };
        if simulate && let Some(old_events) = old_events {
            old_events.connection_lost(guild_id).await;
        }
        Ok(())
    }

    async fn remove(&self, guild_id: GuildId) -> Result<(), String> {
        let mut state = self.state.lock().unwrap();
        if let Some(old_call) = state.calls.remove(&guild_id) {
            state.retired_calls.push((guild_id, old_call));
        }
        state.events.remove(&guild_id);
        state.current_channel.remove(&guild_id);
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

    async fn current_channel(&self, guild_id: GuildId) -> Option<ChannelId> {
        self.state
            .lock()
            .unwrap()
            .current_channel
            .get(&guild_id)
            .copied()
    }

    async fn buffered_source(&self, track: &Track) -> Result<AudioSource, PlaybackError> {
        let gate = {
            let mut state = self.state.lock().unwrap();
            state.buffered_source_calls.push(track.video_id.clone());
            state.download_gate.clone()
        };
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
        match failure {
            Some(Some(message)) => Err(PlaybackError::Other(message)),
            Some(None) => Err(PlaybackError::Unavailable),
            None => Ok(empty_source(&track.video_id)),
        }
    }
}

pub(super) async fn wait_until(mut predicate: impl FnMut() -> bool) {
    for _ in 0..500 {
        if predicate() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    panic!("condition was not met in time");
}

pub(super) fn empty_source(video_id: &str) -> AudioSource {
    AudioSource {
        video_id: video_id.to_string(),
        url: "https://example.invalid/apollo-test.audio".to_string(),
        headers: Vec::new(),
        resolved_at: Instant::now(),
    }
}

pub(super) async fn new_registry() -> (PlayerRegistry, Arc<FakeBackend>, GuildId) {
    let db = db::connect("sqlite::memory:").await.expect("in-memory db");
    let backend = FakeBackend::new();
    let registry = PlayerRegistry::new_for_test(backend.clone(), db);
    (registry, backend, GuildId::new(1))
}

pub(super) async fn joined_registry() -> (PlayerRegistry, Arc<FakeBackend>, GuildId) {
    let (registry, backend, guild_id) = new_registry().await;
    registry
        .join(guild_id, ChannelId::new(2))
        .await
        .expect("fake join always succeeds unless primed to fail");
    (registry, backend, guild_id)
}

pub(super) async fn current_track_id(registry: &PlayerRegistry, guild_id: GuildId) -> Uuid {
    registry
        .guilds
        .lock()
        .await
        .get(&guild_id)
        .and_then(|state| state.current_track_id)
        .expect("a track should be current")
}

pub(super) fn upcoming_ids(snapshot: &QueueSnapshot) -> Vec<String> {
    snapshot
        .upcoming
        .iter()
        .map(|t| t.track.video_id.clone())
        .collect()
}

/// Seeds what a guild left behind: the persisted queue it is restored from,
/// its current track first, then the tracks behind it.
pub(super) async fn persist_queue(registry: &PlayerRegistry, guild_id: GuildId, ids: &[&str]) {
    for id in ids {
        db::queue_push_back(&registry.db, &guild_id.to_string(), &queued(id))
            .await
            .expect("in-memory queue push");
    }
}

/// Seeds the radio settings a guild left behind, alongside `persist_queue`.
pub(super) async fn persist_radio(
    registry: &PlayerRegistry,
    guild_id: GuildId,
    requested_by: Option<&str>,
    history: &[&str],
) {
    let history: Vec<String> = history.iter().map(|id| (*id).to_string()).collect();
    db::save_guild_session_meta(
        &registry.db,
        &guild_id.to_string(),
        requested_by.is_some(),
        requested_by,
        &history,
        None,
    )
    .await
    .expect("in-memory session save");
}
