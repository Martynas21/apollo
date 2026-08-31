//! Per-guild `songbird::Driver` state — the only "business logic" this
//! worker has. Everything else (queueing, DB, panels) stays in `apollo`.

use std::collections::HashMap;
use std::num::NonZeroU64;
use std::sync::Mutex;

use apollo_ipc::dto::{ConnectionInfoDto, TrackStatusDto};
use apollo_ipc::proto::Event as IpcEvent;
use async_trait::async_trait;
use songbird::id::{ChannelId, GuildId, UserId};
use songbird::input::File as SongbirdFile;
use songbird::tracks::{PlayMode, TrackHandle};
use songbird::{
    Config, ConnectionInfo, CoreEvent, Driver, Event, EventContext,
    EventHandler as SongbirdEventHandler, TrackEvent,
};
use tokio::sync::mpsc::UnboundedSender;
use uuid::Uuid;

struct GuildSession {
    driver: Driver,
    current: Option<(Uuid, TrackHandle)>,
}

pub struct Sessions {
    guilds: Mutex<HashMap<u64, GuildSession>>,
    events: UnboundedSender<IpcEvent>,
}

fn nonzero(id: u64, what: &str) -> Result<NonZeroU64, String> {
    NonZeroU64::new(id).ok_or_else(|| format!("{what} must not be zero"))
}

fn to_connection_info(dto: ConnectionInfoDto) -> Result<ConnectionInfo, String> {
    Ok(ConnectionInfo {
        channel_id: ChannelId(nonzero(dto.channel_id, "channel_id")?),
        guild_id: GuildId(nonzero(dto.guild_id, "guild_id")?),
        endpoint: dto.endpoint,
        session_id: dto.session_id,
        token: dto.token,
        user_id: UserId(nonzero(dto.user_id, "user_id")?),
    })
}

/// Relays a track's End/Error events back over IPC — direct analog of
/// apollo's own `TrackEndHandler` (`src/voice/player.rs`), just relocated
/// into this process. Both End and Error are registered (not just End) for
/// the same reason apollo's does: a track whose input fails mid-stream goes
/// to `PlayMode::Errored` without ever firing `End` in songbird 0.6 — the
/// same handler instance (cloned) is registered for both.
#[derive(Clone)]
struct TrackEndHandler {
    guild_id: u64,
    track_id: Uuid,
    audio_path: String,
    events: UnboundedSender<IpcEvent>,
}

#[async_trait]
impl SongbirdEventHandler for TrackEndHandler {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        let errored = matches!(
            ctx,
            EventContext::Track(tracks)
                if tracks.first().is_some_and(|(state, _)| matches!(state.playing, PlayMode::Errored(_)))
        );
        let event = if errored {
            IpcEvent::TrackErrored {
                guild_id: self.guild_id,
                track_id: self.track_id,
                error: "track playback failed".to_string(),
            }
        } else {
            IpcEvent::TrackFinished {
                guild_id: self.guild_id,
                track_id: self.track_id,
            }
        };
        let _ = self.events.send(event);
        delete_audio_file(&self.audio_path);
        None
    }
}

/// The buffer file is on tmpfs (RAM-backed, see `compose.yaml`) and nothing
/// else ever deletes it — every path that ends a track's life must clean up
/// after itself here or it leaks for the life of the container.
fn delete_audio_file(path: &str) {
    if let Err(err) = std::fs::remove_file(path) {
        tracing::warn!(%err, %path, "failed to delete buffered audio file");
    }
}

/// Relays connection loss (kick, reconnect exhausted) — analog of apollo's
/// `DriverDisconnectHandler`. This is the only way to detect the connection
/// dying while nothing was actively playing.
struct DriverDisconnectHandler {
    guild_id: u64,
    events: UnboundedSender<IpcEvent>,
}

#[async_trait]
impl SongbirdEventHandler for DriverDisconnectHandler {
    async fn act(&self, _ctx: &EventContext<'_>) -> Option<Event> {
        let _ = self.events.send(IpcEvent::ConnectionLost {
            guild_id: self.guild_id,
        });
        None
    }
}

