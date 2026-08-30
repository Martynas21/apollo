//! SQLite-backed persistence for per-guild playback settings.
//!
//! Schema lives in `migrations/` and is embedded into the binary via
//! [`sqlx::migrate!`], so a fresh SQLite file is brought up to date
//! automatically on startup.

use anyhow::{Context, Result};
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteSynchronous,
};
use sqlx::{QueryBuilder, Sqlite};
use std::str::FromStr;
use std::time::Duration;

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

/// A guild's in-progress queue/radio session, as persisted by
/// [`save_guild_session`] and restored by [`load_guild_session`]. `queue[0]`
/// is the track to resume as `now_playing`; the rest is the upcoming queue,
/// in order. Each track is paired with the id of the user who requested it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedSession {
    pub radio_enabled: bool,
    pub radio_requested_by: Option<String>,
    /// Oldest first, capped at `RADIO_HISTORY_CAP` by the caller — see
    /// `voice::player::GuildState::radio_history`.
    pub radio_history: Vec<String>,
    pub queue: Vec<(Track, String)>,
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

/// Persists `session` for `guild_id`, replacing whatever was there before.
/// If `session.queue` is empty, there's nothing worth resuming, so this just
/// clears the guild's row instead of leaving an empty one behind — see
/// [`clear_guild_session`].
///
/// Runs as one transaction so a concurrent [`load_guild_session`] never sees
/// a half-replaced queue.
pub async fn save_guild_session(
    pool: &SqlitePool,
    guild_id: &str,
    session: &PersistedSession,
) -> Result<()> {
    if session.queue.is_empty() {
        return clear_guild_session(pool, guild_id).await;
    }

    let mut tx = pool
        .begin()
        .await
        .context("failed to start session save transaction")?;

    sqlx::query(
        "INSERT INTO guild_sessions (guild_id, radio_enabled, radio_requested_by, radio_history) \
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(guild_id) DO UPDATE SET
             radio_enabled = excluded.radio_enabled,
             radio_requested_by = excluded.radio_requested_by,
             radio_history = excluded.radio_history",
    )
    .bind(guild_id)
    .bind(session.radio_enabled)
    .bind(&session.radio_requested_by)
    .bind(encode_radio_history(&session.radio_history))
    .execute(&mut *tx)
    .await
    .context("failed to save guild session")?;

    sqlx::query("DELETE FROM guild_session_queue WHERE guild_id = ?1")
        .bind(guild_id)
        .execute(&mut *tx)
        .await
        .context("failed to clear old session queue")?;

    // Batched into multi-row `INSERT`s, same as `replace_playlist_tracks` —
    // a whole playlist can be queued (and thus persisted) at once.
    let indexed_tracks = session.queue.iter().enumerate().collect::<Vec<_>>();
    for batch in indexed_tracks.chunks(TRACK_INSERT_BATCH_SIZE) {
        let mut builder: QueryBuilder<Sqlite> = QueryBuilder::new(
            "INSERT INTO guild_session_queue \
             (guild_id, position, video_id, title, channel, duration_secs, requested_by) ",
        );
        builder.push_values(batch, |mut b, (position, (track, requested_by))| {
            b.push_bind(guild_id)
                .push_bind(*position as i64)
                .push_bind(&track.video_id)
                .push_bind(&track.title)
                .push_bind(&track.channel)
                .push_bind(track.duration.map(|d| d.as_secs() as i64))
                .push_bind(requested_by);
        });
        builder
            .build()
            .execute(&mut *tx)
            .await
            .context("failed to insert session queue rows")?;
    }

    tx.commit()
        .await
        .context("failed to commit session save transaction")?;

    Ok(())
}

