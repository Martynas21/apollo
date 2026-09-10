use anyhow::{Context, Result};
use sqlx::sqlite::SqlitePool;

pub async fn user_count(pool: &SqlitePool) -> Result<i64> {
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
        .fetch_one(pool)
        .await
        .context("failed to count users")?;
    Ok(count)
}

pub async fn insert_user(
    pool: &SqlitePool,
    username: &str,
    password_hash: &str,
    is_admin: bool,
    is_root: bool,
    guild_id: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO users (username, password_hash, is_admin, is_root, guild_id) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .bind(username)
    .bind(password_hash)
    .bind(is_admin)
    .bind(is_root)
    .bind(guild_id)
    .execute(pool)
    .await
    .context("failed to insert user")?;
    Ok(())
}

/// Pins `username` to a single guild, or clears the pin with `None` so the
/// account can reach every guild again.
pub async fn set_user_guild(
    pool: &SqlitePool,
    username: &str,
    guild_id: Option<&str>,
) -> Result<()> {
    sqlx::query("UPDATE users SET guild_id = ?1 WHERE username = ?2")
        .bind(guild_id)
        .bind(username)
        .execute(pool)
        .await
        .context("failed to update the user's guild")?;
    Ok(())
}

pub async fn set_user_password(
    pool: &SqlitePool,
    username: &str,
    password_hash: &str,
) -> Result<()> {
    sqlx::query("UPDATE users SET password_hash = ?1 WHERE username = ?2")
        .bind(password_hash)
        .bind(username)
        .execute(pool)
        .await
        .context("failed to update user password")?;
    Ok(())
}

pub async fn rename_user(pool: &SqlitePool, old_username: &str, new_username: &str) -> Result<()> {
    sqlx::query("UPDATE users SET username = ?1 WHERE username = ?2")
        .bind(new_username)
        .bind(old_username)
        .execute(pool)
        .await
        .context("failed to rename user")?;
    Ok(())
}

pub struct UserCredentials {
    pub password_hash: String,
    pub is_admin: bool,
    pub is_root: bool,
    pub guild_id: Option<String>,
}

pub async fn user_credentials(
    pool: &SqlitePool,
    username: &str,
) -> Result<Option<UserCredentials>> {
    let row: Option<(String, bool, bool, Option<String>)> = sqlx::query_as(
        "SELECT password_hash, is_admin, is_root, guild_id FROM users WHERE username = ?1",
    )
    .bind(username)
    .fetch_optional(pool)
    .await
    .context("failed to fetch user")?;
    Ok(row.map(
        |(password_hash, is_admin, is_root, guild_id)| UserCredentials {
            password_hash,
            is_admin,
            is_root,
            guild_id,
        },
    ))
}

pub async fn user_exists(pool: &SqlitePool, username: &str) -> Result<bool> {
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users WHERE username = ?1")
        .bind(username)
        .fetch_one(pool)
        .await
        .context("failed to check whether user exists")?;
    Ok(count > 0)
}

pub struct UserSummary {
    pub username: String,
    pub is_admin: bool,
    pub is_root: bool,
    pub guild_id: Option<String>,
}

pub async fn list_users(pool: &SqlitePool) -> Result<Vec<UserSummary>> {
    let rows: Vec<(String, bool, bool, Option<String>)> =
        sqlx::query_as("SELECT username, is_admin, is_root, guild_id FROM users ORDER BY username")
            .fetch_all(pool)
            .await
            .context("failed to list users")?;
    Ok(rows
        .into_iter()
        .map(|(username, is_admin, is_root, guild_id)| UserSummary {
            username,
            is_admin,
            is_root,
            guild_id,
        })
        .collect())
}

pub struct UserPrivileges {
    pub is_admin: bool,
    pub is_root: bool,
}

pub async fn user_privileges(pool: &SqlitePool, username: &str) -> Result<Option<UserPrivileges>> {
    let row: Option<(bool, bool)> =
        sqlx::query_as("SELECT is_admin, is_root FROM users WHERE username = ?1")
            .bind(username)
            .fetch_optional(pool)
            .await
            .context("failed to fetch user privileges")?;
    Ok(row.map(|(is_admin, is_root)| UserPrivileges { is_admin, is_root }))
}

pub async fn admin_count(pool: &SqlitePool) -> Result<i64> {
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users WHERE is_admin = 1")
        .fetch_one(pool)
        .await
        .context("failed to count admins")?;
    Ok(count)
}

pub async fn delete_user(pool: &SqlitePool, username: &str) -> Result<()> {
    sqlx::query("DELETE FROM users WHERE username = ?1")
        .bind(username)
        .execute(pool)
        .await
        .context("failed to delete user")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::connect;

    #[tokio::test]
    async fn rename_user_updates_username_and_keeps_other_fields() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        insert_user(&pool, "old-name", "some-hash", true, false, None).await?;

        rename_user(&pool, "old-name", "new-name").await?;

        assert!(!user_exists(&pool, "old-name").await?);
        let creds = user_credentials(&pool, "new-name")
            .await?
            .expect("the renamed user should exist under its new name");
        assert_eq!(creds.password_hash, "some-hash");
        assert!(creds.is_admin);
        assert!(!creds.is_root);
        Ok(())
    }
}
