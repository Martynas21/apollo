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
use std::time::Duration;

use apollo_ipc::proto::{Envelope, Event as IpcEvent, Request, Response};
use apollo_ipc::{ConnectionInfoDto, read_frame, write_frame};
use async_trait::async_trait;
use poise::serenity_prelude as serenity;
use serenity::{ChannelId, GuildId};
use songbird::Songbird;
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::{Mutex, oneshot};

use crate::voice::player::{
    AudioSource, TrackStatus, VoiceBackend, VoiceCall, VoiceEvents, VoiceTrack,
};
use crate::voice::resolve::{self, PlaybackError};
use crate::youtube::api::Track;

type PendingMap = StdMutex<HashMap<u64, oneshot::Sender<Result<Response, String>>>>;
type EventsMap = StdMutex<HashMap<GuildId, Arc<dyn VoiceEvents>>>;

/// How many times [`Connection::reconnect`] retries `UnixStream::connect`
/// before giving up. Just enough to ride out `apollo-audio-worker`
/// restarting under docker/systemd (a few seconds) — this is a hobby
/// project, not a service with an SLA, so no exponential backoff/jitter.
const RECONNECT_ATTEMPTS: u32 = 5;
const RECONNECT_DELAY: Duration = Duration::from_millis(500);

struct Connection {
    write: Mutex<OwnedWriteHalf>,
    pending: PendingMap,
    guild_events: EventsMap,
    next_id: AtomicU64,
    socket_path: PathBuf,
    /// Bumped every time [`Connection::reconnect`] successfully replaces
    /// `write`. Lets a caller that saw a given connection fail tell whether
    /// someone else already fixed it before it gets its turn at
    /// `reconnecting`'s lock, and lets a stale [`run_reader`] task recognise
    /// that it's reading a connection that's already been superseded.
    epoch: AtomicU64,
    /// Serializes reconnect attempts so concurrent callers hitting a broken
    /// connection at once don't all dial the worker and spawn duplicate
    /// reader tasks.
    reconnecting: Mutex<()>,
}

type PendingRx = oneshot::Receiver<Result<Response, String>>;

/// The result of a successful [`Connection::reconnect`] call.
enum Reconnected {
    /// This call actually re-dialed the worker; the caller owns the new read
    /// half and is responsible for spawning a [`run_reader`] on it.
    New(tokio::net::unix::OwnedReadHalf, u64),
    /// Another caller already reconnected (and spawned its own reader)
    /// while this one waited for `reconnecting`'s lock — nothing further to
    /// do here.
    AlreadyDone,
}

impl Connection {
    /// Writes `body` as a new request and returns a receiver for its
    /// eventual response, without waiting for it. The write itself still
    /// happens inline (under `self.write`'s lock) before this returns, so a
    /// caller that needs the *next* request to reach the worker only after
    /// this one — e.g. `IpcCall::play` immediately followed by a
    /// `set_volume` on the resulting handle — can rely on write order being
    /// preserved even without awaiting this request's response first.
    ///
    /// A broken write triggers one reconnect-and-retry before giving up —
    /// see [`Self::reconnect`].
    async fn send(self: &Arc<Self>, body: Request) -> Result<PendingRx, String> {
        let epoch = self.epoch.load(Ordering::Acquire);
        match self.try_send(body.clone()).await {
            Ok(rx) => Ok(rx),
            Err(e) => {
                tracing::warn!(%e, "IPC write failed, attempting to reconnect");
                if let Reconnected::New(read_half, new_epoch) = self.reconnect(epoch).await? {
                    tokio::spawn(run_reader(read_half, self.clone(), new_epoch));
                }
                self.try_send(body).await
            }
        }
    }

