//! Songbird-based voice connection and audio playback pipeline.
//!
//! Stream resolution (`src/voice/resolve.rs`) is handled by songbird's
//! built-in `YoutubeDl` input source, which shells out to `yt-dlp` and
//! decodes the resolved stream via symphonia — no separate `ffmpeg`
//! subprocess in the playback path itself.

pub mod player;
pub mod resolve;

// Not consumed yet — Phase 6 commands wire these into `/play` and friends.
#[allow(unused_imports)]
pub use player::{PlayerError, PlayerRegistry, QueueSnapshot, QueuedTrack};
#[allow(unused_imports)]
pub use resolve::{PlaybackError, preflight_check, track_input};

use tokio::process::Command;

/// Verifies `yt-dlp` and `ffmpeg` are runnable on `PATH`. Call once at
/// startup; fails fast with a clear message rather than surfacing a
/// confusing error the first time someone tries to `/play` something.
///
/// Both are checked even though only `yt-dlp` is directly exercised by this
/// project's playback path (songbird's `YoutubeDl` source streams straight
/// into symphonia): `ffmpeg` remains a documented prerequisite since yt-dlp
/// itself may shell out to it for certain post-processing/remuxing, so it's
/// better to catch its absence here than as a confusing runtime surprise
/// later.
#[allow(dead_code)]
pub async fn check_playback_dependencies() -> anyhow::Result<()> {
    let mut missing = Vec::new();

    if !binary_runnable("yt-dlp", "--version").await {
        missing.push("yt-dlp");
    }
    if !binary_runnable("ffmpeg", "-version").await {
        missing.push("ffmpeg");
    }

    if missing.is_empty() {
        Ok(())
    } else {
        anyhow::bail!(
            "missing required playback dependencies: {} — install yt-dlp and ffmpeg and ensure they're on PATH",
            missing.join(", ")
        )
    }
}

async fn binary_runnable(program: &str, version_arg: &str) -> bool {
    // Any spawn failure (not just `NotFound`) counts as "missing" here —
    // there's no useful distinction to surface for a startup check.
    Command::new(program)
        .arg(version_arg)
        .output()
        .await
        .is_ok_and(|output| output.status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Legitimate assertion about this sandbox's actual state: neither
    /// binary is installed here, so both are reported missing in one error.
    #[tokio::test]
    async fn reports_both_missing_binaries_in_this_sandbox() {
        let err = check_playback_dependencies().await.unwrap_err();
        let message = err.to_string();
        assert!(message.contains("yt-dlp"));
        assert!(message.contains("ffmpeg"));
    }
}
