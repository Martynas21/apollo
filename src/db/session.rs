use anyhow::{Context, Result};
use serenity::all::UserId;
use sqlx::sqlite::SqlitePool;
use std::time::Duration;

use crate::model::{QueuedTrack, Track};

/// What a guild keeps besides its queue: its radio settings and the track it
/// played last. Nothing here is ever started on its own — the queue holds
/// every track a guild is going to play, the current one included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedSession {
    pub radio_enabled: bool,
    pub radio_requested_by: Option<String>,
    pub radio_history: Vec<String>,
    pub last_played: Option<(Track, String)>,
}

fn encode_radio_history(history: &[String]) -> String {
    history.join(",")
}

fn decode_radio_history(raw: &str) -> Vec<String> {
    raw.split(',')
        .filter(|segment| !segment.is_empty())
        .map(str::to_string)
        .collect()
}

type SessionTrackColumns<'a> = (
    Option<&'a str>,
    Option<&'a str>,
    Option<&'a str>,
    Option<i64>,
    Option<&'a str>,
);

fn session_track_columns<'a>(track: Option<(&'a Track, &'a str)>) -> SessionTrackColumns<'a> {
    match track {
        Some((track, requested_by)) => (
            Some(track.video_id.as_str()),
            Some(track.title.as_str()),
            Some(track.channel.as_str()),
            #[allow(clippy::cast_possible_wrap)]
            track.duration.map(|d| d.as_secs() as i64),
            Some(requested_by),
        ),
        None => (None, None, None, None, None),
    }
}

pub async fn save_guild_session_meta(
    pool: &SqlitePool,
    guild_id: &str,
    radio_enabled: bool,
    radio_requested_by: Option<&str>,
    radio_history: &[String],
    last_played: Option<(&Track, &str)>,
) -> Result<()> {
    let (lp_video_id, lp_title, lp_channel, lp_duration_secs, lp_requested_by) =
        session_track_columns(last_played);

    sqlx::query(
        "INSERT INTO guild_sessions \
         (guild_id, radio_enabled, radio_requested_by, radio_history, \
          last_played_video_id, last_played_title, last_played_channel, \
          last_played_duration_secs, last_played_requested_by) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(guild_id) DO UPDATE SET
             radio_enabled = excluded.radio_enabled,
             radio_requested_by = excluded.radio_requested_by,
             radio_history = excluded.radio_history,
             last_played_video_id = excluded.last_played_video_id,
             last_played_title = excluded.last_played_title,
             last_played_channel = excluded.last_played_channel,
             last_played_duration_secs = excluded.last_played_duration_secs,
             last_played_requested_by = excluded.last_played_requested_by",
    )
    .bind(guild_id)
    .bind(radio_enabled)
    .bind(radio_requested_by)
    .bind(encode_radio_history(radio_history))
    .bind(lp_video_id)
    .bind(lp_title)
    .bind(lp_channel)
    .bind(lp_duration_secs)
    .bind(lp_requested_by)
    .execute(pool)
    .await
    .context("failed to save guild session")?;

    Ok(())
}

fn track_from_columns(
    video_id: Option<String>,
    title: Option<String>,
    channel: Option<String>,
    duration_secs: Option<i64>,
    requested_by: Option<String>,
) -> Option<(Track, String)> {
    match (video_id, title, channel, requested_by) {
        (Some(video_id), Some(title), Some(channel), Some(requested_by)) => {
            #[allow(clippy::cast_sign_loss)]
            let track = Track {
                video_id,
                title,
                channel,
                duration: duration_secs.map(|secs| Duration::from_secs(secs as u64)),
            };
            Some((track, requested_by))
        }
        _ => None,
    }
}

type GuildSessionRow = (
    bool,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<String>,
);

async fn fetch_guild_session_row(
    pool: &SqlitePool,
    guild_id: &str,
) -> Result<Option<GuildSessionRow>> {
    sqlx::query_as(
        "SELECT radio_enabled, radio_requested_by, radio_history, \
                last_played_video_id, last_played_title, last_played_channel, \
                last_played_duration_secs, last_played_requested_by \
         FROM guild_sessions WHERE guild_id = ?1",
    )
    .bind(guild_id)
    .fetch_optional(pool)
    .await
    .context("failed to fetch guild session")
}

pub async fn load_guild_session(
    pool: &SqlitePool,
    guild_id: &str,
) -> Result<Option<PersistedSession>> {
    let Some((
        radio_enabled,
        radio_requested_by,
        radio_history,
        lp_video_id,
        lp_title,
        lp_channel,
        lp_duration_secs,
        lp_requested_by,
    )) = fetch_guild_session_row(pool, guild_id).await?
    else {
        return Ok(None);
    };

    let last_played = track_from_columns(
        lp_video_id,
        lp_title,
        lp_channel,
        lp_duration_secs,
        lp_requested_by,
    );

    Ok(Some(PersistedSession {
        radio_enabled,
        radio_requested_by,
        radio_history: decode_radio_history(&radio_history),
        last_played,
    }))
}

