//! `/play`, `/queue`, `/skip`, `/pause`, `/resume`, `/stop`, `/player`,
//! `/shuffle`, `/volume`: voice playback commands.

use std::time::Duration;

use poise::serenity_prelude as serenity;

use super::library;
use super::{Context, Data, Error};
use crate::voice::panel::format_duration;
use crate::voice::player::{PlayerError, QueuedTrack};
use crate::youtube::api::Track;

/// Max `upcoming` entries shown in `/queue` before truncating with a
/// "...and N more" trailer, to stay well under Discord's ~2000 char message
/// cap on a long queue.
const QUEUE_DISPLAY_LIMIT: usize = 10;

/// How long the `/player` panel's Search button waits for a modal submission
/// before giving up.
const SEARCH_MODAL_TIMEOUT: Duration = Duration::from_secs(300);

/// How long the `/player` panel's Volume button waits for a modal submission
/// before giving up.
const VOLUME_MODAL_TIMEOUT: Duration = Duration::from_secs(120);

/// Extracts a `YouTube` video ID from a URL, recognizing `youtu.be` short
/// links, `.../watch?v=...`, and `.../shorts/...`. Returns `None` for
/// anything that isn't a URL at all (treated by callers as a search query)
/// or a recognized host with an unrecognized path.
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

/// Formats a track as `**title** — channel (mm:ss)`, omitting the duration
/// parenthetical when unknown.
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

/// Handles a `player:*` button/select click from the `/player` panel.
///
/// `search` and `playlists` open a library picker and don't touch playback
/// state, so they're handled up front and return early. Every other id
/// applies a [`crate::voice::PlayerRegistry`] mutation, then rebuilds the
/// panel via [`crate::voice::panel::render`] and edits it in place —
/// `PlayerRegistry` also pushes this same rendering to the panel on its own
/// after the mutation, so this in-place edit is just for zero-latency
/// feedback on the click itself.
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
    if custom_id == "volume" {
        return handle_volume_button(ctx, component, data).await;
    }

    let result: Result<(), PlayerError> = match custom_id {
        "toggle" => match data.player.is_paused(guild_id).await {
            Some(true) => data.player.resume(guild_id).await,
            Some(false) => data.player.pause(guild_id).await,
            None => Err(PlayerError::NothingPlaying),
        },
        "skip" => data.player.skip(guild_id).await,
        "stop" => data.player.stop(guild_id).await,
        "shuffle" => data.player.shuffle(guild_id).await,
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
        _ => return Ok(()),
    };

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

/// Handles the panel's `player:search` button: shows a one-field modal for a
/// search query, waits for it to be submitted, then hands the query off to
/// [`library::handle_search_modal_submit`] for the actual search + picker.
///
/// The modal's custom id is namespaced with the clicking interaction's own
/// id so concurrent searches (different users, or the same user opening it
/// twice) don't cross-collect each other's submissions.
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
        // Nobody submitted before the timeout — nothing to clean up, the
        // modal just closes itself client-side.
        return Ok(());
    };

    library::handle_search_modal_submit(ctx, &modal, data).await
}

/// Parses a typed volume field's raw text into a level (0-100). `None` if
/// it isn't a whole number or is out of range.
fn parse_volume_input(raw: &str) -> Option<u8> {
    raw.trim().parse::<u8>().ok().filter(|level| *level <= 100)
}

/// Reads the typed volume level (0-100) out of a submitted volume modal.
/// `None` if the field is missing or [`parse_volume_input`] rejects it.
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

/// Handles the panel's `player:volume` button: shows a one-field modal for a
/// typed volume level, applies it, then acknowledges the modal submission
/// with no visible change — [`crate::voice::PlayerRegistry::set_volume`]
/// already refreshes the live panel on its own.
async fn handle_volume_button(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    data: &Data,
) -> Result<(), Error> {
    let guild_id = component
        .guild_id
        .expect("checked by handle_component before dispatch");
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
    let Some(modal) = serenity::ModalInteractionCollector::new(&ctx.shard)
        .filter(move |submission| {
            submission.data.custom_id == modal_custom_id && submission.user.id == user_id
        })
        .timeout(VOLUME_MODAL_TIMEOUT)
        .await
    else {
        return Ok(());
    };

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

/// The invoking user's current voice channel in this guild, if any.
///
/// `ctx.guild()` returns a `GuildRef` cache guard that borrows from the
/// serenity cache and is not `Send` — it cannot be held across an `.await`.
/// Extracting just the `ChannelId` we need in one expression, with nothing
/// held afterward, is required for this to compile.
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
    ctx.send(poise::CreateReply::default().content(content.into()))
        .await?;
    Ok(())
}

async fn reply_error(ctx: Context<'_>, content: impl Into<String>) -> Result<(), Error> {
    ctx.send(
        poise::CreateReply::default()
            .content(content.into())
            .ephemeral(true),
    )
    .await?;
    Ok(())
}

