#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod commands;
mod config;
mod db;
mod voice;
mod web;
mod youtube;

use commands::{Data, Error};
use poise::serenity_prelude as serenity;
use songbird::serenity::SerenityInit;
use tracing_subscriber::EnvFilter;
use voice::ipc_backend::IpcBackend;

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

    let data = build_data(&config, db_pool.clone(), voice_backend);
    let dashboard_player = data.player.clone();

    let framework = build_framework(config.discord_guild_id, data);

    let intents = serenity::GatewayIntents::GUILDS | serenity::GatewayIntents::GUILD_VOICE_STATES;
    let mut client = serenity::ClientBuilder::new(config.discord_token.clone(), intents)
        .framework(framework)
        .register_songbird_with(songbird)
        .await?;

    spawn_dashboard(&config, dashboard_player, db_pool, client.cache.clone());

    client.start().await?;

    Ok(())
}

fn spawn_dashboard(
    config: &config::Config,
    player: voice::PlayerRegistry,
    db_pool: sqlx::SqlitePool,
    cache: std::sync::Arc<serenity::Cache>,
) {
    let bind_addr = config.dashboard_bind_addr.clone();
    let state = web::WebState::new(player, db_pool, cache);
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
        voice::check_playback_dependencies(),
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

fn build_data(
    config: &config::Config,
    db_pool: sqlx::SqlitePool,
    voice_backend: IpcBackend,
) -> Data {
    let discord_http = std::sync::Arc::new(serenity::Http::new(&config.discord_token));
    let youtube_client = youtube::api::YouTubeClient::new(
        config.yt_dlp_cookies_file.clone(),
        config.playlist_track_limit,
    );
    let player = voice::PlayerRegistry::new(
        std::sync::Arc::new(voice_backend),
        discord_http,
        config.yt_dlp_cookies_file.clone(),
        db_pool.clone(),
        youtube_client.clone(),
    );

    Data {
        youtube: youtube_client,
        player,
        db: db_pool,
    }
}

fn build_framework(guild_id: Option<u64>, data: Data) -> poise::Framework<Data, Error> {
    poise::Framework::builder()
        .options(poise::FrameworkOptions {
            commands: commands::commands(),
            event_handler: |ctx, event, _framework, data| Box::pin(event_handler(ctx, event, data)),
            ..Default::default()
        })
        .setup(move |ctx, _ready, framework| {
            Box::pin(async move {
                match guild_id {
                    Some(id) => {
                        poise::builtins::register_in_guild(
                            ctx,
                            &framework.options().commands,
                            serenity::GuildId::new(id),
                        )
                        .await?;
                    }
                    None => {
                        poise::builtins::register_globally(ctx, &framework.options().commands)
                            .await?;
                    }
                }
                Ok(data)
            })
        })
        .build()
}

async fn event_handler(
    ctx: &serenity::Context,
    event: &serenity::FullEvent,
    data: &Data,
) -> Result<(), Error> {
    match event {
        serenity::FullEvent::Ready { data_about_bot } => {
            tracing::info!(user = %data_about_bot.user.name, "ready");
        }
        serenity::FullEvent::Resume { .. } => {
            tracing::info!("resumed");
        }
        serenity::FullEvent::InteractionCreate {
            interaction: serenity::Interaction::Component(component),
        } => {
            if component.data.custom_id.starts_with("player:") {
                commands::handle_player_component(ctx, component, data).await?;
            } else if component.data.custom_id.starts_with("library:") {
                commands::handle_library_component(ctx, component, data).await?;
            }
        }
        _ => {}
    }
    Ok(())
}
