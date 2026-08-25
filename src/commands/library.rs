//! `/add_to_queue`, `/playlist_play`: searching `YouTube` and queuing tracks
//! from a search result or a playlist URL/ID.
//!
//! `/add_to_queue` pairs its numbered text with a select-menu picker, so the
//! common path is one click rather than noting a number and re-running a
//! command with it. The numbered form still works too, for anyone who'd
//! rather type it directly. A picker click never needs cross-invocation
//! state — its value is a stable video id, cheap to re-look-up when
//! clicked, rather than a "last results" cache keyed per-user that would
//! need its own expiry/cleanup.

use poise::serenity_prelude as serenity;

use super::{Context, Data, Error};
use crate::voice::QueuedTrack;
use crate::voice::panel::{format_duration, truncate_label};
use crate::youtube::api::Track;

/// Max results shown by `/add_to_queue` when browsing (no `number` given).
const ADD_TO_QUEUE_DISPLAY_LIMIT: usize = 5;

/// Formats one line of a numbered track listing: `N. Title — Channel (mm:ss)`
/// (or `h:mm:ss` past an hour), omitting the duration parens entirely when
/// unknown.
fn format_track_line(index: usize, track: &Track) -> String {
    match track.duration {
        Some(duration) => format!(
            "{index}. {} — {} ({})",
            track.title,
            track.channel,
            format_duration(duration)
        ),
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

/// The voice channel `user_id` is currently in, if any — the
/// component-interaction counterpart to [`author_voice_channel`], which
/// needs a `poise::Context` this handler doesn't have.
fn voice_channel_of(
    ctx: &serenity::Context,
    guild_id: serenity::GuildId,
    user_id: serenity::UserId,
) -> Option<serenity::ChannelId> {
    ctx.cache.guild(guild_id).and_then(|guild| {
        guild
            .voice_states
            .get(&user_id)
            .and_then(|vs| vs.channel_id)
    })
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

/// Routes a `library:*` component interaction to its handler: currently
/// just `library:queue` (queue one track). Any other custom id is ignored.
///
/// Defers immediately (before the `YouTube` lookup, voice join, or track
/// resolution) rather than letting the handler send its own first response:
/// Discord invalidates a component interaction if nothing acknowledges it
/// within 3 seconds, and those steps routinely take longer than that —
/// deferring buys the standard 15-minute follow-up window instead, which
/// the handler then fulfils with an edit.
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
    let serenity::ComponentInteractionDataKind::StringSelect { values } = &component.data.kind
    else {
        return update_picker(ctx, component, "Something went wrong with that selection.").await;
    };
    let (Some(video_id), Some(guild_id)) = (values.first(), component.guild_id) else {
        return update_picker(ctx, component, "Something went wrong with that selection.").await;
    };

    let track = match data.youtube.get_video(video_id).await {
        Ok(track) => track,
        Err(err) => {
            return update_picker(ctx, component, format!("Failed to queue track: {err}")).await;
        }
    };

    if !data.player.is_connected(guild_id) {
        let Some(channel_id) = voice_channel_of(ctx, guild_id, component.user.id) else {
            return update_picker(
                ctx,
                component,
                "Join a voice channel first, or use `/join`.",
            )
            .await;
        };
        if let Err(err) = data.player.join(guild_id, channel_id).await {
            return update_picker(
                ctx,
                component,
                format!("Failed to join voice channel: {err}"),
            )
            .await;
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

/// Extracts the submitted `query` field's value from a modal submission
/// built by [`super::playback::handle_search_button`], or `None` if it was
/// left empty.
fn modal_query(data: &serenity::ModalInteractionData) -> Option<String> {
    data.components.iter().find_map(|row| {
        row.components.iter().find_map(|component| match component {
            serenity::ActionRowComponent::InputText(input) if input.custom_id == "query" => {
                input.value.clone().filter(|v| !v.is_empty())
            }
            _ => None,
        })
    })
}

/// Handles the `/player` panel's Search flow once its modal is submitted:
/// runs the query and replies with a [`track_select_menu`] to queue one —
/// the same shape as `/add_to_queue`'s no-`number` branch, just reached via
/// a modal instead of a slash-command argument.
pub(super) async fn handle_search_modal_submit(
    ctx: &serenity::Context,
    modal: &serenity::ModalInteraction,
    data: &Data,
) -> Result<(), Error> {
    modal.defer_ephemeral(&ctx.http).await?;

    let Some(query) = modal_query(&modal.data) else {
        modal
            .edit_response(
                &ctx.http,
                serenity::EditInteractionResponse::new().content("Enter a search query."),
            )
            .await?;
        return Ok(());
    };

    let results = match data.youtube.search(&query).await {
        Ok(results) => results,
        Err(err) => {
            modal
                .edit_response(
                    &ctx.http,
                    serenity::EditInteractionResponse::new()
                        .content(format!("Search failed: {err}")),
                )
                .await?;
            return Ok(());
        }
    };

    if results.is_empty() {
        modal
            .edit_response(
                &ctx.http,
                serenity::EditInteractionResponse::new()
                    .content(format!("No results found for \"{query}\".")),
            )
            .await?;
        return Ok(());
    }

    let listing = format_track_list(&results, ADD_TO_QUEUE_DISPLAY_LIMIT);
    modal
        .edit_response(
            &ctx.http,
            serenity::EditInteractionResponse::new()
                .content(format!(
                    "Search results for \"{query}\":\n{listing}\n\nSelect one below to queue it."
                ))
                .components(vec![track_select_menu(
                    &results,
                    ADD_TO_QUEUE_DISPLAY_LIMIT,
                )]),
        )
        .await?;
    Ok(())
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
async fn join_and_enqueue_all(
    ctx: Context<'_>,
    label: &str,
    tracks: Vec<Track>,
) -> Result<(), Error> {
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
    ctx.send(poise::CreateReply::default().content(content))
        .await?;

    Ok(())
}

/// Searches `YouTube` for `query` and shows the top 5 matches, or queues one.
///
/// Call with no `number` to see the top 5 matches; call again with the same
/// `query` plus a `number` from that list to queue one.
#[poise::command(slash_command, guild_only)]
pub async fn add_to_queue(
    ctx: Context<'_>,
    #[description = "Search query"] query: String,
    #[description = "Result number from a previous /add_to_queue with this query"] number: Option<
        u8,
    >,
) -> Result<(), Error> {
    let results = match ctx.data().youtube.search(&query).await {
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
                .components(vec![track_select_menu(
                    &results,
                    ADD_TO_QUEUE_DISPLAY_LIMIT,
                )])
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

/// Queues every track in a `YouTube` playlist (URL or bare playlist ID), in order.
#[poise::command(slash_command, guild_only)]
pub async fn playlist_play(
    ctx: Context<'_>,
    #[description = "Playlist URL or ID"] playlist: String,
) -> Result<(), Error> {
    let tracks = match ctx.data().youtube.list_playlist_items(&playlist).await {
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
                .content("That playlist is empty (or couldn't be found).")
                .ephemeral(true),
        )
        .await?;
        return Ok(());
    }

    join_and_enqueue_all(ctx, &playlist, tracks).await
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
        assert_eq!(
            format_track_line(1, &t),
            "1. Some Song — Some Channel (4:13)"
        );
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
    fn formats_line_with_duration_past_an_hour_rolls_over() {
        // Regression: this used to render as the raw minute count (e.g.
        // "1477:03") instead of rolling over into hours, since this
        // function computed mm:ss itself instead of using
        // `panel::format_duration`.
        let t = track("Long Mix", "Channel", Some(Duration::from_secs(88_623)));
        assert_eq!(format_track_line(1, &t), "1. Long Mix — Channel (24:37:03)");
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
}
