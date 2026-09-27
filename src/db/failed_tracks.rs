//! Tracks that never produced audio for a guild, and the replacements a guild
//! chose for them. A failed row lives until it is dismissed, replaced, or the
//! same video later starts successfully. An override lives for as long as one
//! of the guild's saved playlists still contains the original video: pruning
//! after a playlist refresh or removal drops the rest, including overrides
//! whose original only ever came from search or radio.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::sqlite::SqlitePool;

use crate::model::{QueuedTrack, Track};

/// One saved replacement: `replacement` is queued whenever `original` would be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackOverride {
    pub original: Track,
    pub replacement: Track,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedTrack {
    pub track: Track,
    pub requested_by: String,
    pub error: String,
    pub failed_at: i64,
}

type TrackRow = (String, String, String, Option<i64>);
type FailedRow = (String, String, String, Option<i64>, String, String, i64);
type OverrideRow = (
    String,
    String,
    String,
    Option<i64>,
    String,
    String,
    String,
    Option<i64>,
);

fn track_from_row((video_id, title, channel, duration_secs): TrackRow) -> Track {
    Track {
        video_id,
        title,
        channel,
        #[allow(clippy::cast_sign_loss)]
        duration: duration_secs.map(|secs| Duration::from_secs(secs as u64)),
    }
}

pub async fn record_failed_track(
    pool: &SqlitePool,
    guild_id: &str,
    queued: &QueuedTrack,
    error: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO failed_tracks \
         (guild_id, video_id, title, channel, duration_secs, requested_by, error, failed_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, unixepoch())
         ON CONFLICT(guild_id, video_id) DO UPDATE SET
             title = excluded.title,
             channel = excluded.channel,
             duration_secs = excluded.duration_secs,
             requested_by = excluded.requested_by,
             error = excluded.error,
             failed_at = excluded.failed_at",
    )
    .bind(guild_id)
    .bind(&queued.track.video_id)
    .bind(&queued.track.title)
    .bind(&queued.track.channel)
    .bind(queued.track.duration.map(|d| d.as_secs() as i64))
    .bind(queued.requested_by.to_string())
    .bind(error)
    .execute(pool)
    .await
    .context("failed to record failed track")?;

    Ok(())
}

pub async fn failed_tracks(pool: &SqlitePool, guild_id: &str) -> Result<Vec<FailedTrack>> {
    let rows: Vec<FailedRow> = sqlx::query_as(
        "SELECT video_id, title, channel, duration_secs, requested_by, error, failed_at \
         FROM failed_tracks WHERE guild_id = ?1 ORDER BY failed_at DESC, video_id",
    )
    .bind(guild_id)
    .fetch_all(pool)
    .await
    .context("failed to fetch failed tracks")?;

    Ok(rows
        .into_iter()
        .map(
            |(video_id, title, channel, duration_secs, requested_by, error, failed_at)| {
                FailedTrack {
                    track: track_from_row((video_id, title, channel, duration_secs)),
                    requested_by,
                    error,
                    failed_at,
                }
            },
        )
        .collect())
}

pub async fn delete_failed_track(
    pool: &SqlitePool,
    guild_id: &str,
    video_id: &str,
) -> Result<bool> {
    let result = sqlx::query("DELETE FROM failed_tracks WHERE guild_id = ?1 AND video_id = ?2")
        .bind(guild_id)
        .bind(video_id)
        .execute(pool)
        .await
        .context("failed to delete failed track")?;

    Ok(result.rows_affected() > 0)
}

/// Saves `replacement` as the guild's stand-in for `original`. Any override
/// that currently points at the original is redirected too, so a replacement
/// that itself gets replaced never leaves a chain behind.
pub async fn save_track_override(
    pool: &SqlitePool,
    guild_id: &str,
    original: &Track,
    replacement: &Track,
) -> Result<()> {
    let mut tx = pool
        .begin()
        .await
        .context("failed to start track override transaction")?;
    let original_video_id = &original.video_id;
    let duration_secs = replacement.duration.map(|d| d.as_secs() as i64);

    sqlx::query(
        "INSERT INTO track_overrides \
         (guild_id, original_video_id, original_title, original_channel, \
          original_duration_secs, video_id, title, channel, duration_secs, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, unixepoch())
         ON CONFLICT(guild_id, original_video_id) DO UPDATE SET
             original_title = excluded.original_title,
             original_channel = excluded.original_channel,
             original_duration_secs = excluded.original_duration_secs,
             video_id = excluded.video_id,
             title = excluded.title,
             channel = excluded.channel,
             duration_secs = excluded.duration_secs,
             created_at = excluded.created_at",
    )
    .bind(guild_id)
    .bind(original_video_id)
    .bind(&original.title)
    .bind(&original.channel)
    .bind(original.duration.map(|d| d.as_secs() as i64))
    .bind(&replacement.video_id)
    .bind(&replacement.title)
    .bind(&replacement.channel)
    .bind(duration_secs)
    .execute(&mut *tx)
    .await
    .context("failed to save track override")?;

    sqlx::query(
        "UPDATE track_overrides SET video_id = ?3, title = ?4, channel = ?5, duration_secs = ?6 \
         WHERE guild_id = ?1 AND video_id = ?2 AND original_video_id != ?2",
    )
    .bind(guild_id)
    .bind(original_video_id)
    .bind(&replacement.video_id)
    .bind(&replacement.title)
    .bind(&replacement.channel)
    .bind(duration_secs)
    .execute(&mut *tx)
    .await
    .context("failed to redirect chained track overrides")?;

    tx.commit()
        .await
        .context("failed to commit track override transaction")?;
    Ok(())
}

