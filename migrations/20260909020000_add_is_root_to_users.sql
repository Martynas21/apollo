-- The env-bootstrapped account is the only one allowed to change another
-- user's password (see web::users::set_password) — every other admin can
-- only change their own, same as a regular user. There is at most one root:
-- bootstrap only ever creates the first account (web::bootstrap_user_if_needed),
-- which is also promoted to admin, so it's the sole existing admin at the
-- point this runs.
ALTER TABLE users ADD COLUMN is_root INTEGER NOT NULL DEFAULT 0;
UPDATE users SET is_root = 1 WHERE is_admin = 1;
