use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Deserialize;

use crate::web::WebState;
use crate::web::response::{TrackJson, error_response, parse_guild_id, track_json_from_track};
use crate::youtube::api::extract_video_id;

pub fn routes() -> Router<WebState> {
    Router::new().route("/api/guilds/{guild_id}/search", get(search))
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
}

async fn search(
    State(state): State<WebState>,
    Path(guild_id): Path<String>,
    Query(params): Query<SearchQuery>,
) -> Response {
    let Some(_guild_id) = parse_guild_id(&guild_id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid guild id");
    };

    if let Some(video_id) = extract_video_id(&params.q) {
        return match state.youtube.get_video(&video_id).await {
            Ok(track) => Json(vec![track_json_from_track(&track)]).into_response(),
            Err(err) => error_response(StatusCode::BAD_REQUEST, err.to_string()),
        };
    }

    match state.youtube.search(&params.q).await {
        Ok(tracks) => {
            let tracks: Vec<TrackJson> = tracks.iter().map(track_json_from_track).collect();
            Json(tracks).into_response()
        }
        Err(err) => error_response(StatusCode::BAD_REQUEST, err.to_string()),
    }
}
