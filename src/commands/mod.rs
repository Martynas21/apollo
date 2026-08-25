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
        library::playlist_play(),
    ]
}
