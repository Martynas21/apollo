use std::time::Duration;

use poise::serenity_prelude as serenity;

use super::library;
use super::{Context, Data, Error};
use crate::voice::panel::format_duration;
use crate::voice::player::{PanelClaim, PlayerError, QueuedTrack};
use crate::youtube::api::Track;

const QUEUE_DISPLAY_LIMIT: usize = 10;

const SEARCH_MODAL_TIMEOUT: Duration = Duration::from_secs(300);

const VOLUME_MODAL_TIMEOUT: Duration = Duration::from_secs(120);

const STATUS_REPLY_CLEANUP_DELAY: Duration = Duration::from_secs(15);

pub(super) const REPLY_CLEANUP_DELAY: Duration = Duration::from_secs(30);

fn extract_video_id(input: &str) -> Option<String> {
    let url = url::Url::parse(input).ok()?;
    let host = url.host_str()?;

    if host == "youtu.be" {
        return url
            .path_segments()?
            .next()
            .filter(|s| !s.is_empty())
            .map(str::to_string);
    }

    if host == "youtube.com" || host.ends_with(".youtube.com") {
        if url.path() == "/watch" {
            return url
                .query_pairs()
                .find(|(key, _)| key == "v")
                .map(|(_, value)| value.into_owned());
        }
        if let Some(rest) = url.path().strip_prefix("/shorts/") {
            let id = rest.split('/').next()?;
            return (!id.is_empty()).then(|| id.to_string());
        }
        return None;
    }

    None
}

fn looks_like_playlist_url(input: &str) -> bool {
    let Ok(url) = url::Url::parse(input) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    let recognized_host =
        host == "youtu.be" || host == "youtube.com" || host.ends_with(".youtube.com");
    recognized_host && url.query_pairs().any(|(key, _)| key == "list")
}

fn format_track(track: &Track) -> String {
    match track.duration {
        Some(duration) => format!(
            "**{}** — {} ({})",
            track.title,
            track.channel,
            format_duration(duration)
        ),
        None => format!("**{}** — {}", track.title, track.channel),
    }
}

pub async fn handle_component(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    data: &Data,
) -> Result<(), Error> {
    let Some(custom_id) = component.data.custom_id.strip_prefix("player:") else {
        return Ok(());
    };
    let Some(guild_id) = component.guild_id else {
        return Ok(());
    };

    if custom_id == "search" {
        return handle_search_button(ctx, component, data).await;
    }
    if custom_id == "playlists" {
        return library::handle_playlists_button(ctx, component, data).await;
    }
    if custom_id == "volume" {
        return handle_volume_button(ctx, component, guild_id, data).await;
    }

    let Some(result) = apply_component_action(custom_id, component, guild_id, data).await else {
        return Ok(());
    };
    respond_to_component_action(ctx, component, guild_id, data, result).await
}

async fn apply_component_action(
    custom_id: &str,
    component: &serenity::ComponentInteraction,
    guild_id: serenity::GuildId,
    data: &Data,
) -> Option<Result<(), PlayerError>> {
    Some(match custom_id {
        "toggle" => match data.player.is_paused(guild_id).await {
            Some(true) => data.player.resume(guild_id).await,
            Some(false) => data.player.pause(guild_id).await,
            None => Err(PlayerError::NothingPlaying),
        },
        "skip" => data.player.skip(guild_id).await,
        "stop" => data.player.stop(guild_id).await,
        "shuffle" => data.player.shuffle(guild_id).await,
        "clear" => data.player.clear_queue(guild_id).await,
        "radio" => {
            data.player.toggle_radio(guild_id).await;
            Ok(())
        }
        "jump" => match &component.data.kind {
            serenity::ComponentInteractionDataKind::StringSelect { values } => {
                match values.first().and_then(|v| v.parse::<usize>().ok()) {
                    Some(index) => data.player.jump_to(guild_id, index).await,
                    None => Err(PlayerError::InvalidSelection),
                }
            }
            _ => Err(PlayerError::InvalidSelection),
        },
        _ => return None,
    })
}

