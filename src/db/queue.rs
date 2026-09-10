use anyhow::{Context, Result};
use sqlx::sqlite::SqlitePool;
use sqlx::{QueryBuilder, Sqlite};

use crate::model::QueuedTrack;

use super::TRACK_INSERT_BATCH_SIZE;
use super::session::queued_track_from_row;

#[cfg(test)]
pub async fn queue_push_back(
    pool: &SqlitePool,
    guild_id: &str,
    queued: &QueuedTrack,
) -> Result<()> {
    let mut tx = pool
        .begin()
        .await
        .context("failed to start queue push transaction")?;

    sqlx::query(
        "INSERT OR IGNORE INTO guild_sessions (guild_id, radio_enabled, radio_requested_by, radio_history) \
         VALUES (?1, 0, NULL, '')",
    )
    .bind(guild_id)
    .execute(&mut *tx)
    .await
    .context("failed to ensure guild session row")?;

    sqlx::query(
        "INSERT INTO guild_session_queue \
         (guild_id, position, video_id, title, channel, duration_secs, requested_by) \
         VALUES (?1, (SELECT COALESCE(MAX(position), -1) + 1 FROM guild_session_queue WHERE guild_id = ?1), \
                 ?2, ?3, ?4, ?5, ?6)",
    )
    .bind(guild_id)
    .bind(&queued.track.video_id)
    .bind(&queued.track.title)
    .bind(&queued.track.channel)
    .bind(queued.track.duration.map(|d| d.as_secs() as i64))
    .bind(queued.requested_by.to_string())
    .execute(&mut *tx)
    .await
    .context("failed to push a queued track")?;

    tx.commit()
        .await
        .context("failed to commit queue push transaction")?;
    Ok(())
}

pub async fn queue_push_many(
    pool: &SqlitePool,
    guild_id: &str,
    tracks: &[QueuedTrack],
) -> Result<()> {
    if tracks.is_empty() {
        return Ok(());
    }

    let mut tx = pool
        .begin()
        .await
        .context("failed to start queue push transaction")?;

    sqlx::query(
        "INSERT OR IGNORE INTO guild_sessions (guild_id, radio_enabled, radio_requested_by, radio_history) \
         VALUES (?1, 0, NULL, '')",
    )
    .bind(guild_id)
    .execute(&mut *tx)
    .await
    .context("failed to ensure guild session row")?;

    let (next_position,): (i64,) = sqlx::query_as(
        "SELECT COALESCE(MAX(position), -1) + 1 FROM guild_session_queue WHERE guild_id = ?1",
    )
    .bind(guild_id)
    .fetch_one(&mut *tx)
    .await
    .context("failed to compute the next queue position")?;

    let indexed_tracks: Vec<(i64, &QueuedTrack)> = tracks
        .iter()
        .enumerate()
        .map(|(i, queued)| (next_position + i as i64, queued))
        .collect();
    for batch in indexed_tracks.chunks(TRACK_INSERT_BATCH_SIZE) {
        let mut builder: QueryBuilder<Sqlite> = QueryBuilder::new(
            "INSERT INTO guild_session_queue \
             (guild_id, position, video_id, title, channel, duration_secs, requested_by) ",
        );
        builder.push_values(batch, |mut b, (position, queued): &(i64, &QueuedTrack)| {
            b.push_bind(guild_id)
                .push_bind(*position)
                .push_bind(&queued.track.video_id)
                .push_bind(&queued.track.title)
                .push_bind(&queued.track.channel)
                .push_bind(queued.track.duration.map(|d| d.as_secs() as i64))
                .push_bind(queued.requested_by.to_string());
        });
        builder
            .build()
            .execute(&mut *tx)
            .await
            .context("failed to insert queued tracks")?;
    }

    tx.commit()
        .await
        .context("failed to commit queue push transaction")?;
    Ok(())
}

