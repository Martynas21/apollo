use std::sync::Arc;
use std::time::Duration;

use serenity::all::GuildId;
use uuid::Uuid;

use crate::model::QueuedTrack;
use crate::voice::backend::VoiceCall;
use crate::voice::registry::{PlayerRegistry, discard_prefetch};

/// A track that errors before playing this long never really started: its
/// stream URL had most likely expired or the CDN refused it, so it gets one
/// more attempt with a freshly resolved URL before the queue moves on.
const EARLY_FAILURE_WINDOW: Duration = Duration::from_secs(5);

/// Tracks in a row that may fail early even after their retry before the
/// guild stops walking its queue, so a CDN that refuses every stream cannot
/// burn through the whole queue two attempts at a time.
const MAX_CONSECUTIVE_EARLY_FAILURES: u32 = 3;

enum EarlyRetry {
    Retry(QueuedTrack, u64, Arc<dyn VoiceCall>),
    Spent,
    Stale,
}

impl PlayerRegistry {
    pub(super) async fn handle_track_error(
        &self,
        guild_id: GuildId,
        track_id: Uuid,
        position: Duration,
        error: &str,
    ) {
        if position >= EARLY_FAILURE_WINDOW {
            tracing::warn!(
                %guild_id,
                %track_id,
                position_secs = position.as_secs(),
                %error,
                "track failed; moving on"
            );
            self.clear_early_failures(guild_id).await;
            self.advance(guild_id, track_id).await;
            return;
        }
        match self.claim_early_retry(guild_id, track_id).await {
            EarlyRetry::Retry(queued, epoch, call) => {
                tracing::warn!(
                    %guild_id,
                    video_id = %queued.track.video_id,
                    %error,
                    "track failed right at the start; retrying once with a fresh stream URL"
                );
                self.spawn_start_sequence(guild_id, call, queued, epoch, None, true);
            }
            EarlyRetry::Spent if self.note_early_failure(guild_id).await => {
                tracing::error!(
                    %guild_id,
                    %error,
                    "several tracks in a row failed at the start; giving up on the queue"
                );
                self.abandon_current(guild_id).await;
            }
            EarlyRetry::Spent => {
                tracing::warn!(%guild_id, %track_id, %error, "track failed again; moving on");
                self.advance(guild_id, track_id).await;
            }
            EarlyRetry::Stale => {}
        }
    }

    /// Drops the failed track's handle and marks the retry as spent, keeping
    /// it as `now_playing` so the guild reads as buffering while the fresh
    /// start runs.
    async fn claim_early_retry(&self, guild_id: GuildId, track_id: Uuid) -> EarlyRetry {
        let mut guilds = self.guilds.lock().await;
        let Some(state) = guilds.get_mut(&guild_id) else {
            return EarlyRetry::Stale;
        };
        if state.current_track_id != Some(track_id) {
            return EarlyRetry::Stale;
        }
        let (Some(call), Some(queued), false) = (
            self.voice.call(guild_id),
            state.now_playing.clone(),
            state.retry_used,
        ) else {
            return EarlyRetry::Spent;
        };
        state.retry_used = true;
        state.current_handle = None;
        state.current_track_id = None;
        EarlyRetry::Retry(queued, state.epoch, call)
    }

    /// Counts one more track that failed early even after its retry and
    /// reports whether the guild has hit the limit.
    async fn note_early_failure(&self, guild_id: GuildId) -> bool {
        let mut guilds = self.guilds.lock().await;
        let Some(state) = guilds.get_mut(&guild_id) else {
            return false;
        };
        state.consecutive_early_failures += 1;
        state.consecutive_early_failures >= MAX_CONSECUTIVE_EARLY_FAILURES
    }

    pub(super) async fn clear_early_failures(&self, guild_id: GuildId) {
        let mut guilds = self.guilds.lock().await;
        if let Some(state) = guilds.get_mut(&guild_id) {
            state.consecutive_early_failures = 0;
        }
    }

    /// Drops the current track without starting another, leaving the rest
    /// of the queue in place for the next join or click-to-play.
    async fn abandon_current(&self, guild_id: GuildId) {
        {
            let mut guilds = self.guilds.lock().await;
            if let Some(state) = guilds.get_mut(&guild_id) {
                state.current_handle = None;
                state.current_track_id = None;
                state.now_playing = None;
                state.paused = false;
                state.consecutive_early_failures = 0;
                if let Some(prefetch) = state.prefetch.take() {
                    discard_prefetch(prefetch);
                }
                self.finish_queue_head(guild_id).await;
            }
        }
        self.schedule_idle_disconnect(guild_id);
        self.persist_session(guild_id).await;
    }
}