async fn respond_to_component_action(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    guild_id: serenity::GuildId,
    data: &Data,
    result: Result<(), PlayerError>,
) -> Result<(), Error> {
    if let Err(err) = result {
        component
            .create_response(
                &ctx.http,
                serenity::CreateInteractionResponse::Message(
                    serenity::CreateInteractionResponseMessage::new()
                        .content(err.to_string())
                        .ephemeral(true),
                ),
            )
            .await?;
        return Ok(());
    }

    let (content, embed, components) = crate::voice::panel::render(&data.player, guild_id).await;
    let mut message = serenity::CreateInteractionResponseMessage::new()
        .content(content)
        .components(components);
    message = match embed {
        Some(embed) => message.embeds(vec![embed]),
        None => message.embeds(Vec::new()),
    };

    component
        .create_response(
            &ctx.http,
            serenity::CreateInteractionResponse::UpdateMessage(message),
        )
        .await?;
    Ok(())
}

async fn handle_search_button(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    data: &Data,
) -> Result<(), Error> {
    let modal_custom_id = format!("player:search_modal:{}", component.id);

    component
        .create_response(
            &ctx.http,
            serenity::CreateInteractionResponse::Modal(
                serenity::CreateModal::new(modal_custom_id.clone(), "Search YouTube").components(
                    vec![serenity::CreateActionRow::InputText(
                        serenity::CreateInputText::new(
                            serenity::InputTextStyle::Short,
                            "Search query",
                            "query",
                        )
                        .placeholder("e.g. lofi hip hop radio"),
                    )],
                ),
            ),
        )
        .await?;

    let user_id = component.user.id;
    let Some(modal) = serenity::ModalInteractionCollector::new(&ctx.shard)
        .filter(move |submission| {
            submission.data.custom_id == modal_custom_id && submission.user.id == user_id
        })
        .timeout(SEARCH_MODAL_TIMEOUT)
        .await
    else {
        return Ok(());
    };

    library::handle_search_modal_submit(ctx, &modal, data).await
}

fn parse_volume_input(raw: &str) -> Option<u8> {
    raw.trim().parse::<u8>().ok().filter(|level| *level <= 100)
}

fn modal_volume(data: &serenity::ModalInteractionData) -> Option<u8> {
    data.components.iter().find_map(|row| {
        row.components.iter().find_map(|component| match component {
            serenity::ActionRowComponent::InputText(input) if input.custom_id == "level" => {
                parse_volume_input(input.value.as_deref()?)
            }
            _ => None,
        })
    })
}

async fn await_volume_modal(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
) -> Result<Option<serenity::ModalInteraction>, Error> {
    let modal_custom_id = format!("player:volume_modal:{}", component.id);

    component
        .create_response(
            &ctx.http,
            serenity::CreateInteractionResponse::Modal(
                serenity::CreateModal::new(modal_custom_id.clone(), "Set Volume").components(vec![
                    serenity::CreateActionRow::InputText(
                        serenity::CreateInputText::new(
                            serenity::InputTextStyle::Short,
                            "Volume (0-100)",
                            "level",
                        )
                        .placeholder("e.g. 65"),
                    ),
                ]),
            ),
        )
        .await?;

    let user_id = component.user.id;
    Ok(serenity::ModalInteractionCollector::new(&ctx.shard)
        .filter(move |submission| {
            submission.data.custom_id == modal_custom_id && submission.user.id == user_id
        })
        .timeout(VOLUME_MODAL_TIMEOUT)
        .await)
}

