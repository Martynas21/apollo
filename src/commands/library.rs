//! `/add_to_queue`, `/playlists`, `/playlist_play`: browsing a linked
//! account's YouTube library and queuing tracks from it.
//!
//! Every listing (search results, playlists, a playlist's tracks) pairs its
//! numbered text with a select-menu/button picker, so the common path is one
//! click rather than noting a number and re-running a command with it. The
//! numbered form still works too, for anyone who'd rather type it directly.
//! Picker clicks never need cross-invocation state — each option's value is
//! a stable id (a video id, a playlist id) that's cheap to re-look-up when
//! clicked, rather than a "last results" cache keyed per-user that would
//! need its own expiry/cleanup.

use poise::serenity_prelude as serenity;

use super::playback::{access_token_error_message, truncate_label};
use super::{Context, Data, Error};
use crate::voice::QueuedTrack;
use crate::youtube::api::{Playlist, Track};
use crate::youtube::oauth::get_valid_access_token;

/// Max results shown by `/add_to_queue` when browsing (no `number` given).
const ADD_TO_QUEUE_DISPLAY_LIMIT: usize = 5;
/// Max results shown by `/playlists` and when browsing one's tracks.
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
        &ctx.data().token_key,
    )
    .await
    {
        Ok(token) => Ok(Some(token)),
        Err(err) => {
            ctx.send(
                poise::CreateReply::default()
                    .content(access_token_error_message(&err))
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

/// Builds a `library:queue` select menu offering up to `limit` (and never
/// more than Discord's 25-option cap) of `tracks`, so a result can be
/// queued with one click instead of noting its number and re-running the
/// command with it. The clicked option's value is the track's video id —
/// enough on its own for [`handle_component`] to re-look-up and queue it,
/// with no per-invocation "last results" state to keep around.
fn track_select_menu(tracks: &[Track], limit: usize) -> serenity::CreateActionRow {
    let options = tracks
        .iter()
        .take(limit.min(25))
        .map(|track| {
            serenity::CreateSelectMenuOption::new(
                truncate_label(&format!("{} — {}", track.title, track.channel)),
                track.video_id.clone(),
            )
        })
        .collect();

    serenity::CreateActionRow::SelectMenu(
        serenity::CreateSelectMenu::new(
            "library:queue",
            serenity::CreateSelectMenuKind::String { options },
        )
        .placeholder("Queue a track..."),
    )
}

/// Builds a `library:browse_playlist` select menu offering up to `limit`
/// (and never more than Discord's 25-option cap) of `playlists`, so one can
/// be browsed with a click instead of noting its number and running a
/// separate browse command.
fn playlist_select_menu(playlists: &[Playlist], limit: usize) -> serenity::CreateActionRow {
    let options = playlists
        .iter()
        .take(limit.min(25))
        .map(|playlist| {
            let label = match playlist.item_count {
                Some(count) => format!("{} ({count} items)", playlist.title),
                None => playlist.title.clone(),
            };
            serenity::CreateSelectMenuOption::new(truncate_label(&label), playlist.id.clone())
        })
        .collect();

    serenity::CreateActionRow::SelectMenu(
        serenity::CreateSelectMenu::new(
            "library:browse_playlist",
            serenity::CreateSelectMenuKind::String { options },
        )
        .placeholder("Browse a playlist..."),
    )
}

/// The voice channel `user_id` is currently in, if any — the
/// component-interaction counterpart to [`author_voice_channel`], which
/// needs a `poise::Context` this handler doesn't have.
fn voice_channel_of(
    ctx: &serenity::Context,
    guild_id: serenity::GuildId,
    user_id: serenity::UserId,
) -> Option<serenity::ChannelId> {
    ctx.cache
        .guild(guild_id)
        .and_then(|guild| guild.voice_states.get(&user_id).and_then(|vs| vs.channel_id))
}

/// Edits the picker message in place with a plain-text result and drops
/// its select menu — used for both the success confirmation and any error
/// along the way, since the picker is single-use per click either way.
///
/// Edits (not a fresh [`CreateInteractionResponse`]) because every caller
/// has already deferred via [`handle_component`] — see its comment for why.
async fn update_picker(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    content: impl Into<String>,
) -> Result<(), Error> {
    component
        .edit_response(
            &ctx.http,
            serenity::EditInteractionResponse::new()
                .content(content.into())
                .components(Vec::new()),
        )
        .await?;
    Ok(())
}

/// Routes a `library:*` component interaction to its handler:
/// `library:queue` (queue one track), `library:browse_playlist` (show a
/// playlist's tracks), or `library:playlist_play:<id>` (queue a whole
/// playlist). Any other custom id is ignored.
///
/// Defers immediately (before any of the handlers' YouTube API calls, voice
/// joins, or track resolution) rather than letting each handler send its
/// own first response: Discord invalidates a component interaction if
/// nothing acknowledges it within 3 seconds, and those steps routinely take
/// longer than that — deferring buys the standard 15-minute follow-up
/// window instead, which each handler then fulfils with an edit.
pub async fn handle_component(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    data: &Data,
) -> Result<(), Error> {
    let custom_id = component.data.custom_id.as_str();
    if custom_id == "library:queue" {
        component.defer(&ctx.http).await?;
        return handle_queue_track(ctx, component, data).await;
    }
    if custom_id == "library:browse_playlist" {
        component.defer(&ctx.http).await?;
        return handle_browse_playlist(ctx, component, data).await;
    }
    if let Some(playlist_id) = custom_id.strip_prefix("library:playlist_play:") {
        component.defer(&ctx.http).await?;
        return handle_playlist_play_button(ctx, component, data, playlist_id).await;
    }
    Ok(())
}

/// Handles a `library:queue` select-menu click from [`track_select_menu`]:
/// re-looks-up the chosen video (the picker only carries a video id, not
/// full track data) and queues it for the clicking user, auto-joining
/// their voice channel first if the bot isn't already connected.
async fn handle_queue_track(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    data: &Data,
) -> Result<(), Error> {
    let serenity::ComponentInteractionDataKind::StringSelect { values } = &component.data.kind else {
        return update_picker(ctx, component, "Something went wrong with that selection.").await;
    };
    let (Some(video_id), Some(guild_id)) = (values.first(), component.guild_id) else {
        return update_picker(ctx, component, "Something went wrong with that selection.").await;
    };

    let token = match get_valid_access_token(
        &data.oauth_client,
        &data.oauth_http,
        &data.db,
        &component.user.id.to_string(),
        &data.token_key,
    )
    .await
    {
        Ok(token) => token,
        Err(err) => return update_picker(ctx, component, access_token_error_message(&err)).await,
    };

    let track = match data.youtube.get_video(&token, video_id).await {
        Ok(track) => track,
        Err(err) => return update_picker(ctx, component, format!("Failed to queue track: {err}")).await,
    };

    if !data.player.is_connected(guild_id) {
        let Some(channel_id) = voice_channel_of(ctx, guild_id, component.user.id) else {
            return update_picker(ctx, component, "Join a voice channel first, or use `/join`.").await;
        };
        if let Err(err) = data.player.join(guild_id, channel_id).await {
            return update_picker(ctx, component, format!("Failed to join voice channel: {err}")).await;
        }
    }

    let title = track.title.clone();
    let channel = track.channel.clone();
    let queued = QueuedTrack {
        track,
        requested_by: component.user.id,
    };

    match data.player.enqueue(guild_id, queued).await {
        Ok(()) => update_picker(ctx, component, format!("Queued: **{title}** — {channel}")).await,
        Err(err) => update_picker(ctx, component, format!("Failed to queue track: {err}")).await,
    }
}

/// Handles a `library:browse_playlist` select-menu click from
/// [`playlist_select_menu`]: looks up the chosen playlist's tracks and
/// replaces the picker message with a track listing, a [`track_select_menu`]
/// to queue one, and a button to queue the whole playlist — folding what
/// used to be a separate `/playlist_browse` command into this one click.
async fn handle_browse_playlist(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    data: &Data,
) -> Result<(), Error> {
    let serenity::ComponentInteractionDataKind::StringSelect { values } = &component.data.kind else {
        return update_picker(ctx, component, "Something went wrong with that selection.").await;
    };
    let Some(playlist_id) = values.first() else {
        return update_picker(ctx, component, "Something went wrong with that selection.").await;
    };

    let token = match get_valid_access_token(
        &data.oauth_client,
        &data.oauth_http,
        &data.db,
        &component.user.id.to_string(),
        &data.token_key,
    )
    .await
    {
        Ok(token) => token,
        Err(err) => return update_picker(ctx, component, access_token_error_message(&err)).await,
    };

    let tracks = match data.youtube.list_playlist_items(&token, playlist_id).await {
        Ok(tracks) => tracks,
        Err(err) => {
            return update_picker(ctx, component, format!("Failed to list playlist items: {err}")).await;
        }
    };

    if tracks.is_empty() {
        return update_picker(ctx, component, "That playlist is empty.").await;
    }

    let listing = format_track_list(&tracks, LIST_DISPLAY_LIMIT);
    let track_count = tracks.len();
    let components = vec![
        track_select_menu(&tracks, LIST_DISPLAY_LIMIT),
        serenity::CreateActionRow::Buttons(vec![
            serenity::CreateButton::new(format!("library:playlist_play:{playlist_id}"))
                .label(format!("Queue all {track_count} tracks"))
                .style(serenity::ButtonStyle::Primary),
        ]),
    ];

    component
        .edit_response(
            &ctx.http,
            serenity::EditInteractionResponse::new()
                .content(format!(
                    "Tracks:\n{listing}\n\nSelect one below to queue it, or queue the whole playlist."
                ))
                .components(components),
        )
        .await?;
    Ok(())
}

/// Handles a `library:playlist_play:<playlist id>` button click from
/// [`handle_browse_playlist`]'s track view: queues every track in that
/// playlist for the clicking user, auto-joining their voice channel first
/// if the bot isn't already connected — the button counterpart to
/// `/playlist_play`.
async fn handle_playlist_play_button(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    data: &Data,
    playlist_id: &str,
) -> Result<(), Error> {
    let Some(guild_id) = component.guild_id else {
        return update_picker(ctx, component, "Something went wrong with that selection.").await;
    };

    let token = match get_valid_access_token(
        &data.oauth_client,
        &data.oauth_http,
        &data.db,
        &component.user.id.to_string(),
        &data.token_key,
    )
    .await
    {
        Ok(token) => token,
        Err(err) => return update_picker(ctx, component, access_token_error_message(&err)).await,
    };

    let tracks = match data.youtube.list_playlist_items(&token, playlist_id).await {
        Ok(tracks) => tracks,
        Err(err) => {
            return update_picker(ctx, component, format!("Failed to list playlist items: {err}")).await;
        }
    };

    if tracks.is_empty() {
        return update_picker(ctx, component, "That playlist is empty.").await;
    }

    if !data.player.is_connected(guild_id) {
        let Some(channel_id) = voice_channel_of(ctx, guild_id, component.user.id) else {
            return update_picker(ctx, component, "Join a voice channel first, or use `/join`.").await;
        };
        if let Err(err) = data.player.join(guild_id, channel_id).await {
            return update_picker(ctx, component, format!("Failed to join voice channel: {err}")).await;
        }
    }

    let total = tracks.len();
    let mut queued_count = 0usize;
    for track in tracks {
        let queued = QueuedTrack {
            track,
            requested_by: component.user.id,
        };
        if data.player.enqueue(guild_id, queued).await.is_ok() {
            queued_count += 1;
        }
    }

    let content = if queued_count == total {
        format!("Queued {queued_count} track(s).")
    } else {
        format!("Queued {queued_count}/{total} track(s) ({} failed).", total - queued_count)
    };
    update_picker(ctx, component, content).await
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

/// Auto-joins if needed, enqueues every track in `tracks` in order, and
/// sends one public confirmation reply summarizing how many were queued.
/// Unlike [`join_and_enqueue`], a per-track enqueue failure doesn't abort
/// the rest — it's tallied and reported alongside the successes, since one
/// bad track (e.g. region-locked) shouldn't block queuing the rest of a
/// playlist.
async fn join_and_enqueue_all(ctx: Context<'_>, label: &str, tracks: Vec<Track>) -> Result<(), Error> {
    let guild_id = ctx.guild_id().expect("guild_only command has a guild id");

    if !ensure_connected(ctx, guild_id).await? {
        return Ok(());
    }

    let total = tracks.len();
    let mut queued_count = 0usize;
    for track in tracks {
        let queued = QueuedTrack {
            track,
            requested_by: ctx.author().id,
        };
        if ctx.data().player.enqueue(guild_id, queued).await.is_ok() {
            queued_count += 1;
        }
    }

    let content = if queued_count == total {
        format!("Queued {queued_count} track(s) from **{label}**.")
    } else {
        format!(
            "Queued {queued_count}/{total} track(s) from **{label}** ({} failed).",
            total - queued_count
        )
    };
    ctx.send(poise::CreateReply::default().content(content)).await?;

    Ok(())
}

/// Searches YouTube for `query` and shows the top 5 matches, or queues one.
///
/// Call with no `number` to see the top 5 matches; call again with the same
/// `query` plus a `number` from that list to queue one.
#[poise::command(slash_command, guild_only)]
pub async fn add_to_queue(
    ctx: Context<'_>,
    #[description = "Search query"] query: String,
    #[description = "Result number from a previous /add_to_queue with this query"] number: Option<u8>,
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

    let Some(number) = number else {
        if results.is_empty() {
            ctx.send(
                poise::CreateReply::default()
                    .content("No results found.")
                    .ephemeral(true),
            )
            .await?;
            return Ok(());
        }

        let listing = format_track_list(&results, ADD_TO_QUEUE_DISPLAY_LIMIT);
        ctx.send(
            poise::CreateReply::default()
                .content(format!(
                    "Search results for \"{query}\":\n{listing}\n\nSelect one below to queue it, \
                     or run `/add_to_queue {query} <number>` to do the same without the menu."
                ))
                .components(vec![track_select_menu(&results, ADD_TO_QUEUE_DISPLAY_LIMIT)])
                .ephemeral(true),
        )
        .await?;
        return Ok(());
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

/// Lists the linked account's playlists. Select one to browse it.
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
                "Your playlists:\n{listing}\n\nSelect one below to browse its tracks, \
                 or run `/playlist_play <number>` to queue the whole thing."
            ))
            .components(vec![playlist_select_menu(&playlists, LIST_DISPLAY_LIMIT)])
            .ephemeral(true),
    )
    .await?;

    Ok(())
}

/// Queues every track in playlist `playlist_number`, in playlist order.
#[poise::command(slash_command, guild_only)]
pub async fn playlist_play(
    ctx: Context<'_>,
    #[description = "Playlist number from /playlists"] playlist_number: u8,
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

    if tracks.is_empty() {
        ctx.send(
            poise::CreateReply::default()
                .content(format!("**{}** is empty.", playlist.title))
                .ephemeral(true),
        )
        .await?;
        return Ok(());
    }

    join_and_enqueue_all(ctx, &playlist.title, tracks).await
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

    // ---- track_select_menu ----

    fn select_options_json(tracks: &[Track], limit: usize) -> serde_json::Value {
        let row = track_select_menu(tracks, limit);
        let json = serde_json::to_value(&row).expect("action row should serialize");
        json["components"][0]["options"].clone()
    }

    #[test]
    fn select_menu_option_values_are_video_ids() {
        let tracks = vec![track("Some Song", "Some Channel", None)];
        let options = select_options_json(&tracks, 5);
        assert_eq!(options[0]["value"], "abc123");
        assert_eq!(options[0]["label"], "Some Song — Some Channel");
    }

    #[test]
    fn select_menu_respects_limit_and_the_25_option_cap() {
        let tracks: Vec<Track> = (0..10)
            .map(|i| track(&format!("Track {i}"), "Channel", None))
            .collect();
        let options = select_options_json(&tracks, 3);
        assert_eq!(options.as_array().unwrap().len(), 3);

        let many_tracks: Vec<Track> = (0..30)
            .map(|i| track(&format!("Track {i}"), "Channel", None))
            .collect();
        let options = select_options_json(&many_tracks, 30);
        assert_eq!(options.as_array().unwrap().len(), 25);
    }

    // ---- playlist_select_menu ----

    fn playlist(id: &str, title: &str, item_count: Option<u32>) -> Playlist {
        Playlist {
            id: id.to_string(),
            title: title.to_string(),
            item_count,
        }
    }

    fn playlist_select_options_json(playlists: &[Playlist], limit: usize) -> serde_json::Value {
        let row = playlist_select_menu(playlists, limit);
        let json = serde_json::to_value(&row).expect("action row should serialize");
        json["components"][0]["options"].clone()
    }

    #[test]
    fn playlist_select_menu_option_values_are_playlist_ids() {
        let playlists = vec![playlist("PL123", "My Mix", Some(12))];
        let options = playlist_select_options_json(&playlists, 5);
        assert_eq!(options[0]["value"], "PL123");
        assert_eq!(options[0]["label"], "My Mix (12 items)");
    }

    #[test]
    fn playlist_select_menu_omits_item_count_when_unknown() {
        let playlists = vec![playlist("PL123", "My Mix", None)];
        let options = playlist_select_options_json(&playlists, 5);
        assert_eq!(options[0]["label"], "My Mix");
    }
}
