use std::time::Duration;

use poise::serenity_prelude as serenity;

use super::{Context, Data, Error};
use crate::db::SavedPlaylist;
use crate::voice::QueuedTrack;
use crate::voice::panel::{format_duration, truncate_label};
use crate::youtube::api::{PlaylistListing, Track, YouTubeApiError};

const ADD_TO_QUEUE_DISPLAY_LIMIT: usize = 5;

const PLAYLIST_SELECT_LIMIT: usize = 25;

const PLAYLIST_IMPORT_MODAL_TIMEOUT: Duration = Duration::from_secs(300);

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

fn schedule_modal_picker_cleanup(ctx: &serenity::Context, modal: &serenity::ModalInteraction) {
    let modal = modal.clone();
    let http = ctx.http.clone();
    tokio::spawn(async move {
        tokio::time::sleep(super::playback::REPLY_CLEANUP_DELAY).await;
        let _ = modal.delete_response(http).await;
    });
}

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
        serenity::CreateButton::new(format!("library:playlist_remove:{}", playlist.id))
            .label("🗑 Remove")
            .style(serenity::ButtonStyle::Danger),
    ]);

    (content, vec![buttons])
}

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

async fn load_tracks_for_playlist_play(
    data: &Data,
    playlist: &SavedPlaylist,
) -> Result<Vec<Track>, String> {
    match crate::db::get_playlist_tracks(&data.db, playlist.id).await {
        Ok(tracks) if !tracks.is_empty() => Ok(tracks),
        Ok(_) => fetch_and_cache_playlist_tracks(data, playlist).await,
        Err(err) => Err(format!("Failed to load playlist: {err}")),
    }
}

async fn ensure_connected_for_playlist_play(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    data: &Data,
    guild_id: serenity::GuildId,
) -> Result<bool, String> {
    if data.player.is_connected(guild_id) {
        return Ok(true);
    }

    let Some(channel_id) = voice_channel_of(ctx, guild_id, component.user.id) else {
        return Err("Join a voice channel first, or use `/join`.".to_string());
    };
    if let Err(err) = data.player.join(guild_id, channel_id).await {
        return Err(format!("Failed to join voice channel: {err}"));
    }
    Ok(true)
}

async fn enqueue_playlist_tracks(
    data: &Data,
    guild_id: serenity::GuildId,
    requested_by: serenity::UserId,
    playlist_name: &str,
    tracks: Vec<Track>,
) -> Result<String, String> {
    let total = tracks.len();
    let queued: Vec<QueuedTrack> = tracks
        .into_iter()
        .map(|track| QueuedTrack {
            track,
            requested_by,
        })
        .collect();
    data.player
        .enqueue_many(guild_id, queued)
        .await
        .map_err(|err| format!("Failed to queue tracks: {err}"))?;

    Ok(format!("Queued {total} track(s) from **{playlist_name}**."))
}

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

    let tracks = match load_tracks_for_playlist_play(data, &playlist).await {
        Ok(tracks) if !tracks.is_empty() => tracks,
        Ok(_) => {
            return update_picker(
                ctx,
                component,
                "That playlist is empty (or couldn't be found).",
            )
            .await;
        }
        Err(message) => return update_picker(ctx, component, message).await,
    };

    if let Err(message) = ensure_connected_for_playlist_play(ctx, component, data, guild_id).await {
        return update_picker(ctx, component, message).await;
    }

    match enqueue_playlist_tracks(data, guild_id, component.user.id, &playlist.name, tracks).await {
        Ok(content) => update_picker(ctx, component, content).await,
        Err(message) => update_picker(ctx, component, message).await,
    }
}

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

fn modal_query(data: &serenity::ModalInteractionData) -> Option<String> {
    modal_field(data, "query")
}

pub(super) async fn handle_search_modal_submit(
    ctx: &serenity::Context,
    modal: &serenity::ModalInteraction,
    data: &Data,
) -> Result<(), Error> {
    modal.defer_ephemeral(&ctx.http).await?;

    let Some(guild_id) = modal.guild_id else {
        return Ok(());
    };

    let Some(query) = modal_query(&modal.data) else {
        modal
            .edit_response(
                &ctx.http,
                serenity::EditInteractionResponse::new().content("Enter a search query."),
            )
            .await?;
        return Ok(());
    };

    let results = match search_with_cache(data, guild_id, &query).await {
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

fn import_button_row() -> serenity::CreateActionRow {
    serenity::CreateActionRow::Buttons(vec![
        serenity::CreateButton::new("library:playlist_import")
            .label("➕ Import playlist")
            .style(serenity::ButtonStyle::Secondary),
    ])
}

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
        return Ok(());
    };

    handle_playlist_import_modal_submit(ctx, &modal, guild_id, data).await
}

async fn fetch_playlist_for_import(
    ctx: &serenity::Context,
    modal: &serenity::ModalInteraction,
    data: &Data,
) -> Result<Option<(String, PlaylistListing)>, Error> {
    let Some(url) = modal_field(&modal.data, "url") else {
        modal
            .edit_response(
                &ctx.http,
                serenity::EditInteractionResponse::new().content("Enter a playlist URL."),
            )
            .await?;
        return Ok(None);
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
            return Ok(None);
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
        return Ok(None);
    }

    Ok(Some((url, listing)))
}

async fn save_imported_playlist(
    ctx: &serenity::Context,
    modal: &serenity::ModalInteraction,
    data: &Data,
    guild_id: serenity::GuildId,
    name: &str,
    url: &str,
    tracks: &[Track],
) -> Result<Option<i64>, Error> {
    let id = match crate::db::save_guild_playlist(
        &data.db,
        &guild_id.to_string(),
        name,
        url,
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
            return Ok(None);
        }
    };

    if let Err(err) = crate::db::replace_playlist_tracks(&data.db, id, tracks).await {
        tracing::warn!(%err, playlist_id = id, "failed to cache playlist tracks after import");
    }

    Ok(Some(id))
}