pub async fn queue_pop_front(pool: &SqlitePool, guild_id: &str) -> Result<Option<QueuedTrack>> {
    let mut tx = pool
        .begin()
        .await
        .context("failed to start queue pop transaction")?;

    loop {
        let row: Option<(i64, String, String, String, Option<i64>, String)> = sqlx::query_as(
            "SELECT position, video_id, title, channel, duration_secs, requested_by \
             FROM guild_session_queue WHERE guild_id = ?1 ORDER BY position LIMIT 1",
        )
        .bind(guild_id)
        .fetch_optional(&mut *tx)
        .await
        .context("failed to read the front of the queue")?;

        let Some((position, video_id, title, channel, duration_secs, requested_by)) = row else {
            tx.commit()
                .await
                .context("failed to commit queue pop transaction")?;
            return Ok(None);
        };

        sqlx::query("DELETE FROM guild_session_queue WHERE guild_id = ?1 AND position = ?2")
            .bind(guild_id)
            .bind(position)
            .execute(&mut *tx)
            .await
            .context("failed to remove the popped queue row")?;

        let Some(queued) =
            queued_track_from_row((video_id, title, channel, duration_secs, requested_by))
        else {
            tracing::warn!(guild_id, position, "skipping unparsable queue row");
            continue;
        };

        tx.commit()
            .await
            .context("failed to commit queue pop transaction")?;
        return Ok(Some(queued));
    }
}

pub async fn queue_peek_front(pool: &SqlitePool, guild_id: &str) -> Result<Option<QueuedTrack>> {
    let row: Option<(String, String, String, Option<i64>, String)> = sqlx::query_as(
        "SELECT video_id, title, channel, duration_secs, requested_by \
         FROM guild_session_queue WHERE guild_id = ?1 ORDER BY position LIMIT 1",
    )
    .bind(guild_id)
    .fetch_optional(pool)
    .await
    .context("failed to peek the front of the queue")?;

    Ok(row.and_then(queued_track_from_row))
}

