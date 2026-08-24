mod commands;
mod config;
mod voice;
mod youtube;

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let config = config::Config::from_env()?;

    tracing::info!(
        discord_application_id = %config.discord_application_id,
        "apollo starting up"
    );

    // TODO: build the serenity client with the poise framework, register
    // songbird, and start the axum-based OAuth callback server. This lands
    // in a later task.

    Ok(())
}