struct ImportedPlaylistSummary {
    id: i64,
    name: String,
    url: String,
    track_count: usize,
}

async fn reply_with_saved_playlist(
    ctx: &serenity::Context,
    modal: &serenity::ModalInteraction,
    data: &Data,
    guild_id: serenity::GuildId,
    summary: ImportedPlaylistSummary,
) -> Result<(), Error> {
    let ImportedPlaylistSummary {
        id,
        name,
        url,
        track_count,
    } = summary;
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

async fn handle_playlist_import_modal_submit(
    ctx: &serenity::Context,
    modal: &serenity::ModalInteraction,
    guild_id: serenity::GuildId,
    data: &Data,
) -> Result<(), Error> {
    modal.defer_ephemeral(&ctx.http).await?;

    let Some((url, listing)) = fetch_playlist_for_import(ctx, modal, data).await? else {
        return Ok(());
    };

    let name = modal_field(&modal.data, "name")
        .or_else(|| listing.title.clone())
        .unwrap_or_else(|| url.clone());
    let tracks = listing.tracks;
    let track_count = tracks.len();

    let Some(id) = save_imported_playlist(ctx, modal, data, guild_id, &name, &url, &tracks).await?
    else {
        return Ok(());
    };

    reply_with_saved_playlist(
        ctx,
        modal,
        data,
        guild_id,
        ImportedPlaylistSummary {
            id,
            name,
            url,
            track_count,
        },
    )
    .await
}

fn author_voice_channel(ctx: Context<'_>) -> Option<serenity::ChannelId> {
    ctx.guild().and_then(|guild| {
        guild
            .voice_states
            .get(&ctx.author().id)
            .and_then(|vs| vs.channel_id)
    })
}

async fn ensure_connected(ctx: Context<'_>, guild_id: serenity::GuildId) -> Result<bool, Error> {
    let Some(channel_id) = author_voice_channel(ctx) else {
        if ctx.data().player.is_connected(guild_id) {
            return Ok(true);
        }
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

async fn join_and_enqueue(ctx: Context<'_>, track: Track) -> Result<(), Error> {
    let Some(guild_id) = ctx.guild_id() else {
        ctx.send(
            poise::CreateReply::default()
                .content("This command can only be used in a server.")
                .ephemeral(true),
        )
        .await?;
        return Ok(());
    };

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

pub(super) async fn join_and_enqueue_all(
    ctx: Context<'_>,
    label: &str,
    tracks: Vec<Track>,
) -> Result<(), Error> {
    let Some(guild_id) = ctx.guild_id() else {
        ctx.send(
            poise::CreateReply::default()
                .content("This command can only be used in a server.")
                .ephemeral(true),
        )
        .await?;
        return Ok(());
    };

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
    if let Err(err) = ctx.data().player.enqueue_many(guild_id, queued).await {
        ctx.send(
            poise::CreateReply::default()
                .content(format!("Failed to queue tracks: {err}"))
                .ephemeral(true),
        )
        .await?;
        return Ok(());
    }

    super::playback::reply_public(ctx, format!("Queued {total} track(s) from **{label}**.")).await
}

#[poise::command(slash_command, guild_only)]
pub async fn add_to_queue(
    ctx: Context<'_>,
    #[description = "Search query"] query: String,
    #[description = "Result number from a previous /add_to_queue with this query"] number: Option<
        u8,
    >,
) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;

    let Some(guild_id) = ctx.guild_id() else {
        ctx.send(
            poise::CreateReply::default()
                .content("This command can only be used in a server.")
                .ephemeral(true),
        )
        .await?;
        return Ok(());
    };
    let results = match search_with_cache(ctx.data(), guild_id, &query).await {
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
        return present_search_results(ctx, &query, &results).await;
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

pub(super) async fn present_search_results(
    ctx: Context<'_>,
    query: &str,
    results: &[Track],
) -> Result<(), Error> {
    if results.is_empty() {
        ctx.send(
            poise::CreateReply::default()
                .content("No results found.")
                .ephemeral(true),
        )
        .await?;
        return Ok(());
    }

    let listing = format_track_list(results, ADD_TO_QUEUE_DISPLAY_LIMIT);
    let handle = ctx
        .send(
            poise::CreateReply::default()
                .content(format!(
                    "Search results for \"{query}\":\n{listing}\n\nSelect one below to queue it, \
                     or run `/add_to_queue {query} <number>` to do the same without the menu."
                ))
                .components(vec![track_select_menu(results, ADD_TO_QUEUE_DISPLAY_LIMIT)])
                .ephemeral(true),
        )
        .await?;
    schedule_picker_cleanup(ctx, handle).await;
    Ok(())
}

pub(super) async fn search_with_cache(
    data: &Data,
    guild_id: serenity::GuildId,
    query: &str,
) -> Result<Vec<Track>, YouTubeApiError> {
    match crate::db::search_cached_tracks(
        &data.db,
        &guild_id.to_string(),
        query,
        ADD_TO_QUEUE_DISPLAY_LIMIT as i64,
    )
    .await
    {
        Ok(cached) if !cached.is_empty() => return Ok(cached),
        Ok(_) => {}
        Err(err) => {
            tracing::warn!(%err, "failed to search cached playlist tracks; falling back to yt-dlp");
        }
    }

    data.youtube.search(query).await
}

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