async fn apply_volume_modal(
    ctx: &serenity::Context,
    modal: &serenity::ModalInteraction,
    guild_id: serenity::GuildId,
    data: &Data,
) -> Result<(), Error> {
    let Some(level) = modal_volume(&modal.data) else {
        modal
            .create_response(
                &ctx.http,
                serenity::CreateInteractionResponse::Message(
                    serenity::CreateInteractionResponseMessage::new()
                        .content("enter a whole number between 0 and 100.")
                        .ephemeral(true),
                ),
            )
            .await?;
        return Ok(());
    };

    if let Err(err) = data.player.set_volume(guild_id, level).await {
        modal
            .create_response(
                &ctx.http,
                serenity::CreateInteractionResponse::Message(
                    serenity::CreateInteractionResponseMessage::new()
                        .content(err.to_string())
                        .ephemeral(true),
                ),
            )
            .await?;
        return Ok(());
    }

    modal
        .create_response(&ctx.http, serenity::CreateInteractionResponse::Acknowledge)
        .await?;
    Ok(())
}

async fn handle_volume_button(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    guild_id: serenity::GuildId,
    data: &Data,
) -> Result<(), Error> {
    let Some(modal) = await_volume_modal(ctx, component).await? else {
        return Ok(());
    };
    apply_volume_modal(ctx, &modal, guild_id, data).await
}

fn voice_channel_of(ctx: Context<'_>) -> Option<serenity::ChannelId> {
    ctx.guild().and_then(|guild| {
        guild
            .voice_states
            .get(&ctx.author().id)
            .and_then(|vs| vs.channel_id)
    })
}

pub(super) async fn reply_public(
    ctx: Context<'_>,
    content: impl Into<String>,
) -> Result<(), Error> {
    let handle = ctx
        .send(
            poise::CreateReply::default()
                .content(content.into())
                .allowed_mentions(serenity::CreateAllowedMentions::new()),
        )
        .await?;
    schedule_cleanup(ctx, handle).await;
    Ok(())
}

async fn schedule_cleanup(ctx: Context<'_>, handle: poise::ReplyHandle<'_>) {
    let http = ctx.serenity_context().http.clone();
    if let Ok(message) = handle.into_message().await {
        tokio::spawn(async move {
            tokio::time::sleep(STATUS_REPLY_CLEANUP_DELAY).await;
            let _ = message.delete(http).await;
        });
    }
}

pub(super) async fn reply_error(ctx: Context<'_>, content: impl Into<String>) -> Result<(), Error> {
    ctx.send(
        poise::CreateReply::default()
            .content(content.into())
            .ephemeral(true),
    )
    .await?;
    Ok(())
}

fn require_guild_id(ctx: Context<'_>) -> Result<serenity::GuildId, Error> {
    ctx.guild_id()
        .ok_or_else(|| "this command can only be used in a server.".into())
}

async fn play_playlist(ctx: Context<'_>, query: &str) -> Result<(), Error> {
    let tracks = match ctx.data().youtube.list_playlist_items(query).await {
        Ok(listing) => listing.tracks,
        Err(err) => return reply_error(ctx, err.to_string()).await,
    };
    if tracks.is_empty() {
        return reply_error(ctx, "that playlist is empty (or couldn't be found).").await;
    }
    library::join_and_enqueue_all(ctx, query, tracks).await
}

pub(super) async fn join_for_play(
    ctx: Context<'_>,
    guild_id: serenity::GuildId,
) -> Result<(), Error> {
    match voice_channel_of(ctx) {
        Some(channel_id) => ctx
            .data()
            .player
            .join(guild_id, channel_id)
            .await
            .map_err(|err| err.to_string().into()),
        None if !ctx.data().player.is_connected(guild_id) => {
            Err("join a voice channel first, or use `/join`.".into())
        }
        None => Ok(()),
    }
}

async fn resolve_track(ctx: Context<'_>, video_id: &str) -> Result<Option<Track>, Error> {
    match ctx.data().youtube.get_video(video_id).await {
        Ok(track) => Ok(Some(track)),
        Err(err) => {
            reply_error(ctx, err.to_string()).await?;
            Ok(None)
        }
    }
}

