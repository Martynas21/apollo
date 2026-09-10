use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serenity::all::{GuildId, UserId};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::db;
use crate::model::{QueuedTrack, Track};
use crate::voice::backend::{AudioSource, VoiceBackend, VoiceEvents};
use crate::voice::resolve::PlaybackError;
use crate::voice::state::GuildState;
use crate::youtube::api::YouTubeClient;

mod playback;
mod queue;
mod radio;
mod session;

pub(super) type Prefetch = JoinHandle<Result<AudioSource, PlaybackError>>;

const IDLE_DISCONNECT: Duration = Duration::from_secs(150);

fn discard_prefetch(prefetch: Prefetch) {
    prefetch.abort();
}

pub(super) fn persisted_to_queued_track(pair: Option<(Track, String)>) -> Option<QueuedTrack> {
    pair.and_then(|(track, requested_by)| {
        requested_by.parse().ok().map(|id| QueuedTrack {
            track,
            requested_by: UserId::new(id),
        })
    })
}

#[derive(Clone)]
pub struct PlayerRegistry {
    pub(super) voice: Arc<dyn VoiceBackend>,
    pub(super) cookies_file: Option<String>,
    pub(super) db: sqlx::SqlitePool,
    pub(super) youtube: YouTubeClient,
    pub(super) guilds: Arc<Mutex<HashMap<GuildId, GuildState>>>,
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
        cookies_file: Option<String>,
        db: sqlx::SqlitePool,
        youtube: YouTubeClient,
    ) -> Self {
        Self {
            voice,
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
    pub(super) fn new_for_test(voice: Arc<dyn VoiceBackend>, db: sqlx::SqlitePool) -> Self {
        Self::new(voice, None, db, YouTubeClient::default())
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

    pub fn is_connected(&self, guild_id: GuildId) -> bool {
        self.voice.call(guild_id).is_some()
    }

    async fn cached_input(&self, queued: &QueuedTrack) -> Result<AudioSource, PlaybackError> {
        self.voice.buffered_source(&queued.track).await
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

    fn schedule_idle_disconnect(&self, guild_id: GuildId) {
        let registry = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(IDLE_DISCONNECT).await;

            registry.leave_if_idle(guild_id).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::testing::{FakeBackend, current_track_id, joined_registry, queued};

    #[derive(Debug, Clone, Copy)]
    enum MatrixState {
        Empty,
        Buffering,
        Playing,
        Paused,
        QueueFinished,
    }

    enum MatrixAction {
        Pause,
        Resume,
        Skip,
        Stop,
        Shuffle,
        ToggleRadio,
        ClearQueue,
        SetVolume,
        RemoveQueueTrack,
        MoveQueueTrack,
        EnqueueNext,
        PlayQueueTrack,
    }

    impl MatrixAction {
        const ALL: [MatrixAction; 12] = [
            MatrixAction::Pause,
            MatrixAction::Resume,
            MatrixAction::Skip,
            MatrixAction::Stop,
            MatrixAction::Shuffle,
            MatrixAction::ToggleRadio,
            MatrixAction::ClearQueue,
            MatrixAction::SetVolume,
            MatrixAction::RemoveQueueTrack,
            MatrixAction::MoveQueueTrack,
            MatrixAction::EnqueueNext,
            MatrixAction::PlayQueueTrack,
        ];

        async fn invoke(&self, registry: &PlayerRegistry, guild_id: GuildId) {
            match self {
                MatrixAction::Pause => {
                    let _ = registry.pause(guild_id).await;
                }
                MatrixAction::Resume => {
                    let _ = registry.resume(guild_id).await;
                }
                MatrixAction::Skip => {
                    let _ = registry.skip(guild_id).await;
                }
                MatrixAction::Stop => {
                    let _ = registry.stop(guild_id).await;
                }
                MatrixAction::Shuffle => {
                    let _ = registry.shuffle(guild_id).await;
                }
                MatrixAction::ToggleRadio => {
                    registry.toggle_radio(guild_id).await;
                }
                MatrixAction::ClearQueue => {
                    let _ = registry.clear_queue(guild_id).await;
                }
                MatrixAction::SetVolume => {
                    let _ = registry.set_volume(guild_id, 42).await;
                }
                MatrixAction::RemoveQueueTrack => {
                    let _ = registry.remove_queue_track(guild_id, 0).await;
                }
                MatrixAction::MoveQueueTrack => {
                    let _ = registry.move_queue_track(guild_id, 0, 1).await;
                }
                MatrixAction::EnqueueNext => {
                    let _ = registry.enqueue_next(guild_id, queued("z")).await;
                }
                MatrixAction::PlayQueueTrack => {
                    let _ = registry.play_queue_track(guild_id, 0).await;
                }
            }
        }

        fn name(&self) -> &'static str {
            match self {
                MatrixAction::Pause => "pause",
                MatrixAction::Resume => "resume",
                MatrixAction::Skip => "skip",
                MatrixAction::Stop => "stop",
                MatrixAction::Shuffle => "shuffle",
                MatrixAction::ToggleRadio => "toggle_radio",
                MatrixAction::ClearQueue => "clear_queue",
                MatrixAction::SetVolume => "set_volume",
                MatrixAction::RemoveQueueTrack => "remove_queue_track",
                MatrixAction::MoveQueueTrack => "move_queue_track",
                MatrixAction::EnqueueNext => "enqueue_next",
                MatrixAction::PlayQueueTrack => "play_queue_track",
            }
        }
    }

    /// Drives a fresh registry into one of the five documented player states,
    /// with `radio_enabled` set directly (never via the real `toggle_radio`,
    /// which would spawn a real network-hitting refill for a guild that
    /// already has radio history from having played a track). Radio history
    /// is always cleared afterward for the same reason: it's what makes
    /// calling the real `toggle_radio` safe as the *action under test* below,
    /// since an empty history makes `pick_radio_seed` bail out before any
    /// network call happens.
    async fn setup_matrix_state(
        state: MatrixState,
        radio_enabled: bool,
    ) -> (PlayerRegistry, Arc<FakeBackend>, GuildId) {
        let (registry, backend, guild_id) = joined_registry().await;
        match state {
            MatrixState::Empty => {}
            MatrixState::Buffering => {
                let mut guilds = registry.guilds.lock().await;
                let state = guilds.entry(guild_id).or_default();
                state.now_playing = Some(queued("a"));
            }
            MatrixState::Playing => {
                registry.enqueue(guild_id, queued("a")).await.unwrap();
                registry.settle_playback_start(guild_id).await;
            }
            MatrixState::Paused => {
                registry.enqueue(guild_id, queued("a")).await.unwrap();
                registry.settle_playback_start(guild_id).await;
                registry.pause(guild_id).await.unwrap();
            }
            MatrixState::QueueFinished => {
                registry.enqueue(guild_id, queued("a")).await.unwrap();
                registry.settle_playback_start(guild_id).await;
                let a_id = current_track_id(&registry, guild_id).await;
                backend.finish_track(guild_id, a_id).await;
            }
        }

        {
            let mut guilds = registry.guilds.lock().await;
            let state = guilds.entry(guild_id).or_default();
            state.radio_enabled = radio_enabled;
            state.radio_history.clear();
        }

        (registry, backend, guild_id)
    }

    #[tokio::test]
    async fn every_action_is_panic_free_and_keeps_the_handle_track_id_invariant_in_every_state() {
        let states = [
            MatrixState::Empty,
            MatrixState::Buffering,
            MatrixState::Playing,
            MatrixState::Paused,
            MatrixState::QueueFinished,
        ];

        for &state in &states {
            for radio_enabled in [false, true] {
                for action in MatrixAction::ALL {
                    let (registry, _backend, guild_id) =
                        setup_matrix_state(state, radio_enabled).await;

                    action.invoke(&registry, guild_id).await;

                    let guilds = registry.guilds.lock().await;
                    if let Some(s) = guilds.get(&guild_id) {
                        assert_eq!(
                            s.current_handle.is_some(),
                            s.current_track_id.is_some(),
                            "handle/track_id invariant broken for state={state:?} radio={radio_enabled} action={}",
                            action.name()
                        );
                    }
                }
            }
        }
    }
}
