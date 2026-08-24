//! Local HTTP server that receives the Google OAuth2 redirect and completes
//! the `/link` flow.
//!
//! Bound to loopback only (see `src/main.rs`) — this endpoint exists purely
//! to catch a redirect from the user's own browser during `/link`, it has
//! no reason to be reachable from outside the host.

use anyhow::{Context, Result};
use axum::Router;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use serde::Deserialize;

use crate::commands::Data;
use crate::youtube::oauth;

#[derive(Debug, Deserialize)]
struct CallbackParams {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// Extracts the local bind port and callback path from the configured
/// OAuth2 redirect URI. Defaults to port 8080 when the URI has no explicit
/// port (matching `.env.example`'s default `http://localhost:8080/...`) —
/// a missing port here isn't a misconfiguration worth failing startup over.
pub fn parse_redirect_uri(redirect_uri: &str) -> Result<(u16, String)> {
    let url = oauth2::url::Url::parse(redirect_uri)
        .with_context(|| format!("invalid GOOGLE_OAUTH_REDIRECT_URI: {redirect_uri}"))?;
    let port = url.port().unwrap_or(8080);
    Ok((port, url.path().to_string()))
}

/// Builds the axum app serving the OAuth2 callback at `callback_path`.
pub fn app(data: Data, callback_path: &str) -> Router {
    Router::new()
        .route(callback_path, get(callback))
        .with_state(data)
}

async fn callback(
    State(data): State<Data>,
    Query(params): Query<CallbackParams>,
) -> impl IntoResponse {
    if let Some(error) = params.error {
        return (
            StatusCode::OK,
            Html(format!(
                "<p>Linking was declined ({error}). Nothing was linked — you can run \
                 <code>/link</code> again in Discord.</p>"
            )),
        )
            .into_response();
    }

    let (Some(code), Some(state_token)) = (params.code, params.state) else {
        return (StatusCode::BAD_REQUEST, "missing code or state parameter").into_response();
    };

    // Single-use: remove on lookup so a replayed or forged callback can't
    // reuse a state value.
    let discord_user_id = {
        let mut pending = data.pending_links.lock().unwrap();
        pending.remove(&state_token)
    };

    let Some(discord_user_id) = discord_user_id else {
        return (
            StatusCode::BAD_REQUEST,
            "unrecognized or already-used link attempt; run /link again",
        )
            .into_response();
    };

    let result = oauth::exchange_code_and_store(
        &data.oauth_client,
        &data.oauth_http,
        &data.db,
        &discord_user_id.to_string(),
        code,
    )
    .await;

    match result {
        Ok(()) => Html("<p>Linked! You can close this tab and return to Discord.</p>".to_string())
            .into_response(),
        Err(err) => {
            tracing::error!(error = %err, "failed to complete OAuth2 callback");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to complete linking; run /link again",
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_explicit_port_and_path() {
        let (port, path) = parse_redirect_uri("http://localhost:8080/oauth/callback").unwrap();
        assert_eq!(port, 8080);
        assert_eq!(path, "/oauth/callback");
    }

    #[test]
    fn defaults_to_port_8080_when_absent() {
        let (port, path) = parse_redirect_uri("http://localhost/oauth/callback").unwrap();
        assert_eq!(port, 8080);
        assert_eq!(path, "/oauth/callback");
    }

    #[test]
    fn rejects_unparseable_uri() {
        assert!(parse_redirect_uri("not a url").is_err());
    }

    #[test]
    fn callback_params_deserialize_from_query_string() {
        let uri: axum::http::Uri = "http://localhost/oauth/callback?code=abc&state=xyz"
            .parse()
            .unwrap();
        let Query(params) =
            Query::<CallbackParams>::try_from_uri(&uri).expect("should deserialize");
        assert_eq!(params.code.as_deref(), Some("abc"));
        assert_eq!(params.state.as_deref(), Some("xyz"));
        assert_eq!(params.error, None);
    }

    #[test]
    fn callback_params_deserialize_error_case() {
        let uri: axum::http::Uri = "http://localhost/oauth/callback?error=access_denied&state=xyz"
            .parse()
            .unwrap();
        let Query(params) =
            Query::<CallbackParams>::try_from_uri(&uri).expect("should deserialize");
        assert_eq!(params.error.as_deref(), Some("access_denied"));
        assert_eq!(params.code, None);
    }
}
