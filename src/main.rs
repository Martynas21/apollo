mod commands;
mod config;
mod db;
mod voice;
mod youtube;

use commands::{Data, Error};
use poise::serenity_prelude as serenity;
use songbird::serenity::SerenityInit;
use tracing_subscriber::EnvFilter;

// A bot serving a handful of guilds has no real use for one tokio worker
// thread per core (the default) -- apollo's workload is I/O-bound, not
// CPU-bound. Trimming this reduces how many threads compete with the
// songbird mixer thread for CPU time, which is otherwise a plausible source
// of the transient stalls behind the sporadic playback speed-up bug.
#[tokio::main(worker_threads = 4)]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            // stream_lib (songbird's HLS backend) logs a warning for every
            // segment chunk it can't forward once the consumer is gone —
            // see vendor/stream_lib/README.md. Our patch there closes the
            // gap that causes bulk spam, but downgrade the module here too
            // in case a stray warning still slips through mid-teardown.
            EnvFilter::new("info,stream_lib::hls=error")
        }))
        .init();

    let config = config::Config::from_env()?;

    tracing::info!(
        discord_application_id = %config.discord_application_id,
        "apollo starting up"
    );

    // Fail fast on a missing yt-dlp/ffmpeg rather than a confusing error on
    // someone's first `/play`.
    voice::check_playback_dependencies().await?;

    let intents = serenity::GatewayIntents::GUILDS | serenity::GatewayIntents::GUILD_VOICE_STATES;
    let guild_id = config.discord_guild_id;

    let db_pool = db::connect(&config.database_url).await?;

    // Built here (rather than left to `.register_songbird()`) so the same
    // `Arc<Songbird>` can back both the serenity client and `Data::player`.
    let songbird = songbird::Songbird::serenity();
    // Independent of the gateway `Client` (built further down) so
    // `PlayerRegistry` can use it to push `/player` panel edits from
    // contexts that aren't already handling a Discord interaction, e.g. the
    // track-end handler that drives auto-advance.
    let discord_http = std::sync::Arc::new(serenity::Http::new(&config.discord_token));
    let youtube_client = youtube::api::YouTubeClient::new(config.yt_dlp_cookies_file.clone());
    let player = voice::PlayerRegistry::new(
        songbird.clone(),
        reqwest::Client::new(),
        discord_http,
        config.yt_dlp_cookies_file.clone(),
        db_pool,
        youtube_client.clone(),
    );

    let data = Data {
        youtube: youtube_client,
        player,
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
