//! `/join`, `/leave`, `/play`, `/queue`, `/skip`, `/pause`, `/resume`,
//! `/stop`, `/nowplaying`: voice playback commands.

use std::time::Duration;

use poise::serenity_prelude as serenity;

use super::{Context, Error};
use crate::voice::player::{PlayerError, QueuedTrack};
use crate::youtube::api::Track;
use crate::youtube::oauth::get_valid_access_token;

/// Max `upcoming` entries shown in `/queue` before truncating with a
/// "...and N more" trailer, to stay well under Discord's ~2000 char message
/// cap on a long queue.
const QUEUE_DISPLAY_LIMIT: usize = 10;

/// Extracts a YouTube video ID from a URL, recognizing `youtu.be` short
/// links, `.../watch?v=...`, and `.../shorts/...`. Returns `None` for
/// anything that isn't a URL at all (treated by callers as a search query)
/// or a recognized host with an unrecognized path.
fn extract_video_id(input: &str) -> Option<String> {
    let url = oauth2::url::Url::parse(input).ok()?;
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

/// Formats a `Duration` as `mm:ss`, or `h:mm:ss` once it reaches an hour.
fn format_duration(duration: Duration) -> String {
    let total_secs = duration.as_secs();
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
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

/// The invoking user's current voice channel in this guild, if any.
///
/// `ctx.guild()` returns a `GuildRef` cache guard that borrows from the
/// serenity cache and is not `Send` — it cannot be held across an `.await`.
/// Extracting just the `ChannelId` we need in one expression, with nothing
/// held afterward, is required for this to compile.
fn voice_channel_of(ctx: Context<'_>) -> Option<serenity::ChannelId> {
    ctx.guild()
        .and_then(|guild| guild.voice_states.get(&ctx.author().id).and_then(|vs| vs.channel_id))
}

async fn reply_public(ctx: Context<'_>, content: impl Into<String>) -> Result<(), Error> {
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

/// Joins the voice channel of the user invoking the command.
#[poise::command(slash_command, guild_only)]
pub async fn join(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = ctx.guild_id().expect("guild_only commands always have a guild");
    let Some(channel_id) = voice_channel_of(ctx) else {
        reply_error(ctx, "you're not in a voice channel").await?;
        return Ok(());
    };

    match ctx.data().player.join(guild_id, channel_id).await {
        Ok(()) => reply_public(ctx, format!("Joined <#{channel_id}>.")).await,
        Err(err) => reply_error(ctx, err.to_string()).await,
    }
}

/// Leaves the current voice channel and clears the queue.
#[poise::command(slash_command, guild_only)]
pub async fn leave(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = ctx.guild_id().expect("guild_only commands always have a guild");

    match ctx.data().player.leave(guild_id).await {
        Ok(()) => reply_public(ctx, "Left the voice channel.").await,
        Err(PlayerError::NotConnected) => reply_error(ctx, "not currently connected").await,
        Err(err) => reply_error(ctx, err.to_string()).await,
    }
}

/// Plays a YouTube video (URL or video ID), or searches and queues the top
/// result if given free text.
#[poise::command(slash_command, guild_only)]
pub async fn play(
    ctx: Context<'_>,
    #[description = "YouTube URL/video ID, or a search query"] query: String,
) -> Result<(), Error> {
    let guild_id = ctx.guild_id().expect("guild_only commands always have a guild");

    let access_token = match get_valid_access_token(
        &ctx.data().oauth_client,
        &ctx.data().oauth_http,
        &ctx.data().db,
        &ctx.author().id.to_string(),
    )
    .await
    {
        Ok(token) => token,
        Err(_) => {
            reply_error(ctx, "you need to link your Google account first — run `/link`").await?;
            return Ok(());
        }
    };

    if !ctx.data().player.is_connected(guild_id) {
        match voice_channel_of(ctx) {
            Some(channel_id) => {
                if let Err(err) = ctx.data().player.join(guild_id, channel_id).await {
                    reply_error(ctx, err.to_string()).await?;
                    return Ok(());
                }
            }
            None => {
                reply_error(ctx, "join a voice channel first, or use `/join`.").await?;
                return Ok(());
            }
        }
    }

    let track = if let Some(video_id) = extract_video_id(&query) {
        match ctx.data().youtube.get_video(&access_token, &video_id).await {
            Ok(track) => track,
            Err(err) => {
                reply_error(ctx, err.to_string()).await?;
                return Ok(());
            }
        }
    } else {
        // `/play` with free text takes only the top hit for convenience —
        // unlike `/search` (the other Phase 6 command), which shows an
        // interactive multi-result picker.
        match ctx.data().youtube.search(&access_token, &query).await {
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
    let guild_id = ctx.guild_id().expect("guild_only commands always have a guild");
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
    let guild_id = ctx.guild_id().expect("guild_only commands always have a guild");

    match ctx.data().player.skip(guild_id).await {
        Ok(()) => reply_public(ctx, "Skipped.").await,
        Err(PlayerError::NothingPlaying) => reply_error(ctx, "nothing is playing").await,
        Err(err) => reply_error(ctx, err.to_string()).await,
    }
}

/// Pauses the currently playing track.
#[poise::command(slash_command, guild_only)]
pub async fn pause(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = ctx.guild_id().expect("guild_only commands always have a guild");

    match ctx.data().player.pause(guild_id).await {
        Ok(()) => reply_public(ctx, "Paused.").await,
        Err(PlayerError::NothingPlaying) => reply_error(ctx, "nothing is playing").await,
        Err(err) => reply_error(ctx, err.to_string()).await,
    }
}

/// Resumes a paused track.
#[poise::command(slash_command, guild_only)]
pub async fn resume(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = ctx.guild_id().expect("guild_only commands always have a guild");

    match ctx.data().player.resume(guild_id).await {
        Ok(()) => reply_public(ctx, "Resumed.").await,
        Err(PlayerError::NothingPlaying) => reply_error(ctx, "nothing is playing").await,
        Err(err) => reply_error(ctx, err.to_string()).await,
    }
}

/// Stops playback and clears the queue.
#[poise::command(slash_command, guild_only)]
pub async fn stop(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = ctx.guild_id().expect("guild_only commands always have a guild");

    match ctx.data().player.stop(guild_id).await {
        Ok(()) => reply_public(ctx, "Stopped and cleared the queue.").await,
        Err(PlayerError::NothingPlaying) => reply_error(ctx, "nothing is playing").await,
        Err(err) => reply_error(ctx, err.to_string()).await,
    }
}

/// Shows the currently playing track.
#[poise::command(slash_command, guild_only)]
pub async fn nowplaying(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = ctx.guild_id().expect("guild_only commands always have a guild");
    let snapshot = ctx.data().player.queue_snapshot(guild_id).await;

    match snapshot.now_playing {
        Some(queued) => reply_public(ctx, format!("Now playing: {}", format_track(&queued.track))).await,
        None => reply_error(ctx, "nothing is playing").await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    // ---- format_duration ----

    #[test]
    fn formats_sub_hour_duration() {
        assert_eq!(format_duration(Duration::from_secs(213)), "3:33");
    }

    #[test]
    fn formats_over_an_hour_duration() {
        assert_eq!(format_duration(Duration::from_secs(3723)), "1:02:03");
    }
}