#[poise::command(slash_command, guild_only)]
pub async fn play(
    ctx: Context<'_>,
    #[description = "YouTube URL/video ID, or a search query"] query: String,
) -> Result<(), Error> {
    let guild_id = require_guild_id(ctx)?;

    let video_id = extract_video_id(&query);
    if video_id.is_none() && looks_like_playlist_url(&query) {
        ctx.defer().await?;
        return play_playlist(ctx, &query).await;
    }

    let Some(video_id) = video_id else {
        ctx.defer_ephemeral().await?;
        let results = match library::search_with_cache(ctx.data(), guild_id, &query).await {
            Ok(results) => results,
            Err(err) => return reply_error(ctx, format!("Search failed: {err}")).await,
        };
        return library::present_search_results(ctx, &query, &results).await;
    };

    ctx.defer().await?;

    if let Err(err) = join_for_play(ctx, guild_id).await {
        return reply_error(ctx, err.to_string()).await;
    }

    let Some(track) = resolve_track(ctx, &video_id).await? else {
        return Ok(());
    };

    let queued = QueuedTrack {
        track: track.clone(),
        requested_by: ctx.author().id,
    };

    let handle = ctx
        .send(
            poise::CreateReply::default()
                .content(format!("Fetching: {}...", format_track(&track)))
                .allowed_mentions(serenity::CreateAllowedMentions::new()),
        )
        .await?;

    let result = ctx.data().player.enqueue(guild_id, queued).await;
    let content = match &result {
        Ok(()) => format!("Queued: {}", format_track(&track)),
        Err(err) => err.to_string(),
    };
    handle
        .edit(
            ctx,
            poise::CreateReply::default()
                .content(content)
                .allowed_mentions(serenity::CreateAllowedMentions::new()),
        )
        .await?;
    if result.is_ok() {
        schedule_cleanup(ctx, handle).await;
    }
    Ok(())
}

#[poise::command(slash_command, guild_only)]
pub async fn queue(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = require_guild_id(ctx)?;
    let snapshot = ctx.data().player.queue_snapshot(guild_id).await;

    let mut lines = Vec::new();
    match (&snapshot.now_playing, &snapshot.last_played) {
        (Some(queued), _) => lines.push(format!("Now playing: {}", format_track(&queued.track))),
        (None, Some(queued)) => {
            lines.push(format!("Finished playing: {}", format_track(&queued.track)));
        }
        (None, None) => lines.push("Nothing is playing.".to_string()),
    }

    if !snapshot.upcoming.is_empty() {
        lines.push(String::new());
        lines.push("Up next:".to_string());
        let shown = snapshot.upcoming.iter().take(QUEUE_DISPLAY_LIMIT);
        for (i, queued) in shown.enumerate() {
            lines.push(format!("{}. {}", i + 1, format_track(&queued.track)));
        }
        let remaining = snapshot.upcoming.len().saturating_sub(QUEUE_DISPLAY_LIMIT);
        if remaining > 0 {
            lines.push(format!("...and {remaining} more"));
        }
    }

    reply_public(ctx, lines.join("\n")).await
}

#[poise::command(slash_command, guild_only)]
pub async fn skip(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = require_guild_id(ctx)?;

    match ctx.data().player.skip(guild_id).await {
        Ok(()) => reply_public(ctx, "Skipped.").await,
        Err(PlayerError::NothingPlaying) => reply_error(ctx, "nothing is playing").await,
        Err(err) => reply_error(ctx, err.to_string()).await,
    }
}

#[poise::command(slash_command, guild_only)]
pub async fn pause(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = require_guild_id(ctx)?;

    match ctx.data().player.pause(guild_id).await {
        Ok(()) => reply_public(ctx, "Paused.").await,
        Err(PlayerError::NothingPlaying) => reply_error(ctx, "nothing is playing").await,
        Err(err) => reply_error(ctx, err.to_string()).await,
    }
}

#[poise::command(slash_command, guild_only)]
pub async fn resume(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = require_guild_id(ctx)?;

    match ctx.data().player.resume(guild_id).await {
        Ok(()) => reply_public(ctx, "Resumed.").await,
        Err(PlayerError::NothingPlaying) if !ctx.data().player.is_connected(guild_id) => {
            resume_by_rejoining(ctx, guild_id).await
        }
        Err(PlayerError::NothingPlaying) => reply_error(ctx, "nothing is playing").await,
        Err(err) => reply_error(ctx, err.to_string()).await,
    }
}

