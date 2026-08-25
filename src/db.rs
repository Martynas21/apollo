//! SQLite-backed persistence for per-guild playback settings.
//!
//! Schema lives in `migrations/` and is embedded into the binary via
//! [`sqlx::migrate!`], so a fresh SQLite file is brought up to date
//! automatically on startup.

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use std::str::FromStr;
use std::time::Duration;

use crate::youtube::api::Track;

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

/// A guild's saved playlist: a named pointer to a `YouTube` playlist URL,
/// browsable from the `/player` panel instead of re-pasting the URL into
/// `/playlist_play` every time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedPlaylist {
    pub id: i64,
    pub name: String,
    pub url: String,
    /// Unix timestamp (seconds) of the last successful
    /// [`replace_playlist_tracks`] call for this playlist. `None` if it's
    /// never been cached (shouldn't normally happen — `save_guild_playlist`
    /// is always followed by a cache populate — but a row imported before
    /// caching existed would have no tracks to show until refreshed).
    pub cached_at: Option<i64>,
}

fn playlist_from_row(row: (i64, String, String, Option<i64>)) -> SavedPlaylist {
    let (id, name, url, cached_at) = row;
    SavedPlaylist {
        id,
        name,
        url,
        cached_at,
    }
}

/// Lists a guild's saved playlists, in the order they were added.
pub async fn list_guild_playlists(pool: &SqlitePool, guild_id: &str) -> Result<Vec<SavedPlaylist>> {
    let rows: Vec<(i64, String, String, Option<i64>)> = sqlx::query_as(
        "SELECT id, name, url, cached_at FROM playlists WHERE guild_id = ?1 ORDER BY id",
    )
    .bind(guild_id)
    .fetch_all(pool)
    .await
    .context("failed to list guild playlists")?;

    Ok(rows.into_iter().map(playlist_from_row).collect())
}

/// Saves a playlist for a guild, keyed by (guild, url) — importing the same
/// URL again updates its saved name rather than creating a duplicate entry —
/// and returns its row id, so the caller can populate its track cache right
/// after (see [`replace_playlist_tracks`]).
pub async fn save_guild_playlist(
    pool: &SqlitePool,
    guild_id: &str,
    name: &str,
    url: &str,
    added_by: &str,
) -> Result<i64> {
    let (id,): (i64,) = sqlx::query_as(
        "INSERT INTO playlists (guild_id, name, url, added_by) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(guild_id, url) DO UPDATE SET name = excluded.name
         RETURNING id",
    )
    .bind(guild_id)
    .bind(name)
    .bind(url)
    .bind(added_by)
    .fetch_one(pool)
    .await
    .context("failed to save guild playlist")?;

    Ok(id)
}

/// Looks up one of a guild's saved playlists by id. `None` if it doesn't
/// exist or belongs to a different guild.
pub async fn get_guild_playlist(
    pool: &SqlitePool,
    guild_id: &str,
    id: i64,
) -> Result<Option<SavedPlaylist>> {
    let row: Option<(i64, String, String, Option<i64>)> = sqlx::query_as(
        "SELECT id, name, url, cached_at FROM playlists WHERE guild_id = ?1 AND id = ?2",
    )
    .bind(guild_id)
    .bind(id)
    .fetch_optional(pool)
    .await
    .context("failed to fetch guild playlist")?;

    Ok(row.map(playlist_from_row))
}

/// Permanently removes one of a guild's saved playlists, along with its
/// cached track listing. Returns whether a row was actually deleted —
/// `false` if it didn't exist or belonged to a different guild, so the
/// caller can report that accurately without a separate existence check.
pub async fn delete_guild_playlist(pool: &SqlitePool, guild_id: &str, id: i64) -> Result<bool> {
    let mut tx = pool
        .begin()
        .await
        .context("failed to start playlist delete transaction")?;

    sqlx::query("DELETE FROM playlist_tracks WHERE playlist_id = ?1")
        .bind(id)
        .execute(&mut *tx)
        .await
        .context("failed to delete cached playlist tracks")?;

    let result = sqlx::query("DELETE FROM playlists WHERE guild_id = ?1 AND id = ?2")
        .bind(guild_id)
        .bind(id)
        .execute(&mut *tx)
        .await
        .context("failed to delete playlist")?;

    tx.commit()
        .await
        .context("failed to commit playlist delete transaction")?;

    Ok(result.rows_affected() > 0)
}

