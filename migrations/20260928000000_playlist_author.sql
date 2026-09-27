-- The YouTube channel that owns a saved playlist, shown on the dashboard's
-- playlist cards beside the track count. NULL when yt-dlp didn't report one
-- (or for playlists imported before this column existed, until refreshed).
ALTER TABLE playlists ADD COLUMN author TEXT;
