use std::sync::Arc;
use std::time::Duration;

use serenity::all::GuildId;
use tokio::time::Instant;
use uuid::Uuid;

use crate::voice::backend::VoiceTrack;
use crate::voice::registry::PlayerRegistry;
use crate::voice::resolve::MAX_TRACK_DURATION;

#[derive(Clone, Copy)]
pub(crate) struct StallTiming {
    pub(super) check_interval: Duration,
    /// A track that is not paused and reports the same position for this
    /// long is stopped so the queue moves past it.
    pub(super) limit: Duration,
    /// How long the worker gets to report a stopped track as ended before it
    /// is treated as no longer honouring commands for the guild.
    pub(super) stop_grace: Duration,
}

/// The limit sits comfortably above the audio worker's stream read timeout
/// plus a resume round trip, so a stream that stalls and then picks itself
/// back up is never cut off.
pub(super) const STALL_TIMING: StallTiming = StallTiming {
    check_interval: Duration::from_secs(5),
    limit: Duration::from_secs(30),
    stop_grace: Duration::from_secs(10),
};

/// Number of status polls after which the watchdog gives up on its own: the
/// track's length plus the stall allowance, so the loop cannot outlive the
/// track it watches even if the "still current" check never fails.
fn check_budget(track_length: Option<Duration>, timing: StallTiming) -> u64 {
    let length = track_length.unwrap_or(MAX_TRACK_DURATION);
    let polls = (length + timing.limit).as_millis() / timing.check_interval.as_millis().max(1);
    u64::try_from(polls).unwrap_or(u64::MAX).saturating_add(2)
}

impl PlayerRegistry {
    pub(super) fn spawn_stall_watchdog(
        &self,
        guild_id: GuildId,
        handle: Arc<dyn VoiceTrack>,
        track_length: Option<Duration>,
    ) {
        let registry = self.clone();
        tokio::spawn(async move {
            registry
                .run_stall_watchdog(guild_id, handle, track_length)
                .await;
        });
    }

    async fn run_stall_watchdog(
        &self,
        guild_id: GuildId,
        handle: Arc<dyn VoiceTrack>,
        track_length: Option<Duration>,
    ) {
        let timing = self.stall_timing;
        let track_id = handle.uuid();
        let mut last_position: Option<Duration> = None;
        let mut last_progress_at = Instant::now();
        for _ in 0..check_budget(track_length, timing) {
            tokio::time::sleep(timing.check_interval).await;
            if !self.is_current_track(guild_id, track_id).await {
                return;
            }
            let status = handle.status().await;
            let moved = status.as_ref().is_some_and(|status| {
                status.paused || last_position.is_some_and(|previous| status.position != previous)
            });
            last_position = status.map(|status| status.position).or(last_position);
            if moved {
                last_progress_at = Instant::now();
                continue;
            }
            let stalled_for = last_progress_at.elapsed();
            if stalled_for < timing.limit {
                continue;
            }
            tracing::warn!(
                %guild_id,
                %track_id,
                stalled_secs = stalled_for.as_secs(),
                "track reports no progress; stopping it so the queue can move on"
            );
            self.stop_stalled_track(guild_id, handle).await;
            return;
        }
    }

    /// Stops the track and, if the worker never reports it as ended, drops
    /// it and rebuilds the guild's voice session: a worker that ignores a
    /// stop is not going to play the next track either, and the rejoin is
    /// what starts the rest of the queue on the new driver.
    async fn stop_stalled_track(&self, guild_id: GuildId, handle: Arc<dyn VoiceTrack>) {
        let track_id = handle.uuid();
        if let Err(err) = handle.stop().await {
            tracing::warn!(%guild_id, %track_id, %err, "failed to stop a stalled track");
        }
        tokio::time::sleep(self.stall_timing.stop_grace).await;
        if !self.drop_current_track(guild_id, track_id).await {
            return;
        }
        tracing::error!(
            %guild_id,
            %track_id,
            "audio worker never reported the stopped track as ended; skipping it and rebuilding the voice session"
        );
        self.rebuild_voice_session(guild_id).await;
    }

    async fn is_current_track(&self, guild_id: GuildId, track_id: Uuid) -> bool {
        let guilds = self.guilds.lock().await;
        guilds
            .get(&guild_id)
            .is_some_and(|state| state.current_track_id == Some(track_id))
    }

    /// Forgets the track if it is still current, without starting another,
    /// and reports whether it was.
    async fn drop_current_track(&self, guild_id: GuildId, track_id: Uuid) -> bool {
        let mut guilds = self.guilds.lock().await;
        let Some(state) = guilds.get_mut(&guild_id) else {
            return false;
        };
        if state.current_track_id != Some(track_id) {
            return false;
        }
        state.current_handle = None;
        state.current_track_id = None;
        state.now_playing = None;
        state.paused = false;
        true
    }

