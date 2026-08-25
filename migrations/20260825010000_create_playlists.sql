-- Per-guild saved playlists: named pointers to a YouTube playlist URL,
-- browsable from the `/player` panel's Playlists button instead of having
-- to re-paste a URL into `/playlist_play` every time.
--
-- `guild_id`/`added_by` are TEXT for the same reason as elsewhere in this
-- schema: Discord snowflakes can exceed i64::MAX.
CREATE TABLE IF NOT EXISTS playlists (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    guild_id TEXT NOT NULL,
    name TEXT NOT NULL,
    url TEXT NOT NULL,
    added_by TEXT NOT NULL,
    UNIQUE (guild_id, url)
);