    async fn try_send(self: &Arc<Self>, body: Request) -> Result<PendingRx, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, tx);

        let envelope = Envelope::Request { id, body };
        let mut write = self.write.lock().await;
        if let Err(e) = write_frame(&mut *write, &envelope).await {
            drop(write);
            self.pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&id);
            return Err(format!("IPC write failed: {e}"));
        }
        Ok(rx)
    }

    async fn request(self: &Arc<Self>, body: Request) -> Result<Response, String> {
        self.send(body)
            .await?
            .await
            .map_err(|_| "IPC connection closed before a response arrived".to_string())?
    }

    /// Re-dials `socket_path`, replacing `write` on success and handing the
    /// new read half back to the caller (which is responsible for spawning
    /// a fresh [`run_reader`] on it — this fn deliberately doesn't do that
    /// itself, so it never has to know about `run_reader`'s type). `seen_epoch`
    /// is the epoch the caller observed fail; if it no longer matches
    /// `self.epoch` by the time this gets `reconnecting`'s lock, some other
    /// caller already reconnected while this one was waiting.
    async fn reconnect(self: &Arc<Self>, seen_epoch: u64) -> Result<Reconnected, String> {
        let _guard = self.reconnecting.lock().await;
        if self.epoch.load(Ordering::Acquire) != seen_epoch {
            return Ok(Reconnected::AlreadyDone);
        }

        for attempt in 1..=RECONNECT_ATTEMPTS {
            match UnixStream::connect(&self.socket_path).await {
                Ok(stream) => {
                    let (read_half, write_half) = stream.into_split();
                    *self.write.lock().await = write_half;
                    let new_epoch = self.epoch.fetch_add(1, Ordering::AcqRel) + 1;
                    tracing::info!("reconnected to apollo-audio-worker");
                    return Ok(Reconnected::New(read_half, new_epoch));
                }
                Err(e) => {
                    tracing::warn!(attempt, %e, "failed to reconnect to apollo-audio-worker");
                    if attempt < RECONNECT_ATTEMPTS {
                        tokio::time::sleep(RECONNECT_DELAY).await;
                    }
                }
            }
        }
        Err("could not reconnect to apollo-audio-worker".to_string())
    }
}

/// Reads frames off `stream`'s read half for as long as the connection
/// lives, resolving pending requests and dispatching events to whichever
/// guild's [`VoiceEvents`] registered for them. `epoch` is the connection
/// generation this reader was spawned for (see [`Connection::reconnect`]).
///
/// A dead connection isn't necessarily the end: this tries
/// [`Connection::reconnect`] before giving up. Only once that's exhausted
/// its retries do guilds with a registration get told the connection is
/// lost (mirroring how a real songbird `DriverDisconnect` is handled) and
/// in-flight requests get unblocked with an error.
async fn run_reader(
    mut read_half: tokio::net::unix::OwnedReadHalf,
    connection: Arc<Connection>,
    mut epoch: u64,
) {
    // The outer loop re-enters the read loop on a freshly reconnected
    // stream; a plain (non-recursive) loop here, rather than this fn calling
    // itself, keeps its future's type from being self-referential.
    loop {
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
                    if let Some(tx) = connection
                        .pending
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .remove(&id)
                    {
                        let _ = tx.send(body);
                    }
                }
                Envelope::Event(event) => dispatch_event(&connection, event),
                Envelope::Request { .. } => {
                    tracing::warn!("ignoring unexpected request envelope from audio worker");
                }
            }
        }

        // If some other task already reconnected (bumping the epoch) since
        // this loop last started, that reconnect's own reader is the live
        // one now — this one just observed the old, now-superseded stream
        // close and has nothing further to do.
        if connection.epoch.load(Ordering::Acquire) != epoch {
            return;
        }
        match connection.reconnect(epoch).await {
            Ok(Reconnected::New(new_read_half, new_epoch)) => {
                read_half = new_read_half;
                epoch = new_epoch;
                continue;
            }
            Ok(Reconnected::AlreadyDone) => return,
            Err(_) => break,
        }
    }

    handle_reader_exit(&connection);
}

// Reconnecting is exhausted — the worker is really gone. Every guild that
// had a registration needs to be told, the same way a real
// `DriverDisconnectHandler` would report a lost voice connection. Any
// request still awaiting a response also needs to be unblocked with an
// error rather than hanging forever.
fn handle_reader_exit(connection: &Connection) {
    let guild_events: Vec<(GuildId, Arc<dyn VoiceEvents>)> = {
        let mut map = connection
            .guild_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.drain().collect()
    };
    for (guild_id, events) in guild_events {
        tokio::spawn(async move {
            events.connection_lost(guild_id).await;
        });
    }
    let pending: Vec<_> = connection
        .pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .drain()
        .collect();
    for (_, tx) in pending {
        let _ = tx.send(Err("apollo-audio-worker connection closed".to_string()));
    }
}

fn dispatch_event(connection: &Connection, event: IpcEvent) {
    match event {
        IpcEvent::TrackFinished { guild_id, track_id } => {
            notify_finished(connection, guild_id, track_id)
        }
        IpcEvent::TrackErrored {
            guild_id,
            track_id,
            error,
        } => {
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
            events
                .track_finished(GuildId::new(guild_id), track_id)
                .await;
        });
    }
}

