mod commands;
mod config;
mod crypto;
mod db;
mod voice;
mod youtube;

use commands::{Data, Error};
use poise::serenity_prelude as serenity;
use songbird::serenity::SerenityInit;
use tracing_subscriber::EnvFilter;

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

    // Fail fast on a missing yt-dlp/ffmpeg rather than a confusing error on
    // someone's first `/play`.
    voice::check_playback_dependencies().await?;

    let intents = serenity::GatewayIntents::GUILDS | serenity::GatewayIntents::GUILD_VOICE_STATES;
    let guild_id = config.discord_guild_id;

    let db_pool = db::connect(&config.database_url).await?;
    db::verify_token_key(&db_pool, &config.token_encryption_key).await?;
    let oauth_client = youtube::oauth::build_oauth_client(&config)?;
    let oauth_http = youtube::oauth::build_http_client()?;
    let pending_links = youtube::oauth::PendingLinks::default();

    let (callback_port, callback_path) =
        youtube::server::parse_redirect_uri(&config.google_oauth_redirect_uri)?;

    // Built here (rather than left to `.register_songbird()`) so the same
    // `Arc<Songbird>` can back both the serenity client and `Data::player`.
    let songbird = songbird::Songbird::serenity();
    let player = voice::PlayerRegistry::new(
        songbird.clone(),
        oauth2::reqwest::Client::new(),
        config.yt_dlp_cookies_file.clone(),
        db_pool.clone(),
    );
    let youtube_client = youtube::api::YouTubeClient::new(oauth2::reqwest::Client::new());

    let data = Data {
        db: db_pool,
        token_key: config.token_encryption_key,
        oauth_client,
        oauth_http,
        pending_links,
        youtube: youtube_client,
        player,
    };

    // Loopback-only: this endpoint only ever needs to catch the redirect
    // from the linking user's own browser, never traffic from elsewhere.
    // Bound eagerly (before spawning) so a port conflict is a startup
    // error, not a silently-dead background task.
    let callback_listener = tokio::net::TcpListener::bind(("127.0.0.1", callback_port)).await?;
    let oauth_app = youtube::server::app(data.clone(), &callback_path);
    tokio::spawn(async move {
        if let Err(err) = axum::serve(callback_listener, oauth_app).await {
            tracing::error!(error = %err, "OAuth2 callback server stopped");
        }
    });

    let framework = poise::Framework::builder()
        .options(poise::FrameworkOptions {
            commands: commands::commands(),
            event_handler: |_ctx, event, _framework, _data| Box::pin(event_handler(event)),
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

async fn event_handler(event: &serenity::FullEvent) -> Result<(), Error> {
    match event {
        serenity::FullEvent::Ready { data_about_bot } => {
            tracing::info!(user = %data_about_bot.user.name, "ready");
        }
        serenity::FullEvent::Resume { .. } => {
            tracing::info!("resumed");
        }
        _ => {}
    }
    Ok(())
}
