-- Per-guild play counts for individual tracks, keyed by video id rather than
-- a playlist row so a track's count accumulates across however it gets
-- played (search, direct URL, playlist, radio). Bumped from the single spot
-- a track is known to have actually started (`commit_started_track` in
-- src/voice/player.rs), not from each call site that can queue one.
--
-- `guild_id`/`video_id` are TEXT for the same reason as elsewhere in this
-- schema: Discord snowflakes can exceed i64::MAX, and video ids are already
-- stored as strings in `playlist_tracks`.
CREATE TABLE IF NOT EXISTS track_play_counts (
    guild_id TEXT NOT NULL,
    video_id TEXT NOT NULL,
    title TEXT NOT NULL,
    channel TEXT NOT NULL,
    duration_secs INTEGER,
    play_count INTEGER NOT NULL DEFAULT 0,
    last_played_at INTEGER NOT NULL DEFAULT (unixepoch()),
    PRIMARY KEY (guild_id, video_id)
);
