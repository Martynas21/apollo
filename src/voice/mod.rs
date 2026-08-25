//! Songbird-based voice connection and audio playback pipeline.
//!
//! Stream resolution (`src/voice/resolve.rs`) is handled by songbird's
//! built-in `YoutubeDl` input source, which shells out to `yt-dlp` and
//! decodes the resolved stream via symphonia — no separate `ffmpeg`
//! subprocess in the playback path itself.

pub mod panel;
pub mod player;
pub mod radio;
pub mod resolve;

// Not consumed yet — Phase 6 commands wire these into `/play` and friends.
#[allow(unused_imports)]
pub use player::{PlayerError, PlayerRegistry, QueueSnapshot, QueuedTrack};
#[allow(unused_imports)]
pub use resolve::{PlaybackError, preflight_check, track_input};

use std::time::Duration;

use tokio::process::Command;

/// How long a dependency's `--version` probe gets before it's treated as
/// broken. Generous for what should be an instant subprocess — the point is
/// to bound a *hang* (a wrapper script waiting on input, a network mount
/// gone stale), not to race a slow machine.
const DEPENDENCY_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Longest version banner kept for the startup log. `ffmpeg -version` opens
/// with a single long line and then dumps its whole build configuration; one
/// truncated line is all that's useful.
const VERSION_LOG_LEN: usize = 120;

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

/// First line of a version banner, truncated for logging.
fn version_banner(stdout: &str) -> String {
    let line = stdout.lines().next().unwrap_or_default().trim();
    match line.char_indices().nth(VERSION_LOG_LEN) {
        Some((byte_idx, _)) => format!("{}...", &line[..byte_idx]),
        None => line.to_string(),
    }
}

async fn binary_runnable(program: &str, version_arg: &str) -> bool {
    binary_runnable_within(program, version_arg, DEPENDENCY_PROBE_TIMEOUT).await
}

/// The bounded probe behind [`binary_runnable`], with the timeout as a
/// parameter so it can be tested without a ten-second wait.
///
/// The timeout is the point: `output()` on a binary that never exits (a
/// wrapper script blocked on stdin, a stale network mount) otherwise hangs
/// startup forever, before logging is interesting enough to say why.
/// `kill_on_drop` makes sure the abandoned child is reaped rather than left
/// behind when the timeout fires.
async fn binary_runnable_within(program: &str, version_arg: &str, timeout: Duration) -> bool {
    let probe = Command::new(program)
        .arg(version_arg)
        .kill_on_drop(true)
        .output();

    // Any spawn failure (not just `NotFound`) counts as "missing" here —
    // there's no useful distinction to surface for a startup check — but the
    // *reason* is worth logging, since a present-but-broken binary otherwise
    // fails with nothing to go on.
    match tokio::time::timeout(timeout, probe).await {
        Ok(Ok(output)) if output.status.success() => {
            tracing::info!(
                program,
                version = version_banner(&String::from_utf8_lossy(&output.stdout)),
                "playback dependency ok"
            );
            true
        }
        Ok(Ok(output)) => {
            tracing::warn!(
                program,
                status = %output.status,
                stderr = version_banner(&String::from_utf8_lossy(&output.stderr)),
                "playback dependency exited non-zero"
            );
            false
        }
        Ok(Err(err)) => {
            tracing::warn!(program, %err, "playback dependency could not be run");
            false
        }
        Err(_) => {
            tracing::warn!(
                program,
                timeout_secs = timeout.as_secs(),
                "playback dependency check timed out"
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Uses a program name that can never exist on any machine, rather than
    /// relying on `yt-dlp`/`ffmpeg` being absent from this particular
    /// sandbox — that assumption doesn't hold once a dev actually installs
    /// them, as happened here.
    #[tokio::test]
    async fn binary_runnable_reports_false_for_nonexistent_program() {
        assert!(!binary_runnable("apollo-test-definitely-not-a-real-binary", "--version").await);
    }

    /// A dependency that never exits must fail the check rather than hang
    /// startup. Uses a short explicit timeout so the test doesn't sit
    /// through the real ten-second bound.
    #[tokio::test]
    async fn binary_runnable_reports_false_for_a_hanging_program() {
        let hung = binary_runnable_within("sleep", "30", Duration::from_millis(100)).await;
        assert!(!hung);
    }

    #[tokio::test]
    async fn binary_runnable_reports_false_for_a_nonzero_exit() {
        assert!(!binary_runnable_within("false", "", Duration::from_secs(5)).await);
    }

    #[test]
    fn version_banner_keeps_only_a_truncated_first_line() {
        let banner = version_banner("yt-dlp 2024.08.06\nsome other line\n");
        assert_eq!(banner, "yt-dlp 2024.08.06");

        let long = format!("ffmpeg version {}\nconfiguration: ...", "x".repeat(300));
        let banner = version_banner(&long);
        assert!(banner.ends_with("..."));
        assert!(banner.chars().count() <= VERSION_LOG_LEN + 3);
    }
}
