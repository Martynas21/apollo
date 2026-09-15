use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use apollo_ipc::proto::{Envelope, Event as IpcEvent, Request, Response};
use apollo_ipc::{ConnectionInfoDto, read_frame, write_frame};
use async_trait::async_trait;
use serenity::all::{self as serenity, ChannelId, GuildId};
use songbird::Songbird;
use tokio::net::TcpStream;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::model::Track;
use crate::voice::backend::{
    AudioSource, TrackStatus, VoiceBackend, VoiceCall, VoiceEvents, VoiceTrack,
};
use crate::voice::resolve::{self, PlaybackError};

type PendingMap = StdMutex<HashMap<u64, oneshot::Sender<Result<Response, String>>>>;
type EventsMap = StdMutex<HashMap<GuildId, Arc<dyn VoiceEvents>>>;

const RECONNECT_ATTEMPTS: u32 = 5;
const RECONNECT_DELAY: Duration = Duration::from_millis(500);
const INITIAL_CONNECT_RETRY_DELAY: Duration = Duration::from_secs(2);
const INITIAL_CONNECT_LOG_EVERY: u32 = 15;

/// How long any single request may wait for the worker's reply. The worker
/// answers every command from memory, so a reply that takes longer than this
/// means it is wedged, and the caller is told so instead of waiting forever.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// `Join` waits on the worker's own Discord voice handshake, so it gets more
/// room than a local command.
const JOIN_TIMEOUT: Duration = Duration::from_secs(30);

struct Connection {
    /// Hands frames to the writer task; swapped for a fresh one on every
    /// reconnect and for a closed one once the link has been given up on.
    outbound: StdMutex<mpsc::UnboundedSender<Envelope>>,
    pending: PendingMap,
    guild_events: EventsMap,
    next_id: AtomicU64,
    socket_addr: String,
    epoch: AtomicU64,
    reconnecting: Mutex<()>,
}

/// Removes a request's reply slot when the future waiting on it goes away,
/// whether it was answered, timed out, or was cancelled by its caller.
struct PendingSlot {
    connection: Arc<Connection>,
    id: u64,
}

impl Drop for PendingSlot {
    fn drop(&mut self) {
        self.connection
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.id);
    }
}

type PendingRx = oneshot::Receiver<Result<Response, String>>;

enum Reconnected {
    New(tokio::net::tcp::OwnedReadHalf, u64),
    AlreadyDone,
}

impl Connection {
    fn new(write_half: OwnedWriteHalf, socket_addr: String) -> Arc<Self> {
        Arc::new(Self {
            outbound: StdMutex::new(spawn_writer(write_half)),
            pending: StdMutex::new(HashMap::new()),
            guild_events: StdMutex::new(HashMap::new()),
            next_id: AtomicU64::new(0),
            socket_addr,
            epoch: AtomicU64::new(0),
            reconnecting: Mutex::new(()),
        })
    }

    async fn send(self: &Arc<Self>, body: Request) -> Result<(u64, PendingRx), String> {
        let epoch = self.epoch.load(Ordering::Acquire);
        match self.try_send(body.clone()) {
            Ok(sent) => Ok(sent),
            Err(e) => {
                tracing::warn!(%e, "IPC send failed, attempting to reconnect");
                if let Reconnected::New(read_half, new_epoch) = self.reconnect(epoch).await? {
                    tokio::spawn(run_reader(read_half, self.clone(), new_epoch));
                }
                self.try_send(body)
            }
        }
    }

