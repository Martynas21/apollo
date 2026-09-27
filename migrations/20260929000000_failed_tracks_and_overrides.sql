-- Tracks that never produced audio for a guild: a queued track whose stream
-- could not be resolved or started, or one that errored within its first
-- seconds even after its fresh-URL retry. The dashboard lists them under the
-- queue so each can be dismissed or replaced; a later successful start of the
-- same video clears its row.
CREATE TABLE IF NOT EXISTS failed_tracks (
    guild_id TEXT NOT NULL,
    video_id TEXT NOT NULL,
    title TEXT NOT NULL,
    channel TEXT NOT NULL,
    duration_secs INTEGER,
    requested_by TEXT NOT NULL,
    error TEXT NOT NULL,
    failed_at INTEGER NOT NULL DEFAULT (unixepoch()),
    PRIMARY KEY (guild_id, video_id)
);

-- A guild's chosen stand-in for a video that would not play: whenever the
-- original is queued again (saved playlist, radio refill, search), the
-- replacement goes into the queue in its place. Rows are pruned once no saved
-- playlist of the guild contains the original any more.
CREATE TABLE IF NOT EXISTS track_overrides (
    guild_id TEXT NOT NULL,
    original_video_id TEXT NOT NULL,
    video_id TEXT NOT NULL,
    title TEXT NOT NULL,
    channel TEXT NOT NULL,
    duration_secs INTEGER,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    PRIMARY KEY (guild_id, original_video_id)
);
