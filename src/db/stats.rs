use anyhow::{Context, Result};
use sqlx::sqlite::SqlitePool;

use crate::model::Track;

pub async fn record_track_play(pool: &SqlitePool, guild_id: &str, track: &Track) -> Result<()> {
    sqlx::query(
        "INSERT INTO track_play_counts \
         (guild_id, video_id, title, channel, duration_secs, play_count, last_played_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, 1, unixepoch())
         ON CONFLICT(guild_id, video_id) DO UPDATE SET
             play_count = play_count + 1,
             title = excluded.title,
             channel = excluded.channel,
             duration_secs = excluded.duration_secs,
             last_played_at = excluded.last_played_at",
    )
    .bind(guild_id)
    .bind(&track.video_id)
    .bind(&track.title)
    .bind(&track.channel)
    .bind(track.duration.map(|d| d.as_secs() as i64))
    .execute(pool)
    .await
    .context("failed to record track play")?;

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackPlayCount {
    pub video_id: String,
    pub title: String,
    pub channel: String,
    pub duration_secs: Option<u64>,
    pub play_count: i64,
}

pub async fn top_played_tracks(
    pool: &SqlitePool,
    guild_id: &str,
    limit: i64,
) -> Result<Vec<TrackPlayCount>> {
    let rows: Vec<(String, String, String, Option<i64>, i64)> = sqlx::query_as(
        "SELECT video_id, title, channel, duration_secs, play_count FROM track_play_counts \
         WHERE guild_id = ?1 ORDER BY play_count DESC, last_played_at DESC LIMIT ?2",
    )
    .bind(guild_id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .context("failed to fetch top played tracks")?;

    Ok(rows
        .into_iter()
        .map(
            |(video_id, title, channel, duration_secs, play_count)| TrackPlayCount {
                video_id,
                title,
                channel,
                #[allow(clippy::cast_sign_loss)]
                duration_secs: duration_secs.map(|secs| secs as u64),
                play_count,
            },
        )
        .collect())
}

pub async fn increment_playlist_play_count(
    pool: &SqlitePool,
    guild_id: &str,
    playlist_id: i64,
) -> Result<()> {
    sqlx::query("UPDATE playlists SET play_count = play_count + 1 WHERE guild_id = ?1 AND id = ?2")
        .bind(guild_id)
        .bind(playlist_id)
        .execute(pool)
        .await
        .context("failed to increment playlist play count")?;

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistPlayCount {
    pub id: i64,
    pub name: String,
    pub play_count: i64,
}

pub async fn top_played_playlists(
    pool: &SqlitePool,
    guild_id: &str,
    limit: i64,
) -> Result<Vec<PlaylistPlayCount>> {
    let rows: Vec<(i64, String, i64)> = sqlx::query_as(
        "SELECT id, name, play_count FROM playlists WHERE guild_id = ?1 AND play_count > 0 \
         ORDER BY play_count DESC LIMIT ?2",
    )
    .bind(guild_id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .context("failed to fetch top played playlists")?;

    Ok(rows
        .into_iter()
        .map(|(id, name, play_count)| PlaylistPlayCount {
            id,
            name,
            play_count,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::connect;
    use crate::db::save_guild_playlist;
    use crate::db::testing::sample_track;
    use std::time::Duration;

    #[tokio::test]
    async fn record_track_play_starts_at_one_and_increments_on_replay() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        let track = sample_track("a", Some(Duration::from_secs(30)));

        record_track_play(&pool, "1", &track).await?;
        let plays = top_played_tracks(&pool, "1", 10).await?;
        assert_eq!(plays.len(), 1);
        assert_eq!(plays[0].play_count, 1);
        assert_eq!(plays[0].title, "Title a");

        let retitled = Track {
            title: "New Title a".to_string(),
            ..sample_track("a", Some(Duration::from_secs(30)))
        };
        record_track_play(&pool, "1", &retitled).await?;

        let plays = top_played_tracks(&pool, "1", 10).await?;
        assert_eq!(plays.len(), 1);
        assert_eq!(plays[0].play_count, 2);
        assert_eq!(plays[0].title, "New Title a");

        Ok(())
    }

    #[tokio::test]
    async fn top_played_tracks_orders_by_play_count_descending() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;

        record_track_play(&pool, "1", &sample_track("a", None)).await?;
        record_track_play(&pool, "1", &sample_track("b", None)).await?;
        record_track_play(&pool, "1", &sample_track("b", None)).await?;

        let plays = top_played_tracks(&pool, "1", 10).await?;
        assert_eq!(
            plays
                .iter()
                .map(|p| p.video_id.as_str())
                .collect::<Vec<_>>(),
            vec!["b", "a"]
        );
        assert_eq!(
            top_played_tracks(&pool, "2", 10).await?,
            Vec::<TrackPlayCount>::new()
        );

        Ok(())
    }

    #[tokio::test]
    async fn playlist_play_counts_only_list_playlists_that_have_been_played() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        let played_id = save_guild_playlist(
            &pool,
            "1",
            "Chill Mix",
            "https://example.com/list=abc",
            "42",
        )
        .await?;
        save_guild_playlist(&pool, "1", "Workout", "https://example.com/list=def", "42").await?;

        assert_eq!(top_played_playlists(&pool, "1", 10).await?, Vec::new());

        increment_playlist_play_count(&pool, "1", played_id).await?;
        increment_playlist_play_count(&pool, "1", played_id).await?;

        let top = top_played_playlists(&pool, "1", 10).await?;
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].id, played_id);
        assert_eq!(top[0].name, "Chill Mix");
        assert_eq!(top[0].play_count, 2);

        Ok(())
    }
}
