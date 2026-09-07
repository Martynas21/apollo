use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use apollo_ipc::proto::{Envelope, Event as IpcEvent, Request, Response};
use apollo_ipc::{ConnectionInfoDto, read_frame, write_frame};
use async_trait::async_trait;
use poise::serenity_prelude as serenity;
use serenity::{ChannelId, GuildId};
use songbird::Songbird;
use tokio::net::TcpStream;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::sync::{Mutex, oneshot};

use crate::voice::player::{
    AudioSource, TrackStatus, VoiceBackend, VoiceCall, VoiceEvents, VoiceTrack,
};
use crate::voice::resolve::{self, PlaybackError};
use crate::youtube::api::Track;

type PendingMap = StdMutex<HashMap<u64, oneshot::Sender<Result<Response, String>>>>;
type EventsMap = StdMutex<HashMap<GuildId, Arc<dyn VoiceEvents>>>;

const RECONNECT_ATTEMPTS: u32 = 5;
const RECONNECT_DELAY: Duration = Duration::from_millis(500);

struct Connection {
    write: Mutex<OwnedWriteHalf>,
    pending: PendingMap,
    guild_events: EventsMap,
    next_id: AtomicU64,
    socket_addr: String,
    epoch: AtomicU64,
    reconnecting: Mutex<()>,
}

type PendingRx = oneshot::Receiver<Result<Response, String>>;

enum Reconnected {
    New(tokio::net::tcp::OwnedReadHalf, u64),
    AlreadyDone,
}

impl Connection {
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

    async fn reconnect(self: &Arc<Self>, seen_epoch: u64) -> Result<Reconnected, String> {
        let _guard = self.reconnecting.lock().await;
        if self.epoch.load(Ordering::Acquire) != seen_epoch {
            return Ok(Reconnected::AlreadyDone);
        }

        for attempt in 1..=RECONNECT_ATTEMPTS {
            match TcpStream::connect(&self.socket_addr).await {
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

async fn run_reader(
    mut read_half: tokio::net::tcp::OwnedReadHalf,
    connection: Arc<Connection>,
    mut epoch: u64,
) {
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
    connection: Arc<Connection>,
}

impl IpcBackend {
    pub async fn connect(
        socket_addr: &str,
        songbird: Arc<Songbird>,
        cookies_file: Option<String>,
    ) -> std::io::Result<Self> {
        let stream = TcpStream::connect(socket_addr).await?;
        let (read_half, write_half) = stream.into_split();
        let connection = Arc::new(Connection {
            write: Mutex::new(write_half),
            pending: StdMutex::new(HashMap::new()),
            guild_events: StdMutex::new(HashMap::new()),
            next_id: AtomicU64::new(0),
            socket_addr: socket_addr.to_string(),
            epoch: AtomicU64::new(0),
            reconnecting: Mutex::new(()),
        });
        tokio::spawn(run_reader(read_half, connection.clone(), 0));
        Ok(Self {
            songbird,
            cookies_file,
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

    async fn current_channel(&self, guild_id: GuildId) -> Option<ChannelId> {
        let call = self.songbird.get(guild_id)?;
        let call = call.lock().await;
        let channel = call.current_channel()?;
        Some(ChannelId::new(channel.0.get()))
    }

    async fn buffered_source(&self, track: &Track) -> Result<AudioSource, PlaybackError> {
        let resolved = resolve::resolve_stream(
            &track.video_id,
            track.duration,
            self.cookies_file.as_deref(),
        )
        .await?;
        Ok(AudioSource {
            video_id: track.video_id.clone(),
            url: resolved.url,
            headers: resolved.headers,
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
        let request = Request::Play {
            guild_id: self.guild_id.get(),
            track_id,
            stream_url: source.url.clone(),
            headers: source.headers.clone(),
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

    fn notify_when_finished(&self, _guild_id: GuildId, _events: Arc<dyn VoiceEvents>) {}

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
    async fn request(&self, request: Request) -> Result<(), String> {
        match self.connection.request(request).await? {
            Response::Ok => Ok(()),
            other => Err(format!("unexpected response: {other:?}")),
        }
    }
}