/// Plays a `YouTube` video (URL or video ID), or searches and queues the top
/// result if given free text.
#[poise::command(slash_command, guild_only)]
pub async fn play(
    ctx: Context<'_>,
    #[description = "YouTube URL/video ID, or a search query"] query: String,
) -> Result<(), Error> {
    let guild_id = ctx
        .guild_id()
        .expect("guild_only commands always have a guild");

    if !ctx.data().player.is_connected(guild_id) {
        if let Some(channel_id) = voice_channel_of(ctx) {
            if let Err(err) = ctx.data().player.join(guild_id, channel_id).await {
                reply_error(ctx, err.to_string()).await?;
                return Ok(());
            }
        } else {
            reply_error(ctx, "join a voice channel first, or use `/join`.").await?;
            return Ok(());
        }
    }

    let track = if let Some(video_id) = extract_video_id(&query) {
        match ctx.data().youtube.get_video(&video_id).await {
            Ok(track) => track,
            Err(err) => {
                reply_error(ctx, err.to_string()).await?;
                return Ok(());
            }
        }
    } else {
        // `/play` with free text takes only the top hit for convenience —
        // unlike `/add_to_queue`, which shows an interactive multi-result
        // picker.
        match ctx.data().youtube.search(&query).await {
            Ok(results) if results.is_empty() => {
                reply_error(ctx, format!("no results found for '{query}'")).await?;
                return Ok(());
            }
            Ok(mut results) => results.remove(0),
            Err(err) => {
                reply_error(ctx, err.to_string()).await?;
                return Ok(());
            }
        }
    };

    let queued = QueuedTrack {
        track: track.clone(),
        requested_by: ctx.author().id,
    };

    match ctx.data().player.enqueue(guild_id, queued).await {
        Ok(()) => reply_public(ctx, format!("Queued: {}", format_track(&track))).await,
        Err(err) => reply_error(ctx, err.to_string()).await,
    }
}

/// Shows the current queue.
#[poise::command(slash_command, guild_only)]
pub async fn queue(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = ctx
        .guild_id()
        .expect("guild_only commands always have a guild");
    let snapshot = ctx.data().player.queue_snapshot(guild_id).await;

    let mut lines = Vec::new();
    match &snapshot.now_playing {
        Some(queued) => lines.push(format!("Now playing: {}", format_track(&queued.track))),
        None => lines.push("Nothing is playing.".to_string()),
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

/// Skips the currently playing track.
#[poise::command(slash_command, guild_only)]
pub async fn skip(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = ctx
        .guild_id()
        .expect("guild_only commands always have a guild");

    match ctx.data().player.skip(guild_id).await {
        Ok(()) => reply_public(ctx, "Skipped.").await,
        Err(PlayerError::NothingPlaying) => reply_error(ctx, "nothing is playing").await,
        Err(err) => reply_error(ctx, err.to_string()).await,
    }
}

/// Pauses the currently playing track.
#[poise::command(slash_command, guild_only)]
pub async fn pause(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = ctx
        .guild_id()
        .expect("guild_only commands always have a guild");

    match ctx.data().player.pause(guild_id).await {
        Ok(()) => reply_public(ctx, "Paused.").await,
        Err(PlayerError::NothingPlaying) => reply_error(ctx, "nothing is playing").await,
        Err(err) => reply_error(ctx, err.to_string()).await,
    }
}

/// Resumes a paused track.
#[poise::command(slash_command, guild_only)]
pub async fn resume(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = ctx
        .guild_id()
        .expect("guild_only commands always have a guild");

    match ctx.data().player.resume(guild_id).await {
        Ok(()) => reply_public(ctx, "Resumed.").await,
        Err(PlayerError::NothingPlaying) => reply_error(ctx, "nothing is playing").await,
        Err(err) => reply_error(ctx, err.to_string()).await,
    }
}

/// Stops playback and clears the queue.
#[poise::command(slash_command, guild_only)]
pub async fn stop(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = ctx
        .guild_id()
        .expect("guild_only commands always have a guild");

    match ctx.data().player.stop(guild_id).await {
        Ok(()) => reply_public(ctx, "Stopped and cleared the queue.").await,
        Err(PlayerError::NothingPlaying) => reply_error(ctx, "nothing is playing").await,
        Err(err) => reply_error(ctx, err.to_string()).await,
    }
}

/// Posts (or reposts) this guild's persistent player panel.
///
/// Combines playback controls with Search/Playlists entry points into the
/// library, kept up to date on its own (by [`crate::voice::PlayerRegistry`])
/// as state changes — whether via its own buttons, a slash command, or a
/// track ending on its own.
///
/// Deletes this guild's previous panel first, if it had one, so there's
/// never more than one live panel per guild.
#[poise::command(slash_command, guild_only)]
pub async fn player(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = ctx
        .guild_id()
        .expect("guild_only commands always have a guild");

    let (content, embed, components) =
        crate::voice::panel::render(&ctx.data().player, guild_id).await;
    let mut reply = poise::CreateReply::default()
        .content(content)
        .components(components);
    if let Some(embed) = embed {
        reply = reply.embed(embed);
    }

    let handle = ctx.send(reply).await?;
    let message = handle.message().await?;
    ctx.data()
        .player
        .replace_panel(guild_id, message.channel_id, message.id)
        .await;

    Ok(())
}

/// Shuffles the upcoming queue. Leaves the currently playing track alone.
#[poise::command(slash_command, guild_only)]
pub async fn shuffle(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = ctx
        .guild_id()
        .expect("guild_only commands always have a guild");

    match ctx.data().player.shuffle(guild_id).await {
        Ok(()) => reply_public(ctx, "Shuffled the queue.").await,
        Err(PlayerError::NothingToShuffle) => {
            reply_error(ctx, "not enough upcoming tracks to shuffle").await
        }
        Err(err) => reply_error(ctx, err.to_string()).await,
    }
}

/// Sets the playback volume (0-100). Applies immediately and persists for
/// future tracks.
#[poise::command(slash_command, guild_only)]
pub async fn volume(
    ctx: Context<'_>,
    #[description = "Volume, 0-100"]
    #[min = 0]
    #[max = 100]
    level: u8,
) -> Result<(), Error> {
    let guild_id = ctx
        .guild_id()
        .expect("guild_only commands always have a guild");

    match ctx.data().player.set_volume(guild_id, level).await {
        Ok(()) => reply_public(ctx, format!("Volume set to {level}.")).await,
        Err(err) => reply_error(ctx, err.to_string()).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- parse_volume_input ----

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

    // ---- extract_video_id ----

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

    // ---- format_track ----

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
