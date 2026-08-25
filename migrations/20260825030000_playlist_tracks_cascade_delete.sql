-- Adds `ON DELETE CASCADE` from `playlist_tracks.playlist_id` to
-- `playlists(id)` as defense in depth: even if a future caller ever
-- deletes a `playlists` row without also clearing its cached tracks (the
-- way `delete_guild_playlist` correctly does in application code), SQLite
-- now removes the corresponding `playlist_tracks` rows automatically
-- instead of leaving them orphaned.
--
-- SQLite can't add a foreign key to an existing table via ALTER TABLE, so
-- this follows SQLite's documented recreate-and-copy pattern:
-- https://www.sqlite.org/lang_altertable.html#otheralter

-- Defensive cleanup in case any rows were ever orphaned before this
-- constraint existed (shouldn't happen in practice, but the table
-- recreate below would otherwise fail to copy them under FK enforcement).
DELETE FROM playlist_tracks WHERE playlist_id NOT IN (SELECT id FROM playlists);

CREATE TABLE playlist_tracks_new (
    playlist_id INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
    -- 0-indexed position within the playlist, for ordering.
    position INTEGER NOT NULL,
    video_id TEXT NOT NULL,
    title TEXT NOT NULL,
    channel TEXT NOT NULL,
    -- NULL when yt-dlp didn't report a usable duration (e.g. a livestream).
    duration_secs INTEGER,
    PRIMARY KEY (playlist_id, position)
);

INSERT INTO playlist_tracks_new
    (playlist_id, position, video_id, title, channel, duration_secs)
    SELECT playlist_id, position, video_id, title, channel, duration_secs
    FROM playlist_tracks;

DROP TABLE playlist_tracks;

ALTER TABLE playlist_tracks_new RENAME TO playlist_tracks;
