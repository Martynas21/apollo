-- "dashboard_users" was the only kind of user this app has; drop the
-- redundant prefix. Also adds the admin flag: the env-bootstrapped account
-- becomes an admin (see web::bootstrap_user_if_needed), and only admins can
-- create/delete other accounts from the dashboard's Users page.
ALTER TABLE dashboard_users RENAME TO users;
ALTER TABLE users ADD COLUMN is_admin INTEGER NOT NULL DEFAULT 0;

-- Before this feature there was no non-admin concept, so whichever account
-- already exists at this point (the only case `DEFAULT 0` above gets wrong)
-- was already the de facto admin — promote it. On a fresh install the table
-- is still empty here, so this is a no-op.
UPDATE users SET is_admin = 1;
