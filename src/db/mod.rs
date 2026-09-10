use anyhow::{Context, Result};
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteSynchronous,
};
use std::str::FromStr;
use std::time::Duration;

mod playlists;
mod queue;
mod session;
mod settings;
mod stats;
mod users;

pub use playlists::{
    SavedPlaylist, delete_guild_playlist, get_guild_playlist, get_playlist_thumbnail_video_id,
    get_playlist_tracks, list_guild_playlists, replace_playlist_tracks, save_guild_playlist,
};
#[cfg(test)]
pub use queue::queue_push_back;
pub use queue::{
    queue_all, queue_clear, queue_len, queue_peek_front, queue_pop_front, queue_push_many,
    queue_replace_all,
};
pub use session::{
    PersistedSession, clear_guild_session, load_guild_session, save_guild_session_meta,
};
pub use settings::{DEFAULT_VOLUME, get_guild_volume, set_guild_volume};
pub use stats::{
    PlaylistPlayCount, TrackPlayCount, increment_playlist_play_count, record_track_play,
    top_played_playlists, top_played_tracks,
};
pub use users::{
    UserCredentials, UserPrivileges, UserSummary, admin_count, delete_user, insert_user,
    list_users, rename_user, set_user_guild, set_user_password, user_count, user_credentials,
    user_exists, user_privileges,
};

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

#[cfg(test)]
pub(crate) mod testing {
    use std::time::Duration;

    use serenity::all::UserId;

    use crate::model::{QueuedTrack, Track};

    pub(crate) fn sample_track(video_id: &str, duration: Option<Duration>) -> Track {
        Track {
            video_id: video_id.to_string(),
            title: format!("Title {video_id}"),
            channel: "Some Channel".to_string(),
            duration,
        }
    }

    pub(crate) fn sample_queued(
        video_id: &str,
        duration: Option<Duration>,
        requested_by: u64,
    ) -> QueuedTrack {
        QueuedTrack {
            track: sample_track(video_id, duration),
            requested_by: UserId::new(requested_by),
        }
    }
}
