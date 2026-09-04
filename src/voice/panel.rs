use std::time::Duration;

use poise::serenity_prelude as serenity;

use super::player::{PlayerRegistry, QueueSnapshot, QueuedTrack};

const ACCENT_COLOR: serenity::Colour = serenity::Colour::new(0x008B_5CF6);

const PANEL_UPCOMING_PREVIEW: usize = 5;

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

fn volume_glyph(volume: u8) -> char {
    match volume {
        0 => '🔇',
        1..=50 => '🔉',
        _ => '🔊',
    }
}

pub(crate) fn truncate_label(label: &str) -> String {
    if label.chars().count() > 100 {
        let mut truncated: String = label.chars().take(97).collect();
        truncated.push_str("...");
        truncated
    } else {
        label.to_string()
    }
}

fn now_playing_embed(queued: &QueuedTrack, upcoming: &[QueuedTrack]) -> serenity::CreateEmbed {
    let video_url = format!("https://www.youtube.com/watch?v={}", queued.track.video_id);
    let thumbnail_url = format!(
        "https://i.ytimg.com/vi/{}/hqdefault.jpg",
        queued.track.video_id
    );

    let mut embed = serenity::CreateEmbed::new()
        .author(serenity::CreateEmbedAuthor::new("▶ Now Playing"))
        .color(ACCENT_COLOR)
        .title(&queued.track.title)
        .url(video_url)
        .image(thumbnail_url)
        .field("Channel", &queued.track.channel, true)
        .field("Requested by", format!("<@{}>", queued.requested_by), true);

    if let Some((name, value)) = upcoming_field(upcoming) {
        embed = embed.field(name, value, false);
    }

    embed
}

fn upcoming_field(upcoming: &[QueuedTrack]) -> Option<(String, String)> {
    if upcoming.is_empty() {
        return None;
    }
    let mut lines: Vec<String> = upcoming
        .iter()
        .take(PANEL_UPCOMING_PREVIEW)
        .enumerate()
        .map(|(i, q)| format!("{}. {}", i + 1, truncate_label(&q.track.title)))
        .collect();
    let remaining = upcoming.len().saturating_sub(PANEL_UPCOMING_PREVIEW);
    if remaining > 0 {
        lines.push(format!("...and {remaining} more"));
    }
    Some(("Up Next".to_string(), lines.join("\n")))
}

fn last_played_embed(queued: &QueuedTrack) -> serenity::CreateEmbed {
    let video_url = format!("https://www.youtube.com/watch?v={}", queued.track.video_id);
    let thumbnail_url = format!(
        "https://i.ytimg.com/vi/{}/hqdefault.jpg",
        queued.track.video_id
    );

    serenity::CreateEmbed::new()
        .author(serenity::CreateEmbedAuthor::new("⏹ Finished Playing"))
        .color(ACCENT_COLOR)
        .title(&queued.track.title)
        .url(video_url)
        .image(thumbnail_url)
        .field("Channel", &queued.track.channel, true)
        .field("Requested by", format!("<@{}>", queued.requested_by), true)
}

fn panel_components(
    snapshot: &QueueSnapshot,
    paused: Option<bool>,
    volume: u8,
    radio_enabled: bool,
) -> Vec<serenity::CreateActionRow> {
    vec![
        playback_row(snapshot, paused, radio_enabled),
        controls_row(snapshot, volume),
    ]
}

fn playback_row(
    snapshot: &QueueSnapshot,
    paused: Option<bool>,
    radio_enabled: bool,
) -> serenity::CreateActionRow {
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

    let radio_toggle = if radio_enabled {
        serenity::CreateButton::new("player:radio")
            .label("📻 Radio: On")
            .style(serenity::ButtonStyle::Success)
    } else {
        serenity::CreateButton::new("player:radio")
            .label("📻 Radio: Off")
            .style(serenity::ButtonStyle::Secondary)
    };

    serenity::CreateActionRow::Buttons(vec![
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
    ])
}

