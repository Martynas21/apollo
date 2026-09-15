use rand::seq::SliceRandom;
use serenity::all::GuildId;

use crate::db;
use crate::model::QueuedTrack;
use crate::voice::error::PlayerError;
use crate::voice::registry::{PlayerRegistry, discard_prefetch};
use crate::voice::state::GuildState;

impl PlayerRegistry {
    #[cfg(test)]
    pub(super) async fn enqueue(
        &self,
        guild_id: GuildId,
        queued: QueuedTrack,
    ) -> Result<(), PlayerError> {
        let (call, start) = {
            let mut guilds = self.guilds.lock().await;
            let call = self.voice.call(guild_id).ok_or(PlayerError::NotConnected)?;
            let state = guilds.entry(guild_id).or_default();
            state.radio_exhausted = false;
            let should_start = state.now_playing.is_none();
            if should_start {
                state.radio_history.clear();
                state.now_playing = Some(queued.clone());
            } else if let Err(err) =
                db::queue_push_back(&self.db, &guild_id.to_string(), &queued).await
            {
                return Err(PlayerError::Storage(err.to_string()));
            }
            (call, should_start.then_some(state.epoch))
        };

        if let Some(epoch) = start {
            self.spawn_start_sequence(guild_id, call, queued, epoch, None, false);
        }

        self.persist_session(guild_id).await;
        Ok(())
    }

    pub async fn enqueue_next(
        &self,
        guild_id: GuildId,
        queued: QueuedTrack,
    ) -> Result<(), PlayerError> {
        let guild_id_str = guild_id.to_string();
        let (call, start) = {
            let mut guilds = self.guilds.lock().await;
            let call = self.voice.call(guild_id).ok_or(PlayerError::NotConnected)?;
            let state = guilds.entry(guild_id).or_default();
            state.radio_exhausted = false;
            let should_start = state.now_playing.is_none();
            if should_start {
                state.radio_history.clear();
                state.now_playing = Some(queued.clone());
            } else {
                let mut items = db::queue_all(&self.db, &guild_id_str)
                    .await
                    .map_err(|e| PlayerError::Storage(e.to_string()))?;
                items.insert(0, queued.clone());
                db::queue_replace_all(&self.db, &guild_id_str, &items)
                    .await
                    .map_err(|e| PlayerError::Storage(e.to_string()))?;
                self.restart_prefetch(state, items.first().cloned());
            }
            (call, should_start.then_some(state.epoch))
        };

        if let Some(epoch) = start {
            self.spawn_start_sequence(guild_id, call, queued, epoch, None, false);
        }

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
                state.radio_history.clear();
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

            if !needs_start && state.prefetch.is_none() {
                self.restart_prefetch(state, rest.first().cloned());
            }

            let start = match (needs_start, &first) {
                (true, Some(track)) => Some((track.clone(), state.epoch)),
                _ => None,
            };
            (call, start)
        };

        if let Some((first, epoch)) = start {
            self.spawn_start_sequence(guild_id, call, first, epoch, None, false);
        }

        self.persist_session(guild_id).await;
        Ok(())
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

