-- Caches a saved playlist's track listing so opening it from the `/player`
-- panel is instant instead of shelling out to yt-dlp on every click. The
-- cache is populated on import and whenever the panel's Refresh button is
-- used — `cached_at` (unix seconds, NULL until first populated) drives the
-- "cached X ago" freshness note shown alongside it.
ALTER TABLE playlists ADD COLUMN cached_at INTEGER;

CREATE TABLE IF NOT EXISTS playlist_tracks (
    playlist_id INTEGER NOT NULL,
    -- 0-indexed position within the playlist, for ordering.
    position INTEGER NOT NULL,
    video_id TEXT NOT NULL,
    title TEXT NOT NULL,
    channel TEXT NOT NULL,
    -- NULL when yt-dlp didn't report a usable duration (e.g. a livestream).
    duration_secs INTEGER,
    PRIMARY KEY (playlist_id, position)
);
