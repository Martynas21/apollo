//! Dispatches one IPC connection: reads `Request` frames and replies with
//! `Response` frames, while a second task forwards songbird-originated
//! `Event`s onto the same connection as they occur.

use std::sync::Arc;

use apollo_ipc::proto::{Envelope, Event as IpcEvent, Request, Response};
use apollo_ipc::{read_frame, write_frame};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Mutex, mpsc};

use crate::session::Sessions;

async fn dispatch(sessions: &Sessions, request: Request) -> Result<Response, String> {
    match request {
        Request::Join { guild_id, info } => {
            sessions.join(guild_id, info).await?;
            Ok(Response::Ok)
        }
        Request::Leave { guild_id } => {
            sessions.leave(guild_id);
            Ok(Response::Ok)
        }
        Request::Play {
            guild_id,
            track_id,
            audio_path,
        } => {
            sessions.play(guild_id, track_id, audio_path)?;
            Ok(Response::Ok)
        }
        Request::Pause { guild_id, track_id } => {
            sessions.pause(guild_id, track_id)?;
            Ok(Response::Ok)
        }
        Request::Resume { guild_id, track_id } => {
            sessions.resume(guild_id, track_id)?;
            Ok(Response::Ok)
        }
        Request::Stop { guild_id, track_id } => {
            sessions.stop(guild_id, track_id)?;
            Ok(Response::Ok)
        }
        Request::SetVolume {
            guild_id,
            track_id,
            multiplier,
        } => {
            sessions.set_volume(guild_id, track_id, multiplier)?;
            Ok(Response::Ok)
        }
        Request::Status { guild_id, track_id } => sessions
            .status(guild_id, track_id)
            .await
            .map(Response::Status),
    }
}

/// Handles one connection end to end. Returns once the connection closes
/// (cleanly or otherwise), at which point the caller tears down all active
/// sessions — see `main.rs`.
pub async fn handle_connection<R, W>(
    read_half: R,
    write_half: W,
    sessions: Arc<Sessions>,
    events_rx: Arc<Mutex<mpsc::UnboundedReceiver<IpcEvent>>>,
) where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (outbound_tx, outbound_rx) = mpsc::unbounded_channel::<Envelope>();

    let writer_task = tokio::spawn(run_writer(write_half, outbound_rx));

    let event_forwarder = {
        let outbound_tx = outbound_tx.clone();
        tokio::spawn(async move {
            let mut events_rx = events_rx.lock().await;
            while let Some(event) = events_rx.recv().await {
                if outbound_tx.send(Envelope::Event(event)).is_err() {
                    break;
                }
            }
        })
    };

    run_reader(read_half, &sessions, &outbound_tx).await;

    event_forwarder.abort();
    drop(outbound_tx);
    let _ = writer_task.await;
}

/// Reads request frames and dispatches each on its own spawned task, so a
/// slow request for one guild (e.g. `Join`'s voice-gateway handshake) never
/// delays reading — let alone dispatching — a concurrently queued request
/// for another guild. Responses go back over `outbound_tx` (shared with the
/// event forwarder), which `run_writer` serializes onto the one socket; the
/// client correlates them by `id`, not arrival order, so completing out of
/// request order is fine (see `apollo_ipc::proto::Envelope::Response` and
/// `Connection::pending` in apollo's `voice/ipc_backend.rs`).
async fn run_reader<R: AsyncRead + Unpin>(
    mut reader: R,
    sessions: &Arc<Sessions>,
    outbound_tx: &mpsc::UnboundedSender<Envelope>,
) {
    loop {
        let envelope = match read_frame(&mut reader).await {
            Ok(Some(envelope)) => envelope,
            Ok(None) => {
                tracing::info!("IPC connection closed");
                return;
            }
            Err(e) => {
                tracing::warn!("IPC read error, closing connection: {e}");
                return;
            }
        };
        let Envelope::Request { id, body } = envelope else {
            tracing::warn!("ignoring unexpected non-request envelope from client");
            continue;
        };
        let sessions = Arc::clone(sessions);
        let outbound_tx = outbound_tx.clone();
        tokio::spawn(async move {
            // Dispatched on an inner task and joined here so a panic inside
            // `dispatch` (e.g. a songbird call) still yields an error
            // response instead of leaving the client's request pending
            // forever — `dispatch` already reports its own `Err`s over the
            // wire, this only covers the task-died case.
            let response = match tokio::spawn(async move { dispatch(&sessions, body).await }).await
            {
                Ok(response) => response,
                Err(join_err) => Err(format!("audio worker task panicked: {join_err}")),
            };
            let _ = outbound_tx.send(Envelope::Response { id, body: response });
        });
    }
}

async fn run_writer<W: AsyncWrite + Unpin>(
    mut writer: W,
    mut outbound_rx: mpsc::UnboundedReceiver<Envelope>,
) {
    while let Some(envelope) = outbound_rx.recv().await {
        if let Err(e) = write_frame(&mut writer, &envelope).await {
            tracing::warn!("IPC write error, closing connection: {e}");
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::session::Sessions;

    use super::*;

    /// Drives `handle_connection` over an in-memory duplex pipe (standing in
    /// for the real Unix socket) so the actual accept/dispatch/response loop
    /// gets exercised end to end without a real process boundary or a live
    /// Discord voice connection — the one thing this can't cover is
    /// anything past `Sessions::join` actually reaching a real
    /// `songbird::Driver::connect`, which needs live voice credentials (see
    /// the manual end-to-end step in the project's split-worker plan).
    async fn connected_client() -> (
        tokio::io::WriteHalf<tokio::io::DuplexStream>,
        tokio::io::ReadHalf<tokio::io::DuplexStream>,
    ) {
        let (client, worker) = tokio::io::duplex(4096);
        let (worker_read, worker_write) = tokio::io::split(worker);
        let (client_read, client_write) = tokio::io::split(client);

        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let sessions = Arc::new(Sessions::new(events_tx));
        let events_rx = Arc::new(Mutex::new(events_rx));
        tokio::spawn(handle_connection(
            worker_read,
            worker_write,
            sessions,
            events_rx,
        ));

        (client_write, client_read)
    }

    #[tokio::test]
    async fn status_on_an_unjoined_guild_comes_back_as_an_error() {
        let (mut write, mut read) = connected_client().await;

        write_frame(
            &mut write,
            &Envelope::Request {
                id: 1,
                body: Request::Status {
                    guild_id: 42,
                    track_id: uuid::Uuid::new_v4(),
                },
            },
        )
        .await
        .unwrap();

        let response = read_frame(&mut read).await.unwrap().unwrap();
        match response {
            Envelope::Response {
                id: 1,
                body: Err(_),
            } => {}
            other => panic!("expected an error response, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn requests_are_answered_with_matching_ids_in_order() {
        let (mut write, mut read) = connected_client().await;

        for id in [1_u64, 2, 3] {
            write_frame(
                &mut write,
                &Envelope::Request {
                    id,
                    body: Request::Leave { guild_id: id },
                },
            )
            .await
            .unwrap();
        }

        for expected_id in [1_u64, 2, 3] {
            match read_frame(&mut read).await.unwrap().unwrap() {
                Envelope::Response {
                    id,
                    body: Ok(Response::Ok),
                } => assert_eq!(id, expected_id),
                other => panic!("unexpected response: {other:?}"),
            }
        }
    }
}
