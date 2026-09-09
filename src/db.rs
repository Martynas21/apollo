use anyhow::{Context, Result};
use serenity::all::UserId;
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteSynchronous,
};
use sqlx::{QueryBuilder, Sqlite};
use std::str::FromStr;
use std::time::Duration;

use crate::voice::player::QueuedTrack;
use crate::youtube::api::Track;

const TRACK_INSERT_BATCH_SIZE: usize = 500;

pub async fn connect(database_url: &str) -> Result<SqlitePool> {
    let options = SqliteConnectOptions::from_str(database_url)
        .context("invalid DATABASE_URL")?
        .create_if_missing(true)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_secs(5));

    let pool = SqlitePoolOptions::new()
        .connect_with(options)
        .await
        .context("failed to connect to database")?;

    let migrator = sqlx::migrate!("./migrations");
    for _ in 0..migrator.migrations.len() {
        match migrator.run(&pool).await {
            Ok(()) => break,
            Err(sqlx::migrate::MigrateError::VersionMismatch(version)) => {
                let checksum = migrator
                    .migrations
                    .iter()
                    .find(|m| m.version == version)
                    .map(|m| m.checksum.as_ref())
                    .unwrap_or_default();
                tracing::warn!(
                    version,
                    "re-stamping a stale `_sqlx_migrations` checksum to match the current \
                     migration file, without re-running it"
                );
                sqlx::query("UPDATE _sqlx_migrations SET checksum = ?1 WHERE version = ?2")
                    .bind(checksum)
                    .bind(version)
                    .execute(&pool)
                    .await
                    .context("failed to repair stale migration checksum")?;
            }
            Err(err) => return Err(err).context("failed to run database migrations"),
        }
    }

    Ok(pool)
}

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedSession {
    pub radio_enabled: bool,
    pub radio_requested_by: Option<String>,
    pub radio_history: Vec<String>,
    pub now_playing: Option<(Track, String)>,
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
    now_playing: Option<(&Track, &str)>,
    last_played: Option<(&Track, &str)>,
) -> Result<()> {
    let (np_video_id, np_title, np_channel, np_duration_secs, np_requested_by) =
        session_track_columns(now_playing);
    let (lp_video_id, lp_title, lp_channel, lp_duration_secs, lp_requested_by) =
        session_track_columns(last_played);

    sqlx::query(
        "INSERT INTO guild_sessions \
         (guild_id, radio_enabled, radio_requested_by, radio_history, \
          now_playing_video_id, now_playing_title, now_playing_channel, \
          now_playing_duration_secs, now_playing_requested_by, \
          last_played_video_id, last_played_title, last_played_channel, \
          last_played_duration_secs, last_played_requested_by) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
         ON CONFLICT(guild_id) DO UPDATE SET
             radio_enabled = excluded.radio_enabled,
             radio_requested_by = excluded.radio_requested_by,
             radio_history = excluded.radio_history,
             now_playing_video_id = excluded.now_playing_video_id,
             now_playing_title = excluded.now_playing_title,
             now_playing_channel = excluded.now_playing_channel,
             now_playing_duration_secs = excluded.now_playing_duration_secs,
             now_playing_requested_by = excluded.now_playing_requested_by,
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
    .bind(np_video_id)
    .bind(np_title)
    .bind(np_channel)
    .bind(np_duration_secs)
    .bind(np_requested_by)
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
                now_playing_video_id, now_playing_title, now_playing_channel, \
                now_playing_duration_secs, now_playing_requested_by, \
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
        np_video_id,
        np_title,
        np_channel,
        np_duration_secs,
        np_requested_by,
        lp_video_id,
        lp_title,
        lp_channel,
        lp_duration_secs,
        lp_requested_by,
    )) = fetch_guild_session_row(pool, guild_id).await?
    else {
        return Ok(None);
    };

    let now_playing = track_from_columns(
        np_video_id,
        np_title,
        np_channel,
        np_duration_secs,
        np_requested_by,
    );
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
        now_playing,
        last_played,
    }))
}

pub async fn clear_guild_session(pool: &SqlitePool, guild_id: &str) -> Result<()> {
    sqlx::query("DELETE FROM guild_sessions WHERE guild_id = ?1")
        .bind(guild_id)
        .execute(pool)
        .await
        .context("failed to clear guild session")?;

    Ok(())
}

fn queued_track_from_row(
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

pub async fn dashboard_user_count(pool: &SqlitePool) -> Result<i64> {
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM dashboard_users")
        .fetch_one(pool)
        .await
        .context("failed to count dashboard users")?;
    Ok(count)
}

pub async fn insert_dashboard_user(
    pool: &SqlitePool,
    username: &str,
    password_hash: &str,
) -> Result<()> {
    sqlx::query("INSERT INTO dashboard_users (username, password_hash) VALUES (?1, ?2)")
        .bind(username)
        .bind(password_hash)
        .execute(pool)
        .await
        .context("failed to insert dashboard user")?;
    Ok(())
}

pub async fn dashboard_user_password_hash(
    pool: &SqlitePool,
    username: &str,
) -> Result<Option<String>> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT password_hash FROM dashboard_users WHERE username = ?1")
            .bind(username)
            .fetch_optional(pool)
            .await
            .context("failed to fetch dashboard user")?;
    Ok(row.map(|(hash,)| hash))
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

    fn sample_session(now_playing: Option<(Track, &str)>) -> PersistedSession {
        sample_session_with_last_played(now_playing, None)
    }

    fn sample_session_with_last_played(
        now_playing: Option<(Track, &str)>,
        last_played: Option<(Track, &str)>,
    ) -> PersistedSession {
        PersistedSession {
            radio_enabled: true,
            radio_requested_by: Some("42".to_string()),
            radio_history: vec!["a".to_string(), "b".to_string()],
            now_playing: now_playing.map(|(track, requested_by)| (track, requested_by.to_string())),
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
                .now_playing
                .as_ref()
                .map(|(track, requested_by)| (track, requested_by.as_str())),
            session
                .last_played
                .as_ref()
                .map(|(track, requested_by)| (track, requested_by.as_str())),
        )
        .await
    }

    fn sample_queued(video_id: &str, duration: Option<Duration>, requested_by: u64) -> QueuedTrack {
        QueuedTrack {
            track: sample_track(video_id, duration),
            requested_by: UserId::new(requested_by),
        }
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
    async fn guild_session_persists_last_played_independently_of_now_playing() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        let session = sample_session_with_last_played(
            None,
            Some((sample_track("a", Some(Duration::from_secs(30))), "42")),
        );

        save_session(&pool, "1", &session).await?;

        let loaded = load_guild_session(&pool, "1").await?.expect("row exists");
        assert_eq!(loaded.now_playing, None);
        assert_eq!(
            loaded.last_played,
            Some((
                sample_track("a", Some(Duration::from_secs(30))),
                "42".to_string()
            ))
        );

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
    async fn saving_meta_with_no_now_playing_leaves_existing_queue_rows_alone() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        save_session(
            &pool,
            "1",
            &sample_session(Some((sample_track("a", None), "42"))),
        )
        .await?;
        queue_push_back(&pool, "1", &sample_queued("b", None, 42)).await?;

        save_session(&pool, "1", &sample_session(None)).await?;

        let session = load_guild_session(&pool, "1")
            .await?
            .expect("row still exists");
        assert_eq!(session.now_playing, None);
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
