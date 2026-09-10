use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use serenity::all::ChannelType;

use crate::web::WebState;
use crate::web::response::{error_response, parse_channel_id, parse_guild_id};
use crate::web::routes::playback::respond_after;

pub fn routes() -> Router<WebState> {
    Router::new()
        .route("/api/guilds", get(list_guilds))
        .route(
            "/api/guilds/{guild_id}/voice-channels",
            get(list_voice_channels),
        )
        .route("/api/guilds/{guild_id}/join", post(join_voice_channel))
}

#[derive(Serialize)]
struct GuildJson {
    id: String,
    name: String,
}

async fn list_guilds(State(state): State<WebState>) -> Response {
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

async fn list_voice_channels(
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
struct JoinVoiceChannelRequest {
    channel_id: String,
}

async fn join_voice_channel(
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
