//! SQLite-backed persistence for per-guild playback settings.
//!
//! Schema lives in `migrations/` and is embedded into the binary via
//! [`sqlx::migrate!`], so a fresh SQLite file is brought up to date
//! automatically on startup.

use anyhow::{Context, Result};
use poise::serenity_prelude::UserId;
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteSynchronous,
};
use sqlx::{QueryBuilder, Sqlite};
use std::str::FromStr;
use std::time::Duration;

use crate::voice::player::QueuedTrack;
use crate::youtube::api::Track;

/// How many rows [`replace_playlist_tracks`] inserts per multi-row
/// `INSERT`, to stay comfortably under SQLite's bound-parameter limit
/// (default 32766) while still batching real-world playlists (YouTube caps
/// a single playlist at 5000 videos) in a small, fixed number of round
/// trips instead of one `INSERT` per track.
const TRACK_INSERT_BATCH_SIZE: usize = 500;

/// Opens a connection pool for `database_url`, creating the SQLite file if
/// it doesn't exist and running any pending migrations.
pub async fn connect(database_url: &str) -> Result<SqlitePool> {
    // Deliberately don't include `database_url` in these error messages: if
    // it was misconfigured with a full connection string copy-pasted from
    // elsewhere, it could carry credentials, and these errors get logged.
    let options = SqliteConnectOptions::from_str(database_url)
        .context("invalid DATABASE_URL")?
        .create_if_missing(true)
        // Defense in depth alongside the `ON DELETE CASCADE` on
        // `playlist_tracks.playlist_id`: SQLite has foreign key enforcement
        // off by default per-connection even when FKs are declared in the
        // schema, and this applies it to every connection the pool opens.
        .foreign_keys(true)
        // WAL lets readers and a writer proceed concurrently instead of a
        // writer blocking all readers, and the busy timeout below makes a
        // second concurrent writer (e.g. two guilds' playlist operations
        // landing around the same time) retry instead of failing outright.
        .journal_mode(SqliteJournalMode::Wal)
        // Safe and standard under WAL: only a full OS crash (not just this
        // process crashing) can lose the most recent commit, an acceptable
        // tradeoff for a bot that isn't the system of record, in exchange for
        // skipping an fsync on every commit.
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_secs(5));

    let pool = SqlitePoolOptions::new()
        .connect_with(options)
        .await
        .context("failed to connect to database")?;

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
/// `/play` every time.
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

    // Scoped to the same guild (via the subquery) so this can never touch
    // another guild's cached tracks — mirrors the ownership check the
    // `playlists` delete below already enforces. Also backstopped by the
    // `ON DELETE CASCADE` on `playlist_tracks.playlist_id` (see the
    // `playlist_tracks_cascade_delete` migration), but that's defense in
    // depth, not a substitute for scoping this statement correctly.
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

    // Batched into multi-row `INSERT`s (rather than one statement per
    // track) so a large playlist holds the write lock for far fewer round
    // trips.
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

/// A guild's in-progress radio/now-playing session, as persisted by
/// [`save_guild_session_meta`] and restored by [`load_guild_session`]. The
/// *upcoming* queue is not part of this struct — `guild_session_queue` is
/// itself the live, continuously-authoritative queue (written incrementally
/// by the `queue_*` functions below, at the point of each mutation), so
/// there's nothing to snapshot for it here. `now_playing` is paired with the
/// id of the user who requested it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedSession {
    pub radio_enabled: bool,
    pub radio_requested_by: Option<String>,
    /// Oldest first, capped at `RADIO_HISTORY_CAP` by the caller — see
    /// `voice::player::GuildState::radio_history`.
    pub radio_history: Vec<String>,
    pub now_playing: Option<(Track, String)>,
}

/// Joins `radio_history` into the comma-separated form `guild_sessions`
/// stores it in. Video ids are `[A-Za-z0-9_-]{11}` and never contain commas,
/// so no escaping is needed.
fn encode_radio_history(history: &[String]) -> String {
    history.join(",")
}

/// Inverse of [`encode_radio_history`]. Filters out empty segments as
/// defense in depth (e.g. an empty stored string splitting into `[""]`).
fn decode_radio_history(raw: &str) -> Vec<String> {
    raw.split(',')
        .filter(|segment| !segment.is_empty())
        .map(str::to_string)
        .collect()
}