async fn resume_by_rejoining(ctx: Context<'_>, guild_id: serenity::GuildId) -> Result<(), Error> {
    if let Err(err) = join_for_play(ctx, guild_id).await {
        return reply_error(ctx, err.to_string()).await;
    }

    let snapshot = ctx.data().player.queue_snapshot(guild_id).await;
    if snapshot.now_playing.is_some() {
        reply_public(ctx, "Resumed.").await
    } else {
        reply_error(ctx, "nothing is playing").await
    }
}

#[poise::command(slash_command, guild_only)]
pub async fn stop(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = require_guild_id(ctx)?;

    match ctx.data().player.stop(guild_id).await {
        Ok(()) => reply_public(ctx, "Stopped and cleared the queue.").await,
        Err(PlayerError::NothingPlaying) => reply_error(ctx, "nothing is playing").await,
        Err(err) => reply_error(ctx, err.to_string()).await,
    }
}

#[poise::command(slash_command, guild_only)]
pub async fn player(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = require_guild_id(ctx)?;

    let (channel_id, message_id) = match ctx.data().player.claim_panel_slot(guild_id).await {
        PanelClaim::Existing(panel) => panel,
        PanelClaim::InProgress => {
            ctx.send(
                poise::CreateReply::default()
                    .content("The player panel is already being created — try again in a moment.")
                    .ephemeral(true),
            )
            .await?;
            return Ok(());
        }
        PanelClaim::Reserved => return post_new_panel(ctx, guild_id).await,
    };

    let link = message_id.link(channel_id, Some(guild_id));
    ctx.send(
        poise::CreateReply::default()
            .content(format!("The player panel is already active: {link}"))
            .ephemeral(true),
    )
    .await?;
    Ok(())
}

async fn post_new_panel(ctx: Context<'_>, guild_id: serenity::GuildId) -> Result<(), Error> {
    let (content, embed, components) =
        crate::voice::panel::render(&ctx.data().player, guild_id).await;
    let mut reply = poise::CreateReply::default()
        .content(content)
        .components(components);
    if let Some(embed) = embed {
        reply = reply.embed(embed);
    }

    let posted: Result<_, Error> = async {
        let handle = ctx.send(reply).await?;
        let message = handle.message().await?;
        Ok((message.channel_id, message.id))
    }
    .await;

    match posted {
        Ok((channel_id, message_id)) => {
            ctx.data()
                .player
                .set_panel(guild_id, channel_id, message_id)
                .await;
            Ok(())
        }
        Err(err) => {
            ctx.data().player.release_panel_slot(guild_id).await;
            Err(err)
        }
    }
}

#[poise::command(slash_command, guild_only)]
pub async fn shuffle(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = require_guild_id(ctx)?;

    match ctx.data().player.shuffle(guild_id).await {
        Ok(()) => reply_public(ctx, "Shuffled the queue.").await,
        Err(PlayerError::NothingToShuffle) => {
            reply_error(ctx, "not enough upcoming tracks to shuffle").await
        }
        Err(err) => reply_error(ctx, err.to_string()).await,
    }
}

#[poise::command(slash_command, guild_only)]
pub async fn clear(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = require_guild_id(ctx)?;

    match ctx.data().player.clear_queue(guild_id).await {
        Ok(()) => reply_public(ctx, "Cleared the queue.").await,
        Err(PlayerError::QueueEmpty) => reply_error(ctx, "the queue is already empty").await,
        Err(err) => reply_error(ctx, err.to_string()).await,
    }
}

