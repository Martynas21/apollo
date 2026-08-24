//! Google OAuth2 client construction and token lifecycle helpers.
//!
//! Scopes requested are limited to read-only YouTube access (`/link` only
//! needs to look up videos/playlists on the user's behalf, never to modify
//! their account).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use oauth2::basic::BasicClient;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, EndpointNotSet, EndpointSet, RedirectUrl,
    RefreshToken, RevocationUrl, TokenResponse, TokenUrl,
};
use poise::serenity_prelude as serenity;

use crate::config::Config;
use crate::db::{self, StoredToken};

const GOOGLE_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const GOOGLE_REVOKE_URL: &str = "https://oauth2.googleapis.com/revoke";

/// Scope requested when linking an account.
pub const YOUTUBE_READONLY_SCOPE: &str = "https://www.googleapis.com/auth/youtube.readonly";

/// Safety buffer (seconds) subtracted from an access token's real expiry
/// before we consider it due for a refresh, so a token doesn't expire
/// mid-flight between the check and its use.
const EXPIRY_BUFFER_SECS: i64 = 60;

/// Fallback lifetime (seconds) assumed for an access token when Google's
/// token response omits `expires_in`. Google always sends it in practice;
/// this only guards against a technically-permitted absence causing an
/// unbounded/garbage expiry.
const DEFAULT_TOKEN_LIFETIME_SECS: i64 = 3600;

/// Concrete `oauth2` client type once the auth, token, and revocation
/// endpoints are set (device-auth and introspection endpoints are unused
/// and stay `EndpointNotSet`).
pub type GoogleOAuthClient =
    BasicClient<EndpointSet, EndpointNotSet, EndpointNotSet, EndpointSet, EndpointSet>;

/// In-flight `/link` attempts, keyed by CSRF state token, mapping back to
/// the Discord user who started them. No TTL/expiry sweep: an abandoned
/// `/link` just leaves a small, harmless entry until the process restarts,
/// which isn't worth the extra bookkeeping at this bot's scale.
pub type PendingLinks = Arc<Mutex<HashMap<String, serenity::UserId>>>;

/// Builds the Google OAuth2 client from bot configuration.
pub fn build_oauth_client(config: &Config) -> Result<GoogleOAuthClient> {
    Ok(
        BasicClient::new(ClientId::new(config.google_client_id.clone()))
            .set_client_secret(ClientSecret::new(config.google_client_secret.clone()))
            .set_auth_uri(AuthUrl::new(GOOGLE_AUTH_URL.to_string())?)
            .set_token_uri(TokenUrl::new(GOOGLE_TOKEN_URL.to_string())?)
            .set_revocation_url(RevocationUrl::new(GOOGLE_REVOKE_URL.to_string())?)
            .set_redirect_uri(RedirectUrl::new(config.google_oauth_redirect_uri.clone())?),
    )
}

/// Builds the HTTP client used for all token endpoint requests (code
/// exchange, refresh, revocation). Shared across the callback server and
/// command handlers rather than rebuilt per call.
///
/// Disabling redirects follows `oauth2`'s own SSRF guidance: a malicious or
/// misconfigured token endpoint should not be able to redirect this client
/// somewhere else with the credentials/tokens attached.
pub fn build_http_client() -> Result<oauth2::reqwest::Client> {
    oauth2::reqwest::ClientBuilder::new()
        .redirect(oauth2::reqwest::redirect::Policy::none())
        .build()
        .context("failed to build OAuth2 HTTP client")
}

/// Returns whether an access token expiring at `expires_at_unix` should be
/// refreshed given the current time `now_unix`, applying a safety buffer so
/// callers don't hand out a token that expires moments later.
///
/// At exactly the buffer boundary (`now_unix == expires_at_unix -
/// EXPIRY_BUFFER_SECS`) this returns `true` — the buffer is inclusive, so a
/// token is refreshed as soon as it enters the buffer window rather than on
/// the call just after.
pub fn needs_refresh(expires_at_unix: i64, now_unix: i64) -> bool {
    now_unix >= expires_at_unix - EXPIRY_BUFFER_SECS
}

fn now_unix() -> Result<i64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_secs() as i64)
}

/// Why [`get_valid_access_token`] couldn't produce a token, distinguished so
/// callers can prompt correctly: "you've never linked" reads very
/// differently from "your link broke and needs to be redone."
#[derive(Debug)]
pub enum AccessTokenError {
    /// No row in the DB for this Discord user at all.
    NotLinked,
    /// A stored token exists but refreshing it failed.
    RefreshFailed {
        /// `true` when Google's response was specifically `invalid_grant`
        /// (RFC 6749) — the refresh token is revoked, expired, or otherwise
        /// permanently invalid, not a transient failure. The stored row has
        /// already been deleted in this case, since it can never succeed
        /// again unmodified. When `false` (network blip, Google-side
        /// hiccup, etc.), the row is left alone — a later call may well
        /// succeed with no user action needed.
        revoked: bool,
        message: String,
    },
}

impl std::fmt::Display for AccessTokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AccessTokenError::NotLinked => write!(f, "no linked Google account"),
            AccessTokenError::RefreshFailed { revoked: true, .. } => {
                write!(f, "Google access was revoked or expired")
            }
            AccessTokenError::RefreshFailed {
                revoked: false,
                message,
            } => write!(f, "failed to refresh Google access token: {message}"),
        }
    }
}

