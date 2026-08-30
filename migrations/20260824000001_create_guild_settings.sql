-- Per-guild, per-bot-identity playback settings that should survive a bot
-- restart.
--
-- `guild_id` is stored as TEXT for the same reason as `users.discord_user_id`
-- in the previous migration: Discord snowflakes can exceed i64::MAX.
--
-- Keyed on (guild_id, bot_id) rather than guild_id alone: a single guild can
-- be served by multiple independent bot identities at once (each holding
-- its own voice channel — see `Config::bots`), and each identity's volume
-- is its own setting.
CREATE TABLE IF NOT EXISTS guild_settings (
    guild_id TEXT NOT NULL,
    bot_id TEXT NOT NULL,
    -- 0-100. Absent row means "never set" — callers default to 100.
    volume INTEGER NOT NULL,
    PRIMARY KEY (guild_id, bot_id)
);
