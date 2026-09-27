-- Dashboard sessions live in the database rather than in process memory, so
-- a restart (a rebuild, a redeploy, a crash) doesn't sign every open
-- dashboard out. A row is one bearer token; `expires_at` is unix seconds.
--
-- The username cascades so a renamed account keeps its sessions and a
-- deleted one loses them without any bookkeeping on the web side.
CREATE TABLE IF NOT EXISTS sessions (
    token TEXT PRIMARY KEY NOT NULL,
    username TEXT NOT NULL
        REFERENCES users(username) ON DELETE CASCADE ON UPDATE CASCADE,
    expires_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS sessions_username ON sessions (username);
