//! [`VoiceBackend`] implementation talking to `apollo-audio-worker` over a
//! Unix domain socket, instead of driving songbird's mixer in this process.
//!
//! This process still does the Discord voice *gateway* choreography itself
//! (`Songbird::join_gateway`, still requiring the full `songbird` gateway
//! feature set built in `main.rs`) — only the actual `Driver`/mixer moves to
//! the worker. `join_gateway` hands back a `ConnectionInfo` (session/token/
//! endpoint) without starting a local driver; that's shipped to the worker,
//! which opens the real voice UDP connection itself.
//!
//! One connection is held open for this process's whole lifetime. Requests
//! are correlated to responses by an id; a background reader task dispatches
//! incoming `Response`s to whichever caller is waiting and incoming `Event`s
//! to whichever guild's registered [`VoiceEvents`] they belong to.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use apollo_ipc::proto::{Envelope, Event as IpcEvent, Request, Response};
use apollo_ipc::{read_frame, write_frame, ConnectionInfoDto};
use async_trait::async_trait;
use poise::serenity_prelude as serenity;
use serenity::{ChannelId, GuildId};
use songbird::Songbird;
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::UnixStream;
use tokio::sync::{oneshot, Mutex};

use crate::voice::player::{AudioSource, TrackStatus, VoiceBackend, VoiceCall, VoiceEvents, VoiceTrack};
use crate::voice::resolve::{self, PlaybackError};
use crate::youtube::api::Track;

type PendingMap = StdMutex<HashMap<u64, oneshot::Sender<Result<Response, String>>>>;
type EventsMap = StdMutex<HashMap<GuildId, Arc<dyn VoiceEvents>>>;

struct Connection {
    write: Mutex<OwnedWriteHalf>,
    pending: PendingMap,
    guild_events: EventsMap,
    next_id: AtomicU64,
}

type PendingRx = oneshot::Receiver<Result<Response, String>>;

impl Connection {
    /// Writes `body` as a new request and returns a receiver for its
    /// eventual response, without waiting for it. The write itself still
    /// happens inline (under `self.write`'s lock) before this returns, so a
    /// caller that needs the *next* request to reach the worker only after
    /// this one — e.g. `IpcCall::play` immediately followed by a
    /// `set_volume` on the resulting handle — can rely on write order being
    /// preserved even without awaiting this request's response first.
    async fn send(&self, body: Request) -> Result<PendingRx, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);

        let envelope = Envelope::Request { id, body };
        let mut write = self.write.lock().await;
        if let Err(e) = write_frame(&mut *write, &envelope).await {
            drop(write);
            self.pending.lock().unwrap().remove(&id);
            return Err(format!("IPC write failed: {e}"));
        }
        Ok(rx)
    }

    async fn request(&self, body: Request) -> Result<Response, String> {
        self.send(body)
            .await?
            .await
            .map_err(|_| "IPC connection closed before a response arrived".to_string())?
    }
}

/// Reads frames off `stream`'s read half for as long as the connection
/// lives, resolving pending requests and dispatching events to whichever
/// guild's [`VoiceEvents`] registered for them. Runs for the whole process
/// lifetime — if the worker connection drops, every guild with a
/// registration gets told its connection is lost, mirroring how a real
/// songbird `DriverDisconnect` is handled.
async fn run_reader(
    mut read_half: tokio::net::unix::OwnedReadHalf,
    connection: Arc<Connection>,
) {
    loop {
        let envelope = match read_frame(&mut read_half).await {
            Ok(Some(envelope)) => envelope,
            Ok(None) => {
                tracing::error!("apollo-audio-worker connection closed");
                break;
            }
            Err(e) => {
                tracing::error!("apollo-audio-worker IPC read error: {e}");
                break;
            }
        };
        match envelope {
            Envelope::Response { id, body } => {
                if let Some(tx) = connection.pending.lock().unwrap().remove(&id) {
                    let _ = tx.send(body);
                }
            }
            Envelope::Event(event) => dispatch_event(&connection, event),
            Envelope::Request { .. } => {
                tracing::warn!("ignoring unexpected request envelope from audio worker");
            }
        }
    }

    // The worker is gone (or the connection otherwise died) — every guild
    // that had a registration needs to be told, the same way a real
    // `DriverDisconnectHandler` would report a lost voice connection. Any
    // request still awaiting a response also needs to be unblocked with an
    // error rather than hanging forever.
    let guild_events: Vec<(GuildId, Arc<dyn VoiceEvents>)> = {
        let mut map = connection.guild_events.lock().unwrap();
        map.drain().collect()
    };
    for (guild_id, events) in guild_events {
        tokio::spawn(async move {
            events.connection_lost(guild_id).await;
        });
    }
    let pending: Vec<_> = connection.pending.lock().unwrap().drain().collect();
    for (_, tx) in pending {
        let _ = tx.send(Err("apollo-audio-worker connection closed".to_string()));
    }
}

