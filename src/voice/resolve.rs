//! Resolving a YouTube video ID into a playable songbird [`Input`], plus a
//! pre-flight `yt-dlp` check for user-facing errors on common unplayable
//! videos (age-restricted, region-locked, private/deleted).
//!
//! Actual stream resolution/decoding is delegated to songbird's built-in
//! [`songbird::input::YoutubeDl`] source, which shells out to `yt-dlp -j`
//! itself and streams the resolved URL through symphonia — no separate
//! `ffmpeg` subprocess needed for playback.

use std::io::ErrorKind;
use std::time::Duration;

use songbird::input::cached::Memory;
use songbird::input::{Input, YoutubeDl};
use tokio::process::Command;

/// Truncation length for the raw `yt-dlp` stderr embedded in
/// `PlaybackError::Other`, matching `YouTubeApiError`'s convention in
/// `src/youtube/api.rs`.
const STDERR_TRUNCATE_LEN: usize = 200;

/// Tracks longer than this (or with no known duration, e.g. livestreams)
/// skip full pre-buffering and fall back to [`track_input`]'s live-streaming
/// path, to bound worst-case memory use. See `cached_track_input`.
const MAX_BUFFERED_TRACK_DURATION: Duration = Duration::from_secs(20 * 60);

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
    /// Any other non-zero exit, carrying a truncated copy of stderr.
    Other(String),
}

impl std::fmt::Display for PlaybackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlaybackError::AgeRestricted => write!(f, "video is age-restricted"),
            PlaybackError::RegionLocked => write!(f, "video is not available in this region"),
            PlaybackError::Unavailable => write!(f, "video is unavailable (private or deleted)"),
            PlaybackError::YtDlpMissing => write!(f, "yt-dlp is not installed or not on PATH"),
            PlaybackError::Other(message) => write!(f, "yt-dlp failed: {message}"),
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
        PlaybackError::Other(truncate(stderr, STDERR_TRUNCATE_LEN))
    }
}

/// Runs `yt-dlp -j --no-playlist --simulate <url>` directly (bypassing
/// songbird) purely to classify a video as playable/unplayable *before*
/// handing it to songbird, so callers can give a clear user-facing error
/// instead of a silent/late playback failure once already connected to
/// voice.
///
/// `cookies_file`, if set, is passed through as `--cookies` — increasingly
/// required for `yt-dlp` to get YouTube to serve a stream at all (see
/// `Config::yt_dlp_cookies_file`), so the preflight check needs the same
/// credential the real resolution in [`track_input`] will use, or it'll
/// reject videos that would actually have played.
#[allow(dead_code)]
pub async fn preflight_check(
    video_id: &str,
    cookies_file: Option<&str>,
) -> Result<(), PlaybackError> {
    let url = format!("https://www.youtube.com/watch?v={video_id}");

    let mut command = Command::new("yt-dlp");
    command.args(["-j", "--no-playlist", "--simulate"]);
    if let Some(cookies_file) = cookies_file {
        command.args(["--cookies", cookies_file]);
    }
    command.arg(&url);

    let output = command.output().await.map_err(|e| {
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

/// Builds a lazily-resolved songbird input for a YouTube video ID.
/// The actual `yt-dlp` invocation and stream resolution happens when
/// songbird's driver plays this input, not here (songbird's `YoutubeDl`
/// source is lazy by design).
///
/// `cookies_file`, if set, is forwarded to `yt-dlp` via
/// [`YoutubeDl::user_args`] — see `Config::yt_dlp_cookies_file`.
#[allow(dead_code)]
pub fn track_input(
    http: oauth2::reqwest::Client,
    video_id: &str,
    cookies_file: Option<&str>,
) -> Input {
    let url = format!("https://www.youtube.com/watch?v={video_id}");
    let mut ytdl = YoutubeDl::new(http, url);
    if let Some(cookies_file) = cookies_file {
        ytdl = ytdl.user_args(vec!["--cookies".to_string(), cookies_file.to_string()]);
    }
    ytdl.into()
}

/// Builds a fully pre-buffered songbird input: the entire track is
/// downloaded into memory (via songbird's [`Memory`] cache) before this
/// returns, so playback never reads from the network. This is what makes
/// playback immune to network blips — songbird's live-streaming path has
/// only a small, fixed-size buffer, and a stall long enough to drain it
/// causes an audible speed-up as the driver's scheduler bursts packets to
/// catch up (see the plan doc/README for the full root cause).
///
/// `duration` is used to skip pre-buffering for livestreams (`None` — no
/// fixed length to download ahead of) and for tracks longer than
/// [`MAX_BUFFERED_TRACK_DURATION`], to bound memory use; both fall back to
/// [`track_input`]'s live-streaming behavior and remain exposed to the
/// original bug.
pub async fn cached_track_input(
    http: oauth2::reqwest::Client,
    video_id: &str,
    duration: Option<Duration>,
    cookies_file: Option<&str>,
) -> Result<Input, PlaybackError> {
    let lazy = track_input(http, video_id, cookies_file);

    let too_long = duration.is_none_or(|d| d > MAX_BUFFERED_TRACK_DURATION);
    if too_long {
        return Ok(lazy);
    }

    let memory = Memory::new(lazy)
        .await
        .map_err(|e| PlaybackError::Other(truncate(&e.to_string(), STDERR_TRUNCATE_LEN)))?;

    // `Memory`/`Catcher` only fill lazily as a consumer reads through them —
    // drive a cloned handle to read everything up front (it shares the same
    // backing store) so playback later reads purely from RAM.
    let mut loader = memory.raw.new_handle();
    tokio::task::spawn_blocking(move || loader.load_all())
        .await
        .map_err(|e| PlaybackError::Other(truncate(&e.to_string(), STDERR_TRUNCATE_LEN)))?;

    Ok(memory.into())
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
