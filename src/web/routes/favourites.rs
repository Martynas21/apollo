use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Serialize;

use crate::db;
use crate::web::WebState;
use crate::web::response::{error_response, parse_guild_id};

pub fn routes() -> Router<WebState> {
    Router::new().route("/api/guilds/{guild_id}/favourites", get(favourites))
}

const FAVOURITES_LIMIT: i64 = 5;

#[derive(Serialize)]
struct FavouriteTrackJson {
    title: String,
    channel: String,
    video_id: String,
    duration_secs: Option<u64>,
    play_count: i64,
}

#[derive(Serialize)]
struct FavouritePlaylistJson {
    id: i64,
    name: String,
    play_count: i64,
    thumbnail_video_id: Option<String>,
}

#[derive(Serialize)]
struct FavouritesJson {
    tracks: Vec<FavouriteTrackJson>,
    playlists: Vec<FavouritePlaylistJson>,
}

async fn favourites(State(state): State<WebState>, Path(guild_id): Path<String>) -> Response {
    let Some(guild_id) = parse_guild_id(&guild_id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid guild id");
    };
    let guild_id = guild_id.to_string();

    let tracks = match db::top_played_tracks(&state.db, &guild_id, FAVOURITES_LIMIT).await {
        Ok(tracks) => tracks,
        Err(err) => {
            tracing::warn!(%err, "dashboard failed to load favourite tracks");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to load favourites",
            );
        }
    };

    let playlists = match db::top_played_playlists(&state.db, &guild_id, FAVOURITES_LIMIT).await {
        Ok(playlists) => playlists,
        Err(err) => {
            tracing::warn!(%err, "dashboard failed to load favourite playlists");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to load favourites",
            );
        }
    };

    let tracks = tracks
        .into_iter()
        .map(|t| FavouriteTrackJson {
            title: t.title,
            channel: t.channel,
            video_id: t.video_id,
            duration_secs: t.duration_secs,
            play_count: t.play_count,
        })
        .collect();
    let mut playlists_json = Vec::with_capacity(playlists.len());
    for p in playlists {
        let thumbnail_video_id = match db::get_playlist_thumbnail_video_id(&state.db, p.id).await {
            Ok(thumbnail_video_id) => thumbnail_video_id,
            Err(err) => {
                tracing::warn!(%err, "dashboard failed to load favourite playlist thumbnail");
                None
            }
        };
        playlists_json.push(FavouritePlaylistJson {
            id: p.id,
            name: p.name,
            play_count: p.play_count,
            thumbnail_video_id,
        });
    }

    Json(FavouritesJson {
        tracks,
        playlists: playlists_json,
    })
    .into_response()
}