/// The guild's overrides, keyed by the original video id.
pub async fn track_overrides(pool: &SqlitePool, guild_id: &str) -> Result<HashMap<String, Track>> {
    let rows: Vec<(String, String, String, String, Option<i64>)> = sqlx::query_as(
        "SELECT original_video_id, video_id, title, channel, duration_secs \
         FROM track_overrides WHERE guild_id = ?1",
    )
    .bind(guild_id)
    .fetch_all(pool)
    .await
    .context("failed to fetch track overrides")?;

    Ok(rows
        .into_iter()
        .map(|(original, video_id, title, channel, duration_secs)| {
            (
                original,
                track_from_row((video_id, title, channel, duration_secs)),
            )
        })
        .collect())
}

/// Every override of the guild, newest first. The original is described by
/// its copy in one of the guild's saved playlists, so a playlist refresh
/// keeps it current; only an original no playlist holds falls back to the
/// name recorded when the override was saved, or to its video id.
pub async fn list_track_overrides(pool: &SqlitePool, guild_id: &str) -> Result<Vec<TrackOverride>> {
    let rows: Vec<OverrideRow> = sqlx::query_as(
        "SELECT o.original_video_id, \
             COALESCE(pt.title, NULLIF(o.original_title, ''), o.original_video_id), \
             COALESCE(pt.channel, NULLIF(o.original_channel, ''), ''), \
             COALESCE(pt.duration_secs, o.original_duration_secs), \
             o.video_id, o.title, o.channel, o.duration_secs \
         FROM track_overrides o \
         LEFT JOIN playlist_tracks pt ON pt.rowid = (SELECT MIN(c.rowid) FROM playlist_tracks c \
             JOIN playlists p ON p.id = c.playlist_id \
             WHERE p.guild_id = o.guild_id AND c.video_id = o.original_video_id) \
         WHERE o.guild_id = ?1 \
         ORDER BY o.created_at DESC, o.original_video_id",
    )
    .bind(guild_id)
    .fetch_all(pool)
    .await
    .context("failed to list track overrides")?;

    Ok(rows
        .into_iter()
        .map(
            |(o_id, o_title, o_channel, o_secs, video_id, title, channel, duration_secs)| {
                TrackOverride {
                    original: track_from_row((o_id, o_title, o_channel, o_secs)),
                    replacement: track_from_row((video_id, title, channel, duration_secs)),
                }
            },
        )
        .collect())
}

pub async fn delete_track_override(
    pool: &SqlitePool,
    guild_id: &str,
    original_video_id: &str,
) -> Result<bool> {
    let result =
        sqlx::query("DELETE FROM track_overrides WHERE guild_id = ?1 AND original_video_id = ?2")
            .bind(guild_id)
            .bind(original_video_id)
            .execute(pool)
            .await
            .context("failed to delete track override")?;

    Ok(result.rows_affected() > 0)
}

