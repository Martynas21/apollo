use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
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

async fn login(State(state): State<WebState>, Json(body): Json<LoginRequest>) -> Response {
    match auth::verify_login(&state.db, &body.username, &body.password).await {
        Ok(Some(user)) => {
            let token = auth::issue_session(&state.sessions, user);
            Json(LoginResponse { token }).into_response()
        }
        Ok(None) => error_response(StatusCode::UNAUTHORIZED, "invalid username or password"),
        Err(err) => {
            tracing::warn!(%err, "dashboard login failed to check credentials");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "login failed")
        }
    }
}