/// Reads a saved playlist's cached track listing, in playlist order. Empty
/// if it's never been cached — see [`SavedPlaylist::cached_at`].
pub async fn get_playlist_tracks(pool: &SqlitePool, playlist_id: i64) -> Result<Vec<Track>> {
    let rows: Vec<(String, String, String, Option<i64>)> = sqlx::query_as(
        "SELECT video_id, title, channel, duration_secs FROM playlist_tracks \
         WHERE playlist_id = ?1 ORDER BY position",
    )
    .bind(playlist_id)
    .fetch_all(pool)
    .await
    .context("failed to fetch cached playlist tracks")?;

    Ok(rows
        .into_iter()
        .map(|(video_id, title, channel, duration_secs)| Track {
            video_id,
            title,
            channel,
            #[allow(clippy::cast_sign_loss)]
            duration: duration_secs.map(|secs| Duration::from_secs(secs as u64)),
        })
        .collect())
}

/// Replaces a saved playlist's cached track listing wholesale and stamps
/// `cached_at` with the current time — used both right after import and by
/// the panel's per-playlist Refresh button to re-pull from `YouTube`.
///
/// Runs as one transaction so a concurrent read never sees a
/// half-replaced cache (all-old or all-new tracks, never a mix).
pub async fn replace_playlist_tracks(
    pool: &SqlitePool,
    playlist_id: i64,
    tracks: &[Track],
) -> Result<()> {
    let mut tx = pool
        .begin()
        .await
        .context("failed to start playlist cache transaction")?;

    sqlx::query("DELETE FROM playlist_tracks WHERE playlist_id = ?1")
        .bind(playlist_id)
        .execute(&mut *tx)
        .await
        .context("failed to clear old cached playlist tracks")?;

    for (position, track) in tracks.iter().enumerate() {
        sqlx::query(
            "INSERT INTO playlist_tracks \
             (playlist_id, position, video_id, title, channel, duration_secs) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(playlist_id)
        .bind(position as i64)
        .bind(&track.video_id)
        .bind(&track.title)
        .bind(&track.channel)
        .bind(track.duration.map(|d| d.as_secs() as i64))
        .execute(&mut *tx)
        .await
        .context("failed to insert cached playlist track")?;
    }

    sqlx::query("UPDATE playlists SET cached_at = unixepoch() WHERE id = ?1")
        .bind(playlist_id)
        .execute(&mut *tx)
        .await
        .context("failed to stamp playlist cache time")?;

    tx.commit()
        .await
        .context("failed to commit playlist cache transaction")?;

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

    #[tokio::test]
    async fn playlists_empty_when_none_saved() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        assert_eq!(list_guild_playlists(&pool, "1").await?, Vec::new());
        Ok(())
    }

    #[tokio::test]
    async fn playlist_save_list_and_get_round_trip() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;

        save_guild_playlist(
            &pool,
            "1",
            "Chill Mix",
            "https://example.com/list=abc",
            "42",
        )
        .await?;
        save_guild_playlist(&pool, "1", "Workout", "https://example.com/list=def", "42").await?;

        let playlists = list_guild_playlists(&pool, "1").await?;
        assert_eq!(playlists.len(), 2);
        assert_eq!(playlists[0].name, "Chill Mix");
        assert_eq!(playlists[1].name, "Workout");

        let fetched = get_guild_playlist(&pool, "1", playlists[0].id).await?;
        assert_eq!(fetched, Some(playlists[0].clone()));

        // A different guild sees none of these.
        assert_eq!(list_guild_playlists(&pool, "2").await?, Vec::new());

        Ok(())
    }

    #[tokio::test]
    async fn saving_same_url_again_updates_name_instead_of_duplicating() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;

        save_guild_playlist(&pool, "1", "Old Name", "https://example.com/list=abc", "42").await?;
        save_guild_playlist(&pool, "1", "New Name", "https://example.com/list=abc", "42").await?;

        let playlists = list_guild_playlists(&pool, "1").await?;
        assert_eq!(playlists.len(), 1);
        assert_eq!(playlists[0].name, "New Name");

        Ok(())
    }

    #[tokio::test]
    async fn get_guild_playlist_none_for_missing_or_wrong_guild() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        save_guild_playlist(
            &pool,
            "1",
            "Chill Mix",
            "https://example.com/list=abc",
            "42",
        )
        .await?;
        let playlists = list_guild_playlists(&pool, "1").await?;

        assert_eq!(get_guild_playlist(&pool, "1", 9999).await?, None);
        assert_eq!(get_guild_playlist(&pool, "2", playlists[0].id).await?, None);

        Ok(())
    }

    #[tokio::test]
    async fn delete_guild_playlist_removes_it_and_its_cached_tracks() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        let id = save_guild_playlist(
            &pool,
            "1",
            "Chill Mix",
            "https://example.com/list=abc",
            "42",
        )
        .await?;
        replace_playlist_tracks(&pool, id, &[sample_track("a", None)]).await?;

        assert!(delete_guild_playlist(&pool, "1", id).await?);
        assert_eq!(get_guild_playlist(&pool, "1", id).await?, None);
        assert_eq!(get_playlist_tracks(&pool, id).await?, Vec::new());

        Ok(())
    }

    #[tokio::test]
    async fn delete_guild_playlist_returns_false_and_no_ops_for_missing_or_wrong_guild()
    -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        let id = save_guild_playlist(
            &pool,
            "1",
            "Chill Mix",
            "https://example.com/list=abc",
            "42",
        )
        .await?;

        assert!(!delete_guild_playlist(&pool, "2", id).await?);
        assert!(!delete_guild_playlist(&pool, "1", 9999).await?);
        assert!(get_guild_playlist(&pool, "1", id).await?.is_some());

        Ok(())
    }

    #[tokio::test]
    async fn save_guild_playlist_returns_the_row_id() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        let id = save_guild_playlist(
            &pool,
            "1",
            "Chill Mix",
            "https://example.com/list=abc",
            "42",
        )
        .await?;

        let playlists = list_guild_playlists(&pool, "1").await?;
        assert_eq!(
            playlists,
            vec![get_guild_playlist(&pool, "1", id).await?.unwrap()]
        );

        // Re-saving the same URL returns the same id (an update, not a new row).
        let same_id =
            save_guild_playlist(&pool, "1", "Renamed", "https://example.com/list=abc", "42")
                .await?;
        assert_eq!(same_id, id);

        Ok(())
    }

    #[tokio::test]
    async fn newly_saved_playlist_has_no_cached_at_until_populated() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        let id = save_guild_playlist(
            &pool,
            "1",
            "Chill Mix",
            "https://example.com/list=abc",
            "42",
        )
        .await?;

        let playlist = get_guild_playlist(&pool, "1", id).await?.unwrap();
        assert_eq!(playlist.cached_at, None);
        assert_eq!(get_playlist_tracks(&pool, id).await?, Vec::new());

        Ok(())
    }

    fn sample_track(video_id: &str, duration: Option<Duration>) -> Track {
        Track {
            video_id: video_id.to_string(),
            title: format!("Title {video_id}"),
            channel: "Some Channel".to_string(),
            duration,
        }
    }

    #[tokio::test]
    async fn replace_playlist_tracks_caches_in_order_and_stamps_cached_at() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        let id = save_guild_playlist(
            &pool,
            "1",
            "Chill Mix",
            "https://example.com/list=abc",
            "42",
        )
        .await?;

        let tracks = vec![
            sample_track("a", Some(Duration::from_secs(60))),
            sample_track("b", None),
        ];
        replace_playlist_tracks(&pool, id, &tracks).await?;

        assert_eq!(get_playlist_tracks(&pool, id).await?, tracks);

        let playlist = get_guild_playlist(&pool, "1", id).await?.unwrap();
        assert!(playlist.cached_at.is_some());

        Ok(())
    }

    #[tokio::test]
    async fn replace_playlist_tracks_replaces_rather_than_appends() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        let id = save_guild_playlist(
            &pool,
            "1",
            "Chill Mix",
            "https://example.com/list=abc",
            "42",
        )
        .await?;

        replace_playlist_tracks(&pool, id, &[sample_track("old", None)]).await?;
        replace_playlist_tracks(
            &pool,
            id,
            &[sample_track("new1", None), sample_track("new2", None)],
        )
        .await?;

        let tracks = get_playlist_tracks(&pool, id).await?;
        assert_eq!(
            tracks
                .iter()
                .map(|t| t.video_id.as_str())
                .collect::<Vec<_>>(),
            vec!["new1", "new2"]
        );

        Ok(())
    }
}