/// Writes just the radio flag, leaving the rest of the session — its radio
/// seed, history and last-played track — as it stands. What the dashboard
/// reads back is what the next join restores, whether or not a guild had a
/// live session to persist at the time.
pub async fn set_guild_radio_enabled(
    pool: &SqlitePool,
    guild_id: &str,
    enabled: bool,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO guild_sessions (guild_id, radio_enabled, radio_requested_by, radio_history) \
         VALUES (?1, ?2, NULL, '') \
         ON CONFLICT(guild_id) DO UPDATE SET radio_enabled = excluded.radio_enabled",
    )
    .bind(guild_id)
    .bind(enabled)
    .execute(pool)
    .await
    .context("failed to save the radio setting")?;

    Ok(())
}

pub async fn clear_guild_session(pool: &SqlitePool, guild_id: &str) -> Result<()> {
    sqlx::query("DELETE FROM guild_sessions WHERE guild_id = ?1")
        .bind(guild_id)
        .execute(pool)
        .await
        .context("failed to clear guild session")?;

    Ok(())
}

pub(super) fn queued_track_from_row(
    row: (String, String, String, Option<i64>, String),
) -> Option<QueuedTrack> {
    let (video_id, title, channel, duration_secs, requested_by) = row;
    let requested_by = requested_by.parse::<u64>().ok()?;
    #[allow(clippy::cast_sign_loss)]
    let track = Track {
        video_id,
        title,
        channel,
        duration: duration_secs.map(|secs| Duration::from_secs(secs as u64)),
    };
    Some(QueuedTrack {
        track,
        requested_by: UserId::new(requested_by),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::connect;
    use crate::db::testing::{sample_queued, sample_track};
    use crate::db::{queue_all, queue_push_back};

    fn sample_session(last_played: Option<(Track, &str)>) -> PersistedSession {
        PersistedSession {
            radio_enabled: true,
            radio_requested_by: Some("42".to_string()),
            radio_history: vec!["a".to_string(), "b".to_string()],
            last_played: last_played.map(|(track, requested_by)| (track, requested_by.to_string())),
        }
    }

    async fn save_session(
        pool: &SqlitePool,
        guild_id: &str,
        session: &PersistedSession,
    ) -> Result<()> {
        save_guild_session_meta(
            pool,
            guild_id,
            session.radio_enabled,
            session.radio_requested_by.as_deref(),
            &session.radio_history,
            session
                .last_played
                .as_ref()
                .map(|(track, requested_by)| (track, requested_by.as_str())),
        )
        .await
    }

    #[tokio::test]
    async fn load_guild_session_none_when_never_saved() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        assert_eq!(load_guild_session(&pool, "1").await?, None);
        Ok(())
    }

    #[tokio::test]
    async fn guild_session_save_and_load_round_trip() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        let session = sample_session(Some((
            sample_track("a", Some(Duration::from_secs(30))),
            "42",
        )));

        save_session(&pool, "1", &session).await?;

        assert_eq!(load_guild_session(&pool, "1").await?, Some(session));
        assert_eq!(load_guild_session(&pool, "2").await?, None);

        Ok(())
    }

    #[tokio::test]
    async fn guild_session_keeps_its_radio_settings_without_a_last_played() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;

        save_session(&pool, "1", &sample_session(None)).await?;

        let loaded = load_guild_session(&pool, "1").await?.expect("row exists");
        assert_eq!(loaded.last_played, None);
        assert!(loaded.radio_enabled);
        assert_eq!(loaded.radio_history, vec!["a".to_string(), "b".to_string()]);

        Ok(())
    }

    #[tokio::test]
    async fn guild_session_save_replaces_rather_than_appending() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        save_session(
            &pool,
            "1",
            &sample_session(Some((sample_track("old", None), "42"))),
        )
        .await?;

        let replacement = sample_session(Some((sample_track("new", None), "42")));
        save_session(&pool, "1", &replacement).await?;

        assert_eq!(load_guild_session(&pool, "1").await?, Some(replacement));

        Ok(())
    }

    #[tokio::test]
    async fn saving_meta_leaves_the_queue_alone() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        queue_push_back(&pool, "1", &sample_queued("b", None, 42)).await?;

        save_session(
            &pool,
            "1",
            &sample_session(Some((sample_track("a", None), "42"))),
        )
        .await?;

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
    async fn clear_guild_session_removes_it_and_its_queue() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        save_session(
            &pool,
            "1",
            &sample_session(Some((sample_track("a", None), "42"))),
        )
        .await?;
        queue_push_back(&pool, "1", &sample_queued("b", None, 42)).await?;

        clear_guild_session(&pool, "1").await?;

        assert_eq!(load_guild_session(&pool, "1").await?, None);
        assert_eq!(queue_all(&pool, "1").await?, Vec::new());

        Ok(())
    }

    #[tokio::test]
    async fn clear_guild_session_is_a_no_op_when_nothing_was_saved() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        clear_guild_session(&pool, "1").await?;
        assert_eq!(load_guild_session(&pool, "1").await?, None);
        Ok(())
    }
}
