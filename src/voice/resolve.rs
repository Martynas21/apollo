use std::collections::HashMap;
use std::io::ErrorKind;
use std::time::Duration;

use serde::Deserialize;
use tokio::process::Command;

const STDERR_TRUNCATE_LEN: usize = 200;

const MAX_TRACK_DURATION: Duration = Duration::from_hours(14);

const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(30);

const RESOLVE_TIMEOUT: Duration = Duration::from_secs(30);

const AUDIO_FORMAT_SELECTOR: &str = "ba[abr>0][vcodec=none]/best";

#[derive(Debug)]
pub enum PlaybackError {
    AgeRestricted,
    RegionLocked,
    Unavailable,
    YtDlpMissing,
    Timeout,
    TooLong,
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
                "track is too long to queue (over 14 hours, or a livestream)"
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

fn other_err(e: impl std::fmt::Display) -> PlaybackError {
    PlaybackError::Other(truncate(&e.to_string(), STDERR_TRUNCATE_LEN))
}

pub struct ResolvedStream {
    pub url: String,
    pub headers: Vec<(String, String)>,
}

#[derive(Deserialize)]
struct YtDlpJson {
    url: String,
    #[serde(default)]
    http_headers: HashMap<String, String>,
}

pub async fn resolve_stream(
    video_id: &str,
    duration: Option<Duration>,
    cookies_file: Option<&str>,
) -> Result<ResolvedStream, PlaybackError> {
    if duration.is_none_or(|d| d > MAX_TRACK_DURATION) {
        return Err(PlaybackError::TooLong);
    }

    let url = format!("https://www.youtube.com/watch?v={video_id}");

    let mut command = Command::new("yt-dlp");
    command.kill_on_drop(true);
    command.args(["-j", "-f", AUDIO_FORMAT_SELECTOR, "--no-playlist"]);
    if let Some(cookies_file) = cookies_file {
        command.args(["--cookies", cookies_file]);
    }
    command.arg(&url);

    let output = match tokio::time::timeout(RESOLVE_TIMEOUT, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(e)) if e.kind() == ErrorKind::NotFound => return Err(PlaybackError::YtDlpMissing),
        Ok(Err(e)) => return Err(other_err(e)),
        Err(_elapsed) => return Err(PlaybackError::Timeout),
    };

    if !output.status.success() {
        return Err(classify_ytdlp_stderr(&String::from_utf8_lossy(
            &output.stderr,
        )));
    }

    let parsed: YtDlpJson = serde_json::from_slice(&output.stdout)
        .map_err(|e| PlaybackError::Other(format!("failed to parse yt-dlp output: {e}")))?;

    Ok(ResolvedStream {
        url: parsed.url,
        headers: parsed.http_headers.into_iter().collect(),
    })
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
}
