//! `/search`, `/searchplay`, `/playlists`, `/playlistplay`, `/playlistqueue`,
//! `/liked`, `/likedplay`: browsing a linked account's YouTube library and
//! queuing tracks from it.
//!
//! No interactive component picker (buttons/select menus) — a "browse, then
//! queue by number in a follow-up command" pattern is used instead. This
//! also means each `*play`/`*queue` command re-fetches the same listing a
//! preceding `/search`, `/playlists`, or `/liked` already showed, rather
//! than caching results between command invocations: a shared "last
//! results" cache would need per-user expiry/cleanup that isn't worth the
//! complexity for this MVP pass.

use poise::serenity_prelude as serenity;

use super::{Context, Error};
use crate::voice::QueuedTrack;
use crate::youtube::api::Track;
use crate::youtube::oauth::get_valid_access_token;

/// Max results shown by `/search`.
const SEARCH_DISPLAY_LIMIT: usize = 5;
/// Max results shown by `/playlists`, `/playlistplay`, and `/liked`.
const LIST_DISPLAY_LIMIT: usize = 10;

/// Fetches a valid access token for the invoking user, or replies
/// ephemerally with a `/link` prompt and returns `Ok(None)` if there isn't
/// one — the expected case of an unlinked account, not a real error.
async fn require_access_token(ctx: Context<'_>) -> Result<Option<String>, Error> {
    match get_valid_access_token(
        &ctx.data().oauth_client,
        &ctx.data().oauth_http,
        &ctx.data().db,
        &ctx.author().id.to_string(),
    )
    .await
    {
        Ok(token) => Ok(Some(token)),
        Err(_) => {
            ctx.send(
                poise::CreateReply::default()
                    .content("You don't have a linked Google account yet. Run `/link` first.")
                    .ephemeral(true),
            )
            .await?;
            Ok(None)
        }
    }
}

/// Formats one line of a numbered track listing: `N. Title — Channel (mm:ss)`,
/// omitting the duration parens entirely when unknown.
fn format_track_line(index: usize, track: &Track) -> String {
    match track.duration {
        Some(duration) => {
            let total_secs = duration.as_secs();
            let minutes = total_secs / 60;
            let seconds = total_secs % 60;
            format!(
                "{index}. {} — {} ({minutes}:{seconds:02})",
                track.title, track.channel
            )
        }
        None => format!("{index}. {} — {}", track.title, track.channel),
    }
}

/// Renders a numbered, length-capped track listing with a trailing
/// truncation note if there were more results than `limit`.
fn format_track_list(tracks: &[Track], limit: usize) -> String {
    let mut lines: Vec<String> = tracks
        .iter()
        .take(limit)
        .enumerate()
        .map(|(i, track)| format_track_line(i + 1, track))
        .collect();

    if tracks.len() > limit {
        lines.push(format!("...and {} more not shown", tracks.len() - limit));
    }

    lines.join("\n")
}

/// Renders a numbered, length-capped playlist listing with a trailing
/// truncation note if there were more results than [`LIST_DISPLAY_LIMIT`].
fn format_playlist_list(playlists: &[crate::youtube::api::Playlist]) -> String {
    let mut lines: Vec<String> = playlists
        .iter()
        .take(LIST_DISPLAY_LIMIT)
        .enumerate()
        .map(|(i, playlist)| {
            let count_suffix = playlist
                .item_count
                .map(|c| format!(" ({c} items)"))
                .unwrap_or_default();
            format!("{}. {}{count_suffix}", i + 1, playlist.title)
        })
        .collect();

    if playlists.len() > LIST_DISPLAY_LIMIT {
        lines.push(format!(
            "...and {} more not shown",
            playlists.len() - LIST_DISPLAY_LIMIT
        ));
    }

    lines.join("\n")
}

/// Determines the voice channel to auto-join into, if the bot isn't already
/// connected in this guild. `ctx.guild()` returns a cache guard (`GuildRef`)
/// that isn't `Send` and can't be held across an `.await`, so the owned
/// channel ID is extracted in one expression before any `.await` point.
fn author_voice_channel(ctx: Context<'_>) -> Option<serenity::ChannelId> {
    ctx.guild().and_then(|guild| {
        guild
            .voice_states
            .get(&ctx.author().id)
            .and_then(|vs| vs.channel_id)
    })
}