pub async fn queue_len(pool: &SqlitePool, guild_id: &str) -> Result<usize> {
    let (count,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM guild_session_queue WHERE guild_id = ?1")
            .bind(guild_id)
            .fetch_one(pool)
            .await
            .context("failed to count queued tracks")?;
    #[allow(clippy::cast_sign_loss)]
    Ok(count.max(0) as usize)
}

pub async fn queue_all(pool: &SqlitePool, guild_id: &str) -> Result<Vec<QueuedTrack>> {
    let rows: Vec<(String, String, String, Option<i64>, String)> = sqlx::query_as(
        "SELECT video_id, title, channel, duration_secs, requested_by \
         FROM guild_session_queue WHERE guild_id = ?1 ORDER BY position",
    )
    .bind(guild_id)
    .fetch_all(pool)
    .await
    .context("failed to fetch the queue")?;

    Ok(rows.into_iter().filter_map(queued_track_from_row).collect())
}

pub async fn queue_clear(pool: &SqlitePool, guild_id: &str) -> Result<()> {
    sqlx::query("DELETE FROM guild_session_queue WHERE guild_id = ?1")
        .bind(guild_id)
        .execute(pool)
        .await
        .context("failed to clear the queue")?;
    Ok(())
}

pub async fn queue_replace_all(
    pool: &SqlitePool,
    guild_id: &str,
    tracks: &[QueuedTrack],
) -> Result<()> {
    let mut tx = pool
        .begin()
        .await
        .context("failed to start queue replace transaction")?;

    sqlx::query("DELETE FROM guild_session_queue WHERE guild_id = ?1")
        .bind(guild_id)
        .execute(&mut *tx)
        .await
        .context("failed to clear the queue before replacing it")?;

    let indexed_tracks = tracks.iter().enumerate().collect::<Vec<_>>();
    for batch in indexed_tracks.chunks(TRACK_INSERT_BATCH_SIZE) {
        let mut builder: QueryBuilder<Sqlite> = QueryBuilder::new(
            "INSERT INTO guild_session_queue \
             (guild_id, position, video_id, title, channel, duration_secs, requested_by) ",
        );
        builder.push_values(
            batch,
            |mut b, (position, queued): &(usize, &QueuedTrack)| {
                b.push_bind(guild_id)
                    .push_bind(*position as i64)
                    .push_bind(&queued.track.video_id)
                    .push_bind(&queued.track.title)
                    .push_bind(&queued.track.channel)
                    .push_bind(queued.track.duration.map(|d| d.as_secs() as i64))
                    .push_bind(queued.requested_by.to_string());
            },
        );
        builder
            .build()
            .execute(&mut *tx)
            .await
            .context("failed to insert queued tracks")?;
    }

    tx.commit()
        .await
        .context("failed to commit queue replace transaction")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::connect;
    use crate::db::load_guild_session;
    use crate::db::testing::sample_queued;

    #[tokio::test]
    async fn queue_push_back_appends_in_order() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        queue_push_back(&pool, "1", &sample_queued("a", None, 42)).await?;
        queue_push_back(&pool, "1", &sample_queued("b", None, 42)).await?;

        assert_eq!(
            queue_all(&pool, "1")
                .await?
                .iter()
                .map(|q| q.track.video_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );

        Ok(())
    }

    #[tokio::test]
    async fn queue_push_back_creates_a_guild_session_row_if_missing() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        queue_push_back(&pool, "1", &sample_queued("a", None, 42)).await?;

        let session = load_guild_session(&pool, "1")
            .await?
            .expect("row should exist");
        assert_eq!(session.now_playing, None);
        assert!(!session.radio_enabled);

        Ok(())
    }

    #[tokio::test]
    async fn queue_push_many_appends_all_in_order() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        queue_push_back(&pool, "1", &sample_queued("a", None, 42)).await?;
        queue_push_many(
            &pool,
            "1",
            &[sample_queued("b", None, 42), sample_queued("c", None, 42)],
        )
        .await?;

        assert_eq!(
            queue_all(&pool, "1")
                .await?
                .iter()
                .map(|q| q.track.video_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b", "c"]
        );

        Ok(())
    }

    #[tokio::test]
    async fn queue_push_many_is_a_no_op_for_an_empty_slice() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        queue_push_many(&pool, "1", &[]).await?;
        assert_eq!(queue_len(&pool, "1").await?, 0);
        Ok(())
    }

    #[tokio::test]
    async fn queue_pop_front_removes_and_returns_the_earliest_track() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        queue_push_back(&pool, "1", &sample_queued("a", None, 42)).await?;
        queue_push_back(&pool, "1", &sample_queued("b", None, 42)).await?;

        let popped = queue_pop_front(&pool, "1")
            .await?
            .expect("a track should pop");
        assert_eq!(popped.track.video_id, "a");
        assert_eq!(
            queue_all(&pool, "1")
                .await?
                .iter()
                .map(|q| q.track.video_id.as_str())
                .collect::<Vec<_>>(),
            vec!["b"]
        );

        Ok(())
    }

    #[tokio::test]
    async fn queue_pop_front_is_none_on_an_empty_queue() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        assert_eq!(queue_pop_front(&pool, "1").await?, None);
        Ok(())
    }

    #[tokio::test]
    async fn queue_pop_front_skips_a_row_with_an_unparsable_requested_by() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        queue_push_back(&pool, "1", &sample_queued("a", None, 42)).await?;
        sqlx::query(
            "INSERT INTO guild_session_queue \
             (guild_id, position, video_id, title, channel, duration_secs, requested_by) \
             VALUES ('1', 1, 'bad', 'Bad', 'Chan', NULL, 'not-a-number')",
        )
        .execute(&pool)
        .await?;
        queue_push_back(&pool, "1", &sample_queued("b", None, 42)).await?;

        let popped = queue_pop_front(&pool, "1")
            .await?
            .expect("should skip the corrupt row and return the next valid one");
        assert_eq!(popped.track.video_id, "a");

        let popped = queue_pop_front(&pool, "1")
            .await?
            .expect("should skip the corrupt row and return the next valid one");
        assert_eq!(popped.track.video_id, "b");

        assert_eq!(queue_all(&pool, "1").await?, Vec::new());

        Ok(())
    }

    #[tokio::test]
    async fn queue_peek_front_does_not_remove_the_row() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        queue_push_back(&pool, "1", &sample_queued("a", None, 42)).await?;

        let peeked = queue_peek_front(&pool, "1")
            .await?
            .expect("a track should be there");
        assert_eq!(peeked.track.video_id, "a");
        assert_eq!(queue_len(&pool, "1").await?, 1);

        Ok(())
    }

    #[tokio::test]
    async fn queue_clear_empties_the_queue() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        queue_push_back(&pool, "1", &sample_queued("a", None, 42)).await?;

        queue_clear(&pool, "1").await?;

        assert_eq!(queue_len(&pool, "1").await?, 0);
        Ok(())
    }

    #[tokio::test]
    async fn queue_replace_all_reorders_and_renumbers() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        queue_push_back(&pool, "1", &sample_queued("a", None, 42)).await?;
        queue_push_back(&pool, "1", &sample_queued("b", None, 42)).await?;

        queue_replace_all(
            &pool,
            "1",
            &[sample_queued("b", None, 42), sample_queued("a", None, 42)],
        )
        .await?;

        assert_eq!(
            queue_all(&pool, "1")
                .await?
                .iter()
                .map(|q| q.track.video_id.as_str())
                .collect::<Vec<_>>(),
            vec!["b", "a"]
        );

        Ok(())
    }
}
