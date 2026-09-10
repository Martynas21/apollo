use anyhow::{Context, Result};
use sqlx::sqlite::SqlitePool;

pub const DEFAULT_VOLUME: u8 = 100;

pub async fn get_guild_volume(pool: &SqlitePool, guild_id: &str) -> Result<u8> {
    let row: Option<(i64,)> =
        sqlx::query_as("SELECT volume FROM guild_settings WHERE guild_id = ?1")
            .bind(guild_id)
            .fetch_optional(pool)
            .await
            .context("failed to fetch guild volume")?;

    Ok(match row {
        Some((volume,)) => u8::try_from(volume).unwrap_or_else(|_| {
            tracing::warn!(
                guild_id,
                volume,
                "stored guild volume is out of u8 range; falling back to default"
            );
            DEFAULT_VOLUME
        }),
        None => DEFAULT_VOLUME,
    })
}

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
    use crate::db::connect;

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

        assert_eq!(get_guild_volume(&pool, "2").await?, DEFAULT_VOLUME);

        set_guild_volume(&pool, "1", 7).await?;
        assert_eq!(get_guild_volume(&pool, "1").await?, 7);

        Ok(())
    }

    #[tokio::test]
    async fn guild_volume_falls_back_to_default_when_stored_value_is_out_of_range() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;

        sqlx::query(
            "INSERT INTO guild_settings (guild_id, volume) VALUES ('1', 99999)
             ON CONFLICT(guild_id) DO UPDATE SET volume = excluded.volume",
        )
        .execute(&pool)
        .await?;

        assert_eq!(get_guild_volume(&pool, "1").await?, DEFAULT_VOLUME);

        Ok(())
    }
}
