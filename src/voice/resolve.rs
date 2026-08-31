//! Resolving a `YouTube` video ID into a fully-buffered audio file on disk,
//! plus a pre-flight `yt-dlp` check for user-facing errors on common
//! unplayable videos (age-restricted, region-locked, private/deleted).
//!
//! Actual stream resolution/decoding is delegated to songbird's built-in
//! [`songbird::input::YoutubeDl`] source, which shells out to `yt-dlp -j`
//! itself and streams the resolved URL through symphonia — no separate
//! `ffmpeg` subprocess needed here. The fully-decoded bytes are then written
//! to a file under the shared buffer directory for `apollo-audio-worker` to
//! play — the worker process has no yt-dlp/network access of its own, so
//! every track must be fully resolved on this side first (see
//! [`buffer_track_to_file`]).

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;

use songbird::input::cached::Memory;
use songbird::input::{Input, YoutubeDl};
use tokio::process::Command;
use uuid::Uuid;

/// Truncation length for the raw `yt-dlp` stderr embedded in
/// `PlaybackError::Other`, matching `YouTubeApiError`'s convention in
/// `src/youtube/api.rs`.
const STDERR_TRUNCATE_LEN: usize = 200;

/// Tracks longer than this (or with no known duration, e.g. livestreams —
/// though those are already rejected upstream in `youtube/api.rs`) are
/// refused outright rather than queued. There is no fallback path for a
/// track that's too costly to fully buffer: unlike the old in-process design,
/// `apollo-audio-worker` never touches `yt-dlp`/the network itself, so a
/// track that can't be resolved and written to disk here simply can't play.
const MAX_TRACK_DURATION: Duration = Duration::from_hours(3);

/// Timeout for the `yt-dlp` preflight call, matching `YT_DLP_TIMEOUT` in
/// `src/youtube/api.rs` for the same single-video lookup shape.
const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub enum PlaybackError {
    /// yt-dlp reported the video requires sign-in/age verification.
    AgeRestricted,
    /// yt-dlp reported the video is blocked in the bot's region.
    RegionLocked,
    /// yt-dlp reported the video is private, deleted, or otherwise gone.
    Unavailable,
    /// The `yt-dlp` binary itself could not be found on `PATH`.
    YtDlpMissing,
    /// The `yt-dlp` process didn't finish within its allotted timeout and
    /// was killed.
    Timeout,
    /// The track has no known duration, or is longer than
    /// [`MAX_TRACK_DURATION`] — too costly to fully buffer, and there's no
    /// fallback left to fall back to.
    TooLong,
    /// Any other non-zero exit, carrying a truncated copy of stderr.
    Other(String),
}

impl std::fmt::Display for PlaybackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AgeRestricted => write!(f, "video is age-restricted"),
            Self::RegionLocked => write!(f, "video is not available in this region"),
            Self::Unavailable => write!(f, "video is unavailable (private or deleted)"),
            Self::YtDlpMissing => write!(f, "yt-dlp is not installed or not on PATH"),
            Self::Timeout => write!(f, "yt-dlp timed out"),
            Self::TooLong => write!(
                f,
                "track is too long to queue (over 3 hours, or a livestream)"
            ),
            Self::Other(message) => write!(f, "yt-dlp failed: {message}"),
        }
    }
}

impl std::error::Error for PlaybackError {}

fn truncate(s: &str, max_len: usize) -> String {
    match s.char_indices().nth(max_len) {
        Some((byte_idx, _)) => format!("{}...", &s[..byte_idx]),
        None => s.to_string(),
    }
}

/// Like [`truncate`], but keeps the *last* `max_len` characters. yt-dlp
/// prints non-fatal `WARNING:` lines before the fatal `ERROR:` line that
/// actually explains a non-zero exit, so head-truncating stderr tends to
/// surface a warning while hiding the real cause.
fn truncate_tail(s: &str, max_len: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= max_len {
        return s.to_string();
    }
    let byte_idx = s
        .char_indices()
        .nth(char_count - max_len)
        .map_or(0, |(idx, _)| idx);
    format!("...{}", &s[byte_idx..])
}

/// Classifies a failed `yt-dlp` run's stderr into a [`PlaybackError`].
/// Matching is case-insensitive and substring-based since yt-dlp's exact
/// wording shifts across versions.
pub fn classify_ytdlp_stderr(stderr: &str) -> PlaybackError {
    let lower = stderr.to_lowercase();

    if lower.contains("sign in to confirm your age") || lower.contains("age-restricted") {
        PlaybackError::AgeRestricted
    } else if lower.contains("available in your country")
        || lower.contains("available in your region")
    {
        PlaybackError::RegionLocked
    } else if lower.contains("video unavailable")
        || lower.contains("this video is private")
        || lower.contains("video has been removed")
    {
        PlaybackError::Unavailable
    } else {
        PlaybackError::Other(truncate_tail(stderr.trim_end(), STDERR_TRUNCATE_LEN))
    }
}

