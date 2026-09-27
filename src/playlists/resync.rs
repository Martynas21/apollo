//! Refreshes every saved playlist once a day at a configured local hour.

use std::time::Duration;

use chrono::{DateTime, Days, Local, TimeZone};
use sqlx::SqlitePool;

use crate::db;
use crate::youtube::api::YouTubeClient;

/// Spawns the daily worker: it sleeps until the next `hour`:00 local time,
/// refreshes every playlist, then waits for the following day.
pub fn spawn_daily(db: SqlitePool, youtube: YouTubeClient, hour: u32) {
    tracing::info!(hour, "daily playlist resync scheduled");
    tokio::spawn(run_daily(db, youtube, hour));
}

/// The process's one scheduler; it has to outlive every run, so it has no
/// bound.
async fn run_daily(db: SqlitePool, youtube: YouTubeClient, hour: u32) {
    loop {
        let wait = until_next(Local::now(), hour);
        tracing::info!(in_secs = wait.as_secs(), "next playlist resync");
        tokio::time::sleep(wait).await;
        resync_all(&db, &youtube).await;
    }
}

/// Time from `now` until the next `hour`:00:00 strictly after it. A day whose
/// clock skips that hour (a DST gap) is passed over for the following one.
/// Falls back to a full day rather than zero, so a caller can never spin.
pub fn until_next<Tz: TimeZone>(now: DateTime<Tz>, hour: u32) -> Duration {
    let tz = now.timezone();
    (0..3u64)
        .filter_map(|days_ahead| {
            let date = now.date_naive().checked_add_days(Days::new(days_ahead))?;
            let naive = date.and_hms_opt(hour, 0, 0)?;
            tz.from_local_datetime(&naive).earliest()
        })
        .find(|target| *target > now)
        .and_then(|target| target.signed_duration_since(&now).to_std().ok())
        .unwrap_or(Duration::from_secs(24 * 60 * 60))
}

/// Refreshes every saved playlist in turn. One failure never stops the run;
/// each is logged and the closing summary carries the totals.
pub async fn resync_all(db: &SqlitePool, youtube: &YouTubeClient) {
    let playlists = match db::list_all_playlists(db).await {
        Ok(playlists) => playlists,
        Err(err) => {
            tracing::warn!(%err, "playlist resync skipped: failed to list playlists");
            return;
        }
    };

    let mut refreshed = 0usize;
    let mut failed = 0usize;
    for entry in &playlists {
        match super::refresh(db, youtube, &entry.guild_id, &entry.playlist).await {
            Ok(result) => {
                refreshed += 1;
                tracing::debug!(
                    guild_id = %entry.guild_id,
                    playlist_id = entry.playlist.id,
                    tracks = result.tracks.len(),
                    "playlist resynced"
                );
            }
            Err(err) => {
                failed += 1;
                tracing::warn!(
                    guild_id = %entry.guild_id,
                    playlist_id = entry.playlist.id,
                    %err,
                    "playlist resync failed"
                );
            }
        }
    }
    tracing::info!(refreshed, failed, "playlist resync finished");
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;

    const HOUR: u64 = 60 * 60;

    fn at(hour: u32, minute: u32) -> DateTime<FixedOffset> {
        FixedOffset::east_opt(2 * 3600)
            .unwrap()
            .with_ymd_and_hms(2026, 9, 27, hour, minute, 0)
            .unwrap()
    }

    #[test]
    fn before_the_hour_waits_until_it_the_same_day() {
        assert_eq!(
            until_next(at(1, 30), 3),
            Duration::from_secs(HOUR + HOUR / 2)
        );
    }

    #[test]
    fn exactly_at_the_hour_waits_a_full_day() {
        assert_eq!(until_next(at(3, 0), 3), Duration::from_secs(24 * HOUR));
    }

    #[test]
    fn after_the_hour_waits_until_tomorrow() {
        assert_eq!(until_next(at(4, 0), 3), Duration::from_secs(23 * HOUR));
    }

    #[test]
    fn hour_zero_means_the_coming_midnight() {
        assert_eq!(until_next(at(23, 0), 0), Duration::from_secs(HOUR));
    }

    #[test]
    fn impossible_hour_falls_back_to_a_day() {
        assert_eq!(until_next(at(12, 0), 24), Duration::from_secs(24 * HOUR));
    }

    #[tokio::test]
    async fn resync_all_with_no_playlists_completes() -> anyhow::Result<()> {
        let pool = db::connect("sqlite::memory:").await?;
        resync_all(&pool, &YouTubeClient::default()).await;
        Ok(())
    }
}