/// Ensures the bot is connected to a voice channel in this guild, joining
/// the invoking user's current channel if not. Replies ephemerally and
/// returns `Ok(false)` if the bot isn't connected and the user isn't in a
/// voice channel either (the expected "can't queue" case, not a real error).
async fn ensure_connected(ctx: Context<'_>, guild_id: serenity::GuildId) -> Result<bool, Error> {
    if ctx.data().player.is_connected(guild_id) {
        return Ok(true);
    }

    let Some(channel_id) = author_voice_channel(ctx) else {
        ctx.send(
            poise::CreateReply::default()
                .content("Join a voice channel first, or use `/join`.")
                .ephemeral(true),
        )
        .await?;
        return Ok(false);
    };

    if let Err(err) = ctx.data().player.join(guild_id, channel_id).await {
        ctx.send(
            poise::CreateReply::default()
                .content(format!("Failed to join voice channel: {err}"))
                .ephemeral(true),
        )
        .await?;
        return Ok(false);
    }

    Ok(true)
}

/// Auto-joins if needed, enqueues `track`, and sends the public confirmation
/// reply. Shared by every `*play`/`*queue` command below.
async fn join_and_enqueue(ctx: Context<'_>, track: Track) -> Result<(), Error> {
    // guild_only guarantees a guild context; ctx.guild_id() still returns
    // Option per poise's API, so unwrap with an expect documenting why.
    let guild_id = ctx.guild_id().expect("guild_only command has a guild id");

    if !ensure_connected(ctx, guild_id).await? {
        return Ok(());
    }

    let title = track.title.clone();
    let channel = track.channel.clone();
    let queued = QueuedTrack {
        track,
        requested_by: ctx.author().id,
    };

    if let Err(err) = ctx.data().player.enqueue(guild_id, queued).await {
        ctx.send(
            poise::CreateReply::default()
                .content(format!("Failed to queue track: {err}"))
                .ephemeral(true),
        )
        .await?;
        return Ok(());
    }

    ctx.send(poise::CreateReply::default().content(format!("Queued: **{title}** — {channel}")))
        .await?;

    Ok(())
}

/// Searches YouTube for `query` and shows up to 5 results. Queue one with
/// `/searchplay`.
#[poise::command(slash_command, guild_only)]
pub async fn search(ctx: Context<'_>, #[description = "Search query"] query: String) -> Result<(), Error> {
    let Some(token) = require_access_token(ctx).await? else {
        return Ok(());
    };

    let results = match ctx.data().youtube.search(&token, &query).await {
        Ok(results) => results,
        Err(err) => {
            ctx.send(
                poise::CreateReply::default()
                    .content(format!("Search failed: {err}"))
                    .ephemeral(true),
            )
            .await?;
            return Ok(());
        }
    };

    if results.is_empty() {
        ctx.send(
            poise::CreateReply::default()
                .content("No results found.")
                .ephemeral(true),
        )
        .await?;
        return Ok(());
    }

    let listing = format_track_list(&results, SEARCH_DISPLAY_LIMIT);
    ctx.send(
        poise::CreateReply::default()
            .content(format!(
                "Search results for \"{query}\":\n{listing}\n\nRun `/searchplay {query} <number>` to queue one."
            ))
            .ephemeral(true),
    )
    .await?;

    Ok(())
}

/// Re-runs a `/search` and queues result number `number` from it.
#[poise::command(slash_command, guild_only)]
pub async fn searchplay(
    ctx: Context<'_>,
    #[description = "Search query"] query: String,
    #[description = "Result number from /search"] number: u8,
) -> Result<(), Error> {
    let Some(token) = require_access_token(ctx).await? else {
        return Ok(());
    };

    let results = match ctx.data().youtube.search(&token, &query).await {
        Ok(results) => results,
        Err(err) => {
            ctx.send(
                poise::CreateReply::default()
                    .content(format!("Search failed: {err}"))
                    .ephemeral(true),
            )
            .await?;
            return Ok(());
        }
    };

    let Some(track) = select_by_number(&results, number) else {
        ctx.send(
            poise::CreateReply::default()
                .content("Invalid selection.")
                .ephemeral(true),
        )
        .await?;
        return Ok(());
    };

    join_and_enqueue(ctx, track).await
}