    /// Leaves and rejoins the guild's current voice channel, which replaces
    /// the worker's driver for it and resumes from the persisted session.
    async fn rebuild_voice_session(&self, guild_id: GuildId) {
        let Some(channel_id) = self.voice.current_channel(guild_id).await else {
            return;
        };
        if let Err(err) = self.leave(guild_id).await {
            tracing::warn!(%guild_id, %err, "failed to leave voice before rebuilding the session");
        }
        if let Err(err) = self.join(guild_id, channel_id).await {
            tracing::warn!(%guild_id, %err, "failed to rejoin voice while rebuilding the session");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::testing::{
        FakeBackend, current_track_id, joined_registry, queued, wait_until,
    };

    const FAST: StallTiming = StallTiming {
        check_interval: Duration::from_millis(5),
        limit: Duration::from_millis(50),
        stop_grace: Duration::from_millis(20),
    };

    /// Like `registry::playing`, with the fast stall timing applied before
    /// anything starts so the watchdog under test is the real spawned one.
    async fn fast_playing(ids: &[&str]) -> (PlayerRegistry, Arc<FakeBackend>, GuildId) {
        let (mut registry, backend, guild_id) = joined_registry().await;
        registry.stall_timing = FAST;
        for id in ids {
            registry.enqueue(guild_id, queued(id)).await.unwrap();
        }
        registry.settle_playback_start(guild_id).await;
        (registry, backend, guild_id)
    }

    #[test]
    fn check_budget_covers_the_track_length_plus_the_stall_allowance() {
        assert_eq!(
            check_budget(Some(Duration::from_secs(100)), STALL_TIMING),
            28
        );
        assert_eq!(check_budget(None, STALL_TIMING), 10088);
    }

    #[tokio::test]
    async fn a_track_whose_position_never_moves_is_stopped_after_the_stall_limit() {
        let (_registry, backend, guild_id) = fast_playing(&["a"]).await;
        let track = backend.call_for(guild_id).unwrap().last_track();

        wait_until(|| track.was_stopped()).await;
    }

    #[tokio::test]
    async fn a_track_that_keeps_moving_is_left_alone() {
        let (_registry, backend, guild_id) = fast_playing(&["a"]).await;
        let track = backend.call_for(guild_id).unwrap().last_track();

        for step in 1..=30_u32 {
            tokio::time::sleep(FAST.check_interval).await;
            track.set_position(FAST.check_interval * step);
        }

        assert!(!track.was_stopped());
    }

    #[tokio::test]
    async fn a_paused_track_is_never_treated_as_stalled() {
        let (registry, backend, guild_id) = fast_playing(&["a"]).await;
        let track = backend.call_for(guild_id).unwrap().last_track();
        registry.pause(guild_id).await.unwrap();

        tokio::time::sleep(FAST.limit * 4).await;

        assert!(!track.was_stopped());
    }

    #[tokio::test]
    async fn the_watchdog_lets_go_once_its_track_is_no_longer_current() {
        let (registry, backend, guild_id) = fast_playing(&["a"]).await;
        let track = backend.call_for(guild_id).unwrap().last_track();
        let a_id = current_track_id(&registry, guild_id).await;
        backend.finish_track(guild_id, a_id).await;

        tokio::time::sleep(FAST.limit * 4).await;

        assert!(!track.was_stopped());
    }

    #[tokio::test]
    async fn a_stop_the_worker_confirms_advances_the_queue_without_a_rebuild() {
        let (registry, backend, guild_id) = fast_playing(&["a", "b"]).await;
        backend.set_stop_confirmation(true);

        registry.settle_playback_start(guild_id).await;
        wait_until(|| backend.all_played_video_ids(guild_id) == ["a", "b"]).await;
        tokio::time::sleep(FAST.stop_grace * 2).await;

        assert_eq!(backend.join_call_count(guild_id), 1);
        let snapshot = registry.queue_snapshot(guild_id).await;
        assert_eq!(snapshot.now_playing.unwrap().track.video_id, "b");
    }

    #[tokio::test]
    async fn a_stop_the_worker_never_confirms_skips_the_track_and_rebuilds_the_session_once() {
        let (registry, backend, guild_id) = fast_playing(&["a", "b"]).await;
        let track = backend.call_for(guild_id).unwrap().last_track();

        wait_until(|| backend.join_call_count(guild_id) == 2).await;
        registry.settle_playback_start(guild_id).await;
        tokio::time::sleep(FAST.check_interval * 4).await;

        assert!(track.was_stopped());
        assert_eq!(backend.all_played_video_ids(guild_id), ["a", "b"]);
        let snapshot = registry.queue_snapshot(guild_id).await;
        assert_eq!(snapshot.now_playing.unwrap().track.video_id, "b");
    }
}