fn lookup(connection: &Connection, guild_id: u64) -> Option<Arc<dyn VoiceEvents>> {
    connection
        .guild_events
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&GuildId::new(guild_id))
        .cloned()
}

pub struct IpcBackend {
    songbird: Arc<Songbird>,
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
        cookies_file: Option<String>,
    ) -> std::io::Result<Self> {
        let stream = UnixStream::connect(socket_path).await?;
        let (read_half, write_half) = stream.into_split();
        let connection = Arc::new(Connection {
            write: Mutex::new(write_half),
            pending: StdMutex::new(HashMap::new()),
            guild_events: StdMutex::new(HashMap::new()),
            next_id: AtomicU64::new(0),
            socket_path: PathBuf::from(socket_path),
            epoch: AtomicU64::new(0),
            reconnecting: Mutex::new(()),
        });
        tokio::spawn(run_reader(read_half, connection.clone(), 0));
        Ok(Self {
            songbird,
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
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(guild_id, events);

        let dto = ConnectionInfoDto {
            guild_id: guild_id.get(),
            channel_id: channel_id.get(),
            endpoint: info.endpoint,
            session_id: info.session_id,
            token: info.token,
            user_id: info.user_id.0.get(),
        };
        let result = self
            .connection
            .request(Request::Join {
                guild_id: guild_id.get(),
                info: dto,
            })
            .await
            .and_then(|response| match response {
                Response::Ok => Ok(()),
                other => Err(format!("unexpected response to Join: {other:?}")),
            });

        if let Err(err) = result {
            // songbird now has a `Call` for `guild_id` with nothing on the
            // worker side to back it — `is_connected` would otherwise keep
            // reporting this guild as connected. Undo the gateway join so
            // songbird's state can't diverge from the worker's.
            self.connection
                .guild_events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&guild_id);
            if let Err(cleanup_err) = self.songbird.remove(guild_id).await {
                tracing::warn!(%guild_id, %cleanup_err, "failed to undo songbird join after a failed IPC Join");
            }
            return Err(err);
        }
        Ok(())
    }

    async fn remove(&self, guild_id: GuildId) -> Result<(), String> {
        self.connection
            .guild_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&guild_id);
        // Best-effort: the worker may already be gone (which is exactly why
        // this guild is being removed), so a failure here doesn't block
        // tearing down the gateway side below.
        if let Err(err) = self
            .connection
            .request(Request::Leave {
                guild_id: guild_id.get(),
            })
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
    async fn play(&self, source: AudioSource) -> Result<Arc<dyn VoiceTrack>, String> {
        let track_id = uuid::Uuid::new_v4();
        let audio_path = source.path.to_string_lossy().into_owned();
        let request = Request::Play {
            guild_id: self.guild_id.get(),
            track_id,
            audio_path,
        };
        tracing::debug!(video_id = %source.video_id, %track_id, "starting track playback");
        match self.connection.request(request).await {
            Ok(Response::Ok) => Ok(Arc::new(IpcTrack {
                guild_id: self.guild_id,
                track_id,
                connection: self.connection.clone(),
            })),
            Ok(other) => Err(format!("unexpected response to Play: {other:?}")),
            Err(err) => Err(err),
        }
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

    async fn set_volume(&self, multiplier: f32) -> Result<(), String> {
        self.request(Request::SetVolume {
            guild_id: self.guild_id.get(),
            track_id: self.track_id,
            multiplier,
        })
        .await
    }

    async fn stop(&self) -> Result<(), String> {
        self.request(Request::Stop {
            guild_id: self.guild_id.get(),
            track_id: self.track_id,
        })
        .await
    }

    async fn pause(&self) -> Result<(), String> {
        self.request(Request::Pause {
            guild_id: self.guild_id.get(),
            track_id: self.track_id,
        })
        .await
    }

    async fn resume(&self) -> Result<(), String> {
        self.request(Request::Resume {
            guild_id: self.guild_id.get(),
            track_id: self.track_id,
        })
        .await
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
    /// Sends `request` and maps a bare `Ok` response to success — the shape
    /// every `stop`/`pause`/`resume`/`set_volume` request expects back.
    async fn request(&self, request: Request) -> Result<(), String> {
        match self.connection.request(request).await? {
            Response::Ok => Ok(()),
            other => Err(format!("unexpected response: {other:?}")),
        }
    }
}
