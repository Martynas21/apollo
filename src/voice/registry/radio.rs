use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use rand::seq::SliceRandom;
use serenity::all::{GuildId, UserId};
use tokio::sync::Notify;

use crate::db;
use crate::model::{QueuedTrack, Track};
use crate::voice::radio;
use crate::voice::registry::PlayerRegistry;
use crate::voice::state::{AdvanceFill, Promotion};

const RADIO_REFILL_BATCH: usize = 4;

/// How long `advance()` will wait for an in-flight radio refill to finish
/// before declaring the guild idle, when the queue is empty but a refill is
/// already running. Best-effort only: `run_radio_refill`'s own
/// `kick_off_if_idle` call remains the safety net if this wait elapses.
const RADIO_ADVANCE_WAIT: Duration = Duration::from_secs(3);

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

impl PlayerRegistry {
    pub async fn toggle_radio(&self, guild_id: GuildId) -> bool {
        let (enabled, needs_refill) = {
            let mut guilds = self.guilds.lock().await;
            // Flip what the dashboard was showing: for a guild with no live
            // state that is the persisted setting, not a fresh default.
            let was_enabled = match guilds.get(&guild_id) {
                Some(state) => state.radio_enabled,
                None => self.persisted_radio_enabled(guild_id).await,
            };
            let state = guilds.entry(guild_id).or_default();
            state.radio_enabled = !was_enabled;
            state.radio_exhausted = false;
            let enabled = state.radio_enabled;
            let needs_refill = if enabled {
                let queued = db::queue_len(&self.db, &guild_id.to_string())
                    .await
                    .unwrap_or(0);
                // The current track sits at the head of the same queue, so
                // "nothing upcoming" is one row, not none.
                queued <= state.upcoming_offset()
            } else {
                false
            };
            (enabled, needs_refill)
        };

        if enabled && needs_refill {
            self.maybe_spawn_radio_refill(guild_id);
        }

        if self.is_connected(guild_id) {
            self.persist_session(guild_id).await;
        } else if let Err(err) =
            db::set_guild_radio_enabled(&self.db, &guild_id.to_string(), enabled).await
        {
            // Only the flag: persisting the whole session from a guild with
            // no live state would put a blank `last_played` and an empty
            // history over the real ones, which the next join restores.
            tracing::warn!(%guild_id, %err, "failed to persist the radio setting");
        }
        enabled
    }

    /// A guild with no live state still has a radio setting: the one its
    /// session carries and the next join restores. Reading through to it
    /// keeps the dashboard's toggle showing what will actually happen,
    /// exactly as the queue shows what will actually play.
    pub async fn is_radio_enabled(&self, guild_id: GuildId) -> bool {
        {
            let guilds = self.guilds.lock().await;
            if let Some(state) = guilds.get(&guild_id) {
                return state.radio_enabled;
            }
        }
        self.persisted_radio_enabled(guild_id).await
    }

    async fn persisted_radio_enabled(&self, guild_id: GuildId) -> bool {
        match db::load_guild_session(&self.db, &guild_id.to_string()).await {
            Ok(session) => session.is_some_and(|session| session.radio_enabled),
            Err(err) => {
                tracing::warn!(%guild_id, %err, "failed to load the persisted radio setting");
                false
            }
        }
    }

    pub(super) fn maybe_spawn_radio_refill(&self, guild_id: GuildId) {
        let registry = self.clone();
        tokio::spawn(async move { registry.run_radio_refill(guild_id).await });
    }

    /// Runs at most one radio refill per guild at a time (`radio_refill_snapshot`
    /// claims `radio_refill_running` before any of this runs) and always
    /// releases that claim via `finish_radio_refill` on the way out, so
    /// `advance()` can safely wait on `radio_refill_notify` without a lost
    /// wakeup or a stuck "always running" flag.
    async fn run_radio_refill(&self, guild_id: GuildId) {
        let Some(snapshot) = self.radio_refill_snapshot(guild_id).await else {
            return;
        };
        self.run_radio_refill_body(guild_id, snapshot).await;
        self.finish_radio_refill(guild_id).await;
    }

