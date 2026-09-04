ALTER TABLE guild_sessions ADD COLUMN last_played_video_id TEXT;
ALTER TABLE guild_sessions ADD COLUMN last_played_title TEXT;
ALTER TABLE guild_sessions ADD COLUMN last_played_channel TEXT;
ALTER TABLE guild_sessions ADD COLUMN last_played_duration_secs INTEGER;
ALTER TABLE guild_sessions ADD COLUMN last_played_requested_by TEXT;
