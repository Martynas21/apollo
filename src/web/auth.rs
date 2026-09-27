use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use argon2::{Argon2, PasswordHasher, PasswordVerifier};
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use rand::Rng;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::db;

/// Dashboard sessions are opaque bearer tokens stored in the database (the
/// `sessions` table), so they survive restarts and rebuilds: an open
/// dashboard only has to log in again once its token has aged out.
const SESSION_TTL: Duration = Duration::from_secs(72 * 60 * 60);

/// Failed logins allowed for a single username inside [`LOGIN_THROTTLE_WINDOW`]
/// before further attempts for that username are rejected outright — high
/// enough that a few mistyped passwords in a row don't lock anyone out, low
/// enough to make online brute-forcing impractical.
const LOGIN_FAILURE_THRESHOLD: u32 = 5;

/// Rolling window a username's failures are counted over, and how long a
/// throttled username has to wait before it may try again.
const LOGIN_THROTTLE_WINDOW: Duration = Duration::from_secs(5 * 60);

/// Hard cap on distinct usernames tracked at once. The map is keyed by
/// attacker-controlled input, so without a cap it would grow without bound;
/// entries also expire and get swept on every insert (see
/// [`LoginThrottle::record_failure`]) — this cap is just a backstop against a
/// burst of distinct usernames arriving faster than they expire.
const MAX_TRACKED_USERNAMES: usize = 10_000;

/// Upper bound on logins allowed to be hashing a password at once.
/// `Argon2::default()` costs roughly 19 MiB per call, so this bounds peak
/// memory attributable to the login endpoint to about this many times that,
/// no matter how many requests arrive concurrently.
const MAX_CONCURRENT_LOGIN_HASHES: usize = 16;

/// The identity behind a validated session, as resolved at login time.
/// Inserted into request extensions by [`require_session`] so downstream
/// handlers/middleware (e.g. [`require_admin`]) can read who's asking
/// without a further DB round-trip.
#[derive(Clone)]
pub struct CurrentUser {
    pub username: String,
    pub is_admin: bool,
    /// The single env-bootstrapped account — the only one allowed to change
    /// another user's password (see `web::users::set_password`).
    pub is_root: bool,
    /// The one guild this account is confined to, or `None` for every guild
    /// the bot is in. Only meaningful for non-admins — see
    /// [`CurrentUser::may_access_guild`].
    pub guild_id: Option<String>,
}

impl CurrentUser {
    /// Whether this account may see and control `guild_id`. Admins reach
    /// every guild, as does anyone with no guild pinned; everyone else is
    /// confined to the single guild recorded on their account.
    ///
    /// Admins are exempt because they can reassign this field on any account
    /// including their own (see `web::users::set_guild`), so enforcing it
    /// against them would describe a boundary they could lift at will.
    pub fn may_access_guild(&self, guild_id: &str) -> bool {
        self.is_admin
            || self
                .guild_id
                .as_deref()
                .is_none_or(|pinned| pinned == guild_id)
    }
}

/// The bearer token behind a validated session, inserted into request
/// extensions by [`require_session`] alongside [`CurrentUser`] so a handler
/// that needs to act on the caller's own session (e.g. logout) can do so
/// without re-parsing the `Authorization` header.
#[derive(Clone)]
pub struct SessionToken(pub String);

impl From<db::UserSummary> for CurrentUser {
    fn from(user: db::UserSummary) -> Self {
        Self {
            username: user.username,
            is_admin: user.is_admin,
            is_root: user.is_root,
            guild_id: user.guild_id,
        }
    }
}

/// Seconds since the unix epoch, the clock every `sessions.expires_at` is
/// compared against. A clock set before 1970 reads as the epoch itself.
fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Mints a fresh session for `username`, sweeping any expired rows first so
/// the table only ever grows by as many logins as are still live.
pub async fn issue_session(db: &sqlx::SqlitePool, username: &str) -> anyhow::Result<String> {
    let now = unix_now();
    db::delete_expired_sessions(db, now).await?;
    let token = random_token();
    let expires_at = now.saturating_add(i64::try_from(SESSION_TTL.as_secs()).unwrap_or(i64::MAX));
    db::insert_session(db, &token, username, expires_at).await?;
    Ok(token)
}

/// The account behind `token`, or `None` if it's unknown or expired. Read
/// live from `users`, so an account's current privileges and guild pin
/// apply to every session it holds without any per-session bookkeeping;
/// a rename carries its sessions along and a deletion drops them (both
/// cascades on the `sessions` table).
pub async fn session_user(
    db: &sqlx::SqlitePool,
    token: &str,
) -> anyhow::Result<Option<CurrentUser>> {
    Ok(db::session_user(db, token, unix_now())
        .await?
        .map(CurrentUser::from))
}

