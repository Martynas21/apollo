use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use serenity::all::{GuildId, UserId};
use tokio::sync::Notify;
use uuid::Uuid;

use crate::db;
use crate::model::QueuedTrack;
use crate::voice::backend::{TrackStatus, VoiceTrack};
use crate::voice::error::PlayerError;
use crate::voice::registry::{PlayerRegistry, Prefetch, persisted_to_queued_track};

#[derive(Default)]
pub(super) struct GuildState {
    pub(super) now_playing: Option<QueuedTrack>,
    pub(super) last_played: Option<QueuedTrack>,
    pub(super) current_handle: Option<Arc<dyn VoiceTrack>>,
    pub(super) current_track_id: Option<Uuid>,
    pub(super) prefetch: Option<Prefetch>,
    pub(super) radio_enabled: bool,
    pub(super) radio_history: VecDeque<String>,
    pub(super) radio_requested_by: Option<UserId>,
    pub(super) radio_played: HashSet<String>,
    pub(super) radio_exhausted: bool,
    pub(super) radio_refill_running: bool,
    pub(super) radio_refill_notify: Option<Arc<Notify>>,
    pub(super) epoch: u64,
    pub(super) restore_attempted: bool,
    /// Set once the current track has had its one fresh-URL retry after
    /// failing right at the start, so a second early failure moves on.
    pub(super) retry_used: bool,
    /// Whether the current track was paused through this registry.
    pub(super) paused: bool,
    /// A finish or error the worker reported for a track whose start has
    /// not been committed yet; applied once the commit lands.
    pub(super) uncommitted_outcome: Option<(Uuid, TrackOutcome)>,
    /// Tracks in a row that failed within their first seconds even after
    /// their fresh-URL retry.
    pub(super) consecutive_early_failures: u32,
}

impl GuildState {
    /// The stored queue keeps a guild's current track at its head, so an
    /// index into the upcoming list sits one further along whenever the
    /// guild has one.
    pub(super) fn upcoming_offset(&self) -> usize {
        usize::from(self.now_playing.is_some())
    }

    /// Takes the outcome parked for `track_id`, dropping one parked for any
    /// other track since that track can no longer be committed.
    pub(super) fn take_uncommitted_outcome(&mut self, track_id: Uuid) -> Option<TrackOutcome> {
        match self.uncommitted_outcome.take() {
            Some((parked_id, outcome)) if parked_id == track_id => Some(outcome),
            _ => None,
        }
    }
}

/// How a track ended, as the audio worker reported it.
pub(super) enum TrackOutcome {
    Finished,
    Errored { position: Duration, error: String },
}

pub(super) enum Promotion {
    Track(QueuedTrack, u64),
    QueueEmpty,
    Busy,
}

pub struct QueueSnapshot {
    pub now_playing: Option<QueuedTrack>,
    pub loading: Option<QueuedTrack>,
    pub last_played: Option<QueuedTrack>,
    pub upcoming: Vec<QueuedTrack>,
}

pub(super) enum StartOutcome {
    Committed,
    Stale,
    Failed(PlayerError),
}

pub(super) enum AdvanceFill {
    Track(QueuedTrack, u64),
    AlreadyStarted,
    Idle,
}

pub(super) struct SessionSnapshot {
    pub(super) last_played: Option<QueuedTrack>,
    pub(super) radio_enabled: bool,
    pub(super) radio_requested_by: Option<UserId>,
    pub(super) radio_history: Vec<String>,
}

impl From<&GuildState> for SessionSnapshot {
    fn from(state: &GuildState) -> Self {
        Self {
            last_played: state.last_played.clone(),
            radio_enabled: state.radio_enabled,
            radio_requested_by: state.radio_requested_by,
            radio_history: Vec::from(state.radio_history.clone()),
        }
    }
}

impl PlayerRegistry {
    /// The current track's status as the audio worker reports it, or `None`
    /// when nothing is playing or the worker did not answer.
    pub async fn track_status(&self, guild_id: GuildId) -> Option<TrackStatus> {
        let handle = {
            let guilds = self.guilds.lock().await;
            guilds
                .get(&guild_id)
                .and_then(|state| state.current_handle.clone())
        }?;
        handle.status().await
    }

    /// Whether the current track is paused, or `None` when nothing is
    /// playing. Answered from the registry's own flag so a slow worker
    /// cannot make a paused guild read as playing.
    pub async fn is_paused(&self, guild_id: GuildId) -> Option<bool> {
        let guilds = self.guilds.lock().await;
        let state = guilds.get(&guild_id)?;
        state.current_track_id.map(|_| state.paused)
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
        let mut queued = db::queue_all(&self.db, &guild_id.to_string())
            .await
            .unwrap_or_else(|err| {
                tracing::warn!(%guild_id, %err, "failed to load queue snapshot");
                Vec::new()
            });
        // The head of the stored queue is the current track itself, so it
        // belongs to `now_playing`/`loading` rather than to the upcoming
        // list the dashboard renders behind them.
        let upcoming = if (now_playing.is_some() || loading.is_some()) && !queued.is_empty() {
            queued.split_off(1)
        } else {
            queued
        };
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
}
