#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use std::sync::Arc;

use apollo_audio_worker::config::Config;
use apollo_audio_worker::rpc;
use apollo_audio_worker::session::Sessions;
use apollo_ipc::proto::Event as IpcEvent;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, mpsc};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = Config::from_env();

    let listener = TcpListener::bind(&config.bind_addr).await?;
    tracing::info!(bind_addr = %config.bind_addr, "apollo-audio-worker listening");

    let (events_tx, events_rx) = mpsc::unbounded_channel::<IpcEvent>();
    let sessions = Arc::new(Sessions::new(events_tx));
    let events_rx = Arc::new(Mutex::new(events_rx));

    loop {
        tokio::select! {
            accept_result = accept_and_handle(&listener, &sessions, &events_rx) => {
                accept_result?;
            }
            shutdown_result = wait_for_shutdown_signal() => {
                shutdown_result?;
                tracing::info!("received shutdown signal; leaving all active voice sessions before exit");
                sessions.leave_all();
                return Ok(());
            }
        }
    }
}

#[cfg(unix)]
async fn wait_for_shutdown_signal() -> anyhow::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    tokio::select! {
        _ = sigterm.recv() => {}
        _ = sigint.recv() => {}
    }
    Ok(())
}

#[cfg(windows)]
async fn wait_for_shutdown_signal() -> anyhow::Result<()> {
    tokio::signal::ctrl_c().await?;
    Ok(())
}

async fn accept_and_handle(
    listener: &TcpListener,
    sessions: &Arc<Sessions>,
    events_rx: &Arc<Mutex<mpsc::UnboundedReceiver<IpcEvent>>>,
) -> anyhow::Result<()> {
    let (stream, _addr) = listener.accept().await?;
    tracing::info!("apollo connected");

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