    async fn run_radio_refill_body(
        &self,
        guild_id: GuildId,
        (history, requested_by, played, epoch): (Vec<String>, UserId, HashSet<String>, u64),
    ) {
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
            // Must run before `finish_radio_refill` notifies any waiter in
            // `advance()`, so a waiter that wakes up sees the fully-settled
            // outcome (either a track already started here, or genuinely
            // nothing to start) rather than racing this call.
            self.kick_off_if_idle(guild_id).await;
            self.persist_session(guild_id).await;
        }
    }

    async fn finish_radio_refill(&self, guild_id: GuildId) {
        let notify = {
            let mut guilds = self.guilds.lock().await;
            let Some(state) = guilds.get_mut(&guild_id) else {
                return;
            };
            state.radio_refill_running = false;
            state.radio_refill_notify.clone()
        };
        if let Some(notify) = notify {
            notify.notify_waiters();
        }
    }

    pub(super) async fn kick_off_if_idle(&self, guild_id: GuildId) {
        let Some(call) = self.voice.call(guild_id) else {
            return;
        };
        match self.promote_next_if_idle(guild_id).await {
            Promotion::Track(queued, epoch) => {
                self.spawn_start_sequence(guild_id, call, queued, epoch, None, false)
            }
            Promotion::QueueEmpty => self.schedule_idle_disconnect(guild_id),
            Promotion::Busy => {}
        }
    }

    async fn radio_refill_snapshot(
        &self,
        guild_id: GuildId,
    ) -> Option<(Vec<String>, UserId, HashSet<String>, u64)> {
        let mut guilds = self.guilds.lock().await;
        let state = guilds.get_mut(&guild_id)?;
        if state.radio_exhausted || state.radio_refill_running {
            return None;
        }
        let (true, Some(requested_by)) = (state.radio_enabled, state.radio_requested_by) else {
            return None;
        };
        state.radio_refill_running = true;
        state
            .radio_refill_notify
            .get_or_insert_with(|| Arc::new(Notify::new()));
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

    /// Waits briefly for an in-flight radio refill to finish (or for its own
    /// `kick_off_if_idle` to already have started something) before giving
    /// up on filling the queue. Best-effort: on timeout, or if the refill
    /// came back empty, the caller falls back to the normal idle path, and
    /// `run_radio_refill`'s own `kick_off_if_idle` call remains the eventual
    /// safety net.
    pub(super) async fn await_radio_refill_then_repop(
        &self,
        guild_id: GuildId,
        notify: Arc<Notify>,
    ) -> AdvanceFill {
        let notified = notify.notified();
        let _ = tokio::time::timeout(RADIO_ADVANCE_WAIT, notified).await;

        let mut guilds = self.guilds.lock().await;
        let Some(state) = guilds.get_mut(&guild_id) else {
            return AdvanceFill::Idle;
        };
        if state.now_playing.is_some() {
            return AdvanceFill::AlreadyStarted;
        }
        let next = match db::queue_current(&self.db, &guild_id.to_string()).await {
            Ok(next) => next,
            Err(err) => {
                tracing::warn!(
                    %guild_id, %err,
                    "failed to read the queue after the radio refill wait"
                );
                None
            }
        };
        state.now_playing = next.clone();
        match next {
            Some(next) => AdvanceFill::Track(next, state.epoch),
            None => AdvanceFill::Idle,
        }
    }
}

#[cfg(test)]
mod tests {
    use serenity::all::ChannelId;

    use super::*;
    use crate::voice::testing::{
        current_track_id, joined_registry, new_registry, persist_queue, persist_radio, queued,
        upcoming_ids,
    };

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
    async fn concurrent_kick_offs_start_only_one_track() {
        let (registry, backend, guild_id) = joined_registry().await;
        for id in ["a", "b"] {
            db::queue_push_back(&registry.db, &guild_id.to_string(), &queued(id))
                .await
                .unwrap();
        }

        tokio::join!(
            registry.kick_off_if_idle(guild_id),
            registry.kick_off_if_idle(guild_id),
        );
        registry.settle_playback_start(guild_id).await;

        let call = backend.call_for(guild_id).unwrap();
        assert_eq!(call.played_video_ids(), vec!["a"]);
        let snapshot = registry.queue_snapshot(guild_id).await;
        assert_eq!(upcoming_ids(&snapshot), vec!["b"]);
        assert_eq!(snapshot.now_playing.unwrap().track.video_id, "a");
    }