/// Drops a single session by its token, for signing out of just the
/// browser tab that asked.
pub async fn revoke_session(db: &sqlx::SqlitePool, token: &str) -> anyhow::Result<()> {
    db::delete_session(db, token).await
}

/// Drops every session belonging to this account, so one whose password
/// was just reset can't keep authenticating on a token issued before that
/// change.
pub async fn revoke_user_sessions(db: &sqlx::SqlitePool, username: &str) -> anyhow::Result<()> {
    db::delete_user_sessions(db, username).await
}

struct LoginAttempts {
    failures: u32,
    window_started_at: Instant,
}

/// Guards `POST /api/login` against online brute-forcing: a per-username
/// failure counter (see [`LoginThrottle::remaining_lockout`],
/// [`LoginThrottle::record_failure`] and [`LoginThrottle::record_success`])
/// plus a global cap on how many logins may be hashing a password at once
/// (see [`LoginThrottle::try_acquire_hash_permit`]).
#[derive(Clone)]
pub struct LoginThrottle {
    attempts: Arc<Mutex<HashMap<String, LoginAttempts>>>,
    concurrent_hashes: Arc<Semaphore>,
}

impl Default for LoginThrottle {
    fn default() -> Self {
        Self {
            attempts: Arc::new(Mutex::new(HashMap::new())),
            concurrent_hashes: Arc::new(Semaphore::new(MAX_CONCURRENT_LOGIN_HASHES)),
        }
    }
}

impl LoginThrottle {
    /// `Some(remaining)` if `username` is currently locked out, with the
    /// wait left before it may try again; `None` if the attempt may proceed.
    pub fn remaining_lockout(&self, username: &str) -> Option<Duration> {
        let attempts = self
            .attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let record = attempts.get(username)?;
        let elapsed = record.window_started_at.elapsed();
        (record.failures >= LOGIN_FAILURE_THRESHOLD && elapsed < LOGIN_THROTTLE_WINDOW)
            .then(|| LOGIN_THROTTLE_WINDOW - elapsed)
    }

    /// Records a failed attempt for `username`, starting a fresh window if
    /// none is currently active for it. Sweeps expired records first,
    /// mirroring the retain-on-insert pattern [`SessionStore::issue`] uses.
    pub fn record_failure(&self, username: &str) {
        let mut attempts = self
            .attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        attempts.retain(|_, record| record.window_started_at.elapsed() < LOGIN_THROTTLE_WINDOW);

        if let Some(record) = attempts.get_mut(username) {
            record.failures += 1;
            return;
        }
        if attempts.len() < MAX_TRACKED_USERNAMES {
            attempts.insert(
                username.to_string(),
                LoginAttempts {
                    failures: 1,
                    window_started_at: Instant::now(),
                },
            );
        }
    }

    /// Clears any failure record for `username` — called on a successful
    /// login so a past run of bad attempts doesn't linger against an account
    /// that has since proven it holds the right password.
    pub fn record_success(&self, username: &str) {
        let mut attempts = self
            .attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        attempts.remove(username);
    }

    /// Reserves one slot for doing Argon2 work, or `None` if
    /// [`MAX_CONCURRENT_LOGIN_HASHES`] logins are already hashing a
    /// password. The permit must be held for the duration of that work.
    pub fn try_acquire_hash_permit(&self) -> Option<OwnedSemaphorePermit> {
        self.concurrent_hashes.clone().try_acquire_owned().ok()
    }
}

pub fn hash_password(password: &str) -> anyhow::Result<String> {
    let hash = Argon2::default()
        .hash_password(password.as_bytes())
        .map_err(|err| anyhow::anyhow!("failed to hash password: {err}"))?;
    Ok(hash.to_string())
}

fn verify_password(password: &str, hash: &str) -> bool {
    Argon2::default()
        .verify_password(password.as_bytes(), hash)
        .is_ok()
}

