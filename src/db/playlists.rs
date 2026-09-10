use anyhow::{Context, Result};
use sqlx::sqlite::SqlitePool;
use sqlx::{QueryBuilder, Sqlite};
use std::time::Duration;

use crate::model::Track;

use super::TRACK_INSERT_BATCH_SIZE;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedPlaylist {
    pub id: i64,
    pub name: String,
    pub url: String,
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

pub async fn delete_guild_playlist(pool: &SqlitePool, guild_id: &str, id: i64) -> Result<bool> {
    let mut tx = pool
        .begin()
        .await
        .context("failed to start playlist delete transaction")?;

    sqlx::query(
        "DELETE FROM playlist_tracks WHERE playlist_id = \
         (SELECT id FROM playlists WHERE guild_id = ?1 AND id = ?2)",
    )
    .bind(guild_id)
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

pub async fn get_playlist_thumbnail_video_id(
    pool: &SqlitePool,
    playlist_id: i64,
) -> Result<Option<String>> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT video_id FROM playlist_tracks WHERE playlist_id = ?1 ORDER BY position LIMIT 1",
    )
    .bind(playlist_id)
    .fetch_optional(pool)
    .await
    .context("failed to fetch playlist thumbnail track")?;

    Ok(row.map(|(video_id,)| video_id))
}

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

    let indexed_tracks = tracks.iter().enumerate().collect::<Vec<_>>();
    for batch in indexed_tracks.chunks(TRACK_INSERT_BATCH_SIZE) {
        let mut builder: QueryBuilder<Sqlite> = QueryBuilder::new(
            "INSERT INTO playlist_tracks \
             (playlist_id, position, video_id, title, channel, duration_secs) ",
        );
        builder.push_values(batch, |mut b, (position, track)| {
            b.push_bind(playlist_id)
                .push_bind(*position as i64)
                .push_bind(&track.video_id)
                .push_bind(&track.title)
                .push_bind(&track.channel)
                .push_bind(track.duration.map(|d| d.as_secs() as i64));
        });
        builder
            .build()
            .execute(&mut *tx)
            .await
            .context("failed to insert cached playlist tracks")?;
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
    use crate::db::connect;
    use crate::db::testing::sample_track;

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
        replace_playlist_tracks(&pool, id, &[sample_track("a", None)]).await?;

        assert!(!delete_guild_playlist(&pool, "2", id).await?);
        assert!(!delete_guild_playlist(&pool, "1", 9999).await?);
        assert!(get_guild_playlist(&pool, "1", id).await?.is_some());
        assert_eq!(
            get_playlist_tracks(&pool, id).await?,
            vec![sample_track("a", None)]
        );

        Ok(())
    }

    #[tokio::test]
    async fn deleting_playlist_row_directly_cascades_to_its_cached_tracks() -> Result<()> {
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

        sqlx::query("DELETE FROM playlists WHERE id = ?1")
            .bind(id)
            .execute(&pool)
            .await?;

        assert_eq!(get_playlist_tracks(&pool, id).await?, Vec::new());

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