fn dispatch_event(connection: &Connection, event: IpcEvent) {
    match event {
        IpcEvent::TrackFinished { guild_id, track_id } => notify_finished(connection, guild_id, track_id),
        IpcEvent::TrackErrored { guild_id, track_id, error } => {
            tracing::warn!(%error, "worker reported a track error");
            notify_finished(connection, guild_id, track_id);
        }
        IpcEvent::ConnectionLost { guild_id } => {
            if let Some(events) = lookup(connection, guild_id) {
                tokio::spawn(async move {
                    events.connection_lost(GuildId::new(guild_id)).await;
                });
            }
        }
    }
}

fn notify_finished(connection: &Connection, guild_id: u64, track_id: uuid::Uuid) {
    if let Some(events) = lookup(connection, guild_id) {
        tokio::spawn(async move {
            events.track_finished(GuildId::new(guild_id), track_id).await;
        });
    }
}

fn lookup(connection: &Connection, guild_id: u64) -> Option<Arc<dyn VoiceEvents>> {
    connection
        .guild_events
        .lock()
        .unwrap()
        .get(&GuildId::new(guild_id))
        .cloned()
}

pub struct IpcBackend {
    songbird: Arc<Songbird>,
    http: reqwest::Client,
    cookies_file: Option<String>,
    buffer_dir: PathBuf,
    connection: Arc<Connection>,
}

impl IpcBackend {
    /// Connects to `apollo-audio-worker`'s Unix domain socket at
    /// `socket_path`. `buffer_dir` must be the same shared volume the worker
    /// reads pre-buffered tracks from (`AUDIO_BUFFER_DIR`).
    pub async fn connect(
        socket_path: &str,
        buffer_dir: PathBuf,
        songbird: Arc<Songbird>,
        http: reqwest::Client,
        cookies_file: Option<String>,
    ) -> std::io::Result<Self> {
        let stream = UnixStream::connect(socket_path).await?;
        let (read_half, write_half) = stream.into_split();
        let connection = Arc::new(Connection {
            write: Mutex::new(write_half),
            pending: StdMutex::new(HashMap::new()),
            guild_events: StdMutex::new(HashMap::new()),
            next_id: AtomicU64::new(0),
        });
        tokio::spawn(run_reader(read_half, connection.clone()));
        Ok(Self {
            songbird,
            http,
            cookies_file,
            buffer_dir,
            connection,
        })
    }
}

#[async_trait]
impl VoiceBackend for IpcBackend {
    async fn join(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        events: Arc<dyn VoiceEvents>,
    ) -> Result<(), String> {
        let (info, _call) = self
            .songbird
            .join_gateway(guild_id, channel_id)
            .await
            .map_err(|e| e.to_string())?;

        self.connection
            .guild_events
            .lock()
            .unwrap()
            .insert(guild_id, events);

        let dto = ConnectionInfoDto {
            guild_id: guild_id.get(),
            channel_id: channel_id.get(),
            endpoint: info.endpoint,
            session_id: info.session_id,
            token: info.token,
            user_id: info.user_id.0.get(),
        };
        match self
            .connection
            .request(Request::Join { guild_id: guild_id.get(), info: dto })
            .await?
        {
            Response::Ok => Ok(()),
            other => Err(format!("unexpected response to Join: {other:?}")),
        }
    }

    async fn remove(&self, guild_id: GuildId) -> Result<(), String> {
        self.connection
            .guild_events
            .lock()
            .unwrap()
            .remove(&guild_id);
        // Best-effort: the worker may already be gone (which is exactly why
        // this guild is being removed), so a failure here doesn't block
        // tearing down the gateway side below.
        if let Err(err) = self
            .connection
            .request(Request::Leave { guild_id: guild_id.get() })
            .await
        {
            tracing::debug!(%guild_id, %err, "worker leave request failed (may already be gone)");
        }
        self.songbird
            .remove(guild_id)
            .await
            .map_err(|e| e.to_string())
    }

    fn call(&self, guild_id: GuildId) -> Option<Arc<dyn VoiceCall>> {
        self.songbird.get(guild_id)?;
        Some(Arc::new(IpcCall {
            guild_id,
            connection: self.connection.clone(),
        }) as Arc<dyn VoiceCall>)
    }