/// Lists the linked account's playlists. Browse one with `/playlistplay`.
#[poise::command(slash_command, guild_only)]
pub async fn playlists(ctx: Context<'_>) -> Result<(), Error> {
    let Some(token) = require_access_token(ctx).await? else {
        return Ok(());
    };

    let playlists = match ctx.data().youtube.list_playlists(&token).await {
        Ok(playlists) => playlists,
        Err(err) => {
            ctx.send(
                poise::CreateReply::default()
                    .content(format!("Failed to list playlists: {err}"))
                    .ephemeral(true),
            )
            .await?;
            return Ok(());
        }
    };

    if playlists.is_empty() {
        ctx.send(
            poise::CreateReply::default()
                .content("You don't have any playlists.")
                .ephemeral(true),
        )
        .await?;
        return Ok(());
    }

    let listing = format_playlist_list(&playlists);
    ctx.send(
        poise::CreateReply::default()
            .content(format!(
                "Your playlists:\n{listing}\n\nRun `/playlistplay <number>` to browse one."
            ))
            .ephemeral(true),
    )
    .await?;

    Ok(())
}

/// Browses playlist `number`'s tracks. Queue one with `/playlistqueue`.
#[poise::command(slash_command, guild_only)]
pub async fn playlistplay(
    ctx: Context<'_>,
    #[description = "Playlist number from /playlists"] number: u8,
) -> Result<(), Error> {
    let Some(token) = require_access_token(ctx).await? else {
        return Ok(());
    };

    let playlists = match ctx.data().youtube.list_playlists(&token).await {
        Ok(playlists) => playlists,
        Err(err) => {
            ctx.send(
                poise::CreateReply::default()
                    .content(format!("Failed to list playlists: {err}"))
                    .ephemeral(true),
            )
            .await?;
            return Ok(());
        }
    };

    let Some(playlist) = select_by_number(&playlists, number) else {
        ctx.send(
            poise::CreateReply::default()
                .content("Invalid selection.")
                .ephemeral(true),
        )
        .await?;
        return Ok(());
    };

    let tracks = match ctx
        .data()
        .youtube
        .list_playlist_items(&token, &playlist.id)
        .await
    {
        Ok(tracks) => tracks,
        Err(err) => {
            ctx.send(
                poise::CreateReply::default()
                    .content(format!("Failed to list playlist items: {err}"))
                    .ephemeral(true),
            )
            .await?;
            return Ok(());
        }
    };

    if tracks.is_empty() {
        ctx.send(
            poise::CreateReply::default()
                .content(format!("**{}** is empty.", playlist.title))
                .ephemeral(true),
        )
        .await?;
        return Ok(());
    }

    let listing = format_track_list(&tracks, LIST_DISPLAY_LIMIT);
    ctx.send(
        poise::CreateReply::default()
            .content(format!(
                "Tracks in **{}**:\n{listing}\n\nRun `/playlistqueue {number} <track number>` to queue one.",
                playlist.title
            ))
            .ephemeral(true),
    )
    .await?;

    Ok(())
}

/// Queues track `track_number` from playlist `playlist_number`.
#[poise::command(slash_command, guild_only)]
pub async fn playlistqueue(
    ctx: Context<'_>,
    #[description = "Playlist number from /playlists"] playlist_number: u8,
    #[description = "Track number from /playlistplay"] track_number: u8,
) -> Result<(), Error> {
    let Some(token) = require_access_token(ctx).await? else {
        return Ok(());
    };

    let playlists = match ctx.data().youtube.list_playlists(&token).await {
        Ok(playlists) => playlists,
        Err(err) => {
            ctx.send(
                poise::CreateReply::default()
                    .content(format!("Failed to list playlists: {err}"))
                    .ephemeral(true),
            )
            .await?;
            return Ok(());
        }
    };

    let Some(playlist) = select_by_number(&playlists, playlist_number) else {
        ctx.send(
            poise::CreateReply::default()
                .content("Invalid selection.")
                .ephemeral(true),
        )
        .await?;
        return Ok(());
    };

    let tracks = match ctx
        .data()
        .youtube
        .list_playlist_items(&token, &playlist.id)
        .await
    {
        Ok(tracks) => tracks,
        Err(err) => {
            ctx.send(
                poise::CreateReply::default()
                    .content(format!("Failed to list playlist items: {err}"))
                    .ephemeral(true),
            )
            .await?;
            return Ok(());
        }
    };

    let Some(track) = select_by_number(&tracks, track_number) else {
        ctx.send(
            poise::CreateReply::default()
                .content("Invalid selection.")
                .ephemeral(true),
        )
        .await?;
        return Ok(());
    };

    join_and_enqueue(ctx, track).await
}

