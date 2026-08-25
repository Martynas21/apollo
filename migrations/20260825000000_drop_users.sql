-- Google OAuth2 linking has been removed in favor of yt-dlp for all
-- YouTube access (search, playback, playlists) — no more per-user tokens
-- to store.
DROP TABLE IF EXISTS users;
