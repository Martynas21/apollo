use std::collections::HashMap;
use std::sync::Arc;

use serenity::all::{ChannelId, GuildId, UserId};

use crate::db;
use crate::model::QueuedTrack;
use crate::voice::backend::VoiceCall;
use crate::voice::error::PlayerError;
use crate::voice::registry::{PlayerRegistry, discard_prefetch, persisted_to_queued_track};
use crate::voice::state::{GuildState, SessionSnapshot};

impl PlayerRegistry {
    pub async fn join(
        &self,
        guild_id: GuildId,
        voice_channel_id: ChannelId,
    ) -> Result<(), PlayerError> {
        if self.voice.current_channel(guild_id).await == Some(voice_channel_id) {
            // Already connected to the requested channel: nothing to do.
            // Skipping the backend join entirely avoids tearing down and
            // reconnecting a driver that's already in the right place,
            // which is both wasted latency and (on the real IPC backend)
            // what used to cause a self-inflicted disconnect.
            if let Some(call) = self.voice.call(guild_id) {
                self.resume_after_join(guild_id, call).await;
            }
            return Ok(());
        }

        // A real rejoin (moving channels) is about to replace whatever
        // driver is currently backing this guild, if any.
        let was_replacing_existing_driver = self.voice.call(guild_id).is_some();

        self.voice
            .join(guild_id, voice_channel_id, self.events())
            .await
            .map_err(PlayerError::Join)?;

        let Some(call) = self.voice.call(guild_id) else {
            return Ok(());
        };
        if was_replacing_existing_driver {
            self.restart_on_new_driver(guild_id, call.clone()).await;
        }
        self.resume_after_join(guild_id, call).await;
        Ok(())
    }

    /// The old driver took its track with it: whatever was playing or
    /// loading is started again on the new one, and any start still running
    /// against the old driver is made stale by the epoch bump.
    async fn restart_on_new_driver(&self, guild_id: GuildId, call: Arc<dyn VoiceCall>) {
        let restart = {
            let mut guilds = self.guilds.lock().await;
            let Some(state) = guilds.get_mut(&guild_id) else {
                return;
            };
            state.epoch = state.epoch.wrapping_add(1);
            state.current_handle = None;
            state.current_track_id = None;
            state.paused = false;
            state
                .now_playing
                .clone()
                .map(|queued| (queued, state.epoch))
        };
        if let Some((queued, epoch)) = restart {
            self.spawn_start_sequence(guild_id, call, queued, epoch, None, true);
        }
    }

    /// Restores a persisted session on the guild's first join and, either
    /// way, starts whatever the queue holds if nothing is playing, or arms the
    /// idle disconnect if there is nothing to start. Joining is therefore
    /// also how a queue left behind by an abandoned start gets going.
    async fn resume_after_join(&self, guild_id: GuildId, call: Arc<dyn VoiceCall>) {
        self.restore_session_if_new(guild_id, call).await;
        self.kick_off_if_idle(guild_id).await;
    }

    async fn restore_session_if_new(&self, guild_id: GuildId, call: Arc<dyn VoiceCall>) {
        {
            let mut guilds = self.guilds.lock().await;
            let state = guilds.entry(guild_id).or_default();
            if state.restore_attempted {
                return;
            }
            state.restore_attempted = true;
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

        if let Some(queued) = candidate {
            self.spawn_start_sequence(guild_id, call, queued, epoch, None, false);
        }
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

    pub(super) async fn persist_session(&self, guild_id: GuildId) {
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
        let snapshot = {
            let mut guilds = self.guilds.lock().await;
            let snapshot = guilds.get(&guild_id).map(SessionSnapshot::from);
            Self::take_guild_state(guild_id, &mut guilds);
            snapshot
        };
        if let Some(snapshot) = snapshot {
            self.save_session_snapshot(guild_id, snapshot).await;
        }

        self.voice.remove(guild_id).await.map_err(PlayerError::Join)
    }

    /// Idle means nothing is playing or loading, or the current track is
    /// paused. A queue left behind with nothing playing does not keep the
    /// bot in voice; the next join starts it.
    fn is_idle(state: &GuildState) -> bool {
        match (&state.now_playing, state.current_track_id) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some(_), Some(_)) => state.paused,
        }
    }

    fn take_guild_state(guild_id: GuildId, guilds: &mut HashMap<GuildId, GuildState>) {
        let mut removed = guilds.remove(&guild_id);
        if let Some(prefetch) = removed.as_mut().and_then(|state| state.prefetch.take()) {
            discard_prefetch(prefetch);
        }
    }

