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

fn delete_audio_file(path: &str) {
    if let Err(err) = std::fs::remove_file(path) {
        tracing::warn!(%err, %path, "failed to delete buffered audio file");
    }
}

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
        let old = self
            .guilds
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                guild_id,
                GuildSession {
                    driver,
                    current: None,
                },
            );
        if let Some(mut old) = old {
            old.driver.leave();
        }
        Ok(())
    }

    pub fn leave(&self, guild_id: u64) {
        if let Some(mut session) = self
            .guilds
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&guild_id)
        {
            session.driver.leave();
        }
    }

    pub fn leave_all(&self) {
        let mut guilds = self
            .guilds
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (_, mut session) in guilds.drain() {
            session.driver.leave();
        }
    }

    pub fn play(&self, guild_id: u64, track_id: Uuid, audio_path: String) -> Result<(), String> {
        let mut guilds = self
            .guilds
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        if let Err(err) = handle.add_event(Event::Track(TrackEvent::End), end_handler.clone()) {
            tracing::warn!(%err, "failed to register track-end handler");
        }
        if let Err(err) = handle.add_event(Event::Track(TrackEvent::Error), end_handler) {
            tracing::warn!(%err, "failed to register track-error handler");
        }
        session.current = Some((track_id, handle));
        Ok(())
    }

    fn current_handle(&self, guild_id: u64, track_id: Uuid) -> Result<TrackHandle, String> {
        let guilds = self
            .guilds
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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

        assert!(sessions.pause(1, current_id).is_ok());
    }

    #[tokio::test]
    async fn leave_and_leave_all_are_no_ops_on_an_unknown_guild() {
        let sessions = sessions();
        sessions.leave(1);
        sessions.leave_all();
    }
}
