//! Integration tests for the dashboard's axum `Router`, driven end-to-end
//! through `tower::ServiceExt::oneshot` — no network, no Discord gateway, no
//! live yt-dlp. Exercises session auth (401 unauthenticated, login
//! success/failure, admin-vs-non-admin authorization) and the shared error
//! response shape, using a `VoiceBackend` fake since the real one talks to a
//! separate `apollo-audio-worker` process over TCP.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use std::sync::Arc;

use apollo::model::Track;
use apollo::voice::PlayerRegistry;
use apollo::voice::backend::{AudioSource, VoiceBackend, VoiceCall, VoiceEvents};
use apollo::voice::resolve::PlaybackError;
use apollo::web::WebState;
use apollo::youtube::api::YouTubeClient;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use serenity::all::{Cache, ChannelId, GuildId};
use tower::ServiceExt;

/// Stands in for the real IPC-backed voice connection. None of these tests
/// exercise actual playback, so every method either no-ops or reports "not
/// connected" — good enough for routes that only need a `PlayerRegistry` to
/// exist.
struct NullVoiceBackend;

#[async_trait::async_trait]
impl VoiceBackend for NullVoiceBackend {
    async fn join(
        &self,
        _guild_id: GuildId,
        _channel_id: ChannelId,
        _events: Arc<dyn VoiceEvents>,
    ) -> Result<(), String> {
        Ok(())
    }

    async fn remove(&self, _guild_id: GuildId) -> Result<(), String> {
        Ok(())
    }

    fn call(&self, _guild_id: GuildId) -> Option<Arc<dyn VoiceCall>> {
        None
    }

    async fn current_channel(&self, _guild_id: GuildId) -> Option<ChannelId> {
        None
    }

    async fn buffered_source(&self, _track: &Track) -> Result<AudioSource, PlaybackError> {
        Err(PlaybackError::Other(
            "voice backend unavailable in tests".to_string(),
        ))
    }
}

const ADMIN_USERNAME: &str = "admin";
const ADMIN_PASSWORD: &str = "hunter2-hunter2";

async fn test_app() -> Router {
    let db = apollo::db::connect("sqlite::memory:")
        .await
        .expect("in-memory db should connect");
    apollo::web::bootstrap_user_if_needed(&db, Some(ADMIN_USERNAME), Some(ADMIN_PASSWORD))
        .await
        .expect("bootstrapping the initial admin should succeed");

    let youtube = YouTubeClient::default();
    let player = PlayerRegistry::new(
        Arc::new(NullVoiceBackend),
        None,
        db.clone(),
        youtube.clone(),
    );
    let cache = Arc::new(Cache::new());
    let state = WebState::new(player, youtube, db, cache);
    apollo::web::router(state)
}

async fn request(
    app: &Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> axum::response::Response {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let body = match body {
        Some(value) => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };
    app.clone()
        .oneshot(builder.body(body).expect("request should build"))
        .await
        .expect("router should not fail to produce a response")
}

async fn json_body(response: axum::response::Response) -> Value {
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("response body should be readable")
        .to_bytes();
    serde_json::from_slice(&bytes).expect("response body should be valid json")
}

async fn login(app: &Router, username: &str, password: &str) -> Option<String> {
    let response = request(
        app,
        "POST",
        "/api/login",
        None,
        Some(json!({ "username": username, "password": password })),
    )
    .await;
    if response.status() != StatusCode::OK {
        return None;
    }
    let json = json_body(response).await;
    json.get("token")
        .and_then(Value::as_str)
        .map(str::to_string)
}

#[tokio::test]
async fn unauthenticated_request_to_a_protected_route_is_rejected() {
    let app = test_app().await;

    let response = request(&app, "GET", "/api/guilds", None, None).await;

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn login_with_bad_credentials_fails_with_the_shared_error_shape() {
    let app = test_app().await;

    let response = request(
        &app,
        "POST",
        "/api/login",
        None,
        Some(json!({ "username": ADMIN_USERNAME, "password": "not-the-password" })),
    )
    .await;

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body = json_body(response).await;
    assert_eq!(body["error"], "invalid username or password");
}

#[tokio::test]
async fn login_with_good_credentials_returns_a_session_token() {
    let app = test_app().await;

    let token = login(&app, ADMIN_USERNAME, ADMIN_PASSWORD).await;

    assert!(token.is_some_and(|token| !token.is_empty()));
}

#[tokio::test]
async fn a_valid_session_token_reaches_a_protected_route() {
    let app = test_app().await;
    let token = login(&app, ADMIN_USERNAME, ADMIN_PASSWORD)
        .await
        .expect("admin login should succeed");

    let response = request(&app, "GET", "/api/guilds", Some(&token), None).await;

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn a_non_admin_session_is_forbidden_from_admin_routes_but_can_read_its_own_identity() {
    let app = test_app().await;
    let admin_token = login(&app, ADMIN_USERNAME, ADMIN_PASSWORD)
        .await
        .expect("admin login should succeed");

    let create_response = request(
        &app,
        "POST",
        "/api/users",
        Some(&admin_token),
        Some(json!({ "username": "listener", "password": "not-an-admin-pw" })),
    )
    .await;
    assert_eq!(create_response.status(), StatusCode::OK);

    let listener_token = login(&app, "listener", "not-an-admin-pw")
        .await
        .expect("the newly created non-admin user should be able to log in");

    let admin_route_response =
        request(&app, "GET", "/api/users", Some(&listener_token), None).await;
    assert_eq!(admin_route_response.status(), StatusCode::FORBIDDEN);

    let me_response = request(&app, "GET", "/api/me", Some(&listener_token), None).await;
    assert_eq!(me_response.status(), StatusCode::OK);
    let me_body = json_body(me_response).await;
    assert_eq!(me_body["username"], "listener");
    assert_eq!(me_body["is_admin"], false);
}

#[tokio::test]
async fn a_malformed_guild_id_is_rejected_with_bad_request_and_the_shared_error_shape() {
    let app = test_app().await;
    let token = login(&app, ADMIN_USERNAME, ADMIN_PASSWORD)
        .await
        .expect("admin login should succeed");

    let response = request(
        &app,
        "GET",
        "/api/guilds/not-a-snowflake/now-playing",
        Some(&token),
        None,
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = json_body(response).await;
    assert_eq!(body["error"], "invalid guild id");
}
