-- The track a guild is playing now lives at the head of its queue, not in a
-- column of its own: `guild_session_queue` holds the current track followed
-- by the upcoming ones, so nothing a guild is going to play sits outside the
-- queue the dashboard shows.
--
-- Whatever the now-playing columns still hold was interrupted mid-track by a
-- disconnect or a shutdown, so it goes back in at the head of its guild's
-- queue (positions may run negative; order is all `position` means).
INSERT INTO guild_session_queue
    (guild_id, position, video_id, title, channel, duration_secs, requested_by)
SELECT s.guild_id,
       (SELECT COALESCE(MIN(q.position), 1) - 1
        FROM guild_session_queue q WHERE q.guild_id = s.guild_id),
       s.now_playing_video_id, s.now_playing_title, s.now_playing_channel,
       s.now_playing_duration_secs, s.now_playing_requested_by
FROM guild_sessions s
WHERE s.now_playing_video_id IS NOT NULL
  AND s.now_playing_title IS NOT NULL
  AND s.now_playing_channel IS NOT NULL
  AND s.now_playing_requested_by IS NOT NULL;

ALTER TABLE guild_sessions DROP COLUMN now_playing_video_id;
ALTER TABLE guild_sessions DROP COLUMN now_playing_title;
ALTER TABLE guild_sessions DROP COLUMN now_playing_channel;
ALTER TABLE guild_sessions DROP COLUMN now_playing_duration_secs;
ALTER TABLE guild_sessions DROP COLUMN now_playing_requested_by;