    /// Queues the request for the writer task. Queuing neither blocks nor can
    /// be cancelled part-way, so a caller that stops waiting for the reply
    /// never leaves a torn frame on the link.
    fn try_send(&self, body: Request) -> Result<(u64, PendingRx), String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, tx);
        let queued = self
            .outbound
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .send(Envelope::Request { id, body });
        if queued.is_err() {
            self.pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&id);
            return Err("IPC link is down".to_string());
        }
        Ok((id, rx))
    }

    async fn request(self: &Arc<Self>, body: Request) -> Result<Response, String> {
        self.request_with_timeout(body, REQUEST_TIMEOUT).await
    }

    async fn request_with_timeout(
        self: &Arc<Self>,
        body: Request,
        timeout: Duration,
    ) -> Result<Response, String> {
        let (id, rx) = self.send(body).await?;
        let _slot = PendingSlot {
            connection: self.clone(),
            id,
        };
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(response)) => response,
            Ok(Err(_)) => Err("IPC connection closed before a response arrived".to_string()),
            Err(_elapsed) => Err(format!(
                "apollo-audio-worker did not answer within {timeout:?}"
            )),
        }
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
                    // Replies owed on the old link fail before the new link
                    // accepts requests, so nothing sent from here on can be
                    // failed as old traffic.
                    fail_pending(self);
                    self.set_outbound(spawn_writer(write_half));
                    let new_epoch = self.epoch.fetch_add(1, Ordering::AcqRel) + 1;
                    tracing::warn!(
                        "reconnected to apollo-audio-worker; it left every voice session when the old connection dropped, so all guilds are being disconnected"
                    );
                    disconnect_all_guilds(self);
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
        // Sends now fail at once instead of waiting out a reply timeout on a
        // socket nobody reads, and each one triggers a fresh attempt.
        let (closed, _never_read) = mpsc::unbounded_channel();
        self.set_outbound(closed);
        Err("could not reconnect to apollo-audio-worker".to_string())
    }

    fn set_outbound(&self, outbound: mpsc::UnboundedSender<Envelope>) {
        *self
            .outbound
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = outbound;
    }
}

/// Owns the socket's write half: frames go out one at a time, in order, on a
/// task nobody can cancel mid-frame. A failed write ends the task, which
/// closes the channel so the next send reports the link as down.
fn spawn_writer(mut write_half: OwnedWriteHalf) -> mpsc::UnboundedSender<Envelope> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Envelope>();
    tokio::spawn(async move {
        while let Some(envelope) = rx.recv().await {
            if let Err(e) = write_frame(&mut write_half, &envelope).await {
                tracing::warn!(%e, "IPC write failed");
                return;
            }
        }
    });
    tx
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

    drop_sessions(&connection);
}

/// Everything in flight on the old link is void: pending replies fail first
/// so nothing waits on them, then every guild is told its voice connection
/// is gone (any Leave they send goes out on the new link, if there is one).
fn drop_sessions(connection: &Connection) {
    fail_pending(connection);
    disconnect_all_guilds(connection);
}

/// The worker leaves every voice session whenever its IPC connection drops,
/// so each guild registered here is told its voice connection is gone, which
/// persists its session and releases the gateway call.
fn disconnect_all_guilds(connection: &Connection) {
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
}

fn fail_pending(connection: &Connection) {
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
            notify_guild(connection, guild_id, move |events, guild_id| async move {
                events.track_finished(guild_id, track_id).await;
            });
        }
        IpcEvent::TrackErrored {
            guild_id,
            track_id,
            error,
            position_ms,
        } => {
            notify_guild(connection, guild_id, move |events, guild_id| async move {
                events
                    .track_errored(
                        guild_id,
                        track_id,
                        Duration::from_millis(position_ms),
                        error,
                    )
                    .await;
            });
        }
        IpcEvent::ConnectionLost { guild_id } => {
            notify_guild(connection, guild_id, |events, guild_id| async move {
                events.connection_lost(guild_id).await;
            });
        }
    }
}

/// Runs `notify` against the guild's registered event sink, if it has one,
/// on its own task so the reader loop never waits on registry work.
fn notify_guild<F, Fut>(connection: &Connection, guild_id: u64, notify: F)
where
    F: FnOnce(Arc<dyn VoiceEvents>, GuildId) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    if let Some(events) = lookup(connection, guild_id) {
        tokio::spawn(notify(events, GuildId::new(guild_id)));
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
    ) -> Self {
        let stream = connect_with_retry(socket_addr).await;
        let (read_half, write_half) = stream.into_split();
        let connection = Connection::new(write_half, socket_addr.to_string());
        tokio::spawn(run_reader(read_half, connection.clone(), 0));
        Self {
            songbird,
            cookies_file,
            connection,
        }
    }
}

