//! `/add_to_queue`: searching `YouTube` and queuing tracks
//! from a search result or a playlist URL/ID.
//!
//! `/add_to_queue` pairs its numbered text with a select-menu picker, so the
//! common path is one click rather than noting a number and re-running a
//! command with it. The numbered form still works too, for anyone who'd
//! rather type it directly. A picker click never needs cross-invocation
//! state — its value is a stable video id, cheap to re-look-up when
//! clicked, rather than a "last results" cache keyed per-user that would
//! need its own expiry/cleanup.

use std::time::Duration;

use poise::serenity_prelude as serenity;

use super::{Context, Data, Error};
use crate::db::SavedPlaylist;
use crate::voice::QueuedTrack;
use crate::voice::panel::{format_duration, truncate_label};
use crate::youtube::api::Track;

/// Max results shown by `/add_to_queue` when browsing (no `number` given).
const ADD_TO_QUEUE_DISPLAY_LIMIT: usize = 5;

/// Discord select menus cap out at 25 options.
const PLAYLIST_SELECT_LIMIT: usize = 25;

/// How long the Playlists picker's Import button waits for its URL modal to
/// be submitted before giving up — matches `playback::SEARCH_MODAL_TIMEOUT`.
const PLAYLIST_IMPORT_MODAL_TIMEOUT: Duration = Duration::from_secs(300);

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

/// Deletes `handle`'s ephemeral picker message (search results + a
/// [`track_select_menu`]) after [`super::playback::REPLY_CLEANUP_DELAY`],
/// best-effort. `/add_to_queue`'s browsing reply always lands as a followup
/// (it's already `defer_ephemeral`'d by the time it's sent), so cleanup goes
/// through the interaction's followup-delete endpoint rather than a plain
/// channel-level message delete, which fails for ephemeral messages — see
/// `playback::schedule_cleanup` for the public-reply counterpart, which can
/// use the simpler path.
async fn schedule_picker_cleanup(ctx: Context<'_>, handle: poise::ReplyHandle<'_>) {
    let Context::Application(app_ctx) = ctx else {
        return;
    };
    let interaction = app_ctx.interaction.clone();
    let http = app_ctx.serenity_context.http.clone();
    let Ok(message) = handle.into_message().await else {
        return;
    };
    let message_id = message.id;
    tokio::spawn(async move {
        tokio::time::sleep(super::playback::REPLY_CLEANUP_DELAY).await;
        let _ = interaction.delete_followup(http, message_id).await;
    });
}

