-- Persists a guild's in-progress queue/radio session across restarts, so a
-- crash or deploy doesn't silently drop what was queued. Nothing rejoins
-- voice on its own: the next command that starts a session
-- (`PlayerRegistry::join`, called right before every `enqueue`/
-- `enqueue_many`) restores it instead of starting cold. There is no
-- seek-resume — a restored track restarts from the beginning, not from
-- wherever it was cut off.
--
-- `guild_id`/`radio_requested_by`/`requested_by` are TEXT for the same
-- reason as elsewhere in this schema: Discord snowflakes can exceed
-- i64::MAX.
CREATE TABLE IF NOT EXISTS guild_sessions (
    guild_id TEXT PRIMARY KEY NOT NULL,
    radio_enabled INTEGER NOT NULL,
    radio_requested_by TEXT,
    -- Comma-separated video ids, oldest first, matching
    -- `GuildState::radio_history` (capped at `RADIO_HISTORY_CAP` = 5
    -- entries, so no separate table is needed).
    radio_history TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS guild_session_queue (
    guild_id TEXT NOT NULL REFERENCES guild_sessions(guild_id) ON DELETE CASCADE,
    -- 0 = the track to resume as now_playing; the rest is the upcoming
    -- queue, in order.
    position INTEGER NOT NULL,
    video_id TEXT NOT NULL,
    title TEXT NOT NULL,
    channel TEXT NOT NULL,
    -- NULL when yt-dlp didn't report a usable duration (e.g. a livestream).
    duration_secs INTEGER,
    requested_by TEXT NOT NULL,
    PRIMARY KEY (guild_id, position)
);
