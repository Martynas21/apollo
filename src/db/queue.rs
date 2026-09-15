use anyhow::{Context, Result};
use sqlx::sqlite::SqlitePool;
use sqlx::{QueryBuilder, Sqlite};

use crate::model::QueuedTrack;

use super::TRACK_INSERT_BATCH_SIZE;
use super::session::queued_track_from_row;

/// A guild's queue holds the track it is currently playing at the head,
/// followed by the upcoming tracks in order. Starting a track leaves its row
/// where it is; only ending the track removes it. Nothing a guild is going
/// to play therefore lives outside this table.
///
/// Rows whose `requested_by` is not a plain snowflake cannot be turned back
/// into a track, so every read skips them instead of letting one wedge the
/// head of a queue; the next `queue_finish_current` or `queue_replace_all`
/// clears them out.
macro_rules! usable_row {
    () => {
        "requested_by NOT GLOB '*[^0-9]*' AND length(requested_by) BETWEEN 1 AND 20"
    };
}

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

    ensure_guild_session_row(&mut tx, guild_id).await?;

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

/// Puts a track in at the head of the queue, which is where the track a
/// guild is playing lives.
pub async fn queue_push_front(
    pool: &SqlitePool,
    guild_id: &str,
    queued: &QueuedTrack,
) -> Result<()> {
    let mut tx = pool
        .begin()
        .await
        .context("failed to start queue push transaction")?;

    ensure_guild_session_row(&mut tx, guild_id).await?;

    sqlx::query(
        "INSERT INTO guild_session_queue \
         (guild_id, position, video_id, title, channel, duration_secs, requested_by) \
         VALUES (?1, (SELECT COALESCE(MIN(position), 1) - 1 FROM guild_session_queue WHERE guild_id = ?1), \
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

async fn ensure_guild_session_row(tx: &mut sqlx::SqliteConnection, guild_id: &str) -> Result<()> {
    sqlx::query(
        "INSERT OR IGNORE INTO guild_sessions (guild_id, radio_enabled, radio_requested_by, radio_history) \
         VALUES (?1, 0, NULL, '')",
    )
    .bind(guild_id)
    .execute(&mut *tx)
    .await
    .context("failed to ensure guild session row")?;
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

    ensure_guild_session_row(&mut tx, guild_id).await?;

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

/// The track at the head of the queue: the one the guild is playing, or the
/// one it starts next when nothing is playing yet.
pub async fn queue_current(pool: &SqlitePool, guild_id: &str) -> Result<Option<QueuedTrack>> {
    let row: Option<(String, String, String, Option<i64>, String)> = sqlx::query_as(concat!(
        "SELECT video_id, title, channel, duration_secs, requested_by \
         FROM guild_session_queue WHERE guild_id = ?1 AND ",
        usable_row!(),
        " ORDER BY position LIMIT 1"
    ))
    .bind(guild_id)
    .fetch_optional(pool)
    .await
    .context("failed to read the head of the queue")?;

    Ok(row.and_then(queued_track_from_row))
}

/// The first track behind the head — what plays after the current track.
pub async fn queue_upcoming_front(
    pool: &SqlitePool,
    guild_id: &str,
) -> Result<Option<QueuedTrack>> {
    let row: Option<(String, String, String, Option<i64>, String)> = sqlx::query_as(concat!(
        "SELECT video_id, title, channel, duration_secs, requested_by \
         FROM guild_session_queue WHERE guild_id = ?1 AND ",
        usable_row!(),
        " ORDER BY position LIMIT 1 OFFSET 1"
    ))
    .bind(guild_id)
    .fetch_optional(pool)
    .await
    .context("failed to read the first upcoming track")?;

    Ok(row.and_then(queued_track_from_row))
}

/// Drops the head of the queue — the track that has just finished, failed
/// or been given up on — and returns the one that takes its place. Rows
/// ahead of the head that no read can use are dropped along with it.
pub async fn queue_finish_current(
    pool: &SqlitePool,
    guild_id: &str,
) -> Result<Option<QueuedTrack>> {
    let mut tx = pool
        .begin()
        .await
        .context("failed to start queue advance transaction")?;

    let head: Option<(i64,)> = sqlx::query_as(concat!(
        "SELECT position FROM guild_session_queue WHERE guild_id = ?1 AND ",
        usable_row!(),
        " ORDER BY position LIMIT 1"
    ))
    .bind(guild_id)
    .fetch_optional(&mut *tx)
    .await
    .context("failed to read the head of the queue")?;

    match head {
        Some((position,)) => {
            sqlx::query("DELETE FROM guild_session_queue WHERE guild_id = ?1 AND position <= ?2")
                .bind(guild_id)
                .bind(position)
                .execute(&mut *tx)
                .await
                .context("failed to remove the finished track")?;
        }
        None => {
            sqlx::query("DELETE FROM guild_session_queue WHERE guild_id = ?1")
                .bind(guild_id)
                .execute(&mut *tx)
                .await
                .context("failed to clear an unusable queue")?;
        }
    }

    let row: Option<(String, String, String, Option<i64>, String)> = sqlx::query_as(concat!(
        "SELECT video_id, title, channel, duration_secs, requested_by \
         FROM guild_session_queue WHERE guild_id = ?1 AND ",
        usable_row!(),
        " ORDER BY position LIMIT 1"
    ))
    .bind(guild_id)
    .fetch_optional(&mut *tx)
    .await
    .context("failed to read the new head of the queue")?;

    tx.commit()
        .await
        .context("failed to commit queue advance transaction")?;
    Ok(row.and_then(queued_track_from_row))
}

/// The current track and everything behind it.
pub async fn queue_len(pool: &SqlitePool, guild_id: &str) -> Result<usize> {
    let (count,): (i64,) = sqlx::query_as(concat!(
        "SELECT COUNT(*) FROM guild_session_queue WHERE guild_id = ?1 AND ",
        usable_row!()
    ))
    .bind(guild_id)
    .fetch_one(pool)
    .await
    .context("failed to count queued tracks")?;
    #[allow(clippy::cast_sign_loss)]
    Ok(count.max(0) as usize)
}

/// The current track first, then the upcoming ones.
pub async fn queue_all(pool: &SqlitePool, guild_id: &str) -> Result<Vec<QueuedTrack>> {
    let rows: Vec<(String, String, String, Option<i64>, String)> = sqlx::query_as(concat!(
        "SELECT video_id, title, channel, duration_secs, requested_by \
         FROM guild_session_queue WHERE guild_id = ?1 AND ",
        usable_row!(),
        " ORDER BY position"
    ))
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

    async fn video_ids(pool: &SqlitePool, guild_id: &str) -> Result<Vec<String>> {
        Ok(queue_all(pool, guild_id)
            .await?
            .into_iter()
            .map(|q| q.track.video_id)
            .collect())
    }

    async fn insert_unusable_row(pool: &SqlitePool, guild_id: &str, position: i64) -> Result<()> {
        sqlx::query(
            "INSERT OR IGNORE INTO guild_sessions \n             (guild_id, radio_enabled, radio_requested_by, radio_history) \n             VALUES (?1, 0, NULL, '')",
        )
        .bind(guild_id)
        .execute(pool)
        .await?;
        sqlx::query(
            "INSERT INTO guild_session_queue \
             (guild_id, position, video_id, title, channel, duration_secs, requested_by) \
             VALUES (?1, ?2, 'bad', 'Bad', 'Chan', NULL, 'not-a-number')",
        )
        .bind(guild_id)
        .bind(position)
        .execute(pool)
        .await?;
        Ok(())
    }

    #[tokio::test]
    async fn queue_push_back_appends_in_order() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        queue_push_back(&pool, "1", &sample_queued("a", None, 42)).await?;
        queue_push_back(&pool, "1", &sample_queued("b", None, 42)).await?;

        assert_eq!(video_ids(&pool, "1").await?, vec!["a", "b"]);

        Ok(())
    }

    #[tokio::test]
    async fn queue_push_front_puts_the_track_ahead_of_the_rest() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        queue_push_back(&pool, "1", &sample_queued("a", None, 42)).await?;
        queue_push_back(&pool, "1", &sample_queued("b", None, 42)).await?;

        queue_push_front(&pool, "1", &sample_queued("first", None, 42)).await?;

        assert_eq!(video_ids(&pool, "1").await?, vec!["first", "a", "b"]);
        Ok(())
    }

    #[tokio::test]
    async fn queue_push_front_onto_an_empty_queue_creates_a_guild_session_row() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        queue_push_front(&pool, "1", &sample_queued("a", None, 42)).await?;

        assert_eq!(video_ids(&pool, "1").await?, vec!["a"]);
        assert!(load_guild_session(&pool, "1").await?.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn repeated_push_front_keeps_stacking_ahead_of_the_queue() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        for id in ["c", "b", "a"] {
            queue_push_front(&pool, "1", &sample_queued(id, None, 42)).await?;
        }

        assert_eq!(video_ids(&pool, "1").await?, vec!["a", "b", "c"]);
        Ok(())
    }

    #[tokio::test]
    async fn queue_push_back_creates_a_guild_session_row_if_missing() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        queue_push_back(&pool, "1", &sample_queued("a", None, 42)).await?;

        let session = load_guild_session(&pool, "1")
            .await?
            .expect("row should exist");
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

        assert_eq!(video_ids(&pool, "1").await?, vec!["a", "b", "c"]);

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
    async fn queue_current_is_the_head_and_leaves_it_in_place() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        queue_push_back(&pool, "1", &sample_queued("a", None, 42)).await?;
        queue_push_back(&pool, "1", &sample_queued("b", None, 42)).await?;

        let current = queue_current(&pool, "1").await?.expect("a head exists");
        assert_eq!(current.track.video_id, "a");
        assert_eq!(queue_len(&pool, "1").await?, 2);

        Ok(())
    }

    #[tokio::test]
    async fn queue_upcoming_front_is_the_track_behind_the_head() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        queue_push_back(&pool, "1", &sample_queued("a", None, 42)).await?;
        queue_push_back(&pool, "1", &sample_queued("b", None, 42)).await?;

        let next = queue_upcoming_front(&pool, "1")
            .await?
            .expect("a second track exists");
        assert_eq!(next.track.video_id, "b");

        Ok(())
    }

    #[tokio::test]
    async fn queue_upcoming_front_is_none_when_only_the_current_track_is_queued() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        queue_push_back(&pool, "1", &sample_queued("a", None, 42)).await?;

        assert_eq!(queue_upcoming_front(&pool, "1").await?, None);
        Ok(())
    }

    #[tokio::test]
    async fn queue_finish_current_drops_the_head_and_returns_the_next() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        queue_push_back(&pool, "1", &sample_queued("a", None, 42)).await?;
        queue_push_back(&pool, "1", &sample_queued("b", None, 42)).await?;

        let next = queue_finish_current(&pool, "1")
            .await?
            .expect("a track should follow");
        assert_eq!(next.track.video_id, "b");
        assert_eq!(video_ids(&pool, "1").await?, vec!["b"]);

        Ok(())
    }

    #[tokio::test]
    async fn queue_finish_current_empties_the_last_track_out() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        queue_push_back(&pool, "1", &sample_queued("a", None, 42)).await?;

        assert_eq!(queue_finish_current(&pool, "1").await?, None);
        assert_eq!(queue_len(&pool, "1").await?, 0);

        Ok(())
    }

    #[tokio::test]
    async fn queue_finish_current_is_none_on_an_empty_queue() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        assert_eq!(queue_finish_current(&pool, "1").await?, None);
        Ok(())
    }

    #[tokio::test]
    async fn reads_skip_a_row_with_an_unusable_requested_by() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        insert_unusable_row(&pool, "1", 0).await?;
        queue_push_back(&pool, "1", &sample_queued("a", None, 42)).await?;
        queue_push_back(&pool, "1", &sample_queued("b", None, 42)).await?;

        assert_eq!(
            queue_current(&pool, "1").await?.map(|q| q.track.video_id),
            Some("a".to_string())
        );
        assert_eq!(
            queue_upcoming_front(&pool, "1")
                .await?
                .map(|q| q.track.video_id),
            Some("b".to_string())
        );
        assert_eq!(video_ids(&pool, "1").await?, vec!["a", "b"]);
        assert_eq!(queue_len(&pool, "1").await?, 2);

        Ok(())
    }

    #[tokio::test]
    async fn queue_finish_current_clears_unusable_rows_ahead_of_the_head() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        insert_unusable_row(&pool, "1", 0).await?;
        queue_push_back(&pool, "1", &sample_queued("a", None, 42)).await?;
        queue_push_back(&pool, "1", &sample_queued("b", None, 42)).await?;

        let next = queue_finish_current(&pool, "1")
            .await?
            .expect("a track should follow");

        assert_eq!(next.track.video_id, "b");
        let (rows,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM guild_session_queue WHERE guild_id = '1'")
                .fetch_one(&pool)
                .await?;
        assert_eq!(rows, 1, "the unusable row should be gone too");

        Ok(())
    }

    #[tokio::test]
    async fn queue_finish_current_clears_a_queue_of_nothing_but_unusable_rows() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        insert_unusable_row(&pool, "1", 0).await?;

        assert_eq!(queue_finish_current(&pool, "1").await?, None);
        let (rows,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM guild_session_queue WHERE guild_id = '1'")
                .fetch_one(&pool)
                .await?;
        assert_eq!(rows, 0);

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

        assert_eq!(video_ids(&pool, "1").await?, vec!["b", "a"]);

        Ok(())
    }
}
