mod rpc;
mod session;

use std::sync::Arc;

use apollo_ipc::proto::Event as IpcEvent;
use tokio::net::UnixListener;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::{mpsc, Mutex};
use tracing_subscriber::EnvFilter;

use session::Sessions;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Same as `apollo`'s own `main.rs`: tolerate a missing `.env` (e.g. under
    // Docker, where config arrives via the environment directly).
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let socket_path = apollo_ipc::optional_env_var(&|key| std::env::var(key), "AUDIO_WORKER_SOCKET")
        .unwrap_or_else(|| apollo_ipc::DEFAULT_SOCKET_PATH.to_string());

    // A previous run's socket file left behind (crash, restart) makes
    // `UnixListener::bind` fail with "address in use" even though nothing is
    // listening — remove it first.
    if std::path::Path::new(&socket_path).exists() {
        std::fs::remove_file(&socket_path)?;
    }
    if let Some(parent) = std::path::Path::new(&socket_path).parent() {
        std::fs::create_dir_all(parent)?;
    }

    let listener = UnixListener::bind(&socket_path)?;
    tracing::info!(socket_path, "apollo-audio-worker listening");

    let (events_tx, events_rx) = mpsc::unbounded_channel::<IpcEvent>();
    let sessions = Arc::new(Sessions::new(events_tx));
    let events_rx = Arc::new(Mutex::new(events_rx));

    // `docker compose down`/restart sends SIGTERM to this process (PID 1 in
    // its container); Ctrl+C sends SIGINT when run directly. Neither has a
    // default disposition that runs our cleanup, so without handling them
    // explicitly the process would die immediately, leaving active voice
    // connections dangling from Discord's perspective until its own gateway
    // timeout catches up.
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    loop {
        tokio::select! {
            accept_result = accept_and_handle(&listener, &sessions, &events_rx) => {
                accept_result?;
            }
            _ = sigterm.recv() => {
                tracing::info!("received SIGTERM; leaving all active voice sessions before exit");
                sessions.leave_all();
                return Ok(());
            }
            _ = sigint.recv() => {
                tracing::info!("received SIGINT; leaving all active voice sessions before exit");
                sessions.leave_all();
                return Ok(());
            }
        }
    }
}

/// Accepts and fully services one connection, tearing down all sessions once
/// it ends. Split out of `main`'s loop so it can be raced against the signal
/// handlers in a `tokio::select!` — a signal arriving mid-connection cancels
/// this future (dropping the in-flight connection) and runs `leave_all`
/// itself instead.
async fn accept_and_handle(
    listener: &UnixListener,
    sessions: &Arc<Sessions>,
    events_rx: &Arc<Mutex<mpsc::UnboundedReceiver<IpcEvent>>>,
) -> anyhow::Result<()> {
    let (stream, _addr) = listener.accept().await?;
    tracing::info!("apollo connected");

    // Discard anything left over from a previous connection's still-in-flight
    // events — a fresh connection means `apollo` has no sessions registered
    // against this worker yet (see `leave_all` below), so stale events
    // referencing them would be meaningless.
    {
        let mut rx = events_rx.lock().await;
        while rx.try_recv().is_ok() {}
    }

    let (read_half, write_half) = stream.into_split();
    rpc::handle_connection(read_half, write_half, sessions.clone(), events_rx.clone()).await;

    tracing::warn!("apollo disconnected; leaving all active voice sessions");
    sessions.leave_all();
    Ok(())
}
