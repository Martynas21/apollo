//! Saved-playlist maintenance shared by the dashboard's refresh action and
//! the daily worker in `resync`.

pub mod resync;

use sqlx::SqlitePool;

use crate::db::{self, SavedPlaylist};
use crate::model::Track;
use crate::youtube::api::{YouTubeApiError, YouTubeClient};

/// The outcome of a successful refresh: the tracks now cached for the
/// playlist and the author its row ended up with.
#[derive(Debug)]
pub struct Refreshed {
    pub tracks: Vec<Track>,
    pub author: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum RefreshError {
    /// `yt-dlp` could not list the playlist; the cache is left untouched.
    #[error(transparent)]
    Listing(#[from] YouTubeApiError),
    /// The listing succeeded but replacing the cached tracks failed.
    #[error(transparent)]
    Db(#[from] anyhow::Error),
}

/// Re-lists the playlist on YouTube and replaces its cached tracks. Overrides
/// whose original video no longer sits in any of the guild's playlists are
/// pruned and the author is updated afterwards; failures in those two steps
/// are logged rather than returned.
pub async fn refresh(
    db: &SqlitePool,
    youtube: &YouTubeClient,
    guild_id: &str,
    playlist: &SavedPlaylist,
) -> Result<Refreshed, RefreshError> {
    let listing = youtube.list_playlist_items(&playlist.url).await?;
    db::replace_playlist_tracks(db, playlist.id, &listing.tracks).await?;

    if let Err(err) = db::prune_track_overrides(db, guild_id).await {
        tracing::warn!(guild_id, %err, "failed to prune track overrides after playlist refresh");
    }

    let author = match db::set_playlist_author(db, playlist.id, listing.author.as_deref()).await {
        Ok(()) => listing.author,
        Err(err) => {
            tracing::warn!(
                %err,
                playlist_id = playlist.id,
                "failed to update refreshed playlist author"
            );
            playlist.author.clone()
        }
    };

    Ok(Refreshed {
        tracks: listing.tracks,
        author,
    })
}
