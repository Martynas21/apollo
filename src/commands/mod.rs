//! Slash command implementations (registered with `poise`).

mod library;
mod playback;
mod radio;

pub use library::handle_component as handle_library_component;
pub use playback::handle_component as handle_player_component;

/// Shared state made available to every command invocation.
// No `Debug` derive: `PlayerRegistry` holds songbird types (`Arc<Songbird>`,
// `TrackHandle`) that don't implement it, and it's not needed anywhere.
#[derive(Clone)]
pub struct Data {
    /// `yt-dlp`-backed client for search, single-video, and playlist lookups.
    pub youtube: crate::youtube::api::YouTubeClient,
    /// Per-guild playback queues and the shared songbird manager handle.
    pub player: crate::voice::PlayerRegistry,
    /// Connection pool for saved playlists and other guild settings —
    /// separate from `player`, which holds its own clone of the same pool
    /// for its own persisted state (volume).
    pub db: sqlx::SqlitePool,
}

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Context<'a> = poise::Context<'a, Data, Error>;

/// All commands registered with the framework.
pub fn commands() -> Vec<poise::Command<Data, Error>> {
    vec![
        playback::play(),
        playback::queue(),
        playback::skip(),
        playback::pause(),
        playback::resume(),
        playback::stop(),
        playback::player(),
        playback::shuffle(),
        playback::volume(),
        radio::radio(),
        library::add_to_queue(),
    ]
}

#[cfg(test)]
mod tests {
    use super::commands;
    use std::collections::HashSet;

    /// Guards against a copy-paste mistake in the registration list: a
    /// command left out entirely, or accidentally listed twice (which
    /// `poise`/Discord would otherwise reject at registration time with a
    /// less obvious error).
    #[test]
    fn registered_commands_have_unique_non_empty_names() {
        let names: Vec<String> = commands().into_iter().map(|c| c.name).collect();
        assert!(!names.is_empty(), "expected at least one command");
        assert!(
            names.iter().all(|name| !name.is_empty()),
            "every command must have a name"
        );

        let unique: HashSet<&String> = names.iter().collect();
        assert_eq!(
            unique.len(),
            names.len(),
            "duplicate command name(s) in commands(): {names:?}"
        );
    }

    #[test]
    fn commands_includes_expected_top_level_names() {
        let names: HashSet<String> = commands().into_iter().map(|c| c.name).collect();
        for expected in [
            "play", "queue", "skip", "pause", "resume", "stop", "player", "shuffle", "volume",
            "radio",
        ] {
            assert!(
                names.contains(expected),
                "expected commands() to register `{expected}`"
            );
        }
    }
}
