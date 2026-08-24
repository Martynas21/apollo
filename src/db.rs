//! SQLite-backed persistence for linked Google OAuth2 tokens.
//!
//! Schema lives in `migrations/` and is embedded into the binary via
//! [`sqlx::migrate!`], so a fresh SQLite file is brought up to date
//! automatically on startup.

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use std::str::FromStr;

/// A row from the `users` table: one linked Google account's tokens.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct StoredToken {
    pub discord_user_id: String,
    pub access_token: String,
    pub refresh_token: String,
    /// Unix timestamp (seconds) at which `access_token` expires.
    pub expires_at: i64,
    /// Space-separated OAuth2 scopes.
    pub scopes: String,
}

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

/// Inserts a token, or replaces the existing one for the same Discord user.
pub async fn upsert_token(pool: &SqlitePool, token: &StoredToken) -> Result<()> {
    sqlx::query(
        "INSERT INTO users (discord_user_id, access_token, refresh_token, expires_at, scopes)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(discord_user_id) DO UPDATE SET
             access_token = excluded.access_token,
             refresh_token = excluded.refresh_token,
             expires_at = excluded.expires_at,
             scopes = excluded.scopes",
    )
    .bind(&token.discord_user_id)
    .bind(&token.access_token)
    .bind(&token.refresh_token)
    .bind(token.expires_at)
    .bind(&token.scopes)
    .execute(pool)
    .await
    .context("failed to upsert token")?;

    Ok(())
}

/// Looks up the stored token for a Discord user, if one is linked.
pub async fn get_token(pool: &SqlitePool, discord_user_id: &str) -> Result<Option<StoredToken>> {
    let token = sqlx::query_as::<_, StoredToken>(
        "SELECT discord_user_id, access_token, refresh_token, expires_at, scopes
         FROM users WHERE discord_user_id = ?1",
    )
    .bind(discord_user_id)
    .fetch_optional(pool)
    .await
    .context("failed to fetch token")?;

    Ok(token)
}

/// Deletes the stored token for a Discord user, if any. Idempotent.
pub async fn delete_token(pool: &SqlitePool, discord_user_id: &str) -> Result<()> {
    sqlx::query("DELETE FROM users WHERE discord_user_id = ?1")
        .bind(discord_user_id)
        .execute(pool)
        .await
        .context("failed to delete token")?;

    Ok(())
}

/// Default playback volume (percent) for a guild with no `guild_settings` row.
pub const DEFAULT_VOLUME: u8 = 100;

/// Reads a guild's persisted playback volume (0-100), defaulting to
/// [`DEFAULT_VOLUME`] if it's never been set.
pub async fn get_guild_volume(pool: &SqlitePool, guild_id: &str) -> Result<u8> {
    let row: Option<(i64,)> = sqlx::query_as("SELECT volume FROM guild_settings WHERE guild_id = ?1")
        .bind(guild_id)
        .fetch_optional(pool)
        .await
        .context("failed to fetch guild volume")?;

    Ok(row.map_or(DEFAULT_VOLUME, |(volume,)| volume as u8))
}

/// Persists a guild's playback volume (0-100).
pub async fn set_guild_volume(pool: &SqlitePool, guild_id: &str, volume: u8) -> Result<()> {
    sqlx::query(
        "INSERT INTO guild_settings (guild_id, volume) VALUES (?1, ?2)
         ON CONFLICT(guild_id) DO UPDATE SET volume = excluded.volume",
    )
    .bind(guild_id)
    .bind(volume as i64)
    .execute(pool)
    .await
    .context("failed to set guild volume")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_token() -> StoredToken {
        StoredToken {
            discord_user_id: "123456789012345678".to_string(),
            access_token: "access-abc".to_string(),
            refresh_token: "refresh-xyz".to_string(),
            expires_at: 1_800_000_000,
            scopes: "https://www.googleapis.com/auth/youtube.readonly".to_string(),
        }
    }

    #[tokio::test]
    async fn upsert_get_delete_round_trip() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;

        // No row yet.
        assert_eq!(
            get_token(&pool, &sample_token().discord_user_id).await?,
            None
        );

        let token = sample_token();
        upsert_token(&pool, &token).await?;

        let fetched = get_token(&pool, &token.discord_user_id)
            .await?
            .expect("token should have been stored");
        assert_eq!(fetched, token);

        // Upsert again with different values for the same user — should
        // replace, not duplicate.
        let mut updated = token.clone();
        updated.access_token = "access-new".to_string();
        updated.expires_at = 1_900_000_000;
        upsert_token(&pool, &updated).await?;

        let fetched = get_token(&pool, &token.discord_user_id).await?.unwrap();
        assert_eq!(fetched, updated);

        delete_token(&pool, &token.discord_user_id).await?;
        assert_eq!(get_token(&pool, &token.discord_user_id).await?, None);

        // Deleting again is a no-op, not an error.
        delete_token(&pool, &token.discord_user_id).await?;

        Ok(())
    }

    /// Proves persistence survives a "restart": reopening a pool against
    /// the same on-disk file (closing the first pool in between) must still
    /// see previously-written rows.
    #[tokio::test]
    async fn token_survives_pool_reopen() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("apollo-test.db");
        let url = format!("sqlite://{}", db_path.display());

        let token = sample_token();

        {
            let pool = connect(&url).await?;
            upsert_token(&pool, &token).await?;
            pool.close().await;
        }

        // Reopen — simulates the bot restarting and reconnecting to the
        // same database file.
        let pool = connect(&url).await?;
        let fetched = get_token(&pool, &token.discord_user_id)
            .await?
            .expect("token should still be present after reopening the pool");
        assert_eq!(fetched, token);

        Ok(())
    }

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
