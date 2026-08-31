mod commands;
mod config;
mod db;
mod voice;
mod youtube;

use anyhow::Context;
use commands::{Data, Error};
use poise::serenity_prelude as serenity;
use songbird::serenity::SerenityInit;
use tracing_subscriber::EnvFilter;
use voice::ipc_backend::IpcBackend;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = config::Config::from_env()?;

    tracing::info!(
        discord_application_id = %config.discord_application_id,
        "apollo starting up"
    );

    let intents = serenity::GatewayIntents::GUILDS | serenity::GatewayIntents::GUILD_VOICE_STATES;
    let guild_id = config.discord_guild_id;

    // Built here (rather than left to `.register_songbird()`) so the same
    // `Arc<Songbird>` can back both the serenity client and `Data::player`.
    let songbird = songbird::Songbird::serenity();
    // Independent of the gateway `Client` (built further down) so
    // `PlayerRegistry` can use it to push `/player` panel edits from
    // contexts that aren't already handling a Discord interaction, e.g. the
    // track-end handler that drives auto-advance.
    let discord_http = std::sync::Arc::new(serenity::Http::new(&config.discord_token));
    let youtube_client = youtube::api::YouTubeClient::new(
        config.yt_dlp_cookies_file.clone(),
        config.playlist_track_limit,
    );

    // Three independent startup checks/connections — run concurrently
    // rather than one after another, since none needs another's result.
    let (_, db_pool, voice_backend) = tokio::try_join!(
        // Fail fast on a missing yt-dlp/ffmpeg rather than a confusing error
        // on someone's first `/play`.
        voice::check_playback_dependencies(),
        db::connect(&config.database_url),
        async {
            IpcBackend::connect(
                &config.audio_worker_socket,
                std::path::PathBuf::from(&config.audio_buffer_dir),
                songbird.clone(),
                reqwest::Client::new(),
                config.yt_dlp_cookies_file.clone(),
            )
            .await
            .context("failed to connect to apollo-audio-worker")
        },
    )?;
    let player = voice::PlayerRegistry::new(
        std::sync::Arc::new(voice_backend),
        discord_http,
        config.yt_dlp_cookies_file.clone(),
        db_pool.clone(),
        youtube_client.clone(),
    );

    let data = Data {
        youtube: youtube_client,
        player,
        db: db_pool,
    };

    let framework = poise::Framework::builder()
        .options(poise::FrameworkOptions {
            commands: commands::commands(),
            event_handler: |ctx, event, _framework, data| Box::pin(event_handler(ctx, event, data)),
            ..Default::default()
        })
        .setup(move |ctx, _ready, framework| {
            Box::pin(async move {
                // Guild-scoped registration propagates near-instantly, which
                // is what you want while iterating locally; global
                // registration can take up to an hour to show up everywhere.
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
        .build();

    let mut client = serenity::ClientBuilder::new(config.discord_token, intents)
        .framework(framework)
        .register_songbird_with(songbird)
        .await?;

    client.start().await?;

    Ok(())
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
