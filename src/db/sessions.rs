//! Dashboard login sessions: one row per bearer token. Nothing here knows
//! about the clock — callers pass unix-second timestamps in, so the expiry
//! policy stays in `web::auth`.

use anyhow::{Context, Result};
use sqlx::sqlite::SqlitePool;

use super::users::UserSummary;

pub async fn insert_session(
    pool: &SqlitePool,
    token: &str,
    username: &str,
    expires_at: i64,
) -> Result<()> {
    sqlx::query("INSERT INTO sessions (token, username, expires_at) VALUES (?1, ?2, ?3)")
        .bind(token)
        .bind(username)
        .bind(expires_at)
        .execute(pool)
        .await
        .context("failed to insert session")?;
    Ok(())
}

/// The account behind `token`, read live from `users` so a session always
/// reflects the account's current privileges and guild pin, or `None` if
/// the token is unknown or expired as of `now`.
pub async fn session_user(pool: &SqlitePool, token: &str, now: i64) -> Result<Option<UserSummary>> {
    let row: Option<(String, bool, bool, Option<String>)> = sqlx::query_as(
        "SELECT u.username, u.is_admin, u.is_root, u.guild_id \
         FROM sessions s JOIN users u ON u.username = s.username \
         WHERE s.token = ?1 AND s.expires_at > ?2",
    )
    .bind(token)
    .bind(now)
    .fetch_optional(pool)
    .await
    .context("failed to fetch session")?;
    Ok(
        row.map(|(username, is_admin, is_root, guild_id)| UserSummary {
            username,
            is_admin,
            is_root,
            guild_id,
        }),
    )
}

pub async fn delete_session(pool: &SqlitePool, token: &str) -> Result<()> {
    sqlx::query("DELETE FROM sessions WHERE token = ?1")
        .bind(token)
        .execute(pool)
        .await
        .context("failed to delete session")?;
    Ok(())
}

pub async fn delete_user_sessions(pool: &SqlitePool, username: &str) -> Result<()> {
    sqlx::query("DELETE FROM sessions WHERE username = ?1")
        .bind(username)
        .execute(pool)
        .await
        .context("failed to delete the user's sessions")?;
    Ok(())
}

pub async fn delete_expired_sessions(pool: &SqlitePool, now: i64) -> Result<()> {
    sqlx::query("DELETE FROM sessions WHERE expires_at <= ?1")
        .bind(now)
        .execute(pool)
        .await
        .context("failed to sweep expired sessions")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{connect, delete_user, insert_user, rename_user};

    async fn pool_with_user(username: &str) -> Result<SqlitePool> {
        let pool = connect("sqlite::memory:").await?;
        insert_user(&pool, username, "some-hash", true, false, None).await?;
        Ok(pool)
    }

    #[tokio::test]
    async fn a_session_resolves_to_its_user_until_it_expires() -> Result<()> {
        let pool = pool_with_user("alice").await?;
        insert_session(&pool, "tok", "alice", 100).await?;

        let user = session_user(&pool, "tok", 99)
            .await?
            .expect("session should be valid before it expires");
        assert_eq!(user.username, "alice");
        assert!(user.is_admin);
        assert!(session_user(&pool, "tok", 100).await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn renaming_a_user_carries_its_sessions_along() -> Result<()> {
        let pool = pool_with_user("old-name").await?;
        insert_session(&pool, "tok", "old-name", 100).await?;

        rename_user(&pool, "old-name", "new-name").await?;

        let user = session_user(&pool, "tok", 0)
            .await?
            .expect("session should survive the rename");
        assert_eq!(user.username, "new-name");
        Ok(())
    }

    #[tokio::test]
    async fn deleting_a_user_drops_its_sessions() -> Result<()> {
        let pool = pool_with_user("alice").await?;
        insert_session(&pool, "tok", "alice", 100).await?;

        delete_user(&pool, "alice").await?;

        assert!(session_user(&pool, "tok", 0).await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn sweeping_removes_only_expired_sessions() -> Result<()> {
        let pool = pool_with_user("alice").await?;
        insert_session(&pool, "stale", "alice", 10).await?;
        insert_session(&pool, "fresh", "alice", 100).await?;

        delete_expired_sessions(&pool, 50).await?;

        assert!(session_user(&pool, "stale", 0).await?.is_none());
        assert!(session_user(&pool, "fresh", 0).await?.is_some());
        Ok(())
    }
}
