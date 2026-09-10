use std::collections::HashMap;
use std::time::Duration;

use serde::Deserialize;

use crate::youtube::ytdlp::{STDERR_TRUNCATE_LEN, YtDlp, YtDlpError, truncate_tail};

const MAX_TRACK_DURATION: Duration = Duration::from_hours(14);

const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(30);

const RESOLVE_TIMEOUT: Duration = Duration::from_secs(30);

const AUDIO_FORMAT_SELECTOR: &str = "ba[abr>0][vcodec=none]/best";

#[derive(Debug, thiserror::Error)]
pub enum PlaybackError {
    #[error("video is age-restricted")]
    AgeRestricted,
    #[error("video is not available in this region")]
    RegionLocked,
    #[error("video is unavailable (private or deleted)")]
    Unavailable,
    #[error("yt-dlp is not installed or not on PATH")]
    YtDlpMissing,
    #[error("yt-dlp timed out")]
    Timeout,
    #[error("track is too long to queue (over 14 hours, or a livestream)")]
    TooLong,
    #[error("yt-dlp failed: {0}")]
    Other(String),
}

fn truncate(s: &str, max_len: usize) -> String {
    match s.char_indices().nth(max_len) {
        Some((byte_idx, _)) => format!("{}...", &s[..byte_idx]),
        None => s.to_string(),
    }
}

fn map_ytdlp_error(err: YtDlpError) -> PlaybackError {
    match err {
        YtDlpError::Missing => PlaybackError::YtDlpMissing,
        YtDlpError::Timeout => PlaybackError::Timeout,
        YtDlpError::Spawn(message) => PlaybackError::Other(truncate(&message, STDERR_TRUNCATE_LEN)),
        YtDlpError::Failed(stderr) => classify_ytdlp_stderr(&stderr),
    }
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
    let ytdlp = YtDlp::new(cookies_file.map(str::to_string));
    let args = ["-j", "--no-playlist", "--simulate", url.as_str()];

    ytdlp
        .run(&args, PREFLIGHT_TIMEOUT)
        .await
        .map(|_stdout| ())
        .map_err(map_ytdlp_error)
}

#[derive(Debug)]
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

fn parse_resolved_stream(stdout: &str) -> Result<ResolvedStream, PlaybackError> {
    let parsed: YtDlpJson = serde_json::from_str(stdout)
        .map_err(|e| PlaybackError::Other(format!("failed to parse yt-dlp output: {e}")))?;

    Ok(ResolvedStream {
        url: parsed.url,
        headers: parsed.http_headers.into_iter().collect(),
    })
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
    let ytdlp = YtDlp::new(cookies_file.map(str::to_string));
    let args = [
        "-j",
        "-f",
        AUDIO_FORMAT_SELECTOR,
        "--no-playlist",
        url.as_str(),
    ];

    let stdout = ytdlp
        .run(&args, RESOLVE_TIMEOUT)
        .await
        .map_err(map_ytdlp_error)?;

    parse_resolved_stream(&stdout)
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

    #[test]
    fn parse_resolved_stream_extracts_url_and_headers() {
        let stdout = r#"{"url": "https://example.com/stream.m4a", "http_headers": {"User-Agent": "yt-dlp"}}"#;
        let resolved = parse_resolved_stream(stdout).unwrap();
        assert_eq!(resolved.url, "https://example.com/stream.m4a");
        assert_eq!(
            resolved.headers,
            vec![("User-Agent".to_string(), "yt-dlp".to_string())]
        );
    }

    #[test]
    fn parse_resolved_stream_defaults_headers_to_empty_when_absent() {
        let stdout = r#"{"url": "https://example.com/stream.m4a"}"#;
        let resolved = parse_resolved_stream(stdout).unwrap();
        assert!(resolved.headers.is_empty());
    }

    #[test]
    fn parse_resolved_stream_errors_on_malformed_json() {
        match parse_resolved_stream("not json") {
            Err(PlaybackError::Other(message)) => {
                assert!(message.contains("failed to parse yt-dlp output"));
            }
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn parse_resolved_stream_errors_when_url_field_missing() {
        assert!(parse_resolved_stream(r#"{"http_headers": {}}"#).is_err());
    }

    #[test]
    fn map_ytdlp_error_missing_maps_to_missing() {
        assert!(matches!(
            map_ytdlp_error(YtDlpError::Missing),
            PlaybackError::YtDlpMissing
        ));
    }

    #[test]
    fn map_ytdlp_error_timeout_maps_to_timeout() {
        assert!(matches!(
            map_ytdlp_error(YtDlpError::Timeout),
            PlaybackError::Timeout
        ));
    }

    #[test]
    fn map_ytdlp_error_spawn_head_truncates_into_other() {
        let long = "x".repeat(500);
        match map_ytdlp_error(YtDlpError::Spawn(long)) {
            PlaybackError::Other(message) => {
                assert!(message.ends_with("..."));
                assert!(message.len() <= STDERR_TRUNCATE_LEN + 3);
            }
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn map_ytdlp_error_failed_runs_through_classification() {
        let stderr = "ERROR: [youtube] dQw4w9WgXcQ: Sign in to confirm your age.";
        assert!(matches!(
            map_ytdlp_error(YtDlpError::Failed(stderr.to_string())),
            PlaybackError::AgeRestricted
        ));
    }
}
