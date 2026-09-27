//! Integration tests for the dashboard's axum `Router`, driven end-to-end
//! through `tower::ServiceExt::oneshot` — no network, no Discord gateway, no
//! live yt-dlp. Exercises session auth (401 unauthenticated, login
//! success/failure, admin-vs-non-admin authorization) and the shared error
//! response shape, using a `VoiceBackend` fake since the real one talks to a
//! separate `apollo-audio-worker` process over TCP.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use std::sync::Arc;

use apollo::model::{QueuedTrack, Track};
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
use serenity::all::{Cache, ChannelId, GuildId, UserId};
use sqlx::SqlitePool;
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
    async fn preflight(&self, _video_id: &str) -> Result<(), PlaybackError> {
        Ok(())
    }
}

const ADMIN_USERNAME: &str = "admin";
const ADMIN_PASSWORD: &str = "hunter2-hunter2";

async fn test_app() -> Router {
    test_app_with_db().await.0
}

/// The router plus the pool it runs on, for tests that seed rows directly.
async fn test_app_with_db() -> (Router, SqlitePool) {
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
    let state = WebState::new(player, youtube, db.clone(), cache);
    (apollo::web::router(state), db)
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
async fn repeated_bad_logins_for_the_same_username_eventually_return_too_many_requests() {
    let app = test_app().await;

    let mut saw_too_many_requests = false;
    for _ in 0..20 {
        let response = request(
            &app,
            "POST",
            "/api/login",
            None,
            Some(json!({ "username": ADMIN_USERNAME, "password": "not-the-password" })),
        )
        .await;

        if response.status() == StatusCode::TOO_MANY_REQUESTS {
            assert!(response.headers().contains_key(header::RETRY_AFTER));
            saw_too_many_requests = true;
            break;
        }
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    assert!(
        saw_too_many_requests,
        "expected repeated bad logins for one username to eventually be throttled"
    );
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

#[tokio::test]
async fn seek_with_nothing_playing_is_a_conflict_with_the_shared_error_shape() {
    let app = test_app().await;
    let token = login(&app, ADMIN_USERNAME, ADMIN_PASSWORD)
        .await
        .expect("admin login should succeed");

    let response = request(
        &app,
        "POST",
        "/api/guilds/111/seek",
        Some(&token),
        Some(json!({ "position_ms": 1000 })),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(json_body(response).await["error"], "nothing is playing");
}

#[tokio::test]
async fn a_guild_pinned_user_is_confined_to_that_guild_on_every_guild_scoped_route() {
    let app = test_app().await;
    let admin_token = login(&app, ADMIN_USERNAME, ADMIN_PASSWORD)
        .await
        .expect("admin login should succeed");

    let create_response = request(
        &app,
        "POST",
        "/api/users",
        Some(&admin_token),
        Some(json!({
            "username": "pinned",
            "password": "pinned-pw",
            "guild_id": "111",
        })),
    )
    .await;
    assert_eq!(create_response.status(), StatusCode::OK);

    let pinned_token = login(&app, "pinned", "pinned-pw")
        .await
        .expect("the pinned user should be able to log in");

    let own_guild = request(
        &app,
        "GET",
        "/api/guilds/111/now-playing",
        Some(&pinned_token),
        None,
    )
    .await;
    assert_eq!(own_guild.status(), StatusCode::OK);

    let other_guild = request(
        &app,
        "GET",
        "/api/guilds/222/now-playing",
        Some(&pinned_token),
        None,
    )
    .await;
    assert_eq!(other_guild.status(), StatusCode::FORBIDDEN);

    // A write route is refused by the same middleware, not just the reads.
    for path in [
        "/api/guilds/222/skip",
        "/api/guilds/222/failed/abc/dismiss",
        "/api/guilds/222/overrides/abc/remove",
    ] {
        let other_guild_write = request(&app, "POST", path, Some(&pinned_token), None).await;
        assert_eq!(other_guild_write.status(), StatusCode::FORBIDDEN, "{path}");
    }

    // Routes that address no guild stay reachable.
    let me_response = request(&app, "GET", "/api/me", Some(&pinned_token), None).await;
    assert_eq!(me_response.status(), StatusCode::OK);
    assert_eq!(json_body(me_response).await["guild_id"], "111");
}

#[tokio::test]
async fn an_admin_reaches_every_guild_even_with_one_pinned_to_their_account() {
    let app = test_app().await;
    let admin_token = login(&app, ADMIN_USERNAME, ADMIN_PASSWORD)
        .await
        .expect("admin login should succeed");

    let pin_response = request(
        &app,
        "POST",
        &format!("/api/users/{ADMIN_USERNAME}/guild"),
        Some(&admin_token),
        Some(json!({ "guild_id": "111" })),
    )
    .await;
    assert_eq!(pin_response.status(), StatusCode::NO_CONTENT);

    // Rescoping an account drops its sessions, so this is a fresh login.
    let admin_token = login(&app, ADMIN_USERNAME, ADMIN_PASSWORD)
        .await
        .expect("admin login should still succeed after being rescoped");

    let other_guild = request(
        &app,
        "GET",
        "/api/guilds/222/now-playing",
        Some(&admin_token),
        None,
    )
    .await;
    assert_eq!(other_guild.status(), StatusCode::OK);
}

#[tokio::test]
async fn rescoping_a_user_revokes_the_session_that_still_carries_the_old_guild() {
    let app = test_app().await;
    let admin_token = login(&app, ADMIN_USERNAME, ADMIN_PASSWORD)
        .await
        .expect("admin login should succeed");

    let create_response = request(
        &app,
        "POST",
        "/api/users",
        Some(&admin_token),
        Some(json!({ "username": "rescoped", "password": "rescoped-pw" })),
    )
    .await;
    assert_eq!(create_response.status(), StatusCode::OK);

    let stale_token = login(&app, "rescoped", "rescoped-pw")
        .await
        .expect("the new user should be able to log in");
    let before = request(
        &app,
        "GET",
        "/api/guilds/222/now-playing",
        Some(&stale_token),
        None,
    )
    .await;
    assert_eq!(before.status(), StatusCode::OK);

    let pin_response = request(
        &app,
        "POST",
        "/api/users/rescoped/guild",
        Some(&admin_token),
        Some(json!({ "guild_id": "111" })),
    )
    .await;
    assert_eq!(pin_response.status(), StatusCode::NO_CONTENT);

    let after = request(
        &app,
        "GET",
        "/api/guilds/222/now-playing",
        Some(&stale_token),
        None,
    )
    .await;
    assert_eq!(after.status(), StatusCode::UNAUTHORIZED);
}

fn failed_sample(video_id: &str) -> QueuedTrack {
    QueuedTrack {
        track: Track {
            video_id: video_id.to_string(),
            title: format!("Title {video_id}"),
            channel: "Channel".to_string(),
            duration: Some(std::time::Duration::from_secs(90)),
        },
        requested_by: UserId::new(7),
    }
}

#[tokio::test]
async fn skipping_a_failed_track_saves_a_skip_override() {
    let (app, db) = test_app_with_db().await;
    let token = login(&app, ADMIN_USERNAME, ADMIN_PASSWORD)
        .await
        .expect("admin login should succeed");
    apollo::db::record_failed_track(&db, "111", &failed_sample("abc"), "video is unavailable")
        .await
        .expect("seeding a failed track should succeed");

    let skipped = request(
        &app,
        "POST",
        "/api/guilds/111/failed/abc/dismiss",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(skipped.status(), StatusCode::OK);

    let listed = request(&app, "GET", "/api/guilds/111/overrides", Some(&token), None).await;
    let body = json_body(listed).await;
    assert_eq!(body[0]["original"]["video_id"], "abc");
    assert_eq!(body[0]["original"]["title"], "Title abc");
    assert_eq!(body[0]["action"], "skip");
    assert_eq!(body[0]["replacement"], Value::Null);
    assert!(body[0]["created_at"].as_i64().unwrap_or(0) > 0);
}

#[tokio::test]
async fn a_failed_track_shows_in_the_snapshot_until_it_is_dismissed() {
    let (app, db) = test_app_with_db().await;
    let token = login(&app, ADMIN_USERNAME, ADMIN_PASSWORD)
        .await
        .expect("admin login should succeed");
    apollo::db::record_failed_track(&db, "111", &failed_sample("abc"), "video is unavailable")
        .await
        .expect("seeding a failed track should succeed");

    let before = request(
        &app,
        "GET",
        "/api/guilds/111/now-playing",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(before.status(), StatusCode::OK);
    let before_body = json_body(before).await;
    assert_eq!(before_body["failed"][0]["video_id"], "abc");
    assert_eq!(before_body["failed"][0]["title"], "Title abc");
    assert_eq!(before_body["failed"][0]["error"], "video is unavailable");

    let dismissed = request(
        &app,
        "POST",
        "/api/guilds/111/failed/abc/dismiss",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(dismissed.status(), StatusCode::OK);
    assert_eq!(json_body(dismissed).await["failed"], json!([]));
    assert!(
        apollo::db::failed_tracks(&db, "111")
            .await
            .expect("listing failed tracks should succeed")
            .is_empty()
    );
}

#[tokio::test]
async fn dismissing_a_track_that_is_not_listed_is_a_no_op() {
    let app = test_app().await;
    let token = login(&app, ADMIN_USERNAME, ADMIN_PASSWORD)
        .await
        .expect("admin login should succeed");

    let response = request(
        &app,
        "POST",
        "/api/guilds/111/failed/missing/dismiss",
        Some(&token),
        None,
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn failed_track_routes_reject_a_malformed_guild_id() {
    let app = test_app().await;
    let token = login(&app, ADMIN_USERNAME, ADMIN_PASSWORD)
        .await
        .expect("admin login should succeed");

    let dismiss = request(
        &app,
        "POST",
        "/api/guilds/not-a-guild/failed/abc/dismiss",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(dismiss.status(), StatusCode::BAD_REQUEST);
    assert_eq!(json_body(dismiss).await["error"], "invalid guild id");

    let replace = request(
        &app,
        "POST",
        "/api/guilds/not-a-guild/failed/abc/replace",
        Some(&token),
        Some(json!({ "video_id": "xyz" })),
    )
    .await;
    assert_eq!(replace.status(), StatusCode::BAD_REQUEST);
    assert_eq!(json_body(replace).await["error"], "invalid guild id");
}

#[tokio::test]
async fn a_failed_track_cannot_be_replaced_with_itself() {
    let (app, db) = test_app_with_db().await;
    let token = login(&app, ADMIN_USERNAME, ADMIN_PASSWORD)
        .await
        .expect("admin login should succeed");

    let response = request(
        &app,
        "POST",
        "/api/guilds/111/failed/abc/replace",
        Some(&token),
        Some(json!({ "video_id": "abc" })),
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        apollo::db::track_overrides(&db, "111")
            .await
            .expect("listing overrides should succeed")
            .is_empty()
    );
}

#[tokio::test]
async fn overrides_are_listed_by_name_and_can_be_removed() {
    let (app, db) = test_app_with_db().await;
    let token = login(&app, ADMIN_USERNAME, ADMIN_PASSWORD)
        .await
        .expect("admin login should succeed");
    apollo::db::save_track_override(
        &db,
        "111",
        &failed_sample("abc").track,
        &failed_sample("xyz").track,
    )
    .await
    .expect("seeding an override should succeed");

    let listed = request(&app, "GET", "/api/guilds/111/overrides", Some(&token), None).await;
    assert_eq!(listed.status(), StatusCode::OK);
    let listed_body = json_body(listed).await;
    assert_eq!(listed_body[0]["original"]["video_id"], "abc");
    assert_eq!(listed_body[0]["original"]["title"], "Title abc");
    assert_eq!(listed_body[0]["action"], "replace");
    assert_eq!(listed_body[0]["replacement"]["video_id"], "xyz");

    let removed = request(
        &app,
        "POST",
        "/api/guilds/111/overrides/abc/remove",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(removed.status(), StatusCode::OK);
    assert_eq!(json_body(removed).await, json!([]));

    let removed_again = request(
        &app,
        "POST",
        "/api/guilds/111/overrides/abc/remove",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(removed_again.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn changing_an_override_that_does_not_exist_is_not_found() {
    let app = test_app().await;
    let token = login(&app, ADMIN_USERNAME, ADMIN_PASSWORD)
        .await
        .expect("admin login should succeed");

    let response = request(
        &app,
        "POST",
        "/api/guilds/111/overrides/abc/replace",
        Some(&token),
        Some(json!({ "video_id": "xyz" })),
    )
    .await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(json_body(response).await["error"], "override not found");
}

#[tokio::test]
async fn an_override_cannot_point_a_track_at_itself() {
    let app = test_app().await;
    let token = login(&app, ADMIN_USERNAME, ADMIN_PASSWORD)
        .await
        .expect("admin login should succeed");

    let response = request(
        &app,
        "POST",
        "/api/guilds/111/overrides/abc/replace",
        Some(&token),
        Some(json!({ "video_id": "abc" })),
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}
