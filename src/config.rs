use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Config {
    pub discord_token: String,
    pub discord_application_id: String,
    pub discord_guild_id: Option<u64>,
    pub database_url: String,
    pub yt_dlp_cookies_file: Option<String>,
    pub playlist_track_limit: usize,
    pub audio_worker_socket: String,
    pub dashboard_bind_addr: String,
    pub dashboard_username: Option<String>,
    pub dashboard_password: Option<String>,
}

const DEFAULT_PLAYLIST_TRACK_LIMIT: usize = 500;

const DEFAULT_DASHBOARD_BIND_ADDR: &str = "127.0.0.1:8787";

impl Config {
    pub fn from_env() -> Result<Self> {
        Self::from_source(|key| std::env::var(key))
    }

    fn from_source(lookup: impl Fn(&str) -> Result<String, std::env::VarError>) -> Result<Self> {
        Ok(Self {
            discord_token: env_var(&lookup, "DISCORD_TOKEN")?,
            discord_application_id: env_var(&lookup, "DISCORD_APPLICATION_ID")?,
            discord_guild_id: optional_guild_id(&lookup)?,
            database_url: env_var(&lookup, "DATABASE_URL")?,
            yt_dlp_cookies_file: apollo_ipc::optional_env_var(&lookup, "YT_DLP_COOKIES_FILE"),
            playlist_track_limit: playlist_track_limit(&lookup)?,
            audio_worker_socket: apollo_ipc::optional_env_var(&lookup, "AUDIO_WORKER_SOCKET")
                .unwrap_or_else(|| apollo_ipc::DEFAULT_SOCKET_ADDR.to_string()),
            dashboard_bind_addr: apollo_ipc::optional_env_var(&lookup, "DASHBOARD_BIND_ADDR")
                .unwrap_or_else(|| DEFAULT_DASHBOARD_BIND_ADDR.to_string()),
            dashboard_username: apollo_ipc::optional_env_var(&lookup, "DASHBOARD_USERNAME"),
            dashboard_password: apollo_ipc::optional_env_var(&lookup, "DASHBOARD_PASSWORD"),
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
        assert_eq!(config.dashboard_bind_addr, "127.0.0.1:8787");
        assert_eq!(config.dashboard_username, None);
        assert_eq!(config.dashboard_password, None);
    }

    #[test]
    fn dashboard_bind_addr_present_overrides_default() {
        let mut vars = full_vars();
        vars.insert("DASHBOARD_BIND_ADDR", "0.0.0.0:9000");
        let config = Config::from_source(lookup(&vars)).unwrap();
        assert_eq!(config.dashboard_bind_addr, "0.0.0.0:9000");
    }

    #[test]
    fn dashboard_credentials_present_are_some() {
        let mut vars = full_vars();
        vars.insert("DASHBOARD_USERNAME", "admin");
        vars.insert("DASHBOARD_PASSWORD", "hunter2");
        let config = Config::from_source(lookup(&vars)).unwrap();
        assert_eq!(config.dashboard_username, Some("admin".to_string()));
        assert_eq!(config.dashboard_password, Some("hunter2".to_string()));
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
