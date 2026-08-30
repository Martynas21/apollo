-- Per-guild playback settings that should survive a bot restart.
--
-- `guild_id` is stored as TEXT for the same reason as `users.discord_user_id`
-- in the previous migration: Discord snowflakes can exceed i64::MAX.
CREATE TABLE IF NOT EXISTS guild_settings (
    guild_id TEXT PRIMARY KEY NOT NULL,
    -- 0-100. Absent row means "never set" — callers default to 100.
    volume INTEGER NOT NULL
);