    #[tokio::test]
    async fn radio_reads_the_persisted_setting_when_a_guild_has_no_live_state() {
        let (registry, _backend, guild_id) = new_registry().await;
        persist_radio(&registry, guild_id, Some("1"), &["a"]).await;

        assert!(registry.is_radio_enabled(guild_id).await);
    }

    #[tokio::test]
    async fn radio_reads_as_off_for_a_guild_with_nothing_persisted() {
        let (registry, _backend, guild_id) = new_registry().await;

        assert!(!registry.is_radio_enabled(guild_id).await);
    }

    #[tokio::test]
    async fn toggling_radio_while_disconnected_flips_the_persisted_setting() {
        let (registry, _backend, guild_id) = new_registry().await;
        persist_radio(&registry, guild_id, Some("1"), &["a"]).await;

        assert!(!registry.toggle_radio(guild_id).await);

        assert!(!registry.is_radio_enabled(guild_id).await);
        let session = db::load_guild_session(&registry.db, &guild_id.to_string())
            .await
            .unwrap()
            .expect("the session should still be there");
        assert!(!session.radio_enabled);
        assert_eq!(
            session.radio_requested_by,
            Some("1".to_string()),
            "the radio seed and history are none of the toggle's business"
        );
        assert_eq!(session.radio_history, vec!["a".to_string()]);
    }

    #[tokio::test]
    async fn a_radio_toggle_made_while_disconnected_survives_the_next_join() {
        let (registry, _backend, guild_id) = new_registry().await;
        persist_radio(&registry, guild_id, Some("1"), &["a"]).await;

        registry.toggle_radio(guild_id).await;
        registry.join(guild_id, ChannelId::new(2)).await.unwrap();

        assert!(!registry.is_radio_enabled(guild_id).await);
    }

    #[tokio::test]
    async fn toggling_radio_while_disconnected_does_not_block_a_later_session_restore() {
        let (registry, backend, guild_id) = new_registry().await;
        persist_queue(&registry, guild_id, &["a"]).await;
        persist_radio(&registry, guild_id, Some("1"), &["a"]).await;

        // `toggle_radio` creates a `GuildState` map entry with no connection
        // precondition; that must not block the later real `join()` from
        // restoring the persisted session.
        registry.toggle_radio(guild_id).await;

        registry.join(guild_id, ChannelId::new(2)).await.unwrap();
        registry.settle_playback_start(guild_id).await;

        let call = backend.call_for(guild_id).unwrap();
        assert_eq!(call.played_video_ids(), vec!["a"]);
        let snapshot = registry.queue_snapshot(guild_id).await;
        assert_eq!(snapshot.now_playing.unwrap().track.video_id, "a");
    }

    #[tokio::test]
    async fn radio_refill_snapshot_claims_the_running_flag_so_a_second_call_is_skipped() {
        let (registry, _backend, guild_id) = joined_registry().await;
        {
            let mut guilds = registry.guilds.lock().await;
            let state = guilds.entry(guild_id).or_default();
            state.radio_enabled = true;
            state.radio_requested_by = Some(UserId::new(1));
            state.radio_history.push_back("seed".to_string());
        }

        let first = registry.radio_refill_snapshot(guild_id).await;
        assert!(first.is_some());
        assert!(
            registry
                .guilds
                .lock()
                .await
                .get(&guild_id)
                .unwrap()
                .radio_refill_running
        );

        let second = registry.radio_refill_snapshot(guild_id).await;
        assert!(second.is_none());

        registry.finish_radio_refill(guild_id).await;
        assert!(
            !registry
                .guilds
                .lock()
                .await
                .get(&guild_id)
                .unwrap()
                .radio_refill_running
        );

        let third = registry.radio_refill_snapshot(guild_id).await;
        assert!(third.is_some());
    }
}
