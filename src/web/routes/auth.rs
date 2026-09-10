use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde::{Deserialize, Serialize};

use crate::web::WebState;
use crate::web::auth;
use crate::web::response::error_response;

pub fn routes() -> Router<WebState> {
    Router::new().route("/api/login", post(login))
}

#[derive(Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

#[derive(Serialize)]
struct LoginResponse {
    token: String,
}

/// The throttle check and the concurrency permit both have to happen before
/// `verify_login` runs — that's the only way either one actually bounds the
/// Argon2 work an unauthenticated caller can trigger.
async fn login(State(state): State<WebState>, Json(body): Json<LoginRequest>) -> Response {
    if let Some(retry_after) = state.login_throttle.remaining_lockout(&body.username) {
        return too_many_requests(
            retry_after,
            "too many failed login attempts, try again later",
        );
    }

    let Some(_permit) = state.login_throttle.try_acquire_hash_permit() else {
        return too_many_requests(
            Duration::from_secs(1),
            "the login endpoint is busy, try again shortly",
        );
    };

    match auth::verify_login(&state.db, &body.username, &body.password).await {
        Ok(Some(user)) => {
            state.login_throttle.record_success(&body.username);
            let token = auth::issue_session(&state.sessions, user);
            Json(LoginResponse { token }).into_response()
        }
        Ok(None) => {
            state.login_throttle.record_failure(&body.username);
            error_response(StatusCode::UNAUTHORIZED, "invalid username or password")
        }
        Err(err) => {
            tracing::warn!(%err, "dashboard login failed to check credentials");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "login failed")
        }
    }
}

/// A `429` in the shared error-body shape with a `Retry-After` header set to
/// `retry_after`, rounded up to a whole number of seconds (`0` would tell
/// the client it can retry immediately, defeating the point of sending it).
fn too_many_requests(retry_after: Duration, message: &str) -> Response {
    let mut response = error_response(StatusCode::TOO_MANY_REQUESTS, message);
    let seconds = retry_after.as_secs().max(1).to_string();
    if let Ok(value) = HeaderValue::from_str(&seconds) {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
}