impl std::error::Error for AccessTokenError {}

/// Returns a valid (non-expired) access token for `discord_user_id`,
/// transparently refreshing and persisting a new one if the stored token is
/// at or near expiry.
pub async fn get_valid_access_token(
    oauth_client: &GoogleOAuthClient,
    http_client: &oauth2::reqwest::Client,
    pool: &sqlx::SqlitePool,
    discord_user_id: &str,
) -> Result<String, AccessTokenError> {
    let stored = db::get_token(pool, discord_user_id)
        .await
        .map_err(|e| AccessTokenError::RefreshFailed {
            revoked: false,
            message: e.to_string(),
        })?
        .ok_or(AccessTokenError::NotLinked)?;

    let now = now_unix().map_err(|e| AccessTokenError::RefreshFailed {
        revoked: false,
        message: e.to_string(),
    })?;
    if !needs_refresh(stored.expires_at, now) {
        return Ok(stored.access_token);
    }

    let token_result = oauth_client
        .exchange_refresh_token(&RefreshToken::new(stored.refresh_token.clone()))
        .request_async(http_client)
        .await;

    let token_result = match token_result {
        Ok(token_result) => token_result,
        Err(oauth2::RequestTokenError::ServerResponse(resp))
            if matches!(
                resp.error(),
                oauth2::basic::BasicErrorResponseType::InvalidGrant
            ) =>
        {
            // The refresh token itself is invalid/revoked/expired — no retry
            // of this exact request will ever succeed, so there's nothing
            // worth keeping around; delete it so `/link` cleanly re-links.
            let _ = db::delete_token(pool, discord_user_id).await;
            return Err(AccessTokenError::RefreshFailed {
                revoked: true,
                message: "refresh token invalid_grant".to_string(),
            });
        }
        Err(err) => {
            return Err(AccessTokenError::RefreshFailed {
                revoked: false,
                message: err.to_string(),
            });
        }
    };

    let expires_in = token_result
        .expires_in()
        .map(|d| d.as_secs() as i64)
        .unwrap_or(DEFAULT_TOKEN_LIFETIME_SECS);
    let refresh_token = token_result
        .refresh_token()
        .map(|t| t.secret().clone())
        .unwrap_or(stored.refresh_token);
    let access_token = token_result.access_token().secret().clone();

    let updated = StoredToken {
        discord_user_id: discord_user_id.to_string(),
        access_token: access_token.clone(),
        refresh_token,
        expires_at: now + expires_in,
        scopes: stored.scopes,
    };
    db::upsert_token(pool, &updated)
        .await
        .map_err(|e| AccessTokenError::RefreshFailed {
            revoked: false,
            message: e.to_string(),
        })?;

    Ok(access_token)
}

/// Completes the authorization-code exchange for a `/link` callback and
/// persists the resulting tokens.
///
/// Google may omit `refresh_token` on a re-link even with `prompt=consent`
/// in some edge cases; when that happens this falls back to the
/// previously-stored refresh token for the user, only failing if there is
/// neither a fresh one nor a prior one on file.
pub async fn exchange_code_and_store(
    oauth_client: &GoogleOAuthClient,
    http_client: &oauth2::reqwest::Client,
    pool: &sqlx::SqlitePool,
    discord_user_id: &str,
    code: String,
) -> Result<()> {
    let token_result = oauth_client
        .exchange_code(AuthorizationCode::new(code))
        .request_async(http_client)
        .await
        .context("failed to exchange authorization code")?;

    let now = now_unix()?;
    let expires_in = token_result
        .expires_in()
        .map(|d| d.as_secs() as i64)
        .unwrap_or(DEFAULT_TOKEN_LIFETIME_SECS);

    let refresh_token = match token_result.refresh_token() {
        Some(rt) => rt.secret().clone(),
        None => db::get_token(pool, discord_user_id)
            .await?
            .map(|t| t.refresh_token)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Google did not return a refresh token and none is on file for this user"
                )
            })?,
    };

    let scopes = token_result
        .scopes()
        .map(|scopes| {
            scopes
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();

    let token = StoredToken {
        discord_user_id: discord_user_id.to_string(),
        access_token: token_result.access_token().secret().clone(),
        refresh_token,
        expires_at: now + expires_in,
        scopes,
    };
    db::upsert_token(pool, &token).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn well_before_expiry_does_not_need_refresh() {
        assert!(!needs_refresh(1_000_000, 999_000));
    }

    #[test]
    fn inside_buffer_needs_refresh() {
        // 30s left, buffer is 60s.
        assert!(needs_refresh(1_000_000, 999_970));
    }

    #[test]
    fn already_expired_needs_refresh() {
        assert!(needs_refresh(1_000_000, 1_000_100));
    }

    #[test]
    fn exactly_at_buffer_boundary_needs_refresh() {
        // Documented as inclusive: `now == expires_at - buffer` refreshes.
        assert!(needs_refresh(1_000_000, 1_000_000 - EXPIRY_BUFFER_SECS));
    }
}
