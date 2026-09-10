#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use anyhow::Context;
use apollo::voice::PlayerRegistry;
use apollo::voice::ipc_backend::IpcBackend;
use apollo::{config, db, voice, web, youtube};
use serenity::all as serenity;
use songbird::serenity::SerenityInit;
use tracing_subscriber::EnvFilter;

#[tokio::main(worker_threads = 2)]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    init_tracing();

    let config = config::Config::from_env()?;
    tracing::info!(
        discord_application_id = %config.discord_application_id,
        "apollo starting up"
    );

    let songbird = songbird::Songbird::serenity();
    let (db_pool, voice_backend) = connect_backends(&config, songbird.clone()).await?;

    web::bootstrap_user_if_needed(
        &db_pool,
        config.dashboard_username.as_deref(),
        config.dashboard_password.as_deref(),
    )
    .await?;

    let youtube_client = youtube::api::YouTubeClient::new(
        config.yt_dlp_cookies_file.clone(),
        config.playlist_track_limit,
    );
    let player = build_player(
        &config,
        db_pool.clone(),
        voice_backend,
        youtube_client.clone(),
    );
    let dashboard_player = player.clone();

    let application_id: u64 = config
        .discord_application_id
        .parse()
        .context("DISCORD_APPLICATION_ID is not a valid application id")?;

    let intents = serenity::GatewayIntents::GUILDS | serenity::GatewayIntents::GUILD_VOICE_STATES;
    let mut client = serenity::ClientBuilder::new(config.discord_token.clone(), intents)
        .application_id(serenity::ApplicationId::new(application_id))
        .event_handler(Handler)
        .register_songbird_with(songbird)
        .await?;

    clear_stale_slash_commands(&client.http, config.discord_guild_id).await?;

    spawn_dashboard(
        &config,
        dashboard_player,
        youtube_client,
        db_pool,
        client.cache.clone(),
    );

    client.start().await?;

    Ok(())
}

struct Handler;

#[async_trait::async_trait]
impl serenity::EventHandler for Handler {
    async fn ready(&self, _ctx: serenity::Context, data_about_bot: serenity::Ready) {
        tracing::info!(user = %data_about_bot.user.name, "ready");
    }

    async fn resume(&self, _ctx: serenity::Context, _: serenity::ResumedEvent) {
        tracing::info!("resumed");
    }
}

/// Clears any slash commands this bot previously registered, so `/play` etc.
/// don't linger in Discord's UI after the switch to the web dashboard. Mirrors
/// the guild-vs-global scoping the old registration step used.
async fn clear_stale_slash_commands(
    http: &serenity::Http,
    guild_id: Option<u64>,
) -> anyhow::Result<()> {
    match guild_id {
        Some(id) => {
            serenity::GuildId::new(id)
                .set_commands(http, Vec::new())
                .await
                .context("failed to clear guild slash commands")?;
        }
        None => {
            serenity::Command::set_global_commands(http, Vec::new())
                .await
                .context("failed to clear global slash commands")?;
        }
    }
    Ok(())
}

fn spawn_dashboard(
    config: &config::Config,
    player: voice::PlayerRegistry,
    youtube: youtube::api::YouTubeClient,
    db_pool: sqlx::SqlitePool,
    cache: std::sync::Arc<serenity::Cache>,
) {
    let bind_addr = config.dashboard_bind_addr.clone();
    let state = web::WebState::new(player, youtube, db_pool, cache);
    tokio::spawn(async move {
        if let Err(err) = web::serve(&bind_addr, state).await {
            tracing::error!(%err, "dashboard server exited");
        }
    });
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
}

async fn connect_backends(
    config: &config::Config,
    songbird: std::sync::Arc<songbird::Songbird>,
) -> anyhow::Result<(sqlx::SqlitePool, IpcBackend)> {
    let (_, db_pool, voice_backend) = tokio::try_join!(
        async {
            youtube::ytdlp::YtDlp::probe()
                .await
                .context("yt-dlp is missing — install it and ensure it's on PATH")
        },
        db::connect(&config.database_url),
        async {
            anyhow::Ok(
                IpcBackend::connect(
                    &config.audio_worker_socket,
                    songbird,
                    config.yt_dlp_cookies_file.clone(),
                )
                .await,
            )
        },
    )?;
    Ok((db_pool, voice_backend))
}

fn build_player(
    config: &config::Config,
    db_pool: sqlx::SqlitePool,
    voice_backend: IpcBackend,
    youtube_client: youtube::api::YouTubeClient,
) -> PlayerRegistry {
    voice::PlayerRegistry::new(
        std::sync::Arc::new(voice_backend),
        config.yt_dlp_cookies_file.clone(),
        db_pool,
        youtube_client,
    )
}