fn controls_row(snapshot: &QueueSnapshot, volume: u8) -> serenity::CreateActionRow {
    serenity::CreateActionRow::Buttons(vec![
        serenity::CreateButton::new("player:volume")
            .label(format!("{} {volume}%", volume_glyph(volume)))
            .style(serenity::ButtonStyle::Secondary),
        serenity::CreateButton::new("player:search")
            .label("🔍 Search")
            .style(serenity::ButtonStyle::Primary),
        serenity::CreateButton::new("player:playlists")
            .label("🎵 Playlists")
            .style(serenity::ButtonStyle::Secondary),
        serenity::CreateButton::new("player:clear")
            .label("🗑 Clear Queue")
            .style(serenity::ButtonStyle::Danger)
            .disabled(snapshot.upcoming.is_empty()),
    ])
}

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
        Some(queued) => (
            String::new(),
            Some(now_playing_embed(queued, &snapshot.upcoming)),
            components,
        ),
        None => match &snapshot.last_played {
            Some(queued) => (
                "Queue finished — search or `/play` to add more.".to_string(),
                Some(last_played_embed(queued)),
                components,
            ),
            None => (
                "Nothing is playing — search or `/play` to get started.".to_string(),
                None,
                components,
            ),
        },
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

    fn embed_json(embed: serenity::CreateEmbed) -> serde_json::Value {
        serde_json::to_value(embed).expect("CreateEmbed should serialize")
    }

    #[test]
    fn embed_includes_title_url_thumbnail_and_requester() {
        let queued = sample_queued_track(Some(Duration::from_secs(213)));
        let json = embed_json(now_playing_embed(&queued, &[]));

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
    }

    #[test]
    fn embed_omits_up_next_field_when_queue_is_empty() {
        let queued = sample_queued_track(Some(Duration::from_secs(213)));
        let json = embed_json(now_playing_embed(&queued, &[]));

        let fields = json["fields"]
            .as_array()
            .expect("fields should be an array");
        assert!(!fields.iter().any(|f| f["name"] == "Up Next"));
    }

    #[test]
    fn embed_shows_up_next_field_with_upcoming_titles() {
        let queued = sample_queued_track(Some(Duration::from_secs(213)));
        let upcoming = vec![
            QueuedTrack {
                track: Track {
                    video_id: "a".to_string(),
                    title: "Track A".to_string(),
                    channel: "Channel".to_string(),
                    duration: None,
                },
                requested_by: serenity::UserId::new(1),
            },
            QueuedTrack {
                track: Track {
                    video_id: "b".to_string(),
                    title: "Track B".to_string(),
                    channel: "Channel".to_string(),
                    duration: None,
                },
                requested_by: serenity::UserId::new(1),
            },
        ];
        let json = embed_json(now_playing_embed(&queued, &upcoming));

        let fields = json["fields"]
            .as_array()
            .expect("fields should be an array");
        let up_next = fields
            .iter()
            .find(|f| f["name"] == "Up Next")
            .map(|f| f["value"].as_str().unwrap());
        assert_eq!(up_next, Some("1. Track A\n2. Track B"));
    }

    #[test]
    fn embed_up_next_field_truncates_with_remaining_count() {
        let queued = sample_queued_track(Some(Duration::from_secs(213)));
        let upcoming: Vec<QueuedTrack> = (0..8)
            .map(|i| QueuedTrack {
                track: Track {
                    video_id: format!("id{i}"),
                    title: format!("Track {i}"),
                    channel: "Channel".to_string(),
                    duration: None,
                },
                requested_by: serenity::UserId::new(1),
            })
            .collect();
        let json = embed_json(now_playing_embed(&queued, &upcoming));

        let fields = json["fields"]
            .as_array()
            .expect("fields should be an array");
        let up_next = fields
            .iter()
            .find(|f| f["name"] == "Up Next")
            .map(|f| f["value"].as_str().unwrap())
            .expect("Up Next field should be present");
        assert!(up_next.ends_with("...and 3 more"));
        assert_eq!(up_next.lines().count(), PANEL_UPCOMING_PREVIEW + 1);
    }

    #[test]
    fn last_played_embed_shows_finished_header_with_title_and_url() {
        let queued = sample_queued_track(Some(Duration::from_secs(213)));
        let json = embed_json(last_played_embed(&queued));

        assert_eq!(json["author"]["name"], "⏹ Finished Playing");
        assert_eq!(json["title"], "Some Video");
        assert_eq!(json["url"], "https://www.youtube.com/watch?v=dQw4w9WgXcQ");
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
            last_played: None,
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
    fn playlists_button_present_and_stays_enabled_when_nothing_playing() {
        let snapshot = sample_queue_snapshot(false, 0);
        let json = components_json(&panel_components(&snapshot, None, 50, false));
        let playlists_button = &json[1]["components"][2];
        assert_eq!(playlists_button["custom_id"], "player:playlists");
        assert_eq!(playlists_button["disabled"], false);
    }

    #[test]
    fn clear_queue_button_disabled_when_queue_empty_and_enabled_otherwise() {
        let empty = sample_queue_snapshot(true, 0);
        let empty_json = components_json(&panel_components(&empty, Some(false), 50, false));
        let clear_button = &empty_json[1]["components"][3];
        assert_eq!(clear_button["custom_id"], "player:clear");
        assert_eq!(clear_button["disabled"], true);

        let nonempty = sample_queue_snapshot(true, 2);
        let nonempty_json = components_json(&panel_components(&nonempty, Some(false), 50, false));
        assert_eq!(nonempty_json[1]["components"][3]["disabled"], false);
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
    fn panel_components_has_no_queue_select_menu() {
        let snapshot = sample_queue_snapshot(true, 3);
        let json = components_json(&panel_components(&snapshot, Some(false), 50, false));
        assert_eq!(json.as_array().unwrap().len(), 2);
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
