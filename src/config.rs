//! Environment-driven configuration for the bot.
//!
//! Values are loaded once at startup via [`Config::from_env`]. This will be
//! extended as new subsystems (database, etc.) land.

use anyhow::{Context, Result};

/// All configuration the bot needs, sourced from environment variables
/// (see `.env.example`).
#[derive(Debug, Clone)]
pub struct Config {
    pub discord_token: String,
    pub discord_application_id: String,
    /// Guild to register slash commands against for fast dev iteration.
    /// `None` means register globally.
    pub discord_guild_id: Option<u64>,
    pub database_url: String,
    /// Path to a Netscape-format cookies file passed to `yt-dlp` as
    /// `--cookies`. `YouTube` increasingly requires a proof-of-origin signal
    /// from a real logged-in browser session before it'll serve a stream to
    /// `yt-dlp` at all (surfaces as a "Sign in to confirm you're not a bot"
    /// failure) — this is the standard workaround, and matters most from a
    /// datacenter/cloud host IP, which is where this bot will typically run.
    pub yt_dlp_cookies_file: Option<String>,
    /// Max tracks returned by a playlist import. See
    /// `crate::youtube::api::YouTubeClient::playlist_track_limit`.
    pub playlist_track_limit: usize,
}

/// Default for [`Config::playlist_track_limit`] when
/// `PLAYLIST_TRACK_LIMIT` is unset.
const DEFAULT_PLAYLIST_TRACK_LIMIT: usize = 500;

impl Config {
    /// Reads configuration from process environment variables.
    ///
    /// Callers are expected to load a `.env` file (e.g. via `dotenvy::dotenv()`)
    /// before calling this.
    pub fn from_env() -> Result<Self> {
        Self::from_source(|key| std::env::var(key))
    }

    /// Builds configuration from an arbitrary key lookup, matching
    /// [`std::env::var`]'s signature. Split out from [`Self::from_env`] so
    /// the parsing/validation logic can be exercised in tests against an
    /// in-memory map instead of mutating real process environment variables
    /// (which would be flaky under parallel test execution).
    fn from_source(lookup: impl Fn(&str) -> Result<String, std::env::VarError>) -> Result<Self> {
        Ok(Self {
            discord_token: env_var(&lookup, "DISCORD_TOKEN")?,
            discord_application_id: env_var(&lookup, "DISCORD_APPLICATION_ID")?,
            discord_guild_id: optional_guild_id(&lookup)?,
            database_url: env_var(&lookup, "DATABASE_URL")?,
            yt_dlp_cookies_file: optional_env_var(&lookup, "YT_DLP_COOKIES_FILE"),
            playlist_track_limit: playlist_track_limit(&lookup)?,
        })
    }
}

fn playlist_track_limit(
    lookup: &impl Fn(&str) -> Result<String, std::env::VarError>,
) -> Result<usize> {
    match lookup("PLAYLIST_TRACK_LIMIT") {
        Ok(value) if value.trim().is_empty() => Ok(DEFAULT_PLAYLIST_TRACK_LIMIT),
        Ok(value) => value
            .trim()
            .parse()
            .with_context(|| format!("PLAYLIST_TRACK_LIMIT is not a valid number: {value}")),
        Err(std::env::VarError::NotPresent) => Ok(DEFAULT_PLAYLIST_TRACK_LIMIT),
        Err(err) => Err(err).context("failed to read PLAYLIST_TRACK_LIMIT"),
    }
}

fn env_var(
    lookup: &impl Fn(&str) -> Result<String, std::env::VarError>,
    key: &str,
) -> Result<String> {
    lookup(key).with_context(|| format!("missing required environment variable: {key}"))
}

/// Reads an optional environment variable, treating both "unset" and "set
/// but empty" as absent.
fn optional_env_var(
    lookup: &impl Fn(&str) -> Result<String, std::env::VarError>,
    key: &str,
) -> Option<String> {
    lookup(key).ok().filter(|value| !value.trim().is_empty())
}