/// Loads `guild_id`'s persisted session, if any. `None` if there's no row,
/// or its queue is empty (shouldn't normally happen — `save_guild_session`
/// clears rather than saving an empty queue — but treated the same as "no
/// session" defensively).
pub async fn load_guild_session(
    pool: &SqlitePool,
    guild_id: &str,
) -> Result<Option<PersistedSession>> {
    let session_row: Option<(bool, Option<String>, String)> = sqlx::query_as(
        "SELECT radio_enabled, radio_requested_by, radio_history \
         FROM guild_sessions WHERE guild_id = ?1",
    )
    .bind(guild_id)
    .fetch_optional(pool)
    .await
    .context("failed to fetch guild session")?;

    let Some((radio_enabled, radio_requested_by, radio_history)) = session_row else {
        return Ok(None);
    };

    let rows: Vec<(String, String, String, Option<i64>, String)> = sqlx::query_as(
        "SELECT video_id, title, channel, duration_secs, requested_by \
         FROM guild_session_queue WHERE guild_id = ?1 ORDER BY position",
    )
    .bind(guild_id)
    .fetch_all(pool)
    .await
    .context("failed to fetch session queue")?;

    if rows.is_empty() {
        return Ok(None);
    }

    let queue = rows
        .into_iter()
        .map(
            |(video_id, title, channel, duration_secs, requested_by)| {
                #[allow(clippy::cast_sign_loss)]
                let track = Track {
                    video_id,
                    title,
                    channel,
                    duration: duration_secs.map(|secs| Duration::from_secs(secs as u64)),
                };
                (track, requested_by)
            },
        )
        .collect();

    Ok(Some(PersistedSession {
        radio_enabled,
        radio_requested_by,
        radio_history: decode_radio_history(&radio_history),
        queue,
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
    let escaped = query.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
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

    fn sample_session(queue: Vec<(Track, &str)>) -> PersistedSession {
        PersistedSession {
            radio_enabled: true,
            radio_requested_by: Some("42".to_string()),
            radio_history: vec!["a".to_string(), "b".to_string()],
            queue: queue
                .into_iter()
                .map(|(track, requested_by)| (track, requested_by.to_string()))
                .collect(),
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
        let session = sample_session(vec![
            (sample_track("a", Some(Duration::from_secs(30))), "42"),
            (sample_track("b", None), "43"),
        ]);

        save_guild_session(&pool, "1", &session).await?;

        assert_eq!(load_guild_session(&pool, "1").await?, Some(session));
        // A different guild is unaffected.
        assert_eq!(load_guild_session(&pool, "2").await?, None);

        Ok(())
    }

    #[tokio::test]
    async fn guild_session_save_replaces_rather_than_appends() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        save_guild_session(&pool, "1", &sample_session(vec![(sample_track("old", None), "42")]))
            .await?;

        let replacement = sample_session(vec![(sample_track("new", None), "42")]);
        save_guild_session(&pool, "1", &replacement).await?;

        assert_eq!(load_guild_session(&pool, "1").await?, Some(replacement));

        Ok(())
    }

    #[tokio::test]
    async fn guild_session_save_with_empty_queue_clears_instead_of_saving() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        save_guild_session(&pool, "1", &sample_session(vec![(sample_track("a", None), "42")]))
            .await?;

        save_guild_session(&pool, "1", &sample_session(vec![])).await?;

        assert_eq!(load_guild_session(&pool, "1").await?, None);

        Ok(())
    }

    #[tokio::test]
    async fn clear_guild_session_removes_it_and_its_queue() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        save_guild_session(&pool, "1", &sample_session(vec![(sample_track("a", None), "42")]))
            .await?;

        clear_guild_session(&pool, "1").await?;

        assert_eq!(load_guild_session(&pool, "1").await?, None);
        let orphaned: Vec<(String,)> =
            sqlx::query_as("SELECT video_id FROM guild_session_queue WHERE guild_id = '1'")
                .fetch_all(&pool)
                .await?;
        assert!(orphaned.is_empty());

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
    async fn search_cached_tracks_matches_by_title_case_insensitively() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        let id = save_guild_playlist(&pool, "1", "Mix", "https://example.com/list=abc", "42")
            .await?;
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
        assert_eq!(search_cached_tracks(&pool, "2", "title a", 10).await?, Vec::new());

        Ok(())
    }

    #[tokio::test]
    async fn search_cached_tracks_treats_percent_and_underscore_literally() -> Result<()> {
        let pool = connect("sqlite::memory:").await?;
        let id = save_guild_playlist(&pool, "1", "Mix", "https://example.com/list=abc", "42")
            .await?;
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
            results.iter().map(|t| t.video_id.as_str()).collect::<Vec<_>>(),
            vec!["lit"]
        );

        Ok(())
    }
}