impl Sessions {
    pub fn new(events: UnboundedSender<IpcEvent>) -> Self {
        Self {
            guilds: Mutex::new(HashMap::new()),
            events,
        }
    }

    pub async fn join(&self, guild_id: u64, info: ConnectionInfoDto) -> Result<(), String> {
        let info = to_connection_info(info)?;
        let mut driver = Driver::new(Config::default());
        driver.connect(info).await.map_err(|e| e.to_string())?;
        driver.add_global_event(
            Event::Core(CoreEvent::DriverDisconnect),
            DriverDisconnectHandler {
                guild_id,
                events: self.events.clone(),
            },
        );
        let old = self.guilds.lock().unwrap().insert(
            guild_id,
            GuildSession {
                driver,
                current: None,
            },
        );
        // A re-issued Join for a guild that already has a session (e.g. a
        // second /play while one is already active) must not just drop the
        // old Driver — that would orphan its track with no End/Error ever
        // reaching apollo. Tear it down the same way an explicit leave does.
        if let Some(mut old) = old {
            old.driver.leave();
        }
        Ok(())
    }

    pub fn leave(&self, guild_id: u64) {
        if let Some(mut session) = self.guilds.lock().unwrap().remove(&guild_id) {
            session.driver.leave();
        }
    }

    /// Leaves every active session — used when the IPC connection to
    /// `apollo` drops, since nothing will ever consume further events or
    /// send further commands once that happens.
    pub fn leave_all(&self) {
        let mut guilds = self.guilds.lock().unwrap();
        for (_, mut session) in guilds.drain() {
            session.driver.leave();
        }
    }

    pub fn play(&self, guild_id: u64, track_id: Uuid, audio_path: String) -> Result<(), String> {
        let mut guilds = self.guilds.lock().unwrap();
        let Some(session) = guilds.get_mut(&guild_id) else {
            delete_audio_file(&audio_path);
            return Err(format!("no active session for guild {guild_id}"));
        };
        let input = SongbirdFile::new(audio_path.clone()).into();
        let handle = session.driver.play_input(input);
        let end_handler = TrackEndHandler {
            guild_id,
            track_id,
            audio_path,
            events: self.events.clone(),
        };
        // Registration failures are logged, not propagated: the track is
        // already playing by this point, so returning `Err` here would skip
        // `session.current` below and leave a live track this `Sessions`
        // can never look up again to pause/stop/clean up — the same
        // log-and-continue tradeoff apollo's own (pre-split)
        // `notify_when_finished` made.
        if let Err(err) = handle.add_event(Event::Track(TrackEvent::End), end_handler.clone()) {
            tracing::warn!(%err, "failed to register track-end handler");
        }
        if let Err(err) = handle.add_event(Event::Track(TrackEvent::Error), end_handler) {
            tracing::warn!(%err, "failed to register track-error handler");
        }
        session.current = Some((track_id, handle));
        Ok(())
    }

    /// The current track's handle for `guild_id`, if `track_id` is still
    /// that guild's current track — `Err` for an unknown guild or a
    /// stale/superseded `track_id`. Shared by every command below, sync
    /// (`with_current`) or async (`status`), that needs to reach the actual
    /// `TrackHandle`.
    fn current_handle(&self, guild_id: u64, track_id: Uuid) -> Result<TrackHandle, String> {
        let guilds = self.guilds.lock().unwrap();
        let session = guilds
            .get(&guild_id)
            .ok_or_else(|| format!("no active session for guild {guild_id}"))?;
        match &session.current {
            Some((current_id, handle)) if *current_id == track_id => Ok(handle.clone()),
            _ => Err(format!(
                "track {track_id} is not the current track for guild {guild_id}"
            )),
        }
    }

