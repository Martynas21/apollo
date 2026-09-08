use std::time::Duration;

use axum::Json;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use poise::serenity_prelude as serenity;
use serde::{Deserialize, Serialize};
use serenity::{ChannelId, ChannelType, GuildId};

use crate::commands::playback::extract_video_id;
use crate::db;
use crate::voice::QueuedTrack;
use crate::voice::player::{PlayerError, PlayerRegistry};
use crate::web::WebState;
use crate::web::auth;
use crate::youtube::api::Track;

const SNAPSHOT_PUSH_INTERVAL: Duration = Duration::from_millis(1500);

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

fn error_response(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(ErrorBody {
            error: message.into(),
        }),
    )
        .into_response()
}

fn parse_guild_id(raw: &str) -> Option<GuildId> {
    raw.parse::<u64>().ok().map(GuildId::new)
}

fn parse_channel_id(raw: &str) -> Option<ChannelId> {
    raw.parse::<u64>().ok().map(ChannelId::new)
}

fn player_error_status(err: &PlayerError) -> StatusCode {
    match err {
        PlayerError::NothingPlaying | PlayerError::NothingToShuffle | PlayerError::QueueEmpty => {
            StatusCode::CONFLICT
        }
        PlayerError::NotConnected | PlayerError::Join(_) | PlayerError::InvalidQueueIndex => {
            StatusCode::BAD_REQUEST
        }
        PlayerError::Playback(_) | PlayerError::Storage(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

#[derive(Serialize)]
struct GuildJson {
    id: String,
    name: String,
}

#[derive(Serialize)]
struct TrackJson {
    title: String,
    channel: String,
    video_id: String,
    duration_secs: Option<u64>,
}

#[derive(Serialize)]
struct SnapshotJson {
    state: &'static str,
    track: Option<TrackJson>,
    volume: u8,
    radio_enabled: bool,
    upcoming: Vec<TrackJson>,
}

fn track_json(queued: &crate::voice::QueuedTrack) -> TrackJson {
    TrackJson {
        title: queued.track.title.clone(),
        channel: queued.track.channel.clone(),
        video_id: queued.track.video_id.clone(),
        duration_secs: queued.track.duration.map(|d| d.as_secs()),
    }
}

fn track_json_from_track(track: &Track) -> TrackJson {
    TrackJson {
        title: track.title.clone(),
        channel: track.channel.clone(),
        video_id: track.video_id.clone(),
        duration_secs: track.duration.map(|d| d.as_secs()),
    }
}

async fn build_snapshot(player: &PlayerRegistry, guild_id: GuildId) -> SnapshotJson {
    let snapshot = player.queue_snapshot(guild_id).await;
    let paused = player.is_paused(guild_id).await;
    let volume = player.get_volume(guild_id).await;
    let radio_enabled = player.is_radio_enabled(guild_id).await;

    let (state, track) = if let Some(queued) = &snapshot.now_playing {
        let state = if paused == Some(true) {
            "paused"
        } else {
            "playing"
        };
        (state, Some(track_json(queued)))
    } else if let Some(queued) = &snapshot.loading {
        ("buffering", Some(track_json(queued)))
    } else if let Some(queued) = &snapshot.last_played {
        ("queue_finished", Some(track_json(queued)))
    } else {
        ("empty", None)
    };

    let upcoming: Vec<TrackJson> = snapshot.upcoming.iter().map(track_json).collect();

    SnapshotJson {
        state,
        track,
        volume,
        radio_enabled,
        upcoming,
    }
}

async fn snapshot_response(player: &PlayerRegistry, guild_id: GuildId) -> Response {
    Json(build_snapshot(player, guild_id).await).into_response()
}

fn player_error_response(err: PlayerError) -> Response {
    error_response(player_error_status(&err), err.to_string())
}

async fn respond_after(
    player: &PlayerRegistry,
    guild_id: GuildId,
    result: Result<(), PlayerError>,
) -> Response {
    match result {
        Ok(()) => snapshot_response(player, guild_id).await,
        Err(err) => player_error_response(err),
    }
}

#[derive(Deserialize)]
pub struct LoginRequest {
    username: String,
    password: String,
}

#[derive(Serialize)]
pub struct LoginResponse {
    token: String,
}

pub async fn login(State(state): State<WebState>, Json(body): Json<LoginRequest>) -> Response {
    match auth::verify_login(&state.db, &body.username, &body.password).await {
        Ok(true) => {
            let token = auth::issue_session(&state.sessions);
            Json(LoginResponse { token }).into_response()
        }
        Ok(false) => error_response(StatusCode::UNAUTHORIZED, "invalid username or password"),
        Err(err) => {
            tracing::warn!(%err, "dashboard login failed to check credentials");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "login failed")
        }
    }
}

pub async fn list_guilds(State(state): State<WebState>) -> Response {
    let guilds: Vec<GuildJson> = state
        .cache
        .guilds()
        .into_iter()
        .filter_map(|id| {
            state.cache.guild(id).map(|guild| GuildJson {
                id: id.to_string(),
                name: guild.name.clone(),
            })
        })
        .collect();
    Json(guilds).into_response()
}

#[derive(Serialize)]
struct VoiceChannelJson {
    id: String,
    name: String,
}

pub async fn list_voice_channels(
    State(state): State<WebState>,
    Path(guild_id): Path<String>,
) -> Response {
    let Some(guild_id) = parse_guild_id(&guild_id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid guild id");
    };
    let Some(guild) = state.cache.guild(guild_id) else {
        return error_response(StatusCode::NOT_FOUND, "guild not found");
    };

    let mut channels: Vec<(u16, VoiceChannelJson)> = guild
        .channels
        .values()
        .filter(|channel| channel.kind == ChannelType::Voice)
        .map(|channel| {
            (
                channel.position,
                VoiceChannelJson {
                    id: channel.id.to_string(),
                    name: channel.name.clone(),
                },
            )
        })
        .collect();
    channels.sort_by_key(|(position, _)| *position);

    let channels: Vec<VoiceChannelJson> =
        channels.into_iter().map(|(_, channel)| channel).collect();
    Json(channels).into_response()
}

#[derive(Deserialize)]
pub struct JoinVoiceChannelRequest {
    channel_id: String,
}

pub async fn join_voice_channel(
    State(state): State<WebState>,
    Path(guild_id): Path<String>,
    Json(body): Json<JoinVoiceChannelRequest>,
) -> Response {
    let Some(guild_id) = parse_guild_id(&guild_id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid guild id");
    };
    let Some(channel_id) = parse_channel_id(&body.channel_id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid channel id");
    };
    let result = state.player.join(guild_id, channel_id).await;
    respond_after(&state.player, guild_id, result).await
}

pub async fn now_playing(State(state): State<WebState>, Path(guild_id): Path<String>) -> Response {
    let Some(guild_id) = parse_guild_id(&guild_id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid guild id");
    };
    snapshot_response(&state.player, guild_id).await
}

pub async fn now_playing_ws(
    State(state): State<WebState>,
    Path(guild_id): Path<String>,
    ws: WebSocketUpgrade,
) -> Response {
    let Some(guild_id) = parse_guild_id(&guild_id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid guild id");
    };
    ws.on_upgrade(move |socket| stream_now_playing(socket, state, guild_id))
}

async fn stream_now_playing(mut socket: WebSocket, state: WebState, guild_id: GuildId) {
    let mut interval = tokio::time::interval(SNAPSHOT_PUSH_INTERVAL);
    loop {
        tokio::select! {
            _ = interval.tick() => {
                let snapshot = build_snapshot(&state.player, guild_id).await;
                let Ok(text) = serde_json::to_string(&snapshot) else {
                    continue;
                };
                if socket.send(Message::Text(text.into())).await.is_err() {
                    return;
                }
            }
            message = socket.recv() => {
                match message {
                    Some(Ok(Message::Close(_))) | None => return,
                    Some(Err(_)) => return,
                    _ => {}
                }
            }
        }
    }
}

pub async fn toggle_pause(State(state): State<WebState>, Path(guild_id): Path<String>) -> Response {
    let Some(guild_id) = parse_guild_id(&guild_id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid guild id");
    };
    let result = match state.player.is_paused(guild_id).await {
        Some(true) => state.player.resume(guild_id).await,
        Some(false) => state.player.pause(guild_id).await,
        None => Err(PlayerError::NothingPlaying),
    };
    respond_after(&state.player, guild_id, result).await
}

pub async fn skip(State(state): State<WebState>, Path(guild_id): Path<String>) -> Response {
    let Some(guild_id) = parse_guild_id(&guild_id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid guild id");
    };
    let result = state.player.skip(guild_id).await;
    respond_after(&state.player, guild_id, result).await
}

pub async fn stop(State(state): State<WebState>, Path(guild_id): Path<String>) -> Response {
    let Some(guild_id) = parse_guild_id(&guild_id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid guild id");
    };
    let result = state.player.stop(guild_id).await;
    respond_after(&state.player, guild_id, result).await
}

pub async fn shuffle(State(state): State<WebState>, Path(guild_id): Path<String>) -> Response {
    let Some(guild_id) = parse_guild_id(&guild_id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid guild id");
    };
    let result = state.player.shuffle(guild_id).await;
    respond_after(&state.player, guild_id, result).await
}

pub async fn toggle_radio(State(state): State<WebState>, Path(guild_id): Path<String>) -> Response {
    let Some(guild_id) = parse_guild_id(&guild_id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid guild id");
    };
    state.player.toggle_radio(guild_id).await;
    snapshot_response(&state.player, guild_id).await
}

#[derive(Deserialize)]
pub struct VolumeRequest {
    level: u8,
}

pub async fn set_volume(
    State(state): State<WebState>,
    Path(guild_id): Path<String>,
    Json(body): Json<VolumeRequest>,
) -> Response {
    let Some(guild_id) = parse_guild_id(&guild_id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid guild id");
    };
    let result = state.player.set_volume(guild_id, body.level).await;
    respond_after(&state.player, guild_id, result).await
}

pub async fn remove_queue_track(
    State(state): State<WebState>,
    Path((guild_id, index)): Path<(String, usize)>,
) -> Response {
    let Some(guild_id) = parse_guild_id(&guild_id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid guild id");
    };
    let result = state.player.remove_queue_track(guild_id, index).await;
    respond_after(&state.player, guild_id, result).await
}

#[derive(Deserialize)]
pub struct MoveQueueTrackRequest {
    to: usize,
}

pub async fn move_queue_track(
    State(state): State<WebState>,
    Path((guild_id, index)): Path<(String, usize)>,
    Json(body): Json<MoveQueueTrackRequest>,
) -> Response {
    let Some(guild_id) = parse_guild_id(&guild_id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid guild id");
    };
    let result = state
        .player
        .move_queue_track(guild_id, index, body.to)
        .await;
    respond_after(&state.player, guild_id, result).await
}

pub async fn clear_queue(State(state): State<WebState>, Path(guild_id): Path<String>) -> Response {
    let Some(guild_id) = parse_guild_id(&guild_id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid guild id");
    };
    let result = state.player.clear_queue(guild_id).await;
    respond_after(&state.player, guild_id, result).await
}

#[derive(Deserialize)]
pub struct SearchQuery {
    q: String,
}

pub async fn search(
    State(state): State<WebState>,
    Path(guild_id): Path<String>,
    axum::extract::Query(params): axum::extract::Query<SearchQuery>,
) -> Response {
    let Some(_guild_id) = parse_guild_id(&guild_id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid guild id");
    };

    if let Some(video_id) = extract_video_id(&params.q) {
        return match state.player.resolve_video(&video_id).await {
            Ok(track) => Json(vec![track_json_from_track(&track)]).into_response(),
            Err(err) => error_response(StatusCode::BAD_REQUEST, err.to_string()),
        };
    }

    match state.player.search_tracks(&params.q).await {
        Ok(tracks) => {
            let tracks: Vec<TrackJson> = tracks.iter().map(track_json_from_track).collect();
            Json(tracks).into_response()
        }
        Err(err) => error_response(StatusCode::BAD_REQUEST, err.to_string()),
    }
}

#[derive(Deserialize)]
pub struct AddToQueueRequest {
    video_id: String,
}

pub async fn add_to_queue(
    State(state): State<WebState>,
    Path(guild_id): Path<String>,
    Json(body): Json<AddToQueueRequest>,
) -> Response {
    let Some(guild_id) = parse_guild_id(&guild_id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid guild id");
    };

    let track = match state.player.resolve_video(&body.video_id).await {
        Ok(track) => track,
        Err(err) => return error_response(StatusCode::BAD_REQUEST, err.to_string()),
    };

    let queued = QueuedTrack {
        track,
        requested_by: state.cache.current_user().id,
    };
    let result = state.player.enqueue(guild_id, queued).await;
    respond_after(&state.player, guild_id, result).await
}

#[derive(Serialize)]
struct PlaylistJson {
    id: i64,
    name: String,
    track_count: usize,
    thumbnail_video_id: Option<String>,
}

pub async fn list_playlists(
    State(state): State<WebState>,
    Path(guild_id): Path<String>,
) -> Response {
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
pub struct ImportPlaylistRequest {
    url: String,
}

pub async fn import_playlist(
    State(state): State<WebState>,
    Path(guild_id): Path<String>,
    Json(body): Json<ImportPlaylistRequest>,
) -> Response {
    let Some(guild_id) = parse_guild_id(&guild_id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid guild id");
    };

    let listing = match state.player.list_playlist(&body.url).await {
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

pub async fn play_playlist(
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

pub async fn favourites(State(state): State<WebState>, Path(guild_id): Path<String>) -> Response {
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
