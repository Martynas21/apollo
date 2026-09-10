use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use serenity::all::{GuildId, UserId};
use tokio::sync::Notify;
use uuid::Uuid;

use crate::db;
use crate::model::QueuedTrack;
use crate::voice::backend::VoiceTrack;
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
    pub(super) now_playing: Option<QueuedTrack>,
    pub(super) last_played: Option<QueuedTrack>,
    pub(super) radio_enabled: bool,
    pub(super) radio_requested_by: Option<UserId>,
    pub(super) radio_history: Vec<String>,
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

impl PlayerRegistry {
    pub async fn is_paused(&self, guild_id: GuildId) -> Option<bool> {
        let handle = {
            let guilds = self.guilds.lock().await;
            guilds
                .get(&guild_id)
                .and_then(|state| state.current_handle.clone())
        }?;
        handle.status().await.map(|status| status.paused)
    }

    pub async fn track_position(&self, guild_id: GuildId) -> Option<Duration> {
        let handle = {
            let guilds = self.guilds.lock().await;
            guilds
                .get(&guild_id)
                .and_then(|state| state.current_handle.clone())
        }?;
        handle.status().await.map(|status| status.position)
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
}
