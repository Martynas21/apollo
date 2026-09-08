-- Tracks how many times each saved playlist has been played from its Play
-- button (Discord's `/playlists` picker or the web dashboard), for the
-- dashboard's favourites view.
ALTER TABLE playlists ADD COLUMN play_count INTEGER NOT NULL DEFAULT 0;
