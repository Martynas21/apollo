//! Slash command implementations (registered with `poise`).
//!
//! Placeholder — commands such as `/play` and `/queue` will be added in a
//! later task.

mod youtube;

/// Shared state made available to every command invocation. Later phases
/// will add a YouTube Data API client, etc.
// `GoogleOAuthClient` and `oauth2::reqwest::Client` both derive/implement
// `Debug`, so `Data` keeps deriving it too.
#[derive(Debug, Clone)]
pub struct Data {
    /// Pool of connections to the token-persistence SQLite database.
    pub db: sqlx::SqlitePool,
    /// Configured Google OAuth2 client (auth/token/revocation endpoints set).
    pub oauth_client: crate::youtube::oauth::GoogleOAuthClient,
    /// Shared HTTP client used for all OAuth2 token endpoint requests.
    pub oauth_http: oauth2::reqwest::Client,
    /// In-flight `/link` attempts, keyed by CSRF state token.
    pub pending_links: crate::youtube::oauth::PendingLinks,
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
    vec![ping(), youtube::link(), youtube::unlink()]
}
