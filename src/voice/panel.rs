//! Rendering for the `/player` panel: the embed and button/select-menu rows
//! that represent a guild's playback state.
//!
//! Pure functions of [`PlayerRegistry`]'s own state — no `Data`/`Context`
//! dependency — so the same rendering backs the initial `/player` post,
//! [`PlayerRegistry`]'s self-refresh after every state-changing mutation,
//! and the panel's own button clicks. Lives under `voice` rather than
//! `commands` because `PlayerRegistry` needs to call into it directly to
//! refresh a live panel, and `voice` must not depend on `commands`.

use std::time::Duration;

use poise::serenity_prelude as serenity;

use super::player::{PlayerRegistry, QueueSnapshot, QueuedTrack};

/// Discord select menus cap out at 25 options.
const QUEUE_SELECT_LIMIT: usize = 25;

/// Accent color for the now-playing embed.
const ACCENT_COLOR: serenity::Colour = serenity::Colour::new(0x008B_5CF6);

/// Formats a `Duration` as `mm:ss`, or `h:mm:ss` once it reaches an hour.
pub(crate) fn format_duration(duration: Duration) -> String {
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

/// Picks a volume glyph for the panel's volume button: muted, low, or full.
fn volume_glyph(volume: u8) -> char {
    match volume {
        0 => '🔇',
        1..=50 => '🔉',
        _ => '🔊',
    }
}

/// Discord caps select-menu option labels at 100 characters.
pub(crate) fn truncate_label(label: &str) -> String {
    if label.chars().count() > 100 {
        let mut truncated: String = label.chars().take(97).collect();
        truncated.push_str("...");
        truncated
    } else {
        label.to_string()
    }
}

/// Builds the now-playing embed: title (linked to the video), thumbnail,
/// requester, and progress (`position / duration`, or just `position` if the
/// video's total duration is unknown, or omitted entirely if songbird
/// couldn't report a position).
fn now_playing_embed(queued: &QueuedTrack, position: Option<Duration>) -> serenity::CreateEmbed {
    let video_url = format!("https://www.youtube.com/watch?v={}", queued.track.video_id);
    // YouTube's thumbnail CDN follows this URL shape for every public video
    // ID — no extra API call needed to get it.
    let thumbnail_url = format!(
        "https://i.ytimg.com/vi/{}/hqdefault.jpg",
        queued.track.video_id
    );

    let progress = match (position, queued.track.duration) {
        (Some(pos), Some(dur)) => Some(format!(
            "{} / {}",
            format_duration(pos),
            format_duration(dur)
        )),
        (Some(pos), None) => Some(format_duration(pos)),
        (None, _) => None,
    };

    let mut embed = serenity::CreateEmbed::new()
        .author(serenity::CreateEmbedAuthor::new("▶ Now Playing"))
        .color(ACCENT_COLOR)
        .title(&queued.track.title)
        .url(video_url)
        .image(thumbnail_url)
        .field("Channel", &queued.track.channel, true)
        .field("Requested by", format!("<@{}>", queued.requested_by), true);

    if let Some(progress) = progress {
        embed = embed.field("Progress", progress, false);
    }

    embed
}

/// Builds the panel's button/select-menu rows: play/pause toggle, skip,
/// stop, shuffle, and radio toggle on one row; a volume button (opens a
/// type-in modal) alongside the Search entry point into the library on
/// another; and — when the queue isn't empty — a select menu to jump
/// straight to an upcoming track.
///
/// The playback row disables itself when nothing is playing; volume and
/// Search stay enabled always, since volume applies to future tracks too
/// and `/player` is meant to be usable as a cold-start entry point into the
/// whole app.
fn panel_components(
    snapshot: &QueueSnapshot,
    paused: Option<bool>,
    volume: u8,
    radio_enabled: bool,
) -> Vec<serenity::CreateActionRow> {
    let has_now_playing = snapshot.now_playing.is_some();

    let toggle = match paused {
        Some(true) => serenity::CreateButton::new("player:toggle")
            .label("▶ Resume")
            .style(serenity::ButtonStyle::Success),
        _ => serenity::CreateButton::new("player:toggle")
            .label("⏸ Pause")
            .style(serenity::ButtonStyle::Secondary),
    }
    .disabled(!has_now_playing);

    // Not gated by `has_now_playing`, unlike the other buttons in this row —
    // toggling radio mode (or discovering it needs something to play first)
    // should work with nothing currently playing.
    let radio_toggle = if radio_enabled {
        serenity::CreateButton::new("player:radio")
            .label("📻 Radio: On")
            .style(serenity::ButtonStyle::Success)
    } else {
        serenity::CreateButton::new("player:radio")
            .label("📻 Radio: Off")
            .style(serenity::ButtonStyle::Secondary)
    };

    // At Discord's 5-button-per-row cap with `radio_toggle` included — a
    // future addition here needs its own row.
    let playback_row = serenity::CreateActionRow::Buttons(vec![
        toggle,
        serenity::CreateButton::new("player:skip")
            .label("⏭ Skip")
            .style(serenity::ButtonStyle::Primary)
            .disabled(!has_now_playing),
        serenity::CreateButton::new("player:stop")
            .label("⏹ Stop")
            .style(serenity::ButtonStyle::Danger)
            .disabled(!has_now_playing),
        serenity::CreateButton::new("player:shuffle")
            .label("🔀 Shuffle")
            .style(serenity::ButtonStyle::Secondary)
            .disabled(snapshot.upcoming.len() < 2),
        radio_toggle,
    ]);

    // Volume (a single button, rather than the old step +/- pair, so the
    // exact level is one tap — opening a type-in modal — away instead of
    // several) alongside the library entry point, both on one row.
    let controls_row = serenity::CreateActionRow::Buttons(vec![
        serenity::CreateButton::new("player:volume")
            .label(format!("{} {volume}%", volume_glyph(volume)))
            .style(serenity::ButtonStyle::Secondary),
        serenity::CreateButton::new("player:search")
            .label("🔍 Search")
            .style(serenity::ButtonStyle::Primary),
    ]);

    let mut rows = vec![playback_row, controls_row];

    if !snapshot.upcoming.is_empty() {
        let options = snapshot
            .upcoming
            .iter()
            .take(QUEUE_SELECT_LIMIT)
            .enumerate()
            .map(|(i, queued)| {
                serenity::CreateSelectMenuOption::new(
                    truncate_label(&format!("{}. {}", i + 1, queued.track.title)),
                    i.to_string(),
                )
            })
            .collect();
        let select = serenity::CreateSelectMenu::new(
            "player:jump",
            serenity::CreateSelectMenuKind::String { options },
        )
        .placeholder("Jump to a track in the queue...");
        rows.push(serenity::CreateActionRow::SelectMenu(select));
    }

    rows
}

/// Renders the full panel for `guild_id` from [`PlayerRegistry`]'s current
/// state: message content (used when nothing is playing — empty
/// otherwise, since the embed carries the info instead), the now-playing
/// embed (if any), and the button/select rows.
///
/// Single source of truth for the panel's appearance, used by the initial
/// `/player` post, every self-refresh `PlayerRegistry` triggers after a
/// mutation, and the panel's own button-click handler.
pub(crate) async fn render(
    registry: &PlayerRegistry,
    guild_id: serenity::GuildId,
) -> (
    String,
    Option<serenity::CreateEmbed>,
    Vec<serenity::CreateActionRow>,
) {
    let snapshot = registry.queue_snapshot(guild_id).await;
    let paused = registry.is_paused(guild_id).await;
    let volume = registry.get_volume(guild_id).await;
    let radio_enabled = registry.is_radio_enabled(guild_id).await;
    let components = panel_components(&snapshot, paused, volume, radio_enabled);

    match &snapshot.now_playing {
        Some(queued) => {
            let position = registry.now_playing_position(guild_id).await;
            (
                String::new(),
                Some(now_playing_embed(queued, position)),
                components,
            )
        }
        None => (
            "Nothing is playing — search, `/play`, or `/playlist_play` to get started.".to_string(),
            None,
            components,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::youtube::api::Track;

    fn sample_queued_track(duration: Option<Duration>) -> QueuedTrack {
        QueuedTrack {
            track: Track {
                video_id: "dQw4w9WgXcQ".to_string(),
                title: "Some Video".to_string(),
                channel: "Some Channel".to_string(),
                duration,
            },
            requested_by: serenity::UserId::new(123_456_789_012_345_678),
        }
    }

    /// `CreateEmbed` derives `Serialize`; round-tripping through JSON is the
    /// only way to inspect a built embed's fields from outside the crate.
    fn embed_json(embed: serenity::CreateEmbed) -> serde_json::Value {
        serde_json::to_value(embed).expect("CreateEmbed should serialize")
    }

    #[test]
    fn embed_includes_title_url_thumbnail_and_requester() {
        let queued = sample_queued_track(Some(Duration::from_secs(213)));
        let json = embed_json(now_playing_embed(&queued, Some(Duration::from_secs(30))));

        assert_eq!(json["title"], "Some Video");
        assert_eq!(json["url"], "https://www.youtube.com/watch?v=dQw4w9WgXcQ");
        assert_eq!(
            json["image"]["url"],
            "https://i.ytimg.com/vi/dQw4w9WgXcQ/hqdefault.jpg"
        );

        let fields = json["fields"]
            .as_array()
            .expect("fields should be an array");
        let field_value = |name: &str| {
            fields
                .iter()
                .find(|f| f["name"] == name)
                .map(|f| f["value"].as_str().unwrap().to_string())
        };
        assert_eq!(field_value("Channel"), Some("Some Channel".to_string()));
        assert_eq!(
            field_value("Requested by"),
            Some("<@123456789012345678>".to_string())
        );
        assert!(
            field_value("Progress")
                .expect("Progress field should be present")
                .ends_with("0:30 / 3:33")
        );
    }

    #[test]
    fn embed_omits_progress_field_when_position_unknown() {
        let queued = sample_queued_track(Some(Duration::from_secs(213)));
        let json = embed_json(now_playing_embed(&queued, None));

        let fields = json["fields"]
            .as_array()
            .expect("fields should be an array");
        assert!(!fields.iter().any(|f| f["name"] == "Progress"));
    }

    #[test]
    fn embed_shows_bare_position_when_duration_unknown() {
        let queued = sample_queued_track(None);
        let json = embed_json(now_playing_embed(&queued, Some(Duration::from_secs(30))));

        let fields = json["fields"]
            .as_array()
            .expect("fields should be an array");
        let progress = fields
            .iter()
            .find(|f| f["name"] == "Progress")
            .map(|f| f["value"].as_str().unwrap());
        assert_eq!(progress, Some("0:30"));
    }

    #[test]
    fn truncate_label_leaves_short_labels_untouched() {
        assert_eq!(truncate_label("short title"), "short title");
    }

    #[test]
    fn truncate_label_truncates_long_labels_to_100_chars_with_ellipsis() {
        let long = "x".repeat(150);
        let truncated = truncate_label(&long);
        assert_eq!(truncated.chars().count(), 100);
        assert!(truncated.ends_with("..."));
    }

    fn sample_queue_snapshot(now_playing: bool, upcoming_count: usize) -> QueueSnapshot {
        QueueSnapshot {
            now_playing: now_playing.then(|| sample_queued_track(Some(Duration::from_secs(120)))),
            upcoming: (0..upcoming_count)
                .map(|i| QueuedTrack {
                    track: Track {
                        video_id: format!("id{i}"),
                        title: format!("Track {i}"),
                        channel: "Channel".to_string(),
                        duration: None,
                    },
                    requested_by: serenity::UserId::new(1),
                })
                .collect(),
        }
    }

    fn components_json(components: &[serenity::CreateActionRow]) -> serde_json::Value {
        serde_json::to_value(components).expect("components should serialize")
    }

    #[test]
    fn toggle_button_shows_resume_when_paused_and_pause_otherwise() {
        let snapshot = sample_queue_snapshot(true, 0);

        let paused_json = components_json(&panel_components(&snapshot, Some(true), 50, false));
        assert_eq!(paused_json[0]["components"][0]["label"], "▶ Resume");

        let playing_json = components_json(&panel_components(&snapshot, Some(false), 50, false));
        assert_eq!(playing_json[0]["components"][0]["label"], "⏸ Pause");
    }

    #[test]
    fn playback_buttons_disabled_when_nothing_playing() {
        let snapshot = sample_queue_snapshot(false, 0);
        let json = components_json(&panel_components(&snapshot, None, 50, false));
        // toggle, skip, stop, shuffle — the radio toggle (last button) is
        // deliberately exempt, see `radio_button_stays_enabled_when_nothing_playing`.
        let buttons = json[0]["components"].as_array().unwrap();
        for button in &buttons[..buttons.len() - 1] {
            assert_eq!(button["disabled"], true, "{button:?} should be disabled");
        }
    }

    #[test]
    fn radio_button_shows_on_when_enabled_and_off_otherwise() {
        let snapshot = sample_queue_snapshot(true, 0);

        let on_json = components_json(&panel_components(&snapshot, Some(false), 50, true));
        let on_buttons = on_json[0]["components"].as_array().unwrap();
        let on_button = on_buttons.last().unwrap();
        assert_eq!(on_button["label"], "📻 Radio: On");

        let off_json = components_json(&panel_components(&snapshot, Some(false), 50, false));
        let off_buttons = off_json[0]["components"].as_array().unwrap();
        let off_button = off_buttons.last().unwrap();
        assert_eq!(off_button["label"], "📻 Radio: Off");
    }

    #[test]
    fn radio_button_stays_enabled_when_nothing_playing() {
        let snapshot = sample_queue_snapshot(false, 0);
        let json = components_json(&panel_components(&snapshot, None, 50, true));
        let buttons = json[0]["components"].as_array().unwrap();
        assert_eq!(buttons.last().unwrap()["disabled"], false);
    }

    #[test]
    fn search_button_stays_enabled_when_nothing_playing() {
        let snapshot = sample_queue_snapshot(false, 0);
        let json = components_json(&panel_components(&snapshot, None, 50, false));
        let search_button = &json[1]["components"][1];
        assert_eq!(search_button["disabled"], false);
    }

    #[test]
    fn volume_button_shows_glyph_and_level_and_stays_enabled_when_nothing_playing() {
        let snapshot = sample_queue_snapshot(false, 0);
        let json = components_json(&panel_components(&snapshot, None, 0, false));
        let button = &json[1]["components"][0];
        assert_eq!(button["label"], "🔇 0%");
        assert_eq!(button["disabled"], false);
    }

    #[test]
    fn volume_glyph_reflects_level() {
        assert_eq!(volume_glyph(0), '🔇');
        assert_eq!(volume_glyph(50), '🔉');
        assert_eq!(volume_glyph(100), '🔊');
    }

    #[test]
    fn select_menu_omitted_when_queue_empty_and_present_otherwise() {
        let empty = sample_queue_snapshot(true, 0);
        let empty_json = components_json(&panel_components(&empty, Some(false), 50, false));
        assert_eq!(empty_json.as_array().unwrap().len(), 2);

        let nonempty = sample_queue_snapshot(true, 3);
        let nonempty_json = components_json(&panel_components(&nonempty, Some(false), 50, false));
        assert_eq!(nonempty_json.as_array().unwrap().len(), 3);
    }

    #[test]
    fn select_menu_caps_options_at_25() {
        let snapshot = sample_queue_snapshot(true, 30);
        let json = components_json(&panel_components(&snapshot, Some(false), 50, false));
        let options = json[2]["components"][0]["options"].as_array().unwrap();
        assert_eq!(options.len(), 25);
    }

    #[test]
    fn formats_sub_hour_duration() {
        assert_eq!(format_duration(Duration::from_secs(213)), "3:33");
    }

    #[test]
    fn formats_over_an_hour_duration() {
        assert_eq!(format_duration(Duration::from_secs(3723)), "1:02:03");
    }
}