    fn with_current<T>(
        &self,
        guild_id: u64,
        track_id: Uuid,
        f: impl FnOnce(&TrackHandle) -> Result<T, String>,
    ) -> Result<T, String> {
        f(&self.current_handle(guild_id, track_id)?)
    }

    pub fn pause(&self, guild_id: u64, track_id: Uuid) -> Result<(), String> {
        self.with_current(guild_id, track_id, |h| h.pause().map_err(|e| e.to_string()))
    }

    pub fn resume(&self, guild_id: u64, track_id: Uuid) -> Result<(), String> {
        self.with_current(guild_id, track_id, |h| h.play().map_err(|e| e.to_string()))
    }

    pub fn stop(&self, guild_id: u64, track_id: Uuid) -> Result<(), String> {
        self.with_current(guild_id, track_id, |h| h.stop().map_err(|e| e.to_string()))
    }

    pub fn set_volume(&self, guild_id: u64, track_id: Uuid, multiplier: f32) -> Result<(), String> {
        self.with_current(guild_id, track_id, |h| {
            h.set_volume(multiplier).map_err(|e| e.to_string())
        })
    }

    #[cfg(test)]
    fn insert_for_test(&self, guild_id: u64, driver: Driver, current: Option<(Uuid, TrackHandle)>) {
        self.guilds
            .lock()
            .unwrap()
            .insert(guild_id, GuildSession { driver, current });
    }

    pub async fn status(&self, guild_id: u64, track_id: Uuid) -> Result<TrackStatusDto, String> {
        let handle = self.current_handle(guild_id, track_id)?;
        let state = handle.get_info().await.map_err(|e| e.to_string())?;
        Ok(TrackStatusDto {
            position_ms: u64::try_from(state.position.as_millis()).unwrap_or(u64::MAX),
            paused: matches!(state.playing, PlayMode::Pause),
        })
    }
}

#[cfg(test)]
mod tests {
    use songbird::input::Input;

    use super::*;

    fn sessions() -> Sessions {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        Sessions::new(tx)
    }

    /// A `Driver` with no live connection — safe to construct and play into
    /// off-network; only `.connect()` touches anything external.
    fn offline_driver_with_track() -> (Driver, TrackHandle) {
        let mut driver = Driver::new(Config::default());
        let handle = driver.play_input(Input::from(Vec::<u8>::new()));
        (driver, handle)
    }

    #[tokio::test]
    async fn pause_resume_stop_set_volume_reject_an_unknown_guild() {
        let sessions = sessions();
        let track_id = Uuid::new_v4();
        assert!(sessions.pause(1, track_id).is_err());
        assert!(sessions.resume(1, track_id).is_err());
        assert!(sessions.stop(1, track_id).is_err());
        assert!(sessions.set_volume(1, track_id, 0.5).is_err());
        assert!(sessions.status(1, track_id).await.is_err());
    }

    #[tokio::test]
    async fn play_rejects_a_guild_with_no_join() {
        let sessions = sessions();
        // A path that doesn't exist: `play`'s failure path tries to delete
        // it (see `delete_audio_file`), which must not panic when there's
        // nothing there to remove.
        assert!(
            sessions
                .play(
                    1,
                    Uuid::new_v4(),
                    "/nonexistent/apollo-test.audio".to_string()
                )
                .is_err()
        );
    }

    #[tokio::test]
    async fn commands_reject_a_stale_track_id() {
        let sessions = sessions();
        let (driver, handle) = offline_driver_with_track();
        let current_id = Uuid::new_v4();
        sessions.insert_for_test(1, driver, Some((current_id, handle)));

        let stale_id = Uuid::new_v4();
        assert!(sessions.pause(1, stale_id).is_err());
        assert!(sessions.stop(1, stale_id).is_err());

        // The real (non-stale) id is still accepted.
        assert!(sessions.pause(1, current_id).is_ok());
    }

    #[tokio::test]
    async fn leave_and_leave_all_are_no_ops_on_an_unknown_guild() {
        let sessions = sessions();
        sessions.leave(1);
        sessions.leave_all();
    }
}