#[cfg(test)]
mod tests {
    use serenity::all::ChannelId;

    use super::*;
    use crate::db;
    use crate::voice::backend::{VoiceEvents, VoiceTrack};
    use crate::voice::registry::playing;
    use crate::voice::testing::{
        FakeBackend, current_track_id, joined_registry, queued, upcoming_ids, wait_until,
    };

    async fn playing_a_then_b() -> (PlayerRegistry, Arc<FakeBackend>, GuildId, Uuid) {
        let (registry, backend, guild_id) = playing(&["a", "b"]).await;
        let a_id = current_track_id(&registry, guild_id).await;
        (registry, backend, guild_id, a_id)
    }

    async fn abandoned_start() -> (PlayerRegistry, Arc<FakeBackend>, GuildId) {
        let (registry, backend, guild_id) = joined_registry().await;
        for id in ["a", "b", "c", "d"] {
            backend.make_unplayable(id);
        }
        let tracks = ["a", "b", "c", "d", "e"].map(queued).to_vec();
        registry.enqueue_many(guild_id, tracks).await.unwrap();
        registry.settle_playback_start(guild_id).await;
        (registry, backend, guild_id)
    }

    async fn plays_of(registry: &PlayerRegistry, guild_id: GuildId, video_id: &str) -> Option<i64> {
        db::top_played_tracks(&registry.db, &guild_id.to_string(), 5)
            .await
            .unwrap()
            .iter()
            .find(|t| t.video_id == video_id)
            .map(|t| t.play_count)
    }

    #[tokio::test]
    async fn a_track_that_fails_at_the_start_is_retried_once_with_a_fresh_url() {
        let (registry, backend, guild_id, a_id) = playing_a_then_b().await;
        backend.clear_buffered_source_calls();

        backend.fail_track(guild_id, a_id, Duration::ZERO).await;
        registry.settle_playback_start(guild_id).await;

        let call = backend.call_for(guild_id).unwrap();
        assert_eq!(call.played_video_ids(), vec!["a", "a"]);
        let fresh_resolves = backend
            .buffered_source_calls()
            .iter()
            .filter(|id| *id == "a")
            .count();
        assert_eq!(fresh_resolves, 1);
        let snapshot = registry.queue_snapshot(guild_id).await;
        assert_eq!(upcoming_ids(&snapshot), vec!["b"]);
        assert_eq!(snapshot.now_playing.unwrap().track.video_id, "a");
        assert_eq!(plays_of(&registry, guild_id, "a").await, Some(1));
    }

    #[tokio::test]
    async fn a_second_early_failure_moves_on_to_the_next_track() {
        let (registry, backend, guild_id, a_id) = playing_a_then_b().await;
        backend.fail_track(guild_id, a_id, Duration::ZERO).await;
        registry.settle_playback_start(guild_id).await;
        let retry_id = current_track_id(&registry, guild_id).await;
        assert_ne!(retry_id, a_id);

        backend.fail_track(guild_id, retry_id, Duration::ZERO).await;
        registry.settle_playback_start(guild_id).await;

        let call = backend.call_for(guild_id).unwrap();
        assert_eq!(call.played_video_ids(), vec!["a", "a", "b"]);
        let snapshot = registry.queue_snapshot(guild_id).await;
        assert_eq!(snapshot.now_playing.unwrap().track.video_id, "b");
    }

    #[tokio::test]
    async fn the_same_video_queued_again_gets_its_own_retry_and_play_count() {
        let (registry, backend, guild_id) = playing(&["a", "a"]).await;
        let first_id = current_track_id(&registry, guild_id).await;
        backend.fail_track(guild_id, first_id, Duration::ZERO).await;
        registry.settle_playback_start(guild_id).await;
        let retry_id = current_track_id(&registry, guild_id).await;
        backend.finish_track(guild_id, retry_id).await;
        registry.settle_playback_start(guild_id).await;
        let second_id = current_track_id(&registry, guild_id).await;

        backend
            .fail_track(guild_id, second_id, Duration::ZERO)
            .await;
        registry.settle_playback_start(guild_id).await;

        let call = backend.call_for(guild_id).unwrap();
        assert_eq!(call.played_video_ids(), vec!["a", "a", "a", "a"]);
        assert_eq!(plays_of(&registry, guild_id, "a").await, Some(2));
    }

