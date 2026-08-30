//! Listing `YouTube`'s personalized "Mix" (radio) playlist for a seed video
//! (`https://www.youtube.com/watch?v=<id>&list=RD<id>`), via `yt-dlp
//! --flat-playlist` — the official `YouTube` Data API v3 dropped
//! `search.list`'s `relatedToVideoId` parameter years ago, so there is no
//! documented "related videos" endpoint left. This is the same trust
//! boundary the rest of the bot already relies on for stream resolution
//! (`resolve.rs`'s `YoutubeDl`/`preflight_check`) — `yt-dlp` scraping the
//! regular frontend, not a versioned API.
//!
//! Deliberately extracts only bare video ids here — `--flat-playlist`
//! output isn't trustworthy enough (field presence/shape drifts across
//! `yt-dlp` versions and extractors) to build a full `Track` from directly.
//! Titles/channels/durations are hydrated separately through the real Data
//! API (`crate::youtube::api::YouTubeClient::hydrate_videos`), so
//! radio-picked tracks get the same metadata quality as everything else the
//! bot plays.

use std::io::ErrorKind;
use std::time::Duration;

use tokio::process::Command;

/// Max number of Mix entries to ask `yt-dlp` to list. Only a handful of
/// unplayed candidates are ever needed per refill (see
/// `crate::voice::player`'s `maybe_spawn_radio_refill`), and a smaller
/// `--playlist-end` keeps the subprocess itself fast.
const MIX_LISTING_LIMIT: usize = 20;

/// Timeout for the mix-listing `yt-dlp` call, matching `YT_DLP_TIMEOUT` in
/// `src/youtube/api.rs` for the same single-target lookup shape.
const MIX_LISTING_TIMEOUT: Duration = Duration::from_secs(30);

/// Truncation length for stderr embedded in `RadioError::YtDlpFailed`,
/// matching `PlaybackError::Other`'s convention in `resolve.rs`.
const STDERR_TRUNCATE_LEN: usize = 200;

#[derive(Debug)]
pub enum RadioError {
    /// The `yt-dlp` binary itself could not be found on `PATH`.
    YtDlpMissing,
    /// yt-dlp exited non-zero — e.g. the seed video's Mix isn't available
    /// (age-restricted/region-locked/deleted seed, or no Mix generated).
    YtDlpFailed(String),
    /// yt-dlp exited zero but produced no parseable entries at all.
    Empty,
    /// The `yt-dlp` process didn't finish within its allotted timeout and
    /// was killed.
    Timeout,
}

impl std::fmt::Display for RadioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::YtDlpMissing => write!(f, "yt-dlp is not installed or not on PATH"),
            Self::YtDlpFailed(message) => write!(f, "yt-dlp failed to list mix: {message}"),
            Self::Empty => write!(f, "mix listing returned no usable entries"),
            Self::Timeout => write!(f, "yt-dlp timed out"),
        }
    }
}

impl std::error::Error for RadioError {}

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

#[derive(serde::Deserialize)]
struct FlatPlaylistEntry {
    id: Option<String>,
}

/// Pure, unit-testable: parses `yt-dlp -j --flat-playlist`'s JSON-lines
/// stdout into an ordered list of video ids, skipping any line that isn't
/// valid JSON or lacks a non-empty `id` field (happens routinely — e.g.
/// deleted-video placeholder entries — so this is not logged as a warning).
fn parse_flat_playlist_ids(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            serde_json::from_str::<FlatPlaylistEntry>(line)
                .ok()?
                .id
                .filter(|id| !id.is_empty())
        })
        .collect()
}

/// Lists up to [`MIX_LISTING_LIMIT`] video ids from `seed_video_id`'s
/// `YouTube`-generated Mix, via `yt-dlp -j --flat-playlist`. Order is
/// preserved from yt-dlp's own output (`YouTube`'s own relevance ordering)
/// — callers should filter, not reorder.
///
/// `cookies_file`, if set, is passed through as `--cookies` — same
/// credential `resolve.rs`'s `preflight_check`/`track_input` use.
pub async fn list_mix_video_ids(
    seed_video_id: &str,
    cookies_file: Option<&str>,
) -> Result<Vec<String>, RadioError> {
    let url = format!("https://www.youtube.com/watch?v={seed_video_id}&list=RD{seed_video_id}");

    let mut command = Command::new("yt-dlp");
    command.kill_on_drop(true);
    command.args([
        "-j",
        "--flat-playlist",
        "--no-warnings",
        "--playlist-end",
        &MIX_LISTING_LIMIT.to_string(),
    ]);
    if let Some(cookies_file) = cookies_file {
        command.args(["--cookies", cookies_file]);
    }
    command.arg(&url);

    let output = tokio::time::timeout(MIX_LISTING_TIMEOUT, command.output())
        .await
        .map_err(|_elapsed| RadioError::Timeout)?
        .map_err(|e| {
            if e.kind() == ErrorKind::NotFound {
                RadioError::YtDlpMissing
            } else {
                RadioError::YtDlpFailed(truncate_tail(&e.to_string(), STDERR_TRUNCATE_LEN))
            }
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(RadioError::YtDlpFailed(truncate_tail(
            stderr.trim_end(),
            STDERR_TRUNCATE_LEN,
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let ids = parse_flat_playlist_ids(&stdout);
    if ids.is_empty() {
        Err(RadioError::Empty)
    } else {
        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_well_formed_multiline_output() {
        let stdout = "{\"id\": \"abc123\"}\n{\"id\": \"def456\"}\n";
        assert_eq!(
            parse_flat_playlist_ids(stdout),
            vec!["abc123".to_string(), "def456".to_string()]
        );
    }

    #[test]
    fn skips_entries_with_null_or_missing_id() {
        let stdout = "{\"id\": null}\n{\"title\": \"no id field\"}\n{\"id\": \"real123\"}\n";
        assert_eq!(parse_flat_playlist_ids(stdout), vec!["real123".to_string()]);
    }

    #[test]
    fn skips_non_json_lines_without_failing_the_rest() {
        let stdout = "WARNING: something yt-dlp printed\n{\"id\": \"real123\"}\n";
        assert_eq!(parse_flat_playlist_ids(stdout), vec!["real123".to_string()]);
    }

    #[test]
    fn ignores_blank_lines() {
        let stdout = "\n{\"id\": \"real123\"}\n\n   \n";
        assert_eq!(parse_flat_playlist_ids(stdout), vec!["real123".to_string()]);
    }

    #[test]
    fn empty_id_is_treated_like_missing() {
        let stdout = "{\"id\": \"\"}\n{\"id\": \"real123\"}\n";
        assert_eq!(parse_flat_playlist_ids(stdout), vec!["real123".to_string()]);
    }

    #[test]
    fn empty_stdout_yields_empty_vec() {
        assert_eq!(parse_flat_playlist_ids(""), Vec::<String>::new());
    }

    // `list_mix_video_ids` itself (spawning a real `yt-dlp` process) is
    // exercised only by manual E2E testing — mirrors `resolve.rs`'s note on
    // `preflight_check`'s spawn-failure path being untestable
    // deterministically without either a missing binary or a live network
    // call.
}