/// Retries indefinitely so apollo can be started before apollo-audio-worker
/// is up (e.g. both containers starting together) instead of failing to boot.
async fn connect_with_retry(socket_addr: &str) -> TcpStream {
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match TcpStream::connect(socket_addr).await {
            Ok(stream) => {
                if attempt > 1 {
                    tracing::info!("connected to apollo-audio-worker");
                }
                return stream;
            }
            Err(e) => {
                if attempt == 1 {
                    tracing::warn!(%e, %socket_addr, "apollo-audio-worker not reachable yet, waiting for it to start");
                } else if attempt.is_multiple_of(INITIAL_CONNECT_LOG_EVERY) {
                    tracing::warn!(%e, %socket_addr, attempt, "still waiting for apollo-audio-worker");
                }
                tokio::time::sleep(INITIAL_CONNECT_RETRY_DELAY).await;
            }
        }
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
            .request_with_timeout(
                Request::Join {
                    guild_id: guild_id.get(),
                    info: dto,
                },
                JOIN_TIMEOUT,
            )
            .await
            .and_then(|response| match response {
                Response::Ok => Ok(()),
                other => Err(format!("unexpected response to Join: {other:?}")),
            });

        if let Err(err) = result {
            if let Err(cleanup_err) = self.songbird.remove(guild_id).await {
                tracing::warn!(%guild_id, %cleanup_err, "failed to undo songbird join after a failed IPC Join");
            }
            return Err(err);
        }
        // Registered only once the worker holds the session: a reconnect
        // forced by the Join itself drains this map, and an entry added
        // beforehand would be drained with it while the session lives on.
        self.connection
            .guild_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(guild_id, events);
        Ok(())
    }

    async fn remove(&self, guild_id: GuildId) -> Result<(), String> {
        let registered = self
            .connection
            .guild_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&guild_id)
            .is_some();
        // A guild that is no longer registered was already dropped by the
        // worker along with its connection, so there is nothing to leave.
        if registered
            && let Err(err) = self
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
            resolved_at: Instant::now(),
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

#[cfg(test)]
mod tests {
    use tokio::net::TcpListener;

    use super::*;
    use crate::voice::testing::wait_until;

    #[derive(Default)]
    struct RecordingEvents {
        lost: StdMutex<Vec<GuildId>>,
    }

    #[async_trait]
    impl VoiceEvents for RecordingEvents {
        async fn track_finished(&self, _guild_id: GuildId, _track_id: uuid::Uuid) {}

        async fn track_errored(
            &self,
            _guild_id: GuildId,
            _track_id: uuid::Uuid,
            _position: Duration,
            _error: String,
        ) {
        }

        async fn connection_lost(&self, guild_id: GuildId) {
            self.lost.lock().unwrap().push(guild_id);
        }
    }

    async fn connected() -> (Arc<Connection>, TcpListener, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let client = TcpStream::connect(&addr).await.unwrap();
        let (worker_side, _) = listener.accept().await.unwrap();
        let (read_half, write_half) = client.into_split();
        let connection = Connection::new(write_half, addr);
        tokio::spawn(run_reader(read_half, connection.clone(), 0));
        (connection, listener, worker_side)
    }

    #[tokio::test]
    async fn a_request_the_worker_never_answers_times_out_and_is_forgotten() {
        let (connection, _listener, _worker_side) = connected().await;

        let err = connection
            .request_with_timeout(Request::Leave { guild_id: 1 }, Duration::from_millis(50))
            .await
            .unwrap_err();

        assert!(err.contains("did not answer"), "{err}");
        assert!(connection.pending.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_request_abandoned_by_its_caller_still_reaches_the_worker_intact() {
        let (connection, _listener, mut worker_side) = connected().await;

        for guild_id in [1, 2] {
            let _ = connection
                .request_with_timeout(Request::Leave { guild_id }, Duration::ZERO)
                .await;
        }

        for expected in [1, 2] {
            let frame = read_frame(&mut worker_side).await.unwrap().unwrap();
            assert!(
                matches!(frame, Envelope::Request { body: Request::Leave { guild_id }, .. } if guild_id == expected),
                "{frame:?}"
            );
        }
    }

    #[tokio::test]
    async fn reconnecting_after_the_worker_drops_the_link_disconnects_every_guild() {
        let (connection, listener, worker_side) = connected().await;
        let events = Arc::new(RecordingEvents::default());
        connection
            .guild_events
            .lock()
            .unwrap()
            .insert(GuildId::new(7), events.clone());

        drop(worker_side);
        let (_new_worker_side, _) = listener.accept().await.unwrap();

        wait_until(|| !events.lost.lock().unwrap().is_empty()).await;
        assert_eq!(*events.lost.lock().unwrap(), vec![GuildId::new(7)]);
        assert!(connection.guild_events.lock().unwrap().is_empty());
    }
}
