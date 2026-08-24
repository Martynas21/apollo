//! Environment-driven configuration for the bot.
//!
//! Values are loaded once at startup via [`Config::from_env`]. This will be
//! extended as new subsystems (OAuth callback server, database, etc.) land.

use anyhow::{Context, Result};

/// All configuration the bot needs, sourced from environment variables
/// (see `.env.example`).
// Fields beyond `discord_application_id`/`discord_guild_id` aren't read yet
// — they're wired up once the OAuth flow lands.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct Config {
    pub discord_token: String,
    pub discord_application_id: String,
    /// Guild to register slash commands against for fast dev iteration.
    /// `None` means register globally.
    pub discord_guild_id: Option<u64>,
    pub google_client_id: String,
    pub google_client_secret: String,
    pub google_oauth_redirect_uri: String,
}

impl Config {
    /// Reads configuration from process environment variables.
    ///
    /// Callers are expected to load a `.env` file (e.g. via `dotenvy::dotenv()`)
    /// before calling this.
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            discord_token: env_var("DISCORD_TOKEN")?,
            discord_application_id: env_var("DISCORD_APPLICATION_ID")?,
            discord_guild_id: optional_guild_id()?,
            google_client_id: env_var("GOOGLE_CLIENT_ID")?,
            google_client_secret: env_var("GOOGLE_CLIENT_SECRET")?,
            google_oauth_redirect_uri: env_var("GOOGLE_OAUTH_REDIRECT_URI")?,
        })
    }
}

fn env_var(key: &str) -> Result<String> {
    std::env::var(key).with_context(|| format!("missing required environment variable: {key}"))
}

fn optional_guild_id() -> Result<Option<u64>> {
    match std::env::var("DISCORD_GUILD_ID") {
        Ok(value) if value.trim().is_empty() => Ok(None),
        Ok(value) => value
            .trim()
            .parse()
            .map(Some)
            .with_context(|| format!("DISCORD_GUILD_ID is not a valid guild ID: {value}")),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(err) => Err(err).context("failed to read DISCORD_GUILD_ID"),
    }
}