    async fn buffered_source(&self, track: &Track) -> Result<AudioSource, PlaybackError> {
        let path = resolve::buffer_track_to_file(
            self.http.clone(),
            &track.video_id,
            track.duration,
            self.cookies_file.as_deref(),
            &self.buffer_dir,
        )
        .await?;
        Ok(AudioSource {
            video_id: track.video_id.clone(),
            path,
        })
    }
}

struct IpcCall {
    guild_id: GuildId,
    connection: Arc<Connection>,
}

#[async_trait]
impl VoiceCall for IpcCall {
    async fn play(&self, source: AudioSource) -> Arc<dyn VoiceTrack> {
        let track_id = uuid::Uuid::new_v4();
        let audio_path = source.path.to_string_lossy().into_owned();
        let request = Request::Play {
            guild_id: self.guild_id.get(),
            track_id,
            audio_path,
        };
        tracing::debug!(video_id = %source.video_id, %track_id, "starting track playback");
        // `send` (not `request`) so this doesn't block on the worker's round
        // trip: the write itself still happens inline above, before this
        // returns, so a caller that immediately does something else with the
        // resulting handle (e.g. `start_playback` calling `set_volume` right
        // after) still reaches the worker in the same order.
        match self.connection.send(request).await {
            Ok(response) => {
                let video_id = source.video_id;
                tokio::spawn(async move {
                    match response.await {
                        Ok(Err(err)) => {
                            tracing::warn!(%video_id, %err, "audio worker rejected playback request");
                        }
                        Err(_) => {
                            tracing::warn!(%video_id, "audio worker connection closed before playback was confirmed");
                        }
                        Ok(Ok(_)) => {}
                    }
                });
            }
            Err(err) => {
                tracing::warn!(video_id = %source.video_id, %err, "failed to start playback on audio worker");
            }
        }
        Arc::new(IpcTrack {
            guild_id: self.guild_id,
            track_id,
            connection: self.connection.clone(),
        })
    }
}

struct IpcTrack {
    guild_id: GuildId,
    track_id: uuid::Uuid,
    connection: Arc<Connection>,
}

#[async_trait]
impl VoiceTrack for IpcTrack {
    fn uuid(&self) -> uuid::Uuid {
        self.track_id
    }

    fn set_volume(&self, multiplier: f32) -> Result<(), String> {
        self.spawn_request(Request::SetVolume {
            guild_id: self.guild_id.get(),
            track_id: self.track_id,
            multiplier,
        });
        Ok(())
    }

    fn stop(&self) -> Result<(), String> {
        self.spawn_request(Request::Stop {
            guild_id: self.guild_id.get(),
            track_id: self.track_id,
        });
        Ok(())
    }

    fn pause(&self) -> Result<(), String> {
        self.spawn_request(Request::Pause {
            guild_id: self.guild_id.get(),
            track_id: self.track_id,
        });
        Ok(())
    }

    fn resume(&self) -> Result<(), String> {
        self.spawn_request(Request::Resume {
            guild_id: self.guild_id.get(),
            track_id: self.track_id,
        });
        Ok(())
    }

    fn notify_when_finished(&self, _guild_id: GuildId, _events: Arc<dyn VoiceEvents>) {
        // No-op: `apollo-audio-worker` reports `TrackFinished`/`TrackErrored`
        // for every track it plays without a separate opt-in, and
        // `dispatch_event` in this module already routes those to whichever
        // `VoiceEvents` `join` registered for this guild. Unlike songbird's
        // per-track handle, there's nothing further to register here.
    }

    async fn status(&self) -> Option<TrackStatus> {
        match self
            .connection
            .request(Request::Status {
                guild_id: self.guild_id.get(),
                track_id: self.track_id,
            })
            .await
        {
            Ok(Response::Status(status)) => Some(TrackStatus {
                position: std::time::Duration::from_millis(status.position_ms),
                paused: status.paused,
            }),
            Ok(Response::Ok) | Err(_) => None,
        }
    }
}

impl IpcTrack {
    /// Fire-and-forget: `stop`/`pause`/`resume`/`set_volume` are documented
    /// as best-effort by [`VoiceTrack`] (the caller doesn't block on
    /// confirmation from songbird either, in the direct-driver design this
    /// replaced), so a failure here is logged rather than propagated.
    fn spawn_request(&self, request: Request) {
        let connection = self.connection.clone();
        tokio::spawn(async move {
            if let Err(err) = connection.request(request).await {
                tracing::warn!(%err, "audio worker request failed");
            }
        });
    }
}