fn optional_guild_id(
    lookup: &impl Fn(&str) -> Result<String, std::env::VarError>,
) -> Result<Option<u64>> {
    match lookup("DISCORD_GUILD_ID") {
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

#[cfg(test)]
mod tests {
    use super::Config;
    use std::collections::HashMap;
    use std::env::VarError;

    fn lookup<'a>(vars: &'a HashMap<&str, &str>) -> impl Fn(&str) -> Result<String, VarError> + 'a {
        move |key| {
            vars.get(key)
                .map(|v| v.to_string())
                .ok_or(VarError::NotPresent)
        }
    }

    fn full_vars() -> HashMap<&'static str, &'static str> {
        HashMap::from([
            ("DISCORD_TOKEN", "token123"),
            ("DISCORD_APPLICATION_ID", "app456"),
            ("DATABASE_URL", "sqlite://test.db"),
        ])
    }

    #[test]
    fn all_required_vars_present_succeeds() {
        let vars = full_vars();
        let config = Config::from_source(lookup(&vars)).expect("should succeed");
        assert_eq!(config.discord_token, "token123");
        assert_eq!(config.discord_application_id, "app456");
        assert_eq!(config.database_url, "sqlite://test.db");
        assert_eq!(config.discord_guild_id, None);
        assert_eq!(config.yt_dlp_cookies_file, None);
        assert_eq!(config.playlist_track_limit, 500);
    }

    #[test]
    fn missing_discord_token_produces_error_naming_it() {
        let mut vars = full_vars();
        vars.remove("DISCORD_TOKEN");
        let err = Config::from_source(lookup(&vars)).expect_err("should fail");
        assert!(err.to_string().contains("DISCORD_TOKEN"));
    }

    #[test]
    fn missing_discord_application_id_produces_error_naming_it() {
        let mut vars = full_vars();
        vars.remove("DISCORD_APPLICATION_ID");
        let err = Config::from_source(lookup(&vars)).expect_err("should fail");
        assert!(err.to_string().contains("DISCORD_APPLICATION_ID"));
    }

    #[test]
    fn missing_database_url_produces_error_naming_it() {
        let mut vars = full_vars();
        vars.remove("DATABASE_URL");
        let err = Config::from_source(lookup(&vars)).expect_err("should fail");
        assert!(err.to_string().contains("DATABASE_URL"));
    }

    #[test]
    fn optional_guild_id_absent_is_none() {
        let vars = full_vars();
        let config = Config::from_source(lookup(&vars)).unwrap();
        assert_eq!(config.discord_guild_id, None);
    }

    #[test]
    fn optional_guild_id_empty_string_is_none() {
        let mut vars = full_vars();
        vars.insert("DISCORD_GUILD_ID", "");
        let config = Config::from_source(lookup(&vars)).unwrap();
        assert_eq!(config.discord_guild_id, None);
    }

    #[test]
    fn optional_guild_id_present_and_valid_parses() {
        let mut vars = full_vars();
        vars.insert("DISCORD_GUILD_ID", "123456789");
        let config = Config::from_source(lookup(&vars)).unwrap();
        assert_eq!(config.discord_guild_id, Some(123456789));
    }

    #[test]
    fn optional_guild_id_malformed_produces_error_naming_it() {
        let mut vars = full_vars();
        vars.insert("DISCORD_GUILD_ID", "not-a-number");
        let err = Config::from_source(lookup(&vars)).expect_err("should fail");
        assert!(err.to_string().contains("DISCORD_GUILD_ID"));
    }

    #[test]
    fn yt_dlp_cookies_file_absent_is_none() {
        let vars = full_vars();
        let config = Config::from_source(lookup(&vars)).unwrap();
        assert_eq!(config.yt_dlp_cookies_file, None);
    }

    #[test]
    fn yt_dlp_cookies_file_present_is_some() {
        let mut vars = full_vars();
        vars.insert("YT_DLP_COOKIES_FILE", "/path/to/cookies.txt");
        let config = Config::from_source(lookup(&vars)).unwrap();
        assert_eq!(
            config.yt_dlp_cookies_file,
            Some("/path/to/cookies.txt".to_string())
        );
    }

    #[test]
    fn yt_dlp_cookies_file_empty_string_is_none() {
        let mut vars = full_vars();
        vars.insert("YT_DLP_COOKIES_FILE", "");
        let config = Config::from_source(lookup(&vars)).unwrap();
        assert_eq!(config.yt_dlp_cookies_file, None);
    }

    #[test]
    fn playlist_track_limit_absent_uses_default() {
        let vars = full_vars();
        let config = Config::from_source(lookup(&vars)).unwrap();
        assert_eq!(config.playlist_track_limit, 500);
    }

    #[test]
    fn playlist_track_limit_present_parses() {
        let mut vars = full_vars();
        vars.insert("PLAYLIST_TRACK_LIMIT", "50");
        let config = Config::from_source(lookup(&vars)).unwrap();
        assert_eq!(config.playlist_track_limit, 50);
    }

    #[test]
    fn playlist_track_limit_empty_string_uses_default() {
        let mut vars = full_vars();
        vars.insert("PLAYLIST_TRACK_LIMIT", "");
        let config = Config::from_source(lookup(&vars)).unwrap();
        assert_eq!(config.playlist_track_limit, 500);
    }

    #[test]
    fn playlist_track_limit_malformed_produces_error_naming_it() {
        let mut vars = full_vars();
        vars.insert("PLAYLIST_TRACK_LIMIT", "not-a-number");
        let err = Config::from_source(lookup(&vars)).expect_err("should fail");
        assert!(err.to_string().contains("PLAYLIST_TRACK_LIMIT"));
    }
}
