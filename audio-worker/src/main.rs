mod rpc;
mod session;

use std::sync::Arc;

use apollo_ipc::proto::Event as IpcEvent;
use tokio::net::UnixListener;
use tokio::sync::{mpsc, Mutex};
use tracing_subscriber::EnvFilter;

use session::Sessions;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let socket_path = std::env::var("AUDIO_WORKER_SOCKET")
        .unwrap_or_else(|_| apollo_ipc::DEFAULT_SOCKET_PATH.to_string());

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

    loop {
        let (stream, _addr) = listener.accept().await?;
        tracing::info!("apollo connected");

        // Discard anything left over from a previous connection's
        // still-in-flight events — a fresh connection means `apollo` has no
        // sessions registered against this worker yet (see `leave_all`
        // below), so stale events referencing them would be meaningless.
        {
            let mut rx = events_rx.lock().await;
            while rx.try_recv().is_ok() {}
        }

        let (read_half, write_half) = stream.into_split();
        rpc::handle_connection(read_half, write_half, sessions.clone(), events_rx.clone()).await;

        tracing::warn!("apollo disconnected; leaving all active voice sessions");
        sessions.leave_all();
    }
}