/// Persists a guild's radio/now-playing metadata, replacing whatever was
/// there before. Does *not* touch `guild_session_queue` — the upcoming queue
/// is written incrementally by the `queue_*` functions below, at the point
/// of each actual mutation, not as a periodic snapshot here.
pub async fn save_guild_session_meta(
    pool: &SqlitePool,
    guild_id: &str,
    radio_enabled: bool,
    radio_requested_by: Option<&str>,
    radio_history: &[String],
    now_playing: Option<(&Track, &str)>,
) -> Result<()> {
    let (np_video_id, np_title, np_channel, np_duration_secs, np_requested_by) = match now_playing {
        Some((track, requested_by)) => (
            Some(track.video_id.as_str()),
            Some(track.title.as_str()),
            Some(track.channel.as_str()),
            #[allow(clippy::cast_possible_wrap)]
            track.duration.map(|d| d.as_secs() as i64),
            Some(requested_by),
        ),
        None => (None, None, None, None, None),
    };

    sqlx::query(
        "INSERT INTO guild_sessions \
         (guild_id, radio_enabled, radio_requested_by, radio_history, \
          now_playing_video_id, now_playing_title, now_playing_channel, \
          now_playing_duration_secs, now_playing_requested_by) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(guild_id) DO UPDATE SET
             radio_enabled = excluded.radio_enabled,
             radio_requested_by = excluded.radio_requested_by,
             radio_history = excluded.radio_history,
             now_playing_video_id = excluded.now_playing_video_id,
             now_playing_title = excluded.now_playing_title,
             now_playing_channel = excluded.now_playing_channel,
             now_playing_duration_secs = excluded.now_playing_duration_secs,
             now_playing_requested_by = excluded.now_playing_requested_by",
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
    .execute(pool)
    .await
    .context("failed to save guild session")?;

    Ok(())
}

/// Loads `guild_id`'s persisted session metadata, if any — `None` if there's
/// no `guild_sessions` row for it. The upcoming queue is not loaded here:
/// `guild_session_queue` is the live table, already reflecting whatever's
/// actually queued, so there's nothing to restore for it — callers read it
/// separately (`queue_pop_front`/`queue_all`/etc.) once they need to.
pub async fn load_guild_session(
    pool: &SqlitePool,
    guild_id: &str,
) -> Result<Option<PersistedSession>> {
    type SessionRow = (
        bool,
        Option<String>,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<String>,
    );
    let row: Option<SessionRow> = sqlx::query_as(
        "SELECT radio_enabled, radio_requested_by, radio_history, \
                now_playing_video_id, now_playing_title, now_playing_channel, \
                now_playing_duration_secs, now_playing_requested_by \
         FROM guild_sessions WHERE guild_id = ?1",
    )
    .bind(guild_id)
    .fetch_optional(pool)
    .await
    .context("failed to fetch guild session")?;

    let Some((
        radio_enabled,
        radio_requested_by,
        radio_history,
        np_video_id,
        np_title,
        np_channel,
        np_duration_secs,
        np_requested_by,
    )) = row
    else {
        return Ok(None);
    };

    // The five `now_playing_*` columns are always written together (all
    // `Some` or all `None`) by `save_guild_session_meta` — matching on all
    // four non-duration columns being `Some` is just defense in depth.
    let now_playing = match (np_video_id, np_title, np_channel, np_requested_by) {
        (Some(video_id), Some(title), Some(channel), Some(requested_by)) => {
            #[allow(clippy::cast_sign_loss)]
            let track = Track {
                video_id,
                title,
                channel,
                duration: np_duration_secs.map(|secs| Duration::from_secs(secs as u64)),
            };
            Some((track, requested_by))
        }
        _ => None,
    };

    Ok(Some(PersistedSession {
        radio_enabled,
        radio_requested_by,
        radio_history: decode_radio_history(&radio_history),
        now_playing,
    }))
}

/// Deletes `guild_id`'s persisted session (cascading to its queue rows), for
/// when a session ends on purpose — `/leave` or an idle disconnect — rather
/// than a crash. A no-op if there wasn't one.
pub async fn clear_guild_session(pool: &SqlitePool, guild_id: &str) -> Result<()> {
    sqlx::query("DELETE FROM guild_sessions WHERE guild_id = ?1")
        .bind(guild_id)
        .execute(pool)
        .await
        .context("failed to clear guild session")?;

    Ok(())
}

/// Turns a `guild_session_queue` row into a [`QueuedTrack`]. `None` if
/// `requested_by` fails to parse as a Discord snowflake — shouldn't happen,
/// since only valid ids are ever written, but matches this file's existing
/// defensive-parsing style (see `restore_session_if_new`).
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

/// Appends `queued` to `guild_id`'s live upcoming queue, at
/// `MAX(position) + 1`. Ensures a `guild_sessions` parent row exists first
/// (`guild_session_queue.guild_id` has an `ON DELETE CASCADE` FK back to
/// it), since this can be the very first thing ever persisted for a guild.
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

/// Appends `tracks` to `guild_id`'s live upcoming queue in position order,
/// continuing from the current `MAX(position)`. Batched into multi-row
/// `INSERT`s, same as `replace_playlist_tracks` — a whole playlist can be
/// queued (and thus persisted) at once. No-op if `tracks` is empty.
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

/// Removes and returns the lowest-position row in `guild_id`'s upcoming
/// queue — the track that should become the new `now_playing`. `None` if
/// the queue is empty. Runs as one transaction so a concurrent read never
/// sees a row gone without having been returned to somebody, and so a
/// corrupt row (see below) is skipped atomically along with everything
/// still queued behind it in the empty case.
///
/// A row that fails to parse (see `queued_track_from_row`) is deleted and
/// skipped rather than returned as `None` — otherwise it would look
/// indistinguishable from a genuinely empty queue to callers like
/// `promote_next`, which would then idle-disconnect while valid tracks are
/// still queued behind it.
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

/// Reads (without removing) the lowest-position row in `guild_id`'s upcoming
/// queue. `None` if it's empty.
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

/// Number of tracks in `guild_id`'s upcoming queue.
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

/// `guild_id`'s whole upcoming queue, in order.
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

/// Empties `guild_id`'s upcoming queue. Leaves the `guild_sessions` parent
/// row alone — that row's lifecycle (and the `now_playing` it may still
/// carry) is managed separately by `save_guild_session_meta`/
/// `clear_guild_session`.
pub async fn queue_clear(pool: &SqlitePool, guild_id: &str) -> Result<()> {
    sqlx::query("DELETE FROM guild_session_queue WHERE guild_id = ?1")
        .bind(guild_id)
        .execute(pool)
        .await
        .context("failed to clear the queue")?;
    Ok(())
}

/// Removes the `count` lowest-position rows from `guild_id`'s upcoming
/// queue, leaving the rest in their existing order — for `jump_to` dropping
/// the entries ahead of a selected track. No-op if `count` is 0.
pub async fn queue_drop_front(pool: &SqlitePool, guild_id: &str, count: usize) -> Result<()> {
    if count == 0 {
        return Ok(());
    }
    #[allow(clippy::cast_possible_wrap)]
    let count = count as i64;
    sqlx::query(
        "DELETE FROM guild_session_queue WHERE guild_id = ?1 AND position IN \
         (SELECT position FROM guild_session_queue WHERE guild_id = ?1 ORDER BY position LIMIT ?2)",
    )
    .bind(guild_id)
    .bind(count)
    .execute(pool)
    .await
    .context("failed to drop the front of the queue")?;
    Ok(())
}

/// Replaces `guild_id`'s entire upcoming queue with `tracks`, in the given
/// order, renumbering positions `0..N` — used by `shuffle`. Runs as one
/// transaction so a concurrent read never sees a half-replaced queue.
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

/// Searches a guild's cached playlist tracks by title — a fast, local
/// alternative to shelling out to `yt-dlp` when the wanted track is already
/// known from an imported playlist. Case-insensitive for ASCII (SQLite's
/// `LIKE` is by default); `query`'s own `%`/`_` are escaped so they match
/// literally rather than acting as wildcards.
pub async fn search_cached_tracks(
    pool: &SqlitePool,
    guild_id: &str,
    query: &str,
    limit: i64,
) -> Result<Vec<Track>> {
    let escaped = query
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    let pattern = format!("%{escaped}%");

    let rows: Vec<(String, String, String, Option<i64>)> = sqlx::query_as(
        "SELECT DISTINCT pt.video_id, pt.title, pt.channel, pt.duration_secs \
         FROM playlist_tracks pt \
         JOIN playlists p ON p.id = pt.playlist_id \
         WHERE p.guild_id = ?1 AND pt.title LIKE ?2 ESCAPE '\\' \
         ORDER BY pt.title \
         LIMIT ?3",
    )
    .bind(guild_id)
    .bind(pattern)
    .bind(limit)
    .fetch_all(pool)
    .await
    .context("failed to search cached playlist tracks")?;

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
    async fn guild_volume_falls_back_to_default_when_stored_value_is_out_of_range() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;

        // Bypass `set_guild_volume` (which only ever writes valid `u8`
        // values) to simulate a corrupted row.
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
        replace_playlist_tracks(&pool, id, &[sample_track("a", None)]).await?;

        assert!(!delete_guild_playlist(&pool, "2", id).await?);
        assert!(!delete_guild_playlist(&pool, "1", 9999).await?);
        assert!(get_guild_playlist(&pool, "1", id).await?.is_some());
        // The wrong-guild attempt above must not have touched this
        // playlist's cached tracks — regression test for a bug where the
        // `playlist_tracks` delete wasn't scoped to the guild at all.
        assert_eq!(
            get_playlist_tracks(&pool, id).await?,
            vec![sample_track("a", None)]
        );

        Ok(())
    }

    #[tokio::test]
    async fn deleting_playlist_row_directly_cascades_to_its_cached_tracks() -> Result<()> {
        // Bypasses `delete_guild_playlist` entirely to verify the `ON
        // DELETE CASCADE` foreign key itself (added in the
        // `playlist_tracks_cascade_delete` migration) — defense in depth
        // for any future caller that deletes a `playlists` row directly.
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

    fn sample_session(now_playing: Option<(Track, &str)>) -> PersistedSession {
        PersistedSession {
            radio_enabled: true,
            radio_requested_by: Some("42".to_string()),
            radio_history: vec!["a".to_string(), "b".to_string()],
            now_playing: now_playing.map(|(track, requested_by)| (track, requested_by.to_string())),
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
        // A different guild is unaffected.
        assert_eq!(load_guild_session(&pool, "2").await?, None);

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
        // `save_guild_session_meta` only ever touches `guild_sessions` — the
        // upcoming queue lives in `guild_session_queue` continuously and is
        // never rewritten as a side effect of a meta save.
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
        // The FK on `guild_session_queue.guild_id` requires a parent
        // `guild_sessions` row to already exist — `queue_push_back` must
        // create one rather than erroring on a guild's very first track.
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
        // A corrupt row shouldn't be mistaken for an empty queue — it should
        // be dropped and popping should continue on to the next valid one.
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
    async fn queue_drop_front_removes_exactly_the_earliest_n_and_preserves_order() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        for id in ["a", "b", "c", "d"] {
            queue_push_back(&pool, "1", &sample_queued(id, None, 42)).await?;
        }

        queue_drop_front(&pool, "1", 2).await?;

        assert_eq!(
            queue_all(&pool, "1")
                .await?
                .iter()
                .map(|q| q.track.video_id.as_str())
                .collect::<Vec<_>>(),
            vec!["c", "d"]
        );

        Ok(())
    }

    #[tokio::test]
    async fn queue_drop_front_zero_is_a_no_op() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        queue_push_back(&pool, "1", &sample_queued("a", None, 42)).await?;

        queue_drop_front(&pool, "1", 0).await?;

        assert_eq!(queue_len(&pool, "1").await?, 1);
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
    async fn search_cached_tracks_matches_by_title_case_insensitively() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        let id =
            save_guild_playlist(&pool, "1", "Mix", "https://example.com/list=abc", "42").await?;
        replace_playlist_tracks(
            &pool,
            id,
            &[sample_track("a", None), sample_track("b", None)],
        )
        .await?;

        let results = search_cached_tracks(&pool, "1", "title a", 10).await?;

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].video_id, "a");

        // A different guild's cache is not searched.
        assert_eq!(
            search_cached_tracks(&pool, "2", "title a", 10).await?,
            Vec::new()
        );

        Ok(())
    }

    #[tokio::test]
    async fn search_cached_tracks_treats_percent_and_underscore_literally() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        let id =
            save_guild_playlist(&pool, "1", "Mix", "https://example.com/list=abc", "42").await?;
        let literal = Track {
            video_id: "lit".to_string(),
            title: "50% off_sale".to_string(),
            channel: "Some Channel".to_string(),
            duration: None,
        };
        let decoy = Track {
            video_id: "decoy".to_string(),
            title: "50X offXsale".to_string(),
            channel: "Some Channel".to_string(),
            duration: None,
        };
        replace_playlist_tracks(&pool, id, &[literal, decoy]).await?;

        // Without escaping, "%" and "_" would act as SQL wildcards and also
        // match the decoy track ("X" standing in for any single character).
        let results = search_cached_tracks(&pool, "1", "50% off_sale", 10).await?;

        assert_eq!(
            results
                .iter()
                .map(|t| t.video_id.as_str())
                .collect::<Vec<_>>(),
            vec!["lit"]
        );

        Ok(())
    }
}
