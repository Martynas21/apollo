use std::sync::Arc;

use serenity::all::GuildId;
use uuid::Uuid;

use crate::db;
use crate::model::QueuedTrack;
use crate::voice::backend::{AudioSource, VoiceCall, VoiceTrack};
use crate::voice::error::PlayerError;
use crate::voice::registry::{PlayerRegistry, Prefetch, discard_prefetch};
use crate::voice::resolve::{self, PlaybackError};
use crate::voice::state::{AdvanceFill, StartOutcome};

const MAX_VOLUME: u8 = 100;

const RADIO_HISTORY_CAP: usize = 5;

fn volume_multiplier(volume: u8) -> f32 {
    f32::from(volume.min(MAX_VOLUME)) / 100.0
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

impl PlayerRegistry {
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

    async fn classify_playback_failure(&self, video_id: &str, err: PlaybackError) -> PlayerError {
        if !matches!(err, PlaybackError::Other(_)) {
            return PlayerError::Playback(err.to_string());
        }

        let preflight = resolve::preflight_check(video_id, self.cookies_file.as_deref())
            .await
            .err();
        PlayerError::Playback(better_playback_error(err, preflight).to_string())
    }

    pub(super) fn spawn_start_sequence(
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

        if let Err(err) =
            db::record_track_play(&self.db, &guild_id.to_string(), &queued.track).await
        {
            tracing::warn!(%guild_id, %err, "failed to record track play count");
        }

        if needs_radio_refill {
            self.maybe_spawn_radio_refill(guild_id);
        }

        StartOutcome::Committed
    }

    pub(super) async fn advance(&self, guild_id: GuildId, track_id: Uuid) {
        let (next, epoch, prefetch, refill_notify) = {
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
            let refill_notify =
                (next.is_none() && state.radio_enabled && state.radio_refill_running)
                    .then(|| state.radio_refill_notify.clone())
                    .flatten();
            state.now_playing = next.clone();
            (next, state.epoch, state.prefetch.take(), refill_notify)
        };

        let fill = match (next, refill_notify) {
            (Some(next), _) => AdvanceFill::Track(next, epoch),
            (None, Some(notify)) => self.await_radio_refill_then_repop(guild_id, notify).await,
            (None, None) => AdvanceFill::Idle,
        };

        match (fill, self.voice.call(guild_id)) {
            (AdvanceFill::Track(next, epoch), Some(call)) => {
                self.spawn_start_sequence(guild_id, call, next, epoch, prefetch)
            }
            (AdvanceFill::AlreadyStarted, _) => {
                // The in-flight radio refill's own `kick_off_if_idle` beat
                // us to it and already started a track; this prefetch was
                // for a candidate that's no longer relevant.
                if let Some(prefetch) = prefetch {
                    discard_prefetch(prefetch);
                }
            }
            _ => {
                if let Some(prefetch) = prefetch {
                    discard_prefetch(prefetch);
                }
                self.schedule_idle_disconnect(guild_id);
            }
        }

        self.persist_session(guild_id).await;
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
        handle
            .resume()
            .await
            .map_err(|e| PlayerError::Playback(e.to_string()))
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

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::sync::Notify;

    use super::*;
    use crate::voice::testing::{current_track_id, joined_registry, queued, wait_until};

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
    async fn advance_waits_briefly_for_an_in_flight_radio_refill_before_going_idle() {
        let (registry, backend, guild_id) = joined_registry().await;
        registry.enqueue(guild_id, queued("a")).await.unwrap();
        registry.settle_playback_start(guild_id).await;
        let a_id = current_track_id(&registry, guild_id).await;

        let notify = Arc::new(Notify::new());
        {
            let mut guilds = registry.guilds.lock().await;
            let state = guilds.get_mut(&guild_id).unwrap();
            state.radio_enabled = true;
            state.radio_refill_running = true;
            state.radio_refill_notify = Some(notify.clone());
        }

        // Simulates a radio refill that's still in flight when the track
        // ends: it populates the DB queue and notifies shortly after, well
        // within `advance()`'s `RADIO_ADVANCE_WAIT` bound.
        let refill_db = registry.db.clone();
        let refill_guild_id = guild_id.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            db::queue_push_back(&refill_db, &refill_guild_id, &queued("b"))
                .await
                .unwrap();
            notify.notify_waiters();
        });

        backend.finish_track(guild_id, a_id).await;
        registry.settle_playback_start(guild_id).await;

        let call = backend.call_for(guild_id).unwrap();
        assert_eq!(call.played_video_ids(), vec!["a", "b"]);
        let snapshot = registry.queue_snapshot(guild_id).await;
        assert_eq!(snapshot.now_playing.unwrap().track.video_id, "b");
    }
}