/// Lists the linked account's liked videos. Queue one with `/likedplay`.
#[poise::command(slash_command, guild_only)]
pub async fn liked(ctx: Context<'_>) -> Result<(), Error> {
    let Some(token) = require_access_token(ctx).await? else {
        return Ok(());
    };

    let tracks = match ctx.data().youtube.list_liked_videos(&token).await {
        Ok(tracks) => tracks,
        Err(err) => {
            ctx.send(
                poise::CreateReply::default()
                    .content(format!("Failed to list liked videos: {err}"))
                    .ephemeral(true),
            )
            .await?;
            return Ok(());
        }
    };

    if tracks.is_empty() {
        ctx.send(
            poise::CreateReply::default()
                .content("You don't have any liked videos.")
                .ephemeral(true),
        )
        .await?;
        return Ok(());
    }

    let listing = format_track_list(&tracks, LIST_DISPLAY_LIMIT);
    ctx.send(
        poise::CreateReply::default()
            .content(format!(
                "Your liked videos:\n{listing}\n\nRun `/likedplay <number>` to queue one."
            ))
            .ephemeral(true),
    )
    .await?;

    Ok(())
}

/// Re-fetches liked videos and queues result number `number` from them.
#[poise::command(slash_command, guild_only)]
pub async fn likedplay(
    ctx: Context<'_>,
    #[description = "Result number from /liked"] number: u8,
) -> Result<(), Error> {
    let Some(token) = require_access_token(ctx).await? else {
        return Ok(());
    };

    let tracks = match ctx.data().youtube.list_liked_videos(&token).await {
        Ok(tracks) => tracks,
        Err(err) => {
            ctx.send(
                poise::CreateReply::default()
                    .content(format!("Failed to list liked videos: {err}"))
                    .ephemeral(true),
            )
            .await?;
            return Ok(());
        }
    };

    let Some(track) = select_by_number(&tracks, number) else {
        ctx.send(
            poise::CreateReply::default()
                .content("Invalid selection.")
                .ephemeral(true),
        )
        .await?;
        return Ok(());
    };

    join_and_enqueue(ctx, track).await
}

/// 1-indexes into `items` by a `u8` selection, returning a clone. `None` for
/// 0 or out-of-range.
fn select_by_number<T: Clone>(items: &[T], number: u8) -> Option<T> {
    if number == 0 {
        return None;
    }
    items.get(number as usize - 1).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn track(title: &str, channel: &str, duration: Option<Duration>) -> Track {
        Track {
            video_id: "abc123".to_string(),
            title: title.to_string(),
            channel: channel.to_string(),
            duration,
        }
    }

    #[test]
    fn formats_line_with_duration() {
        let t = track("Some Song", "Some Channel", Some(Duration::from_secs(253)));
        assert_eq!(format_track_line(1, &t), "1. Some Song — Some Channel (4:13)");
    }

    #[test]
    fn formats_line_without_duration() {
        let t = track("Some Song", "Some Channel", None);
        assert_eq!(format_track_line(3, &t), "3. Some Song — Some Channel");
    }

    #[test]
    fn formats_line_with_duration_under_a_minute_pads_seconds() {
        let t = track("Short", "Channel", Some(Duration::from_secs(5)));
        assert_eq!(format_track_line(1, &t), "1. Short — Channel (0:05)");
    }

    #[test]
    fn select_by_number_is_one_indexed() {
        let items = vec![1, 2, 3];
        assert_eq!(select_by_number(&items, 1), Some(1));
        assert_eq!(select_by_number(&items, 3), Some(3));
    }

    #[test]
    fn select_by_number_rejects_zero_and_out_of_range() {
        let items = vec![1, 2, 3];
        assert_eq!(select_by_number(&items, 0), None);
        assert_eq!(select_by_number(&items, 4), None);
    }

    #[test]
    fn format_track_list_truncates_with_note() {
        let tracks: Vec<Track> = (0..7)
            .map(|i| track(&format!("Track {i}"), "Channel", None))
            .collect();
        let out = format_track_list(&tracks, 5);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 6);
        assert_eq!(lines[5], "...and 2 more not shown");
    }

    #[test]
    fn format_track_list_no_truncation_note_when_under_limit() {
        let tracks: Vec<Track> = (0..3)
            .map(|i| track(&format!("Track {i}"), "Channel", None))
            .collect();
        let out = format_track_list(&tracks, 5);
        assert_eq!(out.lines().count(), 3);
    }
}
