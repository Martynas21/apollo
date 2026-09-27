-- What the replaced video was called, so the dashboard's overrides page can
-- show each mapping by name. Empty for overrides saved before these columns
-- existed; the page falls back to the video id for those.
ALTER TABLE track_overrides ADD COLUMN original_title TEXT NOT NULL DEFAULT '';
ALTER TABLE track_overrides ADD COLUMN original_channel TEXT NOT NULL DEFAULT '';
ALTER TABLE track_overrides ADD COLUMN original_duration_secs INTEGER;
