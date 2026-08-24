//! Environment-driven configuration for the bot.
//!
//! Values are loaded once at startup via [`Config::from_env`]. This will be
//! extended as new subsystems (OAuth callback server, database, etc.) land.

use anyhow::{Context, Result};

use crate::crypto;

/// All configuration the bot needs, sourced from environment variables
/// (see `.env.example`).
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
    pub database_url: String,
    /// AES-256-GCM key used to encrypt `OAuth2` tokens before they're written
    /// to `users.access_token`/`refresh_token`. See `.env.example` for how
    /// to generate one.
    pub token_encryption_key: crypto::TokenKey,
    /// Path to a Netscape-format cookies file passed to `yt-dlp` as
    /// `--cookies`. `YouTube` increasingly requires a proof-of-origin signal
    /// from a real logged-in browser session before it'll serve a stream to
    /// `yt-dlp` at all (surfaces as a "Sign in to confirm you're not a bot"
    /// failure) — this is the standard workaround, and matters most from a
    /// datacenter/cloud host IP, which is where this bot will typically run.
    // Not read yet — Phase 6 threads this into `voice::track_input`/
    // `preflight_check` alongside the rest of the `/play` wiring.
    #[allow(dead_code)]
    pub yt_dlp_cookies_file: Option<String>,
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
            database_url: env_var("DATABASE_URL")?,
            token_encryption_key: crypto::parse_key(&env_var("TOKEN_ENCRYPTION_KEY")?)?,
            yt_dlp_cookies_file: optional_env_var("YT_DLP_COOKIES_FILE"),
        })
    }
}

fn env_var(key: &str) -> Result<String> {
    std::env::var(key).with_context(|| format!("missing required environment variable: {key}"))
}

/// Reads an optional environment variable, treating both "unset" and "set
/// but empty" as absent.
fn optional_env_var(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .filter(|value| !value.trim().is_empty())
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