        self.persist_session(guild_id).await;
        Ok(())
    }

    pub async fn remove_queue_track(
        &self,
        guild_id: GuildId,
        index: usize,
    ) -> Result<(), PlayerError> {
        let guild_id_str = guild_id.to_string();
        let mut guilds = self.guilds.lock().await;
        let mut items = db::queue_all(&self.db, &guild_id_str)
            .await
            .map_err(|e| PlayerError::Storage(e.to_string()))?;
        if index >= items.len() {
            return Err(PlayerError::InvalidQueueIndex);
        }

        items.remove(index);
        db::queue_replace_all(&self.db, &guild_id_str, &items)
            .await
            .map_err(|e| PlayerError::Storage(e.to_string()))?;
        if let Some(state) = guilds.get_mut(&guild_id) {
            self.restart_prefetch(state, items.first().cloned());
        }
        drop(guilds);

        self.persist_session(guild_id).await;
        Ok(())
    }

    pub async fn move_queue_track(
        &self,
        guild_id: GuildId,
        from: usize,
        to: usize,
    ) -> Result<(), PlayerError> {
        let guild_id_str = guild_id.to_string();
        let mut guilds = self.guilds.lock().await;
        let mut items = db::queue_all(&self.db, &guild_id_str)
            .await
            .map_err(|e| PlayerError::Storage(e.to_string()))?;
        if from >= items.len() || to >= items.len() || from == to {
            return Err(PlayerError::InvalidQueueIndex);
        }

        let track = items.remove(from);
        items.insert(to, track);
        db::queue_replace_all(&self.db, &guild_id_str, &items)
            .await
            .map_err(|e| PlayerError::Storage(e.to_string()))?;
        if let Some(state) = guilds.get_mut(&guild_id) {
            self.restart_prefetch(state, items.first().cloned());
        }
        drop(guilds);

        self.persist_session(guild_id).await;
        Ok(())
    }

    /// Jumps straight to an upcoming queue entry: pulls it out of the queue,
    /// stops whatever is currently playing/buffering without requeuing it
    /// (like `skip`, but landing on a chosen track instead of the front of
    /// the queue), and starts it immediately. Every other upcoming track
    /// keeps its relative order behind the new current track. Bumping the
    /// epoch invalidates any start sequence still in flight for the track
    /// that was buffering, the same guard `stop()` uses.
    pub async fn play_queue_track(
        &self,
        guild_id: GuildId,
        index: usize,
    ) -> Result<(), PlayerError> {
        let guild_id_str = guild_id.to_string();
        let (handle, call, epoch, target) = {
            let mut guilds = self.guilds.lock().await;
            let call = self.voice.call(guild_id).ok_or(PlayerError::NotConnected)?;
            let state = guilds.entry(guild_id).or_default();

            let mut items = db::queue_all(&self.db, &guild_id_str)
                .await
                .map_err(|e| PlayerError::Storage(e.to_string()))?;
            if index >= items.len() {
                return Err(PlayerError::InvalidQueueIndex);
            }
            let target = items.remove(index);
            db::queue_replace_all(&self.db, &guild_id_str, &items)
                .await
                .map_err(|e| PlayerError::Storage(e.to_string()))?;

            if let Some(prefetch) = state.prefetch.take() {
                discard_prefetch(prefetch);
            }
            let handle = state.current_handle.take();
            state.current_track_id = None;
            state.now_playing = Some(target.clone());
            state.epoch = state.epoch.wrapping_add(1);
            // Jumping straight to a chosen track is a deliberate change of
            // direction — drop the recency-weighted history so the next
            // radio refill seeds off this track instead of a stale entry
            // from whatever was playing before.
            state.radio_history.clear();
            (handle, call, state.epoch, target)
        };

        if let Some(handle) = handle {
            let _ = handle.stop().await;
        }

        self.spawn_start_sequence(guild_id, call, target, epoch, None, false);
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

        self.persist_session(guild_id).await;
        Ok(())
    }

    pub(super) fn restart_prefetch(&self, state: &mut GuildState, next: Option<QueuedTrack>) {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::error::PlayerError;
    use crate::voice::testing::{
        current_track_id, joined_registry, new_registry, queued, upcoming_ids,
    };

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
    async fn enqueue_many_into_an_empty_upcoming_queue_prefetches_the_new_track_immediately() {
        let (registry, backend, guild_id) = joined_registry().await;
        registry.enqueue(guild_id, queued("a")).await.unwrap();
        registry.settle_playback_start(guild_id).await;
        backend.clear_buffered_source_calls();

        let gate = backend.hold_downloads();
        registry
            .enqueue_many(guild_id, vec![queued("b")])
            .await
            .unwrap();
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        // "b" should already be resolving in the background, ahead of "a"
        // finishing, instead of only being fetched once `advance()` needs it.
        assert_eq!(backend.buffered_source_calls(), vec!["b".to_string()]);

        gate.notify_one();
        let a_id = current_track_id(&registry, guild_id).await;
        backend.finish_track(guild_id, a_id).await;
        registry.settle_playback_start(guild_id).await;

        let snapshot = registry.queue_snapshot(guild_id).await;
        assert_eq!(snapshot.now_playing.unwrap().track.video_id, "b");
    }

    #[tokio::test]
    async fn enqueue_next_starts_playback_immediately_on_an_empty_queue() {
        let (registry, backend, guild_id) = joined_registry().await;

        registry.enqueue_next(guild_id, queued("a")).await.unwrap();
        registry.settle_playback_start(guild_id).await;

        let call = backend.call_for(guild_id).unwrap();
        assert_eq!(call.played_video_ids(), vec!["a"]);
        let snapshot = registry.queue_snapshot(guild_id).await;
        assert_eq!(snapshot.now_playing.unwrap().track.video_id, "a");
        assert!(snapshot.upcoming.is_empty());
    }

    #[tokio::test]
    async fn enqueue_next_inserts_ahead_of_the_upcoming_queue() {
        let (registry, _backend, guild_id) = joined_registry().await;
        registry
            .enqueue_many(guild_id, vec![queued("a"), queued("b"), queued("c")])
            .await
            .unwrap();
        registry.settle_playback_start(guild_id).await;

        registry.enqueue_next(guild_id, queued("d")).await.unwrap();

        let snapshot = registry.queue_snapshot(guild_id).await;
        assert_eq!(upcoming_ids(&snapshot), vec!["d", "b", "c"]);
        assert_eq!(snapshot.now_playing.unwrap().track.video_id, "a");
    }

    #[tokio::test]
    async fn play_queue_track_promotes_the_chosen_track_and_keeps_the_rest_in_order() {
        let (registry, backend, guild_id) = joined_registry().await;
        registry.enqueue(guild_id, queued("a")).await.unwrap();
        registry
            .enqueue_many(guild_id, vec![queued("b"), queued("c"), queued("d")])
            .await
            .unwrap();
        registry.settle_playback_start(guild_id).await;

        registry.play_queue_track(guild_id, 1).await.unwrap();
        registry.settle_playback_start(guild_id).await;

        let call = backend.call_for(guild_id).unwrap();
        assert_eq!(call.played_video_ids(), vec!["a", "c"]);
        let snapshot = registry.queue_snapshot(guild_id).await;
        assert_eq!(upcoming_ids(&snapshot), vec!["b", "d"]);
        assert_eq!(snapshot.now_playing.unwrap().track.video_id, "c");
    }

    #[tokio::test]
    async fn play_queue_track_resets_radio_history_to_the_jumped_to_track() {
        let (registry, backend, guild_id) = joined_registry().await;
        registry
            .enqueue_many(guild_id, vec![queued("a"), queued("b")])
            .await
            .unwrap();
        registry.settle_playback_start(guild_id).await;
        let a_id = current_track_id(&registry, guild_id).await;
        backend.finish_track(guild_id, a_id).await;
        registry.settle_playback_start(guild_id).await;
        registry.enqueue(guild_id, queued("c")).await.unwrap();

        registry.play_queue_track(guild_id, 0).await.unwrap();
        registry.settle_playback_start(guild_id).await;

        let guilds = registry.guilds.lock().await;
        let history = Vec::from(guilds.get(&guild_id).unwrap().radio_history.clone());
        assert_eq!(history, vec!["c".to_string()]);
    }

    #[tokio::test]
    async fn play_queue_track_starts_a_queue_left_behind_with_nothing_playing() {
        let (registry, backend, guild_id) = joined_registry().await;
        db::queue_push_back(&registry.db, &guild_id.to_string(), &queued("a"))
            .await
            .unwrap();

        registry.play_queue_track(guild_id, 0).await.unwrap();
        registry.settle_playback_start(guild_id).await;

        let call = backend.call_for(guild_id).unwrap();
        assert_eq!(call.played_video_ids(), vec!["a"]);
        let snapshot = registry.queue_snapshot(guild_id).await;
        assert_eq!(snapshot.now_playing.unwrap().track.video_id, "a");
        assert!(snapshot.upcoming.is_empty());
    }

    #[tokio::test]
    async fn play_queue_track_out_of_range_reports_invalid_index() {
        let (registry, _backend, guild_id) = joined_registry().await;
        registry.enqueue(guild_id, queued("a")).await.unwrap();
        registry.settle_playback_start(guild_id).await;

        let result = registry.play_queue_track(guild_id, 0).await;

        assert!(matches!(result, Err(PlayerError::InvalidQueueIndex)));
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
    async fn move_queue_track_reinserts_rather_than_swapping_for_a_non_adjacent_move() {
        let (registry, _backend, guild_id) = joined_registry().await;
        registry.enqueue(guild_id, queued("a")).await.unwrap();
        registry
            .enqueue_many(
                guild_id,
                vec![queued("b"), queued("c"), queued("d"), queued("e")],
            )
            .await
            .unwrap();

        registry.move_queue_track(guild_id, 0, 3).await.unwrap();

        let snapshot = registry.queue_snapshot(guild_id).await;
        // Removing "b" from position 0 and reinserting it at position 3
        // yields c, d, e, b. A swap of positions 0 and 3 would instead
        // yield e, c, d, b, leaving c/d untouched.
        assert_eq!(upcoming_ids(&snapshot), vec!["c", "d", "e", "b"]);
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
}