    pub(super) async fn leave_if_idle(&self, guild_id: GuildId) {
        // Several idle timers can be armed for one guild; whichever fires
        // first does the leaving and the rest find nothing to do.
        if !self.is_connected(guild_id) {
            return;
        }
        let snapshot = {
            let mut guilds = self.guilds.lock().await;
            if guilds
                .get(&guild_id)
                .is_some_and(|state| !Self::is_idle(state))
            {
                return;
            }
            let snapshot = guilds.get(&guild_id).map(SessionSnapshot::from);
            Self::take_guild_state(guild_id, &mut guilds);
            snapshot
        };

        if let Some(snapshot) = snapshot {
            self.save_session_snapshot(guild_id, snapshot).await;
        }

        if let Err(err) = self.voice.remove(guild_id).await.map_err(PlayerError::Join) {
            tracing::warn!(%guild_id, %err, "idle disconnect failed to leave voice");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::backend::VoiceBackend;
    use crate::voice::testing::{
        current_track_id, joined_registry, new_registry, queued, upcoming_ids,
    };

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
    async fn leave_if_idle_leaves_voice_when_only_the_queue_is_non_empty() {
        let (registry, backend, guild_id) = joined_registry().await;
        db::queue_push_back(&registry.db, &guild_id.to_string(), &queued("a"))
            .await
            .unwrap();

        registry.leave_if_idle(guild_id).await;

        assert!(backend.call_for(guild_id).is_none());
        assert_eq!(
            upcoming_ids(&registry.queue_snapshot(guild_id).await),
            vec!["a"]
        );
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
    async fn rejoining_the_same_channel_short_circuits_and_preserves_playing_state() {
        let (registry, backend, guild_id) = joined_registry().await;
        registry.enqueue(guild_id, queued("a")).await.unwrap();
        registry.settle_playback_start(guild_id).await;
        let track_id_before = current_track_id(&registry, guild_id).await;

        registry.join(guild_id, ChannelId::new(2)).await.unwrap();

        assert_eq!(backend.join_call_count(guild_id), 1);
        assert_eq!(current_track_id(&registry, guild_id).await, track_id_before);
        let snapshot = registry.queue_snapshot(guild_id).await;
        assert_eq!(snapshot.now_playing.unwrap().track.video_id, "a");
    }

    #[tokio::test]
    async fn rejoining_the_same_channel_never_reaches_the_backend_even_with_eviction_simulation_enabled()
     {
        let (registry, backend, guild_id) = joined_registry().await;
        backend.enable_eviction_simulation();
        registry.enqueue(guild_id, queued("a")).await.unwrap();
        registry.settle_playback_start(guild_id).await;

        // If the short-circuit in `PlayerRegistry::join` didn't hold, this
        // would reach `FakeBackend::join`, which (with eviction simulation
        // enabled) fires a spurious `connection_lost` for the session this
        // very call just re-registered, tearing the guild state down.
        registry.join(guild_id, ChannelId::new(2)).await.unwrap();

        assert_eq!(backend.join_call_count(guild_id), 1);
        assert!(backend.call_for(guild_id).is_some());
        let snapshot = registry.queue_snapshot(guild_id).await;
        assert_eq!(snapshot.now_playing.unwrap().track.video_id, "a");
    }

    #[tokio::test]
    async fn joining_a_different_channel_restarts_the_current_track_on_the_new_driver() {
        let (registry, backend, guild_id) = joined_registry().await;
        registry.enqueue(guild_id, queued("a")).await.unwrap();
        registry.enqueue(guild_id, queued("b")).await.unwrap();
        registry.settle_playback_start(guild_id).await;
        let track_id_before = current_track_id(&registry, guild_id).await;

        registry.join(guild_id, ChannelId::new(3)).await.unwrap();
        registry.settle_playback_start(guild_id).await;

        let call = backend.call_for(guild_id).unwrap();
        assert_eq!(call.played_video_ids(), vec!["a"]);
        assert_ne!(current_track_id(&registry, guild_id).await, track_id_before);
        let snapshot = registry.queue_snapshot(guild_id).await;
        assert_eq!(upcoming_ids(&snapshot), vec!["b"]);
        assert_eq!(snapshot.now_playing.unwrap().track.video_id, "a");
    }

    #[tokio::test]
    async fn joining_a_different_channel_moves_and_bumps_epoch_for_a_real_rejoin() {
        let (registry, backend, guild_id) = joined_registry().await;
        let epoch_before = registry.guilds.lock().await.get(&guild_id).unwrap().epoch;

        registry.join(guild_id, ChannelId::new(3)).await.unwrap();

        assert_eq!(backend.join_call_count(guild_id), 2);
        assert_eq!(
            backend.current_channel(guild_id).await,
            Some(ChannelId::new(3))
        );
        let epoch_after = registry.guilds.lock().await.get(&guild_id).unwrap().epoch;
        assert_eq!(epoch_after, epoch_before.wrapping_add(1));
    }
}