#[poise::command(slash_command, guild_only)]
pub async fn volume(
    ctx: Context<'_>,
    #[description = "Volume, 0-100"]
    #[min = 0]
    #[max = 100]
    level: u8,
) -> Result<(), Error> {
    let guild_id = require_guild_id(ctx)?;

    match ctx.data().player.set_volume(guild_id, level).await {
        Ok(()) => reply_public(ctx, format!("Volume set to {level}.")).await,
        Err(err) => reply_error(ctx, err.to_string()).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_volume() {
        assert_eq!(parse_volume_input("65"), Some(65));
    }

    #[test]
    fn parses_volume_with_surrounding_whitespace() {
        assert_eq!(parse_volume_input("  42 "), Some(42));
    }

    #[test]
    fn accepts_boundary_volumes() {
        assert_eq!(parse_volume_input("0"), Some(0));
        assert_eq!(parse_volume_input("100"), Some(100));
    }

    #[test]
    fn rejects_out_of_range_volume() {
        assert_eq!(parse_volume_input("101"), None);
    }

    #[test]
    fn rejects_non_numeric_volume() {
        assert_eq!(parse_volume_input("loud"), None);
    }

    #[test]
    fn rejects_fractional_volume() {
        assert_eq!(parse_volume_input("50.5"), None);
    }

    #[test]
    fn extracts_from_youtu_be_short_link() {
        assert_eq!(
            extract_video_id("https://youtu.be/dQw4w9WgXcQ"),
            Some("dQw4w9WgXcQ".to_string())
        );
    }

    #[test]
    fn extracts_from_watch_url_with_extra_params() {
        assert_eq!(
            extract_video_id("https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=PLxxxx"),
            Some("dQw4w9WgXcQ".to_string())
        );
    }

    #[test]
    fn extracts_from_shorts_url() {
        assert_eq!(
            extract_video_id("https://www.youtube.com/shorts/dQw4w9WgXcQ"),
            Some("dQw4w9WgXcQ".to_string())
        );
    }

    #[test]
    fn plain_search_query_returns_none() {
        assert_eq!(extract_video_id("never gonna give you up"), None);
    }

    #[test]
    fn extracts_from_music_youtube_com() {
        assert_eq!(
            extract_video_id("https://music.youtube.com/watch?v=dQw4w9WgXcQ"),
            Some("dQw4w9WgXcQ".to_string())
        );
    }

    #[test]
    fn unrecognized_path_on_known_host_returns_none() {
        assert_eq!(
            extract_video_id("https://www.youtube.com/channel/UCxxxx"),
            None
        );
    }

    #[test]
    fn unrelated_url_returns_none() {
        assert_eq!(extract_video_id("https://example.com/foo"), None);
    }

    #[test]
    fn recognizes_a_bare_playlist_url() {
        assert!(looks_like_playlist_url(
            "https://www.youtube.com/playlist?list=PLxxxx"
        ));
    }

    #[test]
    fn recognizes_a_list_param_on_youtu_be() {
        assert!(looks_like_playlist_url(
            "https://youtu.be/dQw4w9WgXcQ?list=PLxxxx"
        ));
    }

    #[test]
    fn recognizes_a_list_param_on_a_watch_url_too() {
        assert!(looks_like_playlist_url(
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=PLxxxx"
        ));
    }

    #[test]
    fn a_video_url_with_no_list_param_is_not_a_playlist_url() {
        assert!(!looks_like_playlist_url(
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ"
        ));
    }

    #[test]
    fn a_list_param_on_an_unrelated_host_is_not_a_playlist_url() {
        assert!(!looks_like_playlist_url("https://example.com/foo?list=1"));
    }

    #[test]
    fn plain_text_is_not_a_playlist_url() {
        assert!(!looks_like_playlist_url("never gonna give you up"));
    }

    #[test]
    fn format_track_includes_duration_when_known() {
        let track = Track {
            video_id: "abc123".to_string(),
            title: "Some Video".to_string(),
            channel: "Some Channel".to_string(),
            duration: Some(Duration::from_secs(213)),
        };
        assert_eq!(format_track(&track), "**Some Video** — Some Channel (3:33)");
    }

    #[test]
    fn format_track_omits_duration_when_unknown() {
        let track = Track {
            video_id: "abc123".to_string(),
            title: "Some Video".to_string(),
            channel: "Some Channel".to_string(),
            duration: None,
        };
        assert_eq!(format_track(&track), "**Some Video** — Some Channel");
    }
}