/// Runs `yt-dlp -j --no-playlist --simulate <url>` directly (bypassing
/// songbird) purely to classify a video as playable/unplayable *before*
/// handing it to songbird, so callers can give a clear user-facing error
/// instead of a silent/late playback failure once already connected to
/// voice.
///
/// Used by `player`'s `classify_playback_failure` on the failure path: the
/// real resolution goes through songbird's lazy `YoutubeDl` input, whose
/// errors arrive as opaque text, so this re-run is what turns them into a
/// named cause worth showing a user.
///
/// `cookies_file`, if set, is passed through as `--cookies` — increasingly
/// required for `yt-dlp` to get `YouTube` to serve a stream at all (see
/// `Config::yt_dlp_cookies_file`), so the preflight check needs the same
/// credential the real resolution in [`track_input`] will use, or it'll
/// reject videos that would actually have played.
pub async fn preflight_check(
    video_id: &str,
    cookies_file: Option<&str>,
) -> Result<(), PlaybackError> {
    let url = format!("https://www.youtube.com/watch?v={video_id}");

    let mut command = Command::new("yt-dlp");
    command.kill_on_drop(true);
    command.args(["-j", "--no-playlist", "--simulate"]);
    if let Some(cookies_file) = cookies_file {
        command.args(["--cookies", cookies_file]);
    }
    command.arg(&url);

    let output = tokio::time::timeout(PREFLIGHT_TIMEOUT, command.output())
        .await
        .map_err(|_elapsed| PlaybackError::Timeout)?
        .map_err(|e| {
            if e.kind() == ErrorKind::NotFound {
                PlaybackError::YtDlpMissing
            } else {
                PlaybackError::Other(truncate(&e.to_string(), STDERR_TRUNCATE_LEN))
            }
        })?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(classify_ytdlp_stderr(&stderr))
}

/// Builds a lazily-resolved songbird input for a `YouTube` video ID, for
/// [`buffer_track_to_file`] to fully drain into memory. The actual `yt-dlp`
/// invocation and stream resolution happens once something starts reading
/// from this (songbird's `YoutubeDl` source is lazy by design) — never
/// exposed on its own, since nothing plays a lazy `Input` directly anymore
/// (see the module doc comment).
///
/// `cookies_file`, if set, is forwarded to `yt-dlp` via
/// [`YoutubeDl::user_args`] — see `Config::yt_dlp_cookies_file`.
fn track_input(http: reqwest::Client, video_id: &str, cookies_file: Option<&str>) -> Input {
    let url = format!("https://www.youtube.com/watch?v={video_id}");
    let mut ytdl = YoutubeDl::new(http, url);
    if let Some(cookies_file) = cookies_file {
        ytdl = ytdl.user_args(vec!["--cookies".to_string(), cookies_file.to_string()]);
    }
    ytdl.into()
}

/// Fully resolves and downloads a track, writing the decoded bytes to a
/// fresh file under `buffer_dir` and returning its path. This is what makes
/// playback immune to network blips — the whole track is in hand before
/// `apollo-audio-worker` ever starts playing it, rather than reading live off
/// the network — and, since the worker process has no `yt-dlp`/network
/// access of its own, it's also the only way a track ever gets to it at all.
///
/// `duration` gates this outright: `None` (livestreams — already rejected
/// upstream in `youtube/api.rs`, so this shouldn't happen in practice) or
/// anything past [`MAX_TRACK_DURATION`] is refused rather than attempted,
/// since there's no cheaper fallback left to attempt it with.
/// Truncates and wraps any displayable error as [`PlaybackError::Other`] —
/// the common shape every fallible step in [`buffer_track_to_file`] maps to.
fn other_err(e: impl std::fmt::Display) -> PlaybackError {
    PlaybackError::Other(truncate(&e.to_string(), STDERR_TRUNCATE_LEN))
}