/// Deletes `modal`'s picker message after
/// [`super::playback::REPLY_CLEANUP_DELAY`], best-effort. Unlike
/// [`schedule_picker_cleanup`], [`handle_search_modal_submit`] always edits
/// the original deferred response rather than sending a followup, so cleanup
/// goes through `delete_response` instead.
fn schedule_modal_picker_cleanup(ctx: &serenity::Context, modal: &serenity::ModalInteraction) {
    let modal = modal.clone();
    let http = ctx.http.clone();
    tokio::spawn(async move {
        tokio::time::sleep(super::playback::REPLY_CLEANUP_DELAY).await;
        let _ = modal.delete_response(http).await;
    });
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
/// `library:queue` (queue one search result), `library:playlist_select`
/// (open a saved playlist's detail view), `library:playlist_play:<id>`
/// (queue it), `library:playlist_refresh:<id>` (re-pull it from `YouTube`),
/// `library:playlist_remove:<id>` (show a remove confirmation),
/// `library:playlist_remove_confirm:<id>` (actually remove it),
/// `library:playlist_view:<id>` (back out of the remove confirmation to the
/// detail view), `library:playlist_back` (return to the saved-playlists
/// list), or `library:playlist_import` (open the import URL modal). Any
/// other custom id is ignored.
///
/// Every id but `playlist_import` defers immediately (before the `YouTube`
/// lookup, voice join, or track resolution) rather than letting the handler
/// send its own first response: Discord invalidates a component interaction
/// if nothing acknowledges it within 3 seconds, and those steps routinely
/// take longer than that — deferring buys the standard 15-minute follow-up
/// window instead, which the handler then fulfils with an edit.
/// `playlist_import` can't defer first — its first response has to be the
/// modal itself.
pub async fn handle_component(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    data: &Data,
) -> Result<(), Error> {
    let custom_id = component.data.custom_id.as_str();

    if custom_id == "library:playlist_import" {
        return handle_playlist_import_button(ctx, component, data).await;
    }

    if custom_id == "library:queue" {
        component.defer(&ctx.http).await?;
        return handle_queue_track(ctx, component, data).await;
    }
    if custom_id == "library:playlist_select" {
        component.defer(&ctx.http).await?;
        return handle_playlist_select(ctx, component, data).await;
    }
    if custom_id == "library:playlist_back" {
        component.defer(&ctx.http).await?;
        return handle_playlist_back(ctx, component, data).await;
    }
    if let Some(id) = custom_id.strip_prefix("library:playlist_play:") {
        component.defer(&ctx.http).await?;
        return handle_playlist_play_button(ctx, component, data, id).await;
    }
    if let Some(id) = custom_id.strip_prefix("library:playlist_refresh:") {
        component.defer(&ctx.http).await?;
        return handle_playlist_refresh_button(ctx, component, data, id).await;
    }
    if let Some(id) = custom_id.strip_prefix("library:playlist_remove_confirm:") {
        component.defer(&ctx.http).await?;
        return handle_playlist_remove_confirm_button(ctx, component, data, id).await;
    }
    if let Some(id) = custom_id.strip_prefix("library:playlist_remove:") {
        component.defer(&ctx.http).await?;
        return handle_playlist_remove_button(ctx, component, data, id).await;
    }
    if let Some(id) = custom_id.strip_prefix("library:playlist_view:") {
        component.defer(&ctx.http).await?;
        return handle_playlist_view_button(ctx, component, data, id).await;
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

/// Looks up one of a guild's saved playlists by the raw id string carried in
/// a component's value/custom-id suffix (a select-menu option value for
/// [`handle_playlist_select`], or the `<id>` embedded in a
/// `library:playlist_play:<id>`/`library:playlist_refresh:<id>` custom id).
/// A user-displayable error message on any failure — bad id, no such
/// playlist (deleted or wrong guild), or a database error.
async fn load_owned_playlist(
    data: &Data,
    guild_id: serenity::GuildId,
    id_str: &str,
) -> Result<SavedPlaylist, String> {
    let id: i64 = id_str
        .parse()
        .map_err(|_| "Something went wrong with that selection.".to_string())?;

    match crate::db::get_guild_playlist(&data.db, &guild_id.to_string(), id).await {
        Ok(Some(playlist)) => Ok(playlist),
        Ok(None) => Err("That playlist no longer exists.".to_string()),
        Err(err) => Err(format!("Failed to load playlist: {err}")),
    }
}

/// Re-pulls a saved playlist's tracks from `YouTube` and overwrites its
/// cache — the shared core of both the panel's per-playlist Refresh button
/// and the self-heal fetch [`show_playlist_details`] does for a legacy,
/// never-cached row. A user-displayable error message on failure.
async fn fetch_and_cache_playlist_tracks(
    data: &Data,
    playlist: &SavedPlaylist,
) -> Result<Vec<Track>, String> {
    let listing = data
        .youtube
        .list_playlist_items(&playlist.url)
        .await
        .map_err(|err| format!("Failed to load that playlist: {err}"))?;

    crate::db::replace_playlist_tracks(&data.db, playlist.id, &listing.tracks)
        .await
        .map_err(|err| format!("Failed to cache playlist tracks: {err}"))?;

    Ok(listing.tracks)
}

/// Formats a playlist's `cached_at` (unix seconds) as a short relative note
/// for the detail view — `"just now"`, `"5m ago"`, `"3h ago"`, `"2d ago"` —
/// or `"never"` if it's never been cached.
fn format_relative_time(cached_at: Option<i64>) -> String {
    let Some(cached_at) = cached_at else {
        return "never".to_string();
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(cached_at, |d| d.as_secs() as i64);
    let elapsed = (now - cached_at).max(0);

    if elapsed < 60 {
        "just now".to_string()
    } else if elapsed < 3_600 {
        format!("{}m ago", elapsed / 60)
    } else if elapsed < 86_400 {
        format!("{}h ago", elapsed / 3_600)
    } else {
        format!("{}d ago", elapsed / 86_400)
    }
}

/// Renders a saved playlist's detail view: name, cached track count and
/// freshness, and Play/Refresh/Back/Remove buttons — shown after picking one
/// from [`playlist_select_menu`], after a Refresh, and right after
/// importing.
fn render_playlist_details(
    playlist: &SavedPlaylist,
    track_count: usize,
) -> (String, Vec<serenity::CreateActionRow>) {
    let content = format!(
        "**{}**\n{track_count} track(s) · cached {}",
        playlist.name,
        format_relative_time(playlist.cached_at)
    );

    let buttons = serenity::CreateActionRow::Buttons(vec![
        serenity::CreateButton::new(format!("library:playlist_play:{}", playlist.id))
            .label("▶ Play All")
            .style(serenity::ButtonStyle::Primary)
            .disabled(track_count == 0),
        serenity::CreateButton::new(format!("library:playlist_refresh:{}", playlist.id))
            .label("🔄 Refresh")
            .style(serenity::ButtonStyle::Secondary),
        serenity::CreateButton::new("library:playlist_back")
            .label("⬅ Back")
            .style(serenity::ButtonStyle::Secondary),
        // Last and styled `Danger` so it reads as distinct/deliberate from
        // the other three — and still gated behind its own confirmation
        // screen (`render_playlist_remove_confirm`) rather than deleting on
        // a single misclick.
        serenity::CreateButton::new(format!("library:playlist_remove:{}", playlist.id))
            .label("🗑 Remove")
            .style(serenity::ButtonStyle::Danger),
    ]);

    (content, vec![buttons])
}

/// Renders the confirmation screen for removing a saved playlist — shown by
/// [`handle_playlist_remove_button`] before [`handle_playlist_remove_confirm_button`]
/// actually deletes anything, so a misclick on the detail view's Remove
/// button can't destroy a saved playlist outright.
fn render_playlist_remove_confirm(
    playlist: &SavedPlaylist,
) -> (String, Vec<serenity::CreateActionRow>) {
    let content = format!(
        "Remove **{}**? You can always re-import it later from its URL.",
        playlist.name
    );

    let buttons = serenity::CreateActionRow::Buttons(vec![
        serenity::CreateButton::new(format!("library:playlist_remove_confirm:{}", playlist.id))
            .label("🗑 Confirm Remove")
            .style(serenity::ButtonStyle::Danger),
        serenity::CreateButton::new(format!("library:playlist_view:{}", playlist.id))
            .label("⬅ Cancel")
            .style(serenity::ButtonStyle::Secondary),
    ]);

    (content, vec![buttons])
}

/// Edits the picker in place to `playlist`'s detail view. Self-heals a
/// never-cached row (`cached_at` is `None` — a playlist saved before track
/// caching existed) by fetching it live first, rather than showing an empty,
/// confusing screen the user would have to know to hit Refresh on.
async fn show_playlist_details(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    data: &Data,
    guild_id: serenity::GuildId,
    playlist: SavedPlaylist,
) -> Result<(), Error> {
    if playlist.cached_at.is_none()
        && let Err(message) = fetch_and_cache_playlist_tracks(data, &playlist).await
    {
        return update_picker(ctx, component, message).await;
    }

    // Re-read rather than reusing `playlist`: the fetch above (if it ran)
    // stamped a fresh `cached_at` this copy doesn't know about.
    let playlist = match crate::db::get_guild_playlist(&data.db, &guild_id.to_string(), playlist.id)
        .await
    {
        Ok(Some(playlist)) => playlist,
        Ok(None) => return update_picker(ctx, component, "That playlist no longer exists.").await,
        Err(err) => {
            return update_picker(ctx, component, format!("Failed to load playlist: {err}")).await;
        }
    };
    let track_count = match crate::db::get_playlist_tracks(&data.db, playlist.id).await {
        Ok(tracks) => tracks.len(),
        Err(err) => {
            return update_picker(ctx, component, format!("Failed to load playlist: {err}")).await;
        }
    };

    let (content, components) = render_playlist_details(&playlist, track_count);
    component
        .edit_response(
            &ctx.http,
            serenity::EditInteractionResponse::new()
                .content(content)
                .components(components),
        )
        .await?;
    Ok(())
}

/// Handles a `library:playlist_select` select-menu click from
/// [`playlist_select_menu`]: opens the chosen saved playlist's detail view.
async fn handle_playlist_select(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    data: &Data,
) -> Result<(), Error> {
    let serenity::ComponentInteractionDataKind::StringSelect { values } = &component.data.kind
    else {
        return update_picker(ctx, component, "Something went wrong with that selection.").await;
    };
    let (Some(id_str), Some(guild_id)) = (values.first(), component.guild_id) else {
        return update_picker(ctx, component, "Something went wrong with that selection.").await;
    };

    match load_owned_playlist(data, guild_id, id_str).await {
        Ok(playlist) => show_playlist_details(ctx, component, data, guild_id, playlist).await,
        Err(message) => update_picker(ctx, component, message).await,
    }
}

/// Handles a detail view's `library:playlist_refresh:<id>` button: force
/// re-pulls the playlist from `YouTube` (unlike [`show_playlist_details`]'s
/// self-heal, this always re-fetches, even if already cached), then
/// re-renders the detail view with the updated count and freshness.
async fn handle_playlist_refresh_button(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    data: &Data,
    id_str: &str,
) -> Result<(), Error> {
    let Some(guild_id) = component.guild_id else {
        return Ok(());
    };

    let playlist = match load_owned_playlist(data, guild_id, id_str).await {
        Ok(playlist) => playlist,
        Err(message) => return update_picker(ctx, component, message).await,
    };

    if let Err(message) = fetch_and_cache_playlist_tracks(data, &playlist).await {
        return update_picker(ctx, component, message).await;
    }

    show_playlist_details(ctx, component, data, guild_id, playlist).await
}

/// Handles a detail view's `library:playlist_play:<id>` button: queues the
/// playlist's cached tracks in order for the clicking user, auto-joining
/// their voice channel first if the bot isn't already connected. Falls back
/// to a live fetch (populating the cache) if it's somehow still empty at
/// this point, rather than reporting a spurious "empty playlist".
async fn handle_playlist_play_button(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    data: &Data,
    id_str: &str,
) -> Result<(), Error> {
    let Some(guild_id) = component.guild_id else {
        return Ok(());
    };

    let playlist = match load_owned_playlist(data, guild_id, id_str).await {
        Ok(playlist) => playlist,
        Err(message) => return update_picker(ctx, component, message).await,
    };

    let tracks = match crate::db::get_playlist_tracks(&data.db, playlist.id).await {
        Ok(tracks) if !tracks.is_empty() => tracks,
        Ok(_) => match fetch_and_cache_playlist_tracks(data, &playlist).await {
            Ok(tracks) => tracks,
            Err(message) => return update_picker(ctx, component, message).await,
        },
        Err(err) => {
            return update_picker(ctx, component, format!("Failed to load playlist: {err}")).await;
        }
    };

    if tracks.is_empty() {
        return update_picker(
            ctx,
            component,
            "That playlist is empty (or couldn't be found).",
        )
        .await;
    }

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

    let total = tracks.len();
    let queued: Vec<QueuedTrack> = tracks
        .into_iter()
        .map(|track| QueuedTrack {
            track,
            requested_by: component.user.id,
        })
        .collect();
    let queued_count = match data.player.enqueue_many(guild_id, queued).await {
        Ok(count) => count,
        Err(err) => {
            return update_picker(ctx, component, format!("Failed to queue tracks: {err}")).await;
        }
    };

    let content = if queued_count == total {
        format!("Queued {queued_count} track(s) from **{}**.", playlist.name)
    } else {
        format!(
            "Queued {queued_count}/{total} track(s) from **{}** ({} failed).",
            playlist.name,
            total - queued_count
        )
    };
    update_picker(ctx, component, content).await
}

/// Handles a detail view's `library:playlist_view:<id>` button: re-opens
/// that playlist's detail view — currently only reachable as the remove
/// confirmation's Cancel button, but handled generically the same way
/// [`handle_playlist_select`] is.
async fn handle_playlist_view_button(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    data: &Data,
    id_str: &str,
) -> Result<(), Error> {
    let Some(guild_id) = component.guild_id else {
        return Ok(());
    };

    match load_owned_playlist(data, guild_id, id_str).await {
        Ok(playlist) => show_playlist_details(ctx, component, data, guild_id, playlist).await,
        Err(message) => update_picker(ctx, component, message).await,
    }
}

/// Handles a detail view's `library:playlist_remove:<id>` button: shows
/// [`render_playlist_remove_confirm`] rather than deleting immediately.
async fn handle_playlist_remove_button(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    data: &Data,
    id_str: &str,
) -> Result<(), Error> {
    let Some(guild_id) = component.guild_id else {
        return Ok(());
    };

    let playlist = match load_owned_playlist(data, guild_id, id_str).await {
        Ok(playlist) => playlist,
        Err(message) => return update_picker(ctx, component, message).await,
    };

    let (content, components) = render_playlist_remove_confirm(&playlist);
    component
        .edit_response(
            &ctx.http,
            serenity::EditInteractionResponse::new()
                .content(content)
                .components(components),
        )
        .await?;
    Ok(())
}

/// Handles the remove confirmation's `library:playlist_remove_confirm:<id>`
/// button: actually deletes the playlist (and its cached tracks) and lands
/// back on the saved-playlists list, prefixed with a "Removed" note.
async fn handle_playlist_remove_confirm_button(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    data: &Data,
    id_str: &str,
) -> Result<(), Error> {
    let Some(guild_id) = component.guild_id else {
        return Ok(());
    };

    let playlist = match load_owned_playlist(data, guild_id, id_str).await {
        Ok(playlist) => playlist,
        Err(message) => return update_picker(ctx, component, message).await,
    };

    if let Err(err) =
        crate::db::delete_guild_playlist(&data.db, &guild_id.to_string(), playlist.id).await
    {
        return update_picker(ctx, component, format!("Failed to remove playlist: {err}")).await;
    }

    let removed_note = format!("Removed **{}**.\n\n", playlist.name);
    let (content, components) = match render_playlists_list(data, guild_id).await {
        Ok((content, components)) => (format!("{removed_note}{content}"), components),
        Err(message) => (
            format!("{removed_note}{message}"),
            vec![import_button_row()],
        ),
    };

    component
        .edit_response(
            &ctx.http,
            serenity::EditInteractionResponse::new()
                .content(content)
                .components(components),
        )
        .await?;
    Ok(())
}

/// Extracts a submitted modal field's value by its input custom id, trimmed,
/// or `None` if it was left empty/whitespace-only (or isn't present at all)
/// — matches `playback::parse_volume_input`'s trim-before-checking.
fn modal_field(data: &serenity::ModalInteractionData, custom_id: &str) -> Option<String> {
    data.components.iter().find_map(|row| {
        row.components.iter().find_map(|component| match component {
            serenity::ActionRowComponent::InputText(input) if input.custom_id == custom_id => input
                .value
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string),
            _ => None,
        })
    })
}

/// Extracts the submitted `query` field's value from a modal submission
/// built by [`super::playback::handle_search_button`], or `None` if it was
/// left empty.
fn modal_query(data: &serenity::ModalInteractionData) -> Option<String> {
    modal_field(data, "query")
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
    schedule_modal_picker_cleanup(ctx, modal);
    Ok(())
}

/// Builds a `library:playlist_play` select menu offering up to
/// [`PLAYLIST_SELECT_LIMIT`] of a guild's saved playlists — the playlist
/// counterpart to [`track_select_menu`]. The clicked option's value is the
/// playlist's row id, re-looked-up by [`handle_play_saved_playlist`].
fn playlist_select_menu(playlists: &[SavedPlaylist]) -> serenity::CreateActionRow {
    let options = playlists
        .iter()
        .take(PLAYLIST_SELECT_LIMIT)
        .map(|playlist| {
            serenity::CreateSelectMenuOption::new(
                truncate_label(&playlist.name),
                playlist.id.to_string(),
            )
        })
        .collect();

    serenity::CreateActionRow::SelectMenu(
        serenity::CreateSelectMenu::new(
            "library:playlist_select",
            serenity::CreateSelectMenuKind::String { options },
        )
        .placeholder("Open a saved playlist..."),
    )
}

/// The Import button row, appended under the playlist select menu (or shown
/// alone when there are no saved playlists yet) — its own function since
/// both [`render_playlists_list`] and its error fallback need it.
fn import_button_row() -> serenity::CreateActionRow {
    serenity::CreateActionRow::Buttons(vec![
        serenity::CreateButton::new("library:playlist_import")
            .label("➕ Import playlist")
            .style(serenity::ButtonStyle::Secondary),
    ])
}

/// Renders the saved-playlists list view: a [`playlist_select_menu`] (if
/// there are any) plus [`import_button_row`] — shared by
/// [`handle_playlists_button`] (the panel's initial entry point, via a fresh
/// response) and [`handle_playlist_back`] (returning from a detail view, via
/// an edit). A user-displayable error message on a database failure.
async fn render_playlists_list(
    data: &Data,
    guild_id: serenity::GuildId,
) -> Result<(String, Vec<serenity::CreateActionRow>), String> {
    let playlists = crate::db::list_guild_playlists(&data.db, &guild_id.to_string())
        .await
        .map_err(|err| format!("Failed to load saved playlists: {err}"))?;

    let content = if playlists.is_empty() {
        "No saved playlists yet — import one below.".to_string()
    } else {
        "Saved playlists for this server:".to_string()
    };

    let mut components = Vec::new();
    if !playlists.is_empty() {
        components.push(playlist_select_menu(&playlists));
    }
    components.push(import_button_row());

    Ok((content, components))
}

/// Handles the `/player` panel's `player:playlists` button: shows this
/// guild's saved playlists as a picker (if any), alongside an Import button
/// to save a new one via [`handle_playlist_import_button`].
pub(super) async fn handle_playlists_button(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    data: &Data,
) -> Result<(), Error> {
    let Some(guild_id) = component.guild_id else {
        return Ok(());
    };

    let (content, components) = match render_playlists_list(data, guild_id).await {
        Ok(rendered) => rendered,
        Err(message) => (message, vec![import_button_row()]),
    };

    component
        .create_response(
            &ctx.http,
            serenity::CreateInteractionResponse::Message(
                serenity::CreateInteractionResponseMessage::new()
                    .content(content)
                    .components(components)
                    .ephemeral(true),
            ),
        )
        .await?;
    Ok(())
}

/// Handles a detail view's `library:playlist_back` button: returns to the
/// saved-playlists list, same content [`handle_playlists_button`] shows,
/// just via an edit instead of a fresh response.
async fn handle_playlist_back(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    data: &Data,
) -> Result<(), Error> {
    let Some(guild_id) = component.guild_id else {
        return Ok(());
    };

    let (content, components) = match render_playlists_list(data, guild_id).await {
        Ok(rendered) => rendered,
        Err(message) => (message, vec![import_button_row()]),
    };

    component
        .edit_response(
            &ctx.http,
            serenity::EditInteractionResponse::new()
                .content(content)
                .components(components),
        )
        .await?;
    Ok(())
}

/// Handles the Playlists picker's `library:playlist_import` button: shows a
/// two-field modal (URL, optional name), waits for it to be submitted, then
/// hands it off to [`handle_playlist_import_modal_submit`].
///
/// The modal's custom id is namespaced with the clicking interaction's own
/// id, same reasoning as `playback::handle_search_button`'s search modal.
async fn handle_playlist_import_button(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    data: &Data,
) -> Result<(), Error> {
    let Some(guild_id) = component.guild_id else {
        return Ok(());
    };
    let modal_custom_id = format!("library:playlist_import_modal:{}", component.id);

    component
        .create_response(
            &ctx.http,
            serenity::CreateInteractionResponse::Modal(
                serenity::CreateModal::new(modal_custom_id.clone(), "Import Playlist").components(
                    vec![
                        serenity::CreateActionRow::InputText(
                            serenity::CreateInputText::new(
                                serenity::InputTextStyle::Short,
                                "Playlist URL",
                                "url",
                            )
                            .placeholder("https://www.youtube.com/playlist?list=..."),
                        ),
                        serenity::CreateActionRow::InputText(
                            serenity::CreateInputText::new(
                                serenity::InputTextStyle::Short,
                                "Name (optional)",
                                "name",
                            )
                            .required(false)
                            .placeholder("Defaults to the playlist's YouTube title"),
                        ),
                    ],
                ),
            ),
        )
        .await?;

    let user_id = component.user.id;
    let Some(modal) = serenity::ModalInteractionCollector::new(&ctx.shard)
        .filter(move |submission| {
            submission.data.custom_id == modal_custom_id && submission.user.id == user_id
        })
        .timeout(PLAYLIST_IMPORT_MODAL_TIMEOUT)
        .await
    else {
        // Nobody submitted before the timeout — nothing to clean up, the
        // modal just closes itself client-side.
        return Ok(());
    };

    handle_playlist_import_modal_submit(ctx, &modal, guild_id, data).await
}

/// Handles the Playlists picker's import modal once submitted: validates the
/// URL resolves to a non-empty playlist, then saves it for this guild.
async fn handle_playlist_import_modal_submit(
    ctx: &serenity::Context,
    modal: &serenity::ModalInteraction,
    guild_id: serenity::GuildId,
    data: &Data,
) -> Result<(), Error> {
    modal.defer_ephemeral(&ctx.http).await?;

    let Some(url) = modal_field(&modal.data, "url") else {
        modal
            .edit_response(
                &ctx.http,
                serenity::EditInteractionResponse::new().content("Enter a playlist URL."),
            )
            .await?;
        return Ok(());
    };

    let listing = match data.youtube.list_playlist_items(&url).await {
        Ok(listing) => listing,
        Err(err) => {
            modal
                .edit_response(
                    &ctx.http,
                    serenity::EditInteractionResponse::new()
                        .content(format!("Failed to load that playlist: {err}")),
                )
                .await?;
            return Ok(());
        }
    };

    if listing.tracks.is_empty() {
        modal
            .edit_response(
                &ctx.http,
                serenity::EditInteractionResponse::new()
                    .content("That playlist is empty (or couldn't be found)."),
            )
            .await?;
        return Ok(());
    }

    // Prefer what the user typed; otherwise fall back to the playlist's own
    // YouTube title (reliably present on a flat-playlist listing) rather
    // than the raw URL, so a blank-name import still shows something
    // readable in the saved-playlists list.
    let name = modal_field(&modal.data, "name")
        .or_else(|| listing.title.clone())
        .unwrap_or_else(|| url.clone());
    let tracks = listing.tracks;

    let id = match crate::db::save_guild_playlist(
        &data.db,
        &guild_id.to_string(),
        &name,
        &url,
        &modal.user.id.to_string(),
    )
    .await
    {
        Ok(id) => id,
        Err(err) => {
            modal
                .edit_response(
                    &ctx.http,
                    serenity::EditInteractionResponse::new()
                        .content(format!("Failed to save playlist: {err}")),
                )
                .await?;
            return Ok(());
        }
    };

    let track_count = tracks.len();
    // Best-effort: the playlist itself is already saved either way — a
    // caching hiccup here just means the detail view below shows "cached
    // never" and self-heals on next view (see `show_playlist_details`)
    // rather than blocking the import on it.
    if let Err(err) = crate::db::replace_playlist_tracks(&data.db, id, &tracks).await {
        tracing::warn!(%err, playlist_id = id, "failed to cache playlist tracks after import");
    }

    let playlist = match crate::db::get_guild_playlist(&data.db, &guild_id.to_string(), id).await {
        Ok(Some(playlist)) => playlist,
        _ => SavedPlaylist {
            id,
            name,
            url,
            cached_at: None,
        },
    };

    let (details, components) = render_playlist_details(&playlist, track_count);
    modal
        .edit_response(
            &ctx.http,
            serenity::EditInteractionResponse::new()
                .content(format!("Saved!\n\n{details}"))
                .components(components),
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

    super::playback::reply_public(ctx, format!("Queued: **{title}** — {channel}")).await
}

/// Auto-joins if needed, enqueues every track in `tracks` in order, and
/// sends one public confirmation reply summarizing how many were queued.
/// Unlike [`join_and_enqueue`], a per-track enqueue failure doesn't abort
/// the rest — it's tallied and reported alongside the successes, since one
/// bad track (e.g. region-locked) shouldn't block queuing the rest of a
/// playlist.
pub(super) async fn join_and_enqueue_all(
    ctx: Context<'_>,
    label: &str,
    tracks: Vec<Track>,
) -> Result<(), Error> {
    let guild_id = ctx.guild_id().expect("guild_only command has a guild id");

    if !ensure_connected(ctx, guild_id).await? {
        return Ok(());
    }

    let total = tracks.len();
    let queued: Vec<QueuedTrack> = tracks
        .into_iter()
        .map(|track| QueuedTrack {
            track,
            requested_by: ctx.author().id,
        })
        .collect();
    let queued_count = match ctx.data().player.enqueue_many(guild_id, queued).await {
        Ok(count) => count,
        Err(err) => {
            ctx.send(
                poise::CreateReply::default()
                    .content(format!("Failed to queue tracks: {err}"))
                    .ephemeral(true),
            )
            .await?;
            return Ok(());
        }
    };

    let content = if queued_count == total {
        format!("Queued {queued_count} track(s) from **{label}**.")
    } else {
        format!(
            "Queued {queued_count}/{total} track(s) from **{label}** ({} failed).",
            total - queued_count
        )
    };
    super::playback::reply_public(ctx, content).await
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
    // `youtube.search` shells out to yt-dlp and can easily exceed Discord's
    // 3-second ack deadline (cold-start especially) — deferred before it so
    // a slow response doesn't drop the interaction. Ephemeral to match the
    // no-`number` browsing branch below, the common entry point into this
    // command; the `number`-given success path replies publicly regardless
    // (see `join_and_enqueue`), which works fine as its own followup.
    ctx.defer_ephemeral().await?;

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
        let handle = ctx
            .send(
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
        schedule_picker_cleanup(ctx, handle).await;
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

    // ---- format_relative_time ----

    fn seconds_ago(secs: i64) -> i64 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        now - secs
    }

    #[test]
    fn relative_time_never_when_uncached() {
        assert_eq!(format_relative_time(None), "never");
    }

    #[test]
    fn relative_time_just_now_under_a_minute() {
        assert_eq!(format_relative_time(Some(seconds_ago(30))), "just now");
    }

    #[test]
    fn relative_time_minutes_ago() {
        assert_eq!(format_relative_time(Some(seconds_ago(300))), "5m ago");
    }

    #[test]
    fn relative_time_hours_ago() {
        assert_eq!(format_relative_time(Some(seconds_ago(7_200))), "2h ago");
    }

    #[test]
    fn relative_time_days_ago() {
        assert_eq!(format_relative_time(Some(seconds_ago(259_200))), "3d ago");
    }

    // ---- playlist_select_menu ----

    fn saved_playlist(id: i64, name: &str) -> SavedPlaylist {
        SavedPlaylist {
            id,
            name: name.to_string(),
            url: "https://example.com/list=abc".to_string(),
            cached_at: None,
        }
    }

    #[test]
    fn playlist_select_menu_uses_playlist_select_custom_id_and_ids_as_values() {
        let playlists = vec![saved_playlist(7, "Chill Mix")];
        let row = playlist_select_menu(&playlists);
        let json = serde_json::to_value(&row).expect("action row should serialize");
        assert_eq!(
            json["components"][0]["custom_id"],
            "library:playlist_select"
        );
        assert_eq!(json["components"][0]["options"][0]["value"], "7");
        assert_eq!(json["components"][0]["options"][0]["label"], "Chill Mix");
    }

    // ---- render_playlist_details ----

    #[test]
    fn playlist_details_buttons_carry_the_playlist_id() {
        let playlist = saved_playlist(42, "Chill Mix");
        let (_, components) = render_playlist_details(&playlist, 10);
        let json = serde_json::to_value(&components).expect("components should serialize");
        let buttons = json[0]["components"].as_array().unwrap();
        assert_eq!(buttons[0]["custom_id"], "library:playlist_play:42");
        assert_eq!(buttons[1]["custom_id"], "library:playlist_refresh:42");
        assert_eq!(buttons[2]["custom_id"], "library:playlist_back");
    }

    #[test]
    fn playlist_details_play_button_disabled_when_no_tracks() {
        let playlist = saved_playlist(1, "Empty");
        let (_, components) = render_playlist_details(&playlist, 0);
        let json = serde_json::to_value(&components).expect("components should serialize");
        assert_eq!(json[0]["components"][0]["disabled"], true);
    }

    #[test]
    fn playlist_details_play_button_enabled_when_tracks_present() {
        let playlist = saved_playlist(1, "Has Tracks");
        let (_, components) = render_playlist_details(&playlist, 5);
        let json = serde_json::to_value(&components).expect("components should serialize");
        assert_eq!(json[0]["components"][0]["disabled"], false);
    }

    #[test]
    fn playlist_details_content_includes_name_count_and_freshness() {
        let playlist = saved_playlist(1, "Chill Mix");
        let (content, _) = render_playlist_details(&playlist, 12);
        assert!(content.contains("Chill Mix"));
        assert!(content.contains("12 track(s)"));
        assert!(content.contains("never"));
    }

    #[test]
    fn playlist_details_has_a_remove_button_for_the_playlist() {
        let playlist = saved_playlist(42, "Chill Mix");
        let (_, components) = render_playlist_details(&playlist, 10);
        let json = serde_json::to_value(&components).expect("components should serialize");
        let buttons = json[0]["components"].as_array().unwrap();
        assert!(
            buttons
                .iter()
                .any(|b| b["custom_id"] == "library:playlist_remove:42")
        );
    }

    // ---- render_playlist_remove_confirm ----

    #[test]
    fn remove_confirm_content_names_the_playlist() {
        let playlist = saved_playlist(1, "Chill Mix");
        let (content, _) = render_playlist_remove_confirm(&playlist);
        assert!(content.contains("Chill Mix"));
    }

    #[test]
    fn remove_confirm_buttons_carry_the_playlist_id() {
        let playlist = saved_playlist(42, "Chill Mix");
        let (_, components) = render_playlist_remove_confirm(&playlist);
        let json = serde_json::to_value(&components).expect("components should serialize");
        let buttons = json[0]["components"].as_array().unwrap();
        assert_eq!(
            buttons[0]["custom_id"],
            "library:playlist_remove_confirm:42"
        );
        assert_eq!(buttons[1]["custom_id"], "library:playlist_view:42");
    }

    // ---- modal_field ----

    /// Builds a single-field submitted modal, round-tripped through JSON the
    /// same way `panel.rs`'s tests round-trip `CreateEmbed` — `serenity`'s
    /// receive-side modal types implement `Deserialize` but have no public
    /// constructor.
    fn modal_data_with_field(custom_id: &str, value: &str) -> serenity::ModalInteractionData {
        serde_json::from_value(serde_json::json!({
            "custom_id": "test_modal",
            "components": [{
                "type": 1,
                "components": [{
                    "type": 4,
                    "custom_id": custom_id,
                    "value": value
                }]
            }]
        }))
        .expect("modal data should deserialize")
    }

    #[test]
    fn modal_field_trims_surrounding_whitespace() {
        let data = modal_data_with_field("url", "  https://example.com  ");
        assert_eq!(
            modal_field(&data, "url"),
            Some("https://example.com".to_string())
        );
    }

    #[test]
    fn modal_field_treats_whitespace_only_value_as_empty() {
        let data = modal_data_with_field("url", "   ");
        assert_eq!(modal_field(&data, "url"), None);
    }

    #[test]
    fn modal_field_treats_truly_empty_value_as_empty() {
        let data = modal_data_with_field("url", "");
        assert_eq!(modal_field(&data, "url"), None);
    }

    #[test]
    fn modal_field_none_when_custom_id_not_present() {
        let data = modal_data_with_field("url", "https://example.com");
        assert_eq!(modal_field(&data, "name"), None);
    }
}
