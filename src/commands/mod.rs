//! Slash command implementations (registered with `poise`).

mod library;
mod playback;
mod youtube;

/// Shared state made available to every command invocation.
// No `Debug` derive: `PlayerRegistry` holds songbird types (`Arc<Songbird>`,
// `TrackHandle`) that don't implement it, and it's not needed anywhere.
#[derive(Clone)]
pub struct Data {
    /// Pool of connections to the token-persistence SQLite database.
    pub db: sqlx::SqlitePool,
    /// Configured Google OAuth2 client (auth/token/revocation endpoints set).
    pub oauth_client: crate::youtube::oauth::GoogleOAuthClient,
    /// Shared HTTP client used for all OAuth2 token endpoint requests.
    pub oauth_http: oauth2::reqwest::Client,
    /// In-flight `/link` attempts, keyed by CSRF state token.
    pub pending_links: crate::youtube::oauth::PendingLinks,
    /// Thin wrapper around the YouTube Data API v3 endpoints used to browse
    /// a linked account's playlists/liked videos and to search.
    pub youtube: crate::youtube::api::YouTubeClient,
    /// Per-guild playback queues and the shared songbird manager handle.
    pub player: crate::voice::PlayerRegistry,
}

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Context<'a> = poise::Context<'a, Data, Error>;

/// Trivial connectivity check.
#[poise::command(slash_command)]
async fn ping(ctx: Context<'_>) -> Result<(), Error> {
    ctx.say("Pong!").await?;
    Ok(())
}

/// All commands registered with the framework.
pub fn commands() -> Vec<poise::Command<Data, Error>> {
    vec![
        ping(),
        youtube::link(),
        youtube::unlink(),
        playback::join(),
        playback::leave(),
        playback::play(),
        playback::queue(),
        playback::skip(),
        playback::pause(),
        playback::resume(),
        playback::stop(),
        playback::nowplaying(),
        library::search(),
        library::searchplay(),
        library::playlists(),
        library::playlistplay(),
        library::playlistqueue(),
        library::liked(),
        library::likedplay(),
    ]
}
