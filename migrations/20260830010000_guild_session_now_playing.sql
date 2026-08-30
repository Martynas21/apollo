-- `guild_session_queue` is now the live, continuously-authoritative upcoming
-- queue (written incrementally at each mutation, not as a periodic
-- snapshot), so "now playing" — which isn't part of the upcoming queue —
-- needs its own home instead of living at position 0 of that table.
-- All five columns are always written together: all NULL means nothing was
-- playing when this was persisted.
ALTER TABLE guild_sessions ADD COLUMN now_playing_video_id TEXT;
ALTER TABLE guild_sessions ADD COLUMN now_playing_title TEXT;
ALTER TABLE guild_sessions ADD COLUMN now_playing_channel TEXT;
ALTER TABLE guild_sessions ADD COLUMN now_playing_duration_secs INTEGER;
ALTER TABLE guild_sessions ADD COLUMN now_playing_requested_by TEXT;