/// Creates the dashboard's first login from `DASHBOARD_USERNAME`/
/// `DASHBOARD_PASSWORD`, but only while no dashboard user exists yet — this
/// runs on every startup, so it must never overwrite a password someone
/// has since changed (there's no "change password" flow yet, but there
/// will be, and this must not stomp on it).
pub async fn bootstrap_user_if_needed(
    db: &sqlx::SqlitePool,
    username: Option<&str>,
    password: Option<&str>,
) -> anyhow::Result<()> {
    if db::user_count(db).await? > 0 {
        return Ok(());
    }
    let (Some(username), Some(password)) = (username, password) else {
        tracing::warn!(
            "no dashboard users exist yet and DASHBOARD_USERNAME/DASHBOARD_PASSWORD are not \
             set — the web dashboard will reject all logins until a user is created"
        );
        return Ok(());
    };
    let hash = hash_password(password)?;
    db::insert_user(db, username, &hash, true, true, None).await?;
    tracing::info!(username, "bootstrapped the initial dashboard admin");
    Ok(())
}

/// A validly-formatted Argon2 hash with no corresponding account, computed
/// once and reused so the unknown-username branch of [`verify_login`] pays
/// the same Argon2 cost as a wrong password against a real account. Without
/// this, the two failure modes are trivially distinguishable by timing,
/// which is exactly what `verify_login`'s contract rules out.
fn dummy_login_hash() -> Option<&'static str> {
    static DUMMY_HASH: OnceLock<Option<String>> = OnceLock::new();
    DUMMY_HASH
        .get_or_init(|| hash_password("apollo dashboard dummy hash for timing equalization").ok())
        .as_deref()
}

/// Returns the resolved identity on success, `None` for an unknown username
/// or a wrong password (deliberately not distinguished, so a login failure
/// can't be used to enumerate valid usernames).
pub async fn verify_login(
    db: &sqlx::SqlitePool,
    username: &str,
    password: &str,
) -> anyhow::Result<Option<CurrentUser>> {
    let Some(creds) = db::user_credentials(db, username).await? else {
        if let Some(dummy_hash) = dummy_login_hash() {
            verify_password(password, dummy_hash);
        }
        return Ok(None);
    };
    Ok(
        verify_password(password, &creds.password_hash).then(|| CurrentUser {
            username: username.to_string(),
            is_admin: creds.is_admin,
            is_root: creds.is_root,
            guild_id: creds.guild_id,
        }),
    )
}

fn bearer_token(request: &Request) -> Option<String> {
    let header = request.headers().get(axum::http::header::AUTHORIZATION)?;
    let header = header.to_str().ok()?;
    header
        .strip_prefix("Bearer ")
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string)
}

/// The WebSocket route can't rely on the `Authorization` header at all —
/// browsers don't let JavaScript attach custom headers to a WebSocket
/// handshake — so its session token travels as a query parameter instead.
fn query_token(request: &Request) -> Option<String> {
    let query = request.uri().query()?;
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == "token").then(|| value.to_string())
    })
}

