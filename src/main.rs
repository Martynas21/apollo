mod commands;
mod config;
mod db;
mod voice;
mod youtube;

use commands::{Data, Error};
use config::BotIdentity;
use poise::serenity_prelude as serenity;
use songbird::serenity::SerenityInit;
use sqlx::SqlitePool;
use tracing_subscriber::EnvFilter;
use youtube::api::YouTubeClient;

// A bot serving a handful of guilds has no real use for one tokio worker
// thread per core (the default) -- apollo's workload is I/O-bound, not
// CPU-bound. Trimming this reduces how many threads compete with the
// songbird mixer thread for CPU time, which is otherwise a plausible source
// of the transient stalls behind the sporadic playback speed-up bug. This
// stays fixed regardless of how many bot identities are configured: the
// mixer thread pool songbird schedules onto is process-global and shared by
// every identity, not duplicated per identity.
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

    tracing::info!(bot_count = config.bots.len(), "apollo starting up");

    // Fail fast on a missing yt-dlp/ffmpeg rather than a confusing error on
    // someone's first `/play`.
    voice::check_playback_dependencies().await?;

    let db_pool = db::connect(&config.database_url).await?;
    let youtube_client = YouTubeClient::new(
        config.yt_dlp_cookies_file.clone(),
        config.playlist_track_limit,
    );

    let mut identities = tokio::task::JoinSet::new();
    for identity in config.bots {
        let db_pool = db_pool.clone();
        let youtube_client = youtube_client.clone();
        let guild_id = config.discord_guild_id;
        let cookies_file = config.yt_dlp_cookies_file.clone();
        identities.spawn(async move {
            let application_id = identity.application_id.clone();
            let result = run_identity(identity, guild_id, db_pool, youtube_client, cookies_file).await;
            (application_id, result)
        });
    }

    while let Some(outcome) = identities.join_next().await {
        let (application_id, result) = outcome?;
        if let Err(err) = result {
            tracing::error!(%application_id, %err, "bot identity exited with an error");
        }
    }

    Ok(())
}

/// Runs a single bot identity to completion: its own `Songbird` manager,
/// `PlayerRegistry`, and `serenity::Client`, sharing only the DB pool and
/// YouTube client passed in. Independent identities let different voice
/// channels in the same guild be served simultaneously — a single bot user
/// can only hold one voice connection per guild, so per-channel playback
/// needs per-channel identities.
async fn run_identity(
    identity: BotIdentity,
    guild_id: Option<u64>,
    db_pool: SqlitePool,
    youtube_client: YouTubeClient,
    cookies_file: Option<String>,
) -> anyhow::Result<()> {
    let intents = serenity::GatewayIntents::GUILDS | serenity::GatewayIntents::GUILD_VOICE_STATES;

    // Built here (rather than left to `.register_songbird()`) so the same
    // `Arc<Songbird>` can back both the serenity client and `Data::player`.
    let songbird = songbird::Songbird::serenity();
    // Independent of the gateway `Client` (built further down) so
    // `PlayerRegistry` can use it to push `/player` panel edits from
    // contexts that aren't already handling a Discord interaction, e.g. the
    // track-end handler that drives auto-advance.
    let discord_http = std::sync::Arc::new(serenity::Http::new(&identity.token));
    let player = voice::PlayerRegistry::new(
        songbird.clone(),
        reqwest::Client::new(),
        discord_http,
        cookies_file,
        db_pool.clone(),
        youtube_client.clone(),
        identity.application_id.clone(),
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

    let mut client = serenity::ClientBuilder::new(identity.token, intents)
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
