-- Linked Google OAuth2 tokens, keyed by Discord user ID.
--
-- `discord_user_id` is stored as TEXT (not INTEGER) because Discord
-- snowflake IDs are unsigned 64-bit values that can exceed i64::MAX, which
-- SQLite's INTEGER (a signed 64-bit type) cannot represent safely.
CREATE TABLE IF NOT EXISTS users (
    discord_user_id TEXT PRIMARY KEY NOT NULL,
    access_token TEXT NOT NULL,
    refresh_token TEXT NOT NULL,
    -- Unix timestamp (seconds) at which `access_token` expires.
    expires_at INTEGER NOT NULL,
    -- Space-separated OAuth2 scopes, e.g. "https://www.googleapis.com/auth/youtube.readonly".
    scopes TEXT NOT NULL
);