pub async fn buffer_track_to_file(
    http: reqwest::Client,
    video_id: &str,
    duration: Option<Duration>,
    cookies_file: Option<&str>,
    buffer_dir: &Path,
) -> Result<PathBuf, PlaybackError> {
    if duration.is_none_or(|d| d > MAX_TRACK_DURATION) {
        return Err(PlaybackError::TooLong);
    }

    let lazy = track_input(http, video_id, cookies_file);
    let memory = Memory::new(lazy).await.map_err(other_err)?;

    let path = buffer_dir.join(format!("{}.audio", Uuid::new_v4()));
    let write_path = path.clone();
    // `Memory`/`Catcher` only fills lazily as a consumer reads through it —
    // copying it straight into the destination file both forces that fill
    // and writes it out, without ever holding the whole track in memory a
    // second time as a `Vec<u8>`.
    tokio::task::spawn_blocking(move || {
        let mut reader = memory.new_handle();
        let mut file = std::fs::File::create(&write_path)?;
        std::io::copy(&mut reader, &mut file)?;
        std::io::Result::Ok(())
    })
    .await
    .map_err(other_err)?
    .map_err(other_err)?;

    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_age_restriction() {
        let stderr = "ERROR: [youtube] dQw4w9WgXcQ: Sign in to confirm your age. This video may be inappropriate for some users.";
        assert!(matches!(
            classify_ytdlp_stderr(stderr),
            PlaybackError::AgeRestricted
        ));
    }

    #[test]
    fn classifies_age_restriction_alt_phrasing() {
        let stderr = "ERROR: [youtube] abc123: This video is age-restricted and requires signing in to confirm your age.";
        assert!(matches!(
            classify_ytdlp_stderr(stderr),
            PlaybackError::AgeRestricted
        ));
    }

    #[test]
    fn classifies_region_lock_country_phrasing() {
        let stderr = "ERROR: [youtube] xyz789: The uploader has not made this video available in your country.";
        assert!(matches!(
            classify_ytdlp_stderr(stderr),
            PlaybackError::RegionLocked
        ));
    }

    #[test]
    fn classifies_region_lock_region_phrasing() {
        let stderr = "ERROR: [youtube] xyz789: This video is not available in your region.";
        assert!(matches!(
            classify_ytdlp_stderr(stderr),
            PlaybackError::RegionLocked
        ));
    }

    #[test]
    fn classifies_private_video() {
        let stderr = "ERROR: [youtube] priv123: This video is private.";
        assert!(matches!(
            classify_ytdlp_stderr(stderr),
            PlaybackError::Unavailable
        ));
    }

    #[test]
    fn classifies_deleted_video() {
        let stderr = "ERROR: [youtube] del123: Video unavailable. This video has been removed by the uploader.";
        assert!(matches!(
            classify_ytdlp_stderr(stderr),
            PlaybackError::Unavailable
        ));
    }

    #[test]
    fn classifies_generic_video_unavailable() {
        let stderr = "ERROR: [youtube] gone123: Video unavailable";
        assert!(matches!(
            classify_ytdlp_stderr(stderr),
            PlaybackError::Unavailable
        ));
    }

    #[test]
    fn classification_is_case_insensitive() {
        let stderr = "ERROR: SIGN IN TO CONFIRM YOUR AGE.";
        assert!(matches!(
            classify_ytdlp_stderr(stderr),
            PlaybackError::AgeRestricted
        ));
    }

    #[test]
    fn unrecognized_message_falls_through_to_other() {
        let stderr = "ERROR: [youtube] weird123: Some completely novel yt-dlp failure mode we haven't seen before.";
        match classify_ytdlp_stderr(stderr) {
            PlaybackError::Other(message) => {
                assert!(message.contains("novel yt-dlp failure mode"));
            }
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn other_message_keeps_final_error_over_leading_warning() {
        // Mirrors a real yt-dlp failure: a long non-fatal warning first,
        // then the short `ERROR:` line that actually explains the
        // non-zero exit. Head-truncation would show only the warning.
        let stderr = format!(
            "WARNING: [youtube] {}\nERROR: [youtube] NgsWGfUlwJI: Requested format is not available.\n",
            "x".repeat(300)
        );
        match classify_ytdlp_stderr(&stderr) {
            PlaybackError::Other(message) => {
                assert!(message.contains("Requested format is not available"));
            }
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn other_message_is_truncated() {
        let long_stderr = "x".repeat(500);
        match classify_ytdlp_stderr(&long_stderr) {
            PlaybackError::Other(message) => {
                assert!(message.len() <= STDERR_TRUNCATE_LEN + 3);
            }
            other => panic!("expected Other, got {other:?}"),
        }
    }

    // `preflight_check` hardcodes the `yt-dlp` program name, so its
    // spawn-failure (`YtDlpMissing`) branch can't be exercised
    // deterministically without either a real missing binary (no longer
    // true now that yt-dlp is actually installed) or a live network call
    // (a real video ID, unsuitable for a unit test). The same
    // spawn-failure-detection logic is covered deterministically by
    // `voice::tests::binary_runnable_reports_false_for_nonexistent_program`;
    // this path is otherwise exercised by manual E2E testing
    // (`docs/e2e-test-plan.md`).
}
