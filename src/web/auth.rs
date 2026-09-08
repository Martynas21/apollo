use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use argon2::{Argon2, PasswordHasher, PasswordVerifier};
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use rand::Rng;

use crate::db;

/// Dashboard sessions are opaque bearer tokens with no persistence: they
/// live only in memory and are gone on restart (every open dashboard tab
/// just has to log in again), which is fine for a small, self-hosted,
/// single-operator tool.
const SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);

#[derive(Clone, Default)]
pub struct SessionStore(Arc<Mutex<HashMap<String, Instant>>>);

impl SessionStore {
    fn issue(&self) -> String {
        let mut bytes = [0u8; 32];
        rand::rng().fill_bytes(&mut bytes);
        let token = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();

        let mut sessions = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sessions.retain(|_, expires_at| *expires_at > Instant::now());
        sessions.insert(token.clone(), Instant::now() + SESSION_TTL);
        token
    }

    fn is_valid(&self, token: &str) -> bool {
        let sessions = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sessions
            .get(token)
            .is_some_and(|expires_at| *expires_at > Instant::now())
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
    if db::dashboard_user_count(db).await? > 0 {
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
    db::insert_dashboard_user(db, username, &hash).await?;
    tracing::info!(username, "bootstrapped the initial dashboard user");
    Ok(())
}

pub async fn verify_login(
    db: &sqlx::SqlitePool,
    username: &str,
    password: &str,
) -> anyhow::Result<bool> {
    let Some(hash) = db::dashboard_user_password_hash(db, username).await? else {
        return Ok(false);
    };
    Ok(verify_password(password, &hash))
}

pub fn issue_session(sessions: &SessionStore) -> String {
    sessions.issue()
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
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let token = bearer_token(&request).or_else(|| query_token(&request));
    match token {
        Some(token) if state.sessions.is_valid(&token) => Ok(next.run(request).await),
        _ => Err(StatusCode::UNAUTHORIZED),
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
        let token = sessions.issue();
        assert!(sessions.is_valid(&token));
    }

    #[test]
    fn an_unknown_token_is_never_valid() {
        let sessions = SessionStore::default();
        assert!(!sessions.is_valid("not-a-real-token"));
    }

    #[tokio::test]
    async fn bootstrap_creates_the_first_user_from_env_credentials() -> anyhow::Result<()> {
        let pool = db::connect("sqlite::memory:").await?;

        bootstrap_user_if_needed(&pool, Some("admin"), Some("hunter2")).await?;

        assert_eq!(db::dashboard_user_count(&pool).await?, 1);
        assert!(verify_login(&pool, "admin", "hunter2").await?);
        assert!(!verify_login(&pool, "admin", "wrong-password").await?);
        Ok(())
    }

    #[tokio::test]
    async fn bootstrap_without_env_credentials_creates_no_user() -> anyhow::Result<()> {
        let pool = db::connect("sqlite::memory:").await?;

        bootstrap_user_if_needed(&pool, None, None).await?;

        assert_eq!(db::dashboard_user_count(&pool).await?, 0);
        Ok(())
    }

    #[tokio::test]
    async fn bootstrap_never_overwrites_an_existing_user() -> anyhow::Result<()> {
        let pool = db::connect("sqlite::memory:").await?;
        bootstrap_user_if_needed(&pool, Some("admin"), Some("first-password")).await?;

        // A restart with different DASHBOARD_* env vars must not reset a
        // password someone has since changed via some future admin flow.
        bootstrap_user_if_needed(&pool, Some("admin"), Some("second-password")).await?;

        assert_eq!(db::dashboard_user_count(&pool).await?, 1);
        assert!(verify_login(&pool, "admin", "first-password").await?);
        Ok(())
    }

    #[tokio::test]
    async fn verify_login_for_an_unknown_username_is_false_not_an_error() -> anyhow::Result<()> {
        let pool = db::connect("sqlite::memory:").await?;
        assert!(!verify_login(&pool, "nobody", "anything").await?);
        Ok(())
    }
}