    #[tokio::test]
    async fn an_error_reported_before_the_commit_lands_is_still_retried() {
        let (registry, backend, guild_id) = joined_registry().await;
        let gate = backend.hold_volume();
        registry.enqueue(guild_id, queued("a")).await.unwrap();
        registry.enqueue(guild_id, queued("b")).await.unwrap();
        let call = backend.call_for(guild_id).unwrap();
        wait_until(|| call.played_video_ids() == ["a"]).await;
        let held = call.last_track();

        registry
            .track_errored(
                guild_id,
                held.uuid(),
                Duration::ZERO,
                "stream refused".into(),
            )
            .await;
        gate.notify_one();
        wait_until(|| call.played_video_ids() == ["a", "a"]).await;
        registry.settle_playback_start(guild_id).await;

        assert_ne!(current_track_id(&registry, guild_id).await, held.uuid());
        let snapshot = registry.queue_snapshot(guild_id).await;
        assert_eq!(upcoming_ids(&snapshot), vec!["b"]);
        assert_eq!(snapshot.now_playing.unwrap().track.video_id, "a");
    }

    #[tokio::test]
    async fn three_tracks_in_a_row_failing_early_give_up_and_keep_the_rest_of_the_queue() {
        let (registry, backend, guild_id) = playing(&["a", "b", "c", "d", "e"]).await;

        for _ in 0..3 {
            for _ in 0..2 {
                let id = current_track_id(&registry, guild_id).await;
                backend.fail_track(guild_id, id, Duration::ZERO).await;
                registry.settle_playback_start(guild_id).await;
            }
        }

        let call = backend.call_for(guild_id).unwrap();
        assert_eq!(call.played_video_ids(), ["a", "a", "b", "b", "c", "c"]);
        let snapshot = registry.queue_snapshot(guild_id).await;
        assert!(snapshot.now_playing.is_none());
        assert_eq!(upcoming_ids(&snapshot), vec!["d", "e"]);
    }

    #[tokio::test]
    async fn a_failure_well_into_a_track_moves_on_without_a_retry() {
        let (registry, backend, guild_id, a_id) = playing_a_then_b().await;

        backend
            .fail_track(guild_id, a_id, Duration::from_secs(60))
            .await;
        registry.settle_playback_start(guild_id).await;

        let call = backend.call_for(guild_id).unwrap();
        assert_eq!(call.played_video_ids(), vec!["a", "b"]);
    }

    #[tokio::test]
    async fn a_start_sequence_gives_up_after_three_failures_and_keeps_the_rest_of_the_queue() {
        let (registry, backend, guild_id) = abandoned_start().await;

        let snapshot = registry.queue_snapshot(guild_id).await;
        assert!(snapshot.now_playing.is_none());
        assert_eq!(upcoming_ids(&snapshot), vec!["d", "e"]);
        assert!(
            backend
                .call_for(guild_id)
                .unwrap()
                .played_video_ids()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn joining_again_starts_a_queue_left_behind_by_an_abandoned_start() {
        let (registry, backend, guild_id) = abandoned_start().await;

        registry.join(guild_id, ChannelId::new(2)).await.unwrap();
        registry.settle_playback_start(guild_id).await;

        let call = backend.call_for(guild_id).unwrap();
        assert_eq!(call.played_video_ids(), vec!["e"]);
        let snapshot = registry.queue_snapshot(guild_id).await;
        assert_eq!(snapshot.now_playing.unwrap().track.video_id, "e");
        assert!(snapshot.upcoming.is_empty());
    }

    #[tokio::test]
    async fn a_prefetch_older_than_the_limit_is_resolved_again() {
        let (mut registry, backend, guild_id) = joined_registry().await;
        registry.prefetch_max_age = Duration::ZERO;
        registry.enqueue(guild_id, queued("a")).await.unwrap();
        registry.enqueue(guild_id, queued("b")).await.unwrap();
        registry.settle_playback_start(guild_id).await;
        let a_id = current_track_id(&registry, guild_id).await;

        backend.finish_track(guild_id, a_id).await;
        // Waiting on the track itself, since the guild still reads as playing
        // "a" for as long as it takes the finish to be applied.
        wait_until(|| backend.call_for(guild_id).unwrap().played_video_ids().len() == 2).await;

        assert_eq!(
            backend.call_for(guild_id).unwrap().played_video_ids(),
            vec!["a", "b"]
        );
        let resolves_of_b = backend
            .buffered_source_calls()
            .iter()
            .filter(|id| *id == "b")
            .count();
        assert_eq!(resolves_of_b, 2);
    }
}
