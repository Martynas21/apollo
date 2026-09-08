-- Local accounts allowed to sign in to the web dashboard.
--
-- There is no self-service signup: the first account is bootstrapped from
-- DASHBOARD_USERNAME/DASHBOARD_PASSWORD on startup, but only while this
-- table is empty (see `web::bootstrap_user_if_needed`).
CREATE TABLE IF NOT EXISTS dashboard_users (
    username TEXT PRIMARY KEY NOT NULL,
    password_hash TEXT NOT NULL
);
