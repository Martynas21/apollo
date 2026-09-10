use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};

use crate::db;
use crate::model::QueuedTrack;
use crate::web::WebState;
use crate::web::response::{error_response, parse_guild_id};
use crate::web::routes::playback::respond_after;

pub fn routes() -> Router<WebState> {
    Router::new()
        .route("/api/guilds/{guild_id}/playlists", get(list_playlists))
        .route(
            "/api/guilds/{guild_id}/playlists/import",
            post(import_playlist),
        )
        .route(
            "/api/guilds/{guild_id}/playlists/{playlist_id}/play",
            post(play_playlist),
        )
        .route(
            "/api/guilds/{guild_id}/playlists/{playlist_id}/refresh",
            post(refresh_playlist),
        )
        .route(
            "/api/guilds/{guild_id}/playlists/{playlist_id}/remove",
            post(remove_playlist),
        )
}

#[derive(Serialize)]
struct PlaylistJson {
    id: i64,
    name: String,
    track_count: usize,
    thumbnail_video_id: Option<String>,
}

async fn list_playlists(State(state): State<WebState>, Path(guild_id): Path<String>) -> Response {
    let Some(guild_id) = parse_guild_id(&guild_id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid guild id");
    };

    let playlists = match db::list_guild_playlists(&state.db, &guild_id.to_string()).await {
        Ok(playlists) => playlists,
        Err(err) => {
            tracing::warn!(%err, "dashboard failed to list guild playlists");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to load playlists",
            );
        }
    };

    let mut playlists_json = Vec::with_capacity(playlists.len());
    for playlist in playlists {
        let tracks = match db::get_playlist_tracks(&state.db, playlist.id).await {
            Ok(tracks) => tracks,
            Err(err) => {
                tracing::warn!(%err, "dashboard failed to load playlist track count");
                return error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "failed to load playlists",
                );
            }
        };
        let thumbnail_video_id = tracks.first().map(|track| track.video_id.clone());
        playlists_json.push(PlaylistJson {
            id: playlist.id,
            name: playlist.name,
            track_count: tracks.len(),
            thumbnail_video_id,
        });
    }

    Json(playlists_json).into_response()
}

#[derive(Deserialize)]
struct ImportPlaylistRequest {
    url: String,
}

async fn import_playlist(
    State(state): State<WebState>,
    Path(guild_id): Path<String>,
    Json(body): Json<ImportPlaylistRequest>,
) -> Response {
    let Some(guild_id) = parse_guild_id(&guild_id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid guild id");
    };

    let listing = match state.youtube.list_playlist_items(&body.url).await {
        Ok(listing) => listing,
        Err(err) => return error_response(StatusCode::BAD_REQUEST, err.to_string()),
    };
    if listing.tracks.is_empty() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "that playlist is empty or could not be found",
        );
    }

    let name = listing.title.clone().unwrap_or_else(|| body.url.clone());
    let added_by = state.cache.current_user().id.to_string();
    let id = match db::save_guild_playlist(
        &state.db,
        &guild_id.to_string(),
        &name,
        &body.url,
        &added_by,
    )
    .await
    {
        Ok(id) => id,
        Err(err) => {
            tracing::warn!(%err, "dashboard failed to save imported playlist");
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "failed to save playlist");
        }
    };

    if let Err(err) = db::replace_playlist_tracks(&state.db, id, &listing.tracks).await {
        tracing::warn!(%err, playlist_id = id, "failed to cache playlist tracks after import");
    }

    let thumbnail_video_id = listing.tracks.first().map(|track| track.video_id.clone());
    Json(PlaylistJson {
        id,
        name,
        track_count: listing.tracks.len(),
        thumbnail_video_id,
    })
    .into_response()
}

async fn refresh_playlist(
    State(state): State<WebState>,
    Path((guild_id, playlist_id)): Path<(String, i64)>,
) -> Response {
    let Some(guild_id) = parse_guild_id(&guild_id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid guild id");
    };

    let playlist = match db::get_guild_playlist(&state.db, &guild_id.to_string(), playlist_id).await
    {
        Ok(Some(playlist)) => playlist,
        Ok(None) => return error_response(StatusCode::NOT_FOUND, "playlist not found"),
        Err(err) => {
            tracing::warn!(%err, "dashboard failed to load playlist for refresh");
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "failed to load playlist");
        }
    };

    let listing = match state.youtube.list_playlist_items(&playlist.url).await {
        Ok(listing) => listing,
        Err(err) => return error_response(StatusCode::BAD_REQUEST, err.to_string()),
    };

    if let Err(err) = db::replace_playlist_tracks(&state.db, playlist.id, &listing.tracks).await {
        tracing::warn!(%err, playlist_id, "dashboard failed to cache refreshed playlist tracks");
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to refresh playlist",
        );
    }

    let thumbnail_video_id = listing.tracks.first().map(|track| track.video_id.clone());
    Json(PlaylistJson {
        id: playlist.id,
        name: playlist.name,
        track_count: listing.tracks.len(),
        thumbnail_video_id,
    })
    .into_response()
}

async fn remove_playlist(
    State(state): State<WebState>,
    Path((guild_id, playlist_id)): Path<(String, i64)>,
) -> Response {
    let Some(guild_id) = parse_guild_id(&guild_id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid guild id");
    };

    match db::delete_guild_playlist(&state.db, &guild_id.to_string(), playlist_id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => error_response(StatusCode::NOT_FOUND, "playlist not found"),
        Err(err) => {
            tracing::warn!(%err, "dashboard failed to remove playlist");
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to remove playlist",
            )
        }
    }
}

async fn play_playlist(
    State(state): State<WebState>,
    Path((guild_id, playlist_id)): Path<(String, i64)>,
) -> Response {
    let Some(guild_id) = parse_guild_id(&guild_id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid guild id");
    };

    let tracks = match db::get_playlist_tracks(&state.db, playlist_id).await {
        Ok(tracks) => tracks,
        Err(err) => {
            tracing::warn!(%err, "dashboard failed to load playlist tracks");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to load playlists",
            );
        }
    };

    if tracks.is_empty() {
        return error_response(
            StatusCode::NOT_FOUND,
            "playlist is empty or has not been cached yet — refresh it from Discord's /playlists first",
        );
    }

    let requested_by = state.cache.current_user().id;
    let queued: Vec<QueuedTrack> = tracks
        .into_iter()
        .map(|track| QueuedTrack {
            track,
            requested_by,
        })
        .collect();
    let result = state.player.enqueue_many(guild_id, queued).await;
    if result.is_ok()
        && let Err(err) =
            db::increment_playlist_play_count(&state.db, &guild_id.to_string(), playlist_id).await
    {
        tracing::warn!(%err, playlist_id, "dashboard failed to record playlist play count");
    }
    respond_after(&state.player, guild_id, result).await
}
