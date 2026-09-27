-- What an override does when its original would be queued from a playlist:
-- 'replace' queues the row's replacement track instead, 'skip' leaves the
-- original out and carries no replacement (its track columns stay empty).
ALTER TABLE track_overrides ADD COLUMN action TEXT NOT NULL DEFAULT 'replace';