/// Drops the guild's overrides whose original video is no longer in any of
/// its saved playlists. Returns how many were dropped.
pub async fn prune_track_overrides(pool: &SqlitePool, guild_id: &str) -> Result<u64> {
    let result = sqlx::query(
        "DELETE FROM track_overrides WHERE guild_id = ?1 AND original_video_id NOT IN (
             SELECT pt.video_id FROM playlist_tracks pt
             JOIN playlists p ON p.id = pt.playlist_id
             WHERE p.guild_id = ?1)",
    )
    .bind(guild_id)
    .execute(pool)
    .await
    .context("failed to prune track overrides")?;

    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::testing::{sample_queued, sample_track};
    use crate::db::{connect, replace_playlist_tracks, save_guild_playlist};

    fn queued(video_id: &str) -> QueuedTrack {
        sample_queued(video_id, Some(Duration::from_secs(120)), 42)
    }

    #[tokio::test]
    async fn recorded_failures_are_listed_with_their_error() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        record_failed_track(&pool, "1", &queued("a"), "video is unavailable").await?;

        let failed = failed_tracks(&pool, "1").await?;
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].track, queued("a").track);
        assert_eq!(failed[0].requested_by, "42");
        assert_eq!(failed[0].error, "video is unavailable");
        Ok(())
    }

    #[tokio::test]
    async fn recording_the_same_video_again_keeps_one_row_with_the_latest_error() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        record_failed_track(&pool, "1", &queued("a"), "first").await?;
        record_failed_track(&pool, "1", &queued("a"), "second").await?;

        let failed = failed_tracks(&pool, "1").await?;
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].error, "second");
        Ok(())
    }

    #[tokio::test]
    async fn failed_tracks_are_scoped_to_their_guild() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        record_failed_track(&pool, "1", &queued("a"), "err").await?;
        record_failed_track(&pool, "2", &queued("b"), "err").await?;

        let ids: Vec<String> = failed_tracks(&pool, "1")
            .await?
            .into_iter()
            .map(|f| f.track.video_id)
            .collect();
        assert_eq!(ids, vec!["a".to_string()]);
        Ok(())
    }

    #[tokio::test]
    async fn deleting_reports_whether_a_row_existed() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        record_failed_track(&pool, "1", &queued("a"), "err").await?;

        assert!(delete_failed_track(&pool, "1", "a").await?);
        assert!(!delete_failed_track(&pool, "1", "a").await?);
        assert!(failed_tracks(&pool, "1").await?.is_empty());
        Ok(())
    }

    async fn save(
        pool: &SqlitePool,
        guild_id: &str,
        original: &str,
        replacement: &str,
    ) -> Result<()> {
        save_track_override(
            pool,
            guild_id,
            &sample_track(original, None),
            &sample_track(replacement, None),
        )
        .await
    }

    #[tokio::test]
    async fn overrides_are_saved_per_guild_and_replaced_on_conflict() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        save(&pool, "1", "a", "b").await?;
        save(&pool, "1", "a", "c").await?;
        save(&pool, "2", "a", "d").await?;

        let overrides = track_overrides(&pool, "1").await?;
        assert_eq!(overrides.len(), 1);
        assert_eq!(overrides["a"], sample_track("c", None));
        Ok(())
    }

    #[tokio::test]
    async fn overrides_are_listed_with_both_sides_named_and_can_be_deleted() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        save(&pool, "1", "a", "b").await?;
        save(&pool, "2", "x", "y").await?;

        let listed = list_track_overrides(&pool, "1").await?;
        assert_eq!(
            listed,
            vec![TrackOverride {
                original: sample_track("a", None),
                replacement: sample_track("b", None),
            }]
        );

        assert!(delete_track_override(&pool, "1", "a").await?);
        assert!(!delete_track_override(&pool, "1", "a").await?);
        assert!(list_track_overrides(&pool, "1").await?.is_empty());
        assert_eq!(list_track_overrides(&pool, "2").await?.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn an_original_is_described_by_its_saved_playlist_copy() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        let playlist = save_guild_playlist(&pool, "1", "mix", "https://p", None, "42").await?;
        let in_playlist = Track {
            video_id: "a".to_string(),
            title: "Refreshed title".to_string(),
            channel: "Refreshed channel".to_string(),
            duration: Some(Duration::from_secs(9)),
        };
        replace_playlist_tracks(&pool, playlist, std::slice::from_ref(&in_playlist)).await?;
        save_track_override(&pool, "1", &unnamed("a"), &sample_track("b", None)).await?;
        save_track_override(
            &pool,
            "1",
            &sample_track("z", None),
            &sample_track("y", None),
        )
        .await?;
        save_track_override(&pool, "1", &unnamed("q"), &sample_track("y", None)).await?;

        let listed = list_track_overrides(&pool, "1").await?;
        let original = |id: &str| listed.iter().find(|m| m.original.video_id == id).unwrap();
        assert_eq!(original("a").original, in_playlist);
        assert_eq!(original("z").original, sample_track("z", None));
        assert_eq!(original("q").original.title, "q");
        Ok(())
    }

    fn unnamed(video_id: &str) -> Track {
        Track {
            video_id: video_id.to_string(),
            title: String::new(),
            channel: String::new(),
            duration: None,
        }
    }

    #[tokio::test]
    async fn replacing_a_replacement_redirects_the_original_override() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        save(&pool, "1", "a", "b").await?;
        save(&pool, "1", "b", "c").await?;

        let overrides = track_overrides(&pool, "1").await?;
        assert_eq!(overrides["a"], sample_track("c", None));
        assert_eq!(overrides["b"], sample_track("c", None));
        Ok(())
    }

    #[tokio::test]
    async fn pruning_keeps_overrides_whose_original_is_still_in_a_saved_playlist() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        let playlist = save_guild_playlist(&pool, "1", "mix", "https://p", None, "42").await?;
        replace_playlist_tracks(&pool, playlist, &[sample_track("a", None)]).await?;
        save(&pool, "1", "a", "x").await?;
        save(&pool, "1", "b", "y").await?;
        save(&pool, "2", "b", "z").await?;

        assert_eq!(prune_track_overrides(&pool, "1").await?, 1);

        let kept: Vec<String> = track_overrides(&pool, "1").await?.into_keys().collect();
        assert_eq!(kept, vec!["a".to_string()]);
        assert_eq!(track_overrides(&pool, "2").await?.len(), 1);
        Ok(())
    }
}
