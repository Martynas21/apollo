use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use argon2::{Argon2, PasswordHasher, PasswordVerifier};
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use rand::Rng;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::db;

/// Dashboard sessions are opaque bearer tokens with no persistence: they
/// live only in memory and are gone on restart (every open dashboard tab
/// just has to log in again), which is fine for a small, self-hosted,
/// single-operator tool.
const SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);

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

struct SessionInfo {
    user: CurrentUser,
    expires_at: Instant,
}

#[derive(Clone, Default)]
pub struct SessionStore(Arc<Mutex<HashMap<String, SessionInfo>>>);

impl SessionStore {
    fn issue(&self, user: CurrentUser) -> String {
        let mut bytes = [0u8; 32];
        rand::rng().fill_bytes(&mut bytes);
        let token = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();

        let mut sessions = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sessions.retain(|_, info| info.expires_at > Instant::now());
        sessions.insert(
            token.clone(),
            SessionInfo {
                user,
                expires_at: Instant::now() + SESSION_TTL,
            },
        );
        token
    }

    fn get(&self, token: &str) -> Option<CurrentUser> {
        let sessions = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sessions
            .get(token)
            .filter(|info| info.expires_at > Instant::now())
            .map(|info| info.user.clone())
    }

    /// Keeps any already-issued session(s) for this account pointing at its
    /// new name, so a rename doesn't strand an active session under a
    /// username the DB no longer has — without this, that session's own
    /// self-checks (e.g. in `users::set_password`) would start failing.
    pub fn rename(&self, old_username: &str, new_username: &str) {
        let mut sessions = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for info in sessions.values_mut() {
            if info.user.username == old_username {
                info.user.username = new_username.to_string();
            }
        }
    }

    /// Drops every session belonging to this account, so a deleted user or
    /// one whose password was just reset can't keep authenticating on a
    /// token issued before that change.
    pub fn revoke_user(&self, username: &str) {
        let mut sessions = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sessions.retain(|_, info| info.user.username != username);
    }

    /// Drops a single session by its token, for signing out of just the
    /// browser tab that asked.
    pub fn revoke_token(&self, token: &str) {
        let mut sessions = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sessions.remove(token);
    }
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

pub fn issue_session(sessions: &SessionStore, user: CurrentUser) -> String {
    sessions.issue(user)
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
    let Some(user) = state.sessions.get(&token) else {
        return Err(StatusCode::UNAUTHORIZED);
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

    #[test]
    fn a_freshly_issued_session_is_valid() {
        let sessions = SessionStore::default();
        let token = sessions.issue(CurrentUser {
            username: "admin".to_string(),
            is_admin: true,
            is_root: true,
            guild_id: None,
        });
        let user = sessions.get(&token).expect("session should be valid");
        assert_eq!(user.username, "admin");
        assert!(user.is_admin);
        assert!(user.is_root);
    }

    #[test]
    fn an_unknown_token_is_never_valid() {
        let sessions = SessionStore::default();
        assert!(sessions.get("not-a-real-token").is_none());
    }

    #[test]
    fn renaming_a_session_updates_its_username_in_place() {
        let sessions = SessionStore::default();
        let token = sessions.issue(CurrentUser {
            username: "old-name".to_string(),
            is_admin: true,
            is_root: false,
            guild_id: None,
        });

        sessions.rename("old-name", "new-name");

        let user = sessions.get(&token).expect("session should still be valid");
        assert_eq!(user.username, "new-name");
        assert!(user.is_admin);
    }

    #[test]
    fn renaming_leaves_sessions_for_other_usernames_untouched() {
        let sessions = SessionStore::default();
        let token = sessions.issue(CurrentUser {
            username: "someone-else".to_string(),
            is_admin: false,
            is_root: false,
            guild_id: None,
        });

        sessions.rename("old-name", "new-name");

        assert_eq!(
            sessions
                .get(&token)
                .expect("session should still be valid")
                .username,
            "someone-else"
        );
    }

    #[test]
    fn revoking_a_user_invalidates_that_users_session() {
        let sessions = SessionStore::default();
        let token = sessions.issue(CurrentUser {
            username: "alice".to_string(),
            is_admin: false,
            is_root: false,
            guild_id: None,
        });

        sessions.revoke_user("alice");

        assert!(sessions.get(&token).is_none());
    }

    #[test]
    fn revoking_a_user_leaves_other_users_sessions_valid() {
        let sessions = SessionStore::default();
        let alice_token = sessions.issue(CurrentUser {
            username: "alice".to_string(),
            is_admin: false,
            is_root: false,
            guild_id: None,
        });
        let bob_token = sessions.issue(CurrentUser {
            username: "bob".to_string(),
            is_admin: false,
            is_root: false,
            guild_id: None,
        });

        sessions.revoke_user("alice");

        assert!(sessions.get(&alice_token).is_none());
        assert_eq!(
            sessions
                .get(&bob_token)
                .expect("bob's session should still be valid")
                .username,
            "bob"
        );
    }

    #[test]
    fn revoking_a_token_invalidates_only_that_token() {
        let sessions = SessionStore::default();
        let first_token = sessions.issue(CurrentUser {
            username: "alice".to_string(),
            is_admin: false,
            is_root: false,
            guild_id: None,
        });
        let second_token = sessions.issue(CurrentUser {
            username: "alice".to_string(),
            is_admin: false,
            is_root: false,
            guild_id: None,
        });

        sessions.revoke_token(&first_token);

        assert!(sessions.get(&first_token).is_none());
        assert!(sessions.get(&second_token).is_some());
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
