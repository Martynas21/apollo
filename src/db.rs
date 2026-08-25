//! SQLite-backed persistence for per-guild playback settings.
//!
//! Schema lives in `migrations/` and is embedded into the binary via
//! [`sqlx::migrate!`], so a fresh SQLite file is brought up to date
//! automatically on startup.

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use std::str::FromStr;

/// Opens a connection pool for `database_url`, creating the SQLite file if
/// it doesn't exist and running any pending migrations.
pub async fn connect(database_url: &str) -> Result<SqlitePool> {
    let options = SqliteConnectOptions::from_str(database_url)
        .with_context(|| format!("invalid DATABASE_URL: {database_url}"))?
        .create_if_missing(true);

    let pool = SqlitePoolOptions::new()
        .connect_with(options)
        .await
        .with_context(|| format!("failed to connect to database: {database_url}"))?;

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .context("failed to run database migrations")?;

    Ok(pool)
}

/// Default playback volume (percent) for a guild with no `guild_settings` row.
pub const DEFAULT_VOLUME: u8 = 100;

/// Reads a guild's persisted playback volume (0-100), defaulting to
/// [`DEFAULT_VOLUME`] if it's never been set.
pub async fn get_guild_volume(pool: &SqlitePool, guild_id: &str) -> Result<u8> {
    let row: Option<(i64,)> =
        sqlx::query_as("SELECT volume FROM guild_settings WHERE guild_id = ?1")
            .bind(guild_id)
            .fetch_optional(pool)
            .await
            .context("failed to fetch guild volume")?;

    Ok(row
        .and_then(|(volume,)| u8::try_from(volume).ok())
        .unwrap_or(DEFAULT_VOLUME))
}

/// Persists a guild's playback volume (0-100).
pub async fn set_guild_volume(pool: &SqlitePool, guild_id: &str, volume: u8) -> Result<()> {
    sqlx::query(
        "INSERT INTO guild_settings (guild_id, volume) VALUES (?1, ?2)
         ON CONFLICT(guild_id) DO UPDATE SET volume = excluded.volume",
    )
    .bind(guild_id)
    .bind(i64::from(volume))
    .execute(pool)
    .await
    .context("failed to set guild volume")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn guild_volume_defaults_when_unset() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        assert_eq!(get_guild_volume(&pool, "1").await?, DEFAULT_VOLUME);
        Ok(())
    }

    #[tokio::test]
    async fn guild_volume_set_and_get_round_trip() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;

        set_guild_volume(&pool, "1", 42).await?;
        assert_eq!(get_guild_volume(&pool, "1").await?, 42);

        // A different guild is unaffected.
        assert_eq!(get_guild_volume(&pool, "2").await?, DEFAULT_VOLUME);

        // Setting again replaces rather than erroring on the existing row.
        set_guild_volume(&pool, "1", 7).await?;
        assert_eq!(get_guild_volume(&pool, "1").await?, 7);

        Ok(())
    }
}