pub async fn require_session(
    State(state): State<crate::web::WebState>,
    mut request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let Some(token) = bearer_token(&request).or_else(|| query_token(&request)) else {
        return Err(StatusCode::UNAUTHORIZED);
    };
    let user = match session_user(&state.db, &token).await {
        Ok(Some(user)) => user,
        Ok(None) => return Err(StatusCode::UNAUTHORIZED),
        Err(err) => {
            tracing::warn!(%err, "failed to look up a dashboard session");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    request.extensions_mut().insert(user);
    request.extensions_mut().insert(SessionToken(token));
    Ok(next.run(request).await)
}

/// The guild a `/api/guilds/{guild_id}/...` path is addressing, or `None`
/// for any other path.
fn guild_id_from_path(path: &str) -> Option<&str> {
    path.strip_prefix("/api/guilds/")?
        .split('/')
        .next()
        .filter(|segment| !segment.is_empty())
}

/// Confines a guild-pinned account to its own guild. Paths outside
/// `/api/guilds/{guild_id}/...` address no guild and pass straight through,
/// so a guild-scoped route has to live under that prefix to be covered here.
///
/// Must run after [`require_session`], which puts the [`CurrentUser`] this
/// reads into the request's extensions.
pub async fn require_guild_access(request: Request, next: Next) -> Result<Response, StatusCode> {
    let guild_id = guild_id_from_path(request.uri().path()).map(str::to_string);
    let Some(guild_id) = guild_id else {
        return Ok(next.run(request).await);
    };
    match request.extensions().get::<CurrentUser>() {
        Some(user) if user.may_access_guild(&guild_id) => Ok(next.run(request).await),
        Some(_) => Err(StatusCode::FORBIDDEN),
        None => Err(StatusCode::UNAUTHORIZED),
    }
}

/// Must run after [`require_session`] on the same route (which puts the
/// [`CurrentUser`] extension in place) — see the merge-then-`route_layer`
/// ordering in `web::serve`.
pub async fn require_admin(request: Request, next: Next) -> Result<Response, StatusCode> {
    match request.extensions().get::<CurrentUser>() {
        Some(user) if user.is_admin => Ok(next.run(request).await),
        Some(_) => Err(StatusCode::FORBIDDEN),
        None => Err(StatusCode::UNAUTHORIZED),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    #[test]
    fn a_password_verifies_against_its_own_hash() {
        let hash = hash_password("hunter2").expect("hashing should succeed");
        assert!(verify_password("hunter2", &hash));
    }

    #[test]
    fn the_wrong_password_does_not_verify() {
        let hash = hash_password("hunter2").expect("hashing should succeed");
        assert!(!verify_password("not-hunter2", &hash));
    }

    #[test]
    fn a_malformed_hash_never_verifies() {
        assert!(!verify_password("hunter2", "not-a-real-phc-hash"));
    }

    async fn pool_with_user(username: &str, is_admin: bool) -> anyhow::Result<sqlx::SqlitePool> {
        let pool = db::connect("sqlite::memory:").await?;
        db::insert_user(&pool, username, "some-hash", is_admin, is_admin, None).await?;
        Ok(pool)
    }

    #[tokio::test]
    async fn a_freshly_issued_session_is_valid() -> anyhow::Result<()> {
        let pool = pool_with_user("admin", true).await?;

        let token = issue_session(&pool, "admin").await?;

        let user = session_user(&pool, &token)
            .await?
            .expect("session should be valid");
        assert_eq!(user.username, "admin");
        assert!(user.is_admin);
        assert!(user.is_root);
        Ok(())
    }

    #[tokio::test]
    async fn an_unknown_token_is_never_valid() -> anyhow::Result<()> {
        let pool = db::connect("sqlite::memory:").await?;
        assert!(session_user(&pool, "not-a-real-token").await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn an_expired_session_is_not_valid() -> anyhow::Result<()> {
        let pool = pool_with_user("admin", true).await?;
        db::insert_session(&pool, "stale", "admin", unix_now() - 1).await?;

        assert!(session_user(&pool, "stale").await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn issuing_a_session_sweeps_expired_ones() -> anyhow::Result<()> {
        let pool = pool_with_user("admin", true).await?;
        db::insert_session(&pool, "stale", "admin", unix_now() - 1).await?;

        issue_session(&pool, "admin").await?;

        let (count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM sessions WHERE token = 'stale'")
                .fetch_one(&pool)
                .await?;
        assert_eq!(count, 0);
        Ok(())
    }

    #[tokio::test]
    async fn a_session_reflects_the_accounts_current_privileges() -> anyhow::Result<()> {
        let pool = pool_with_user("old-name", false).await?;
        let token = issue_session(&pool, "old-name").await?;

        db::rename_user(&pool, "old-name", "new-name").await?;
        db::set_user_guild(&pool, "new-name", Some("42")).await?;

        let user = session_user(&pool, &token)
            .await?
            .expect("session should survive the rename");
        assert_eq!(user.username, "new-name");
        assert_eq!(user.guild_id.as_deref(), Some("42"));
        Ok(())
    }

    #[tokio::test]
    async fn revoking_a_user_invalidates_only_that_users_sessions() -> anyhow::Result<()> {
        let pool = pool_with_user("alice", false).await?;
        db::insert_user(&pool, "bob", "some-hash", false, false, None).await?;
        let alice_token = issue_session(&pool, "alice").await?;
        let bob_token = issue_session(&pool, "bob").await?;

        revoke_user_sessions(&pool, "alice").await?;

        assert!(session_user(&pool, &alice_token).await?.is_none());
        assert_eq!(
            session_user(&pool, &bob_token)
                .await?
                .expect("bob's session should still be valid")
                .username,
            "bob"
        );
        Ok(())
    }

    #[tokio::test]
    async fn revoking_a_token_invalidates_only_that_token() -> anyhow::Result<()> {
        let pool = pool_with_user("alice", false).await?;
        let first_token = issue_session(&pool, "alice").await?;
        let second_token = issue_session(&pool, "alice").await?;

        revoke_session(&pool, &first_token).await?;

        assert!(session_user(&pool, &first_token).await?.is_none());
        assert!(session_user(&pool, &second_token).await?.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn bootstrap_creates_the_first_user_as_an_admin_and_root() -> anyhow::Result<()> {
        let pool = db::connect("sqlite::memory:").await?;

        bootstrap_user_if_needed(&pool, Some("admin"), Some("hunter2")).await?;

        assert_eq!(db::user_count(&pool).await?, 1);
        let user = verify_login(&pool, "admin", "hunter2")
            .await?
            .expect("login should succeed");
        assert!(user.is_admin);
        assert!(user.is_root);
        assert!(
            verify_login(&pool, "admin", "wrong-password")
                .await?
                .is_none()
        );
        Ok(())
    }

    #[tokio::test]
    async fn bootstrap_without_env_credentials_creates_no_user() -> anyhow::Result<()> {
        let pool = db::connect("sqlite::memory:").await?;

        bootstrap_user_if_needed(&pool, None, None).await?;

        assert_eq!(db::user_count(&pool).await?, 0);
        Ok(())
    }

    #[tokio::test]
    async fn bootstrap_never_overwrites_an_existing_user() -> anyhow::Result<()> {
        let pool = db::connect("sqlite::memory:").await?;
        bootstrap_user_if_needed(&pool, Some("admin"), Some("first-password")).await?;

        // A restart with different DASHBOARD_* env vars must not reset a
        // password someone has since changed via some future admin flow.
        bootstrap_user_if_needed(&pool, Some("admin"), Some("second-password")).await?;

        assert_eq!(db::user_count(&pool).await?, 1);
        assert!(
            verify_login(&pool, "admin", "first-password")
                .await?
                .is_some_and(|user| user.is_admin)
        );
        Ok(())
    }

    #[tokio::test]
    async fn verify_login_for_an_unknown_username_is_none_not_an_error() -> anyhow::Result<()> {
        let pool = db::connect("sqlite::memory:").await?;
        assert!(verify_login(&pool, "nobody", "anything").await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn a_non_admin_user_created_by_an_admin_verifies_as_non_admin_non_root()
    -> anyhow::Result<()> {
        let pool = db::connect("sqlite::memory:").await?;
        let hash = hash_password("hunter2")?;
        db::insert_user(&pool, "listener", &hash, false, false, None).await?;

        let user = verify_login(&pool, "listener", "hunter2")
            .await?
            .expect("login should succeed");
        assert!(!user.is_admin);
        assert!(!user.is_root);
        Ok(())
    }

    #[tokio::test]
    async fn an_unknown_username_and_a_wrong_password_for_a_known_username_both_return_none()
    -> anyhow::Result<()> {
        let pool = db::connect("sqlite::memory:").await?;
        bootstrap_user_if_needed(&pool, Some("admin"), Some("hunter2")).await?;

        assert!(
            verify_login(&pool, "nobody-registered", "whatever")
                .await?
                .is_none()
        );
        assert!(
            verify_login(&pool, "admin", "wrong-password")
                .await?
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn the_throttle_locks_out_a_username_after_the_failure_threshold_and_resets_on_success() {
        let throttle = LoginThrottle::default();
        let username = "flaky-login";

        for _ in 0..LOGIN_FAILURE_THRESHOLD {
            assert!(throttle.remaining_lockout(username).is_none());
            throttle.record_failure(username);
        }
        assert!(throttle.remaining_lockout(username).is_some());

        throttle.record_success(username);
        assert!(throttle.remaining_lockout(username).is_none());
    }

    #[test]
    fn the_throttle_leaves_other_usernames_unaffected_by_one_usernames_failures() {
        let throttle = LoginThrottle::default();

        for _ in 0..LOGIN_FAILURE_THRESHOLD {
            throttle.record_failure("attacker-controlled");
        }

        assert!(throttle.remaining_lockout("attacker-controlled").is_some());
        assert!(throttle.remaining_lockout("someone-else").is_none());
    }

    #[test]
    fn expired_throttle_records_are_swept_rather_than_accumulating() {
        let throttle = LoginThrottle::default();
        {
            let mut attempts = throttle
                .attempts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            attempts.insert(
                "stale-user".to_string(),
                LoginAttempts {
                    failures: LOGIN_FAILURE_THRESHOLD,
                    window_started_at: Instant::now()
                        - LOGIN_THROTTLE_WINDOW
                        - Duration::from_secs(1),
                },
            );
        }

        throttle.record_failure("someone-else");

        let attempts = throttle
            .attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(!attempts.contains_key("stale-user"));
    }

    #[test]
    fn hash_permits_are_bounded_by_the_concurrency_cap() {
        let throttle = LoginThrottle::default();

        let permits: Vec<_> = (0..MAX_CONCURRENT_LOGIN_HASHES)
            .map(|_| {
                throttle
                    .try_acquire_hash_permit()
                    .expect("permit should be available under the cap")
            })
            .collect();

        assert!(throttle.try_acquire_hash_permit().is_none());

        drop(permits);
        assert!(throttle.try_acquire_hash_permit().is_some());
    }
}
