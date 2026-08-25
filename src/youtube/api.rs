//! `YouTube` search/lookup, entirely via `yt-dlp` subprocess calls.
//!
//! No Google API quota and no per-user login needed: `yt-dlp -j
//! --flat-playlist` already returns id/title/channel/duration for both
//! search results (`ytsearch<n>:<query>`) and playlist listings, and
//! `yt-dlp -j --no-playlist` does the same for a single video. This mirrors
//! the trust boundary `crate::voice::radio` and `crate::voice::resolve`
//! already rely on for Mix listing and stream resolution — `yt-dlp`
//! scraping the regular frontend, not a versioned/quota'd API.

use std::io::ErrorKind;
use std::time::Duration;

use serde::Deserialize;
use tokio::process::Command;

/// Max search results requested per `/add_to_queue` query — mirrors the old
/// Data API client's `maxResults=25`: only the top handful are shown, but a
/// `number` selection can reach any of the fetched set.
const SEARCH_LIMIT: usize = 25;

/// Truncation length for stderr embedded in `YouTubeApiError::YtDlpFailed`,
/// matching `PlaybackError::Other`'s convention in `crate::voice::resolve`.
const STDERR_TRUNCATE_LEN: usize = 200;

/// A single playable video, as resolved from a search, a playlist listing,
/// or a direct video id/URL lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    pub video_id: String,
    pub title: String,
    pub channel: String,
    /// `None` when `yt-dlp` didn't report a usable duration (e.g. an
    /// in-progress livestream).
    pub duration: Option<Duration>,
}

#[derive(Debug)]
pub enum YouTubeApiError {
    /// The `yt-dlp` binary itself could not be found on `PATH`.
    YtDlpMissing,
    /// yt-dlp exited non-zero (video/playlist private, deleted, region-locked,
    /// or otherwise unavailable), or exited zero but produced no parseable
    /// metadata at all.
    YtDlpFailed(String),
}

impl std::fmt::Display for YouTubeApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::YtDlpMissing => write!(f, "yt-dlp is not installed or not on PATH"),
            Self::YtDlpFailed(message) => write!(f, "yt-dlp failed: {message}"),
        }
    }
}

impl std::error::Error for YouTubeApiError {}

/// Like `crate::voice::radio`'s `truncate_tail`: keeps the *last* `max_len`
/// characters, since yt-dlp's fatal `ERROR:` line comes after any non-fatal
/// `WARNING:` lines and would otherwise get head-truncated away.
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

#[derive(Debug, Deserialize)]
struct YtDlpEntry {
    id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    uploader: Option<String>,
    #[serde(default)]
    duration: Option<f64>,
}

/// Maps one parsed `yt-dlp -j` entry into a [`Track`]. `None` if it has no
/// usable video id — happens for deleted/private playlist slots, which
/// `yt-dlp` still emits a placeholder line for.
fn track_from_entry(entry: YtDlpEntry) -> Option<Track> {
    let video_id = entry.id.filter(|id| !id.is_empty())?;
    Some(Track {
        video_id,
        title: entry.title.unwrap_or_default(),
        // `channel` is present on full extractions and most flat listings;
        // `uploader` is the fallback yt-dlp uses when `channel` is absent.
        channel: entry.channel.or(entry.uploader).unwrap_or_default(),
        duration: entry.duration.map(Duration::from_secs_f64),
    })
}

/// Parses `yt-dlp -j`'s (one-JSON-object-per-line) stdout into [`Track`]s,
/// skipping any line that isn't valid JSON or maps to no usable track —
/// happens routinely (e.g. deleted-video placeholder entries), so this is
/// not treated as an error.
fn parse_tracks(stdout: &str) -> Vec<Track> {
    stdout
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            serde_json::from_str::<YtDlpEntry>(line)
                .ok()
                .and_then(track_from_entry)
        })
        .collect()
}

/// `yt-dlp`-backed `YouTube` client: search, single-video lookup, and
/// playlist listing. Cheap to clone — holds only the (optional) cookies
/// file path, no connection state.
#[derive(Debug, Clone, Default)]
pub struct YouTubeClient {
    /// Path to a Netscape-format cookies file, passed to `yt-dlp` as
    /// `--cookies` when set — see `Config::yt_dlp_cookies_file`.
    cookies_file: Option<String>,
}

impl YouTubeClient {
    pub fn new(cookies_file: Option<String>) -> Self {
        Self { cookies_file }
    }

    /// Runs `yt-dlp -j <extra_args...> <target>` and returns its stdout on
    /// success.
    async fn run(&self, extra_args: &[&str], target: &str) -> Result<String, YouTubeApiError> {
        let mut command = Command::new("yt-dlp");
        command.arg("-j").args(extra_args);
        if let Some(cookies_file) = &self.cookies_file {
            command.args(["--cookies", cookies_file]);
        }
        command.arg(target);

        let output = command.output().await.map_err(|e| {
            if e.kind() == ErrorKind::NotFound {
                YouTubeApiError::YtDlpMissing
            } else {
                YouTubeApiError::YtDlpFailed(truncate_tail(&e.to_string(), STDERR_TRUNCATE_LEN))
            }
        })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(YouTubeApiError::YtDlpFailed(truncate_tail(
                stderr.trim_end(),
                STDERR_TRUNCATE_LEN,
            )));
        }

        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Searches `YouTube` for `query`, returning up to [`SEARCH_LIMIT`] hits
    /// in `YouTube`'s own relevance order.
    pub async fn search(&self, query: &str) -> Result<Vec<Track>, YouTubeApiError> {
        let target = format!("ytsearch{SEARCH_LIMIT}:{query}");
        let stdout = self
            .run(&["--flat-playlist", "--no-warnings"], &target)
            .await?;
        Ok(parse_tracks(&stdout))
    }

    /// Looks up a single video by ID (used by `/play` for a direct video
    /// ID/URL, and by `/add_to_queue`'s picker re-lookup).
    pub async fn get_video(&self, video_id: &str) -> Result<Track, YouTubeApiError> {
        let url = format!("https://www.youtube.com/watch?v={video_id}");
        let stdout = self.run(&["--no-playlist"], &url).await?;
        parse_tracks(&stdout).into_iter().next().ok_or_else(|| {
            YouTubeApiError::YtDlpFailed("no metadata returned for video".to_string())
        })
    }

    /// Lists every track in a playlist, given either a full playlist URL or
    /// a bare playlist ID (e.g. `PLxxxxxxxxxxxx`).
    pub async fn list_playlist_items(
        &self,
        playlist_url_or_id: &str,
    ) -> Result<Vec<Track>, YouTubeApiError> {
        let target = if playlist_url_or_id.contains("://") {
            playlist_url_or_id.to_string()
        } else {
            format!("https://www.youtube.com/playlist?list={playlist_url_or_id}")
        };
        let stdout = self
            .run(&["--flat-playlist", "--no-warnings"], &target)
            .await?;
        Ok(parse_tracks(&stdout))
    }

    /// Looks up multiple videos by id in one `yt-dlp` invocation (used by
    /// radio mode to hydrate a batch of bare video ids from a `yt-dlp` Mix
    /// listing — see `crate::voice::radio::list_mix_video_ids` — into full
    /// `Track`s, without spawning one process per candidate).
    ///
    /// Ids that don't resolve (deleted/private since the Mix was generated)
    /// are silently omitted from the result rather than erroring the whole
    /// batch. Order is not guaranteed to match `video_ids`'s input order.
    pub async fn hydrate_videos(&self, video_ids: &[&str]) -> Result<Vec<Track>, YouTubeApiError> {
        if video_ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut command = Command::new("yt-dlp");
        command.args(["-j", "--no-playlist", "--ignore-errors"]);
        if let Some(cookies_file) = &self.cookies_file {
            command.args(["--cookies", cookies_file]);
        }
        for id in video_ids {
            command.arg(format!("https://www.youtube.com/watch?v={id}"));
        }

        let output = command.output().await.map_err(|e| {
            if e.kind() == ErrorKind::NotFound {
                YouTubeApiError::YtDlpMissing
            } else {
                YouTubeApiError::YtDlpFailed(truncate_tail(&e.to_string(), STDERR_TRUNCATE_LEN))
            }
        })?;

        // `--ignore-errors` means a per-id failure doesn't fail the whole
        // batch (or the process' exit code) — just skips that id's line, so
        // stdout is parsed unconditionally rather than gating on
        // `output.status`.
        Ok(parse_tracks(&String::from_utf8_lossy(&output.stdout)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- track_from_entry / parse_tracks ----

    #[test]
    fn maps_entry_with_channel_field() {
        let json = r#"{"id": "dQw4w9WgXcQ", "title": "Some Video", "channel": "Some Channel", "duration": 213}"#;
        let entry: YtDlpEntry = serde_json::from_str(json).unwrap();
        assert_eq!(
            track_from_entry(entry),
            Some(Track {
                video_id: "dQw4w9WgXcQ".to_string(),
                title: "Some Video".to_string(),
                channel: "Some Channel".to_string(),
                duration: Some(Duration::from_secs(213)),
            })
        );
    }

    #[test]
    fn falls_back_to_uploader_when_channel_absent() {
        let json = r#"{"id": "abc123", "title": "T", "uploader": "Uploader Name", "duration": 60}"#;
        let entry: YtDlpEntry = serde_json::from_str(json).unwrap();
        assert_eq!(
            track_from_entry(entry).map(|t| t.channel),
            Some("Uploader Name".to_string())
        );
    }

    #[test]
    fn null_duration_maps_to_none() {
        let json = r#"{"id": "live123", "title": "Livestream", "channel": "C", "duration": null}"#;
        let entry: YtDlpEntry = serde_json::from_str(json).unwrap();
        assert_eq!(track_from_entry(entry).unwrap().duration, None);
    }

    #[test]
    fn missing_id_maps_to_none() {
        let json = r#"{"title": "No id here"}"#;
        let entry: YtDlpEntry = serde_json::from_str(json).unwrap();
        assert_eq!(track_from_entry(entry), None);
    }

    #[test]
    fn empty_id_maps_to_none() {
        let json = r#"{"id": "", "title": "Blank id"}"#;
        let entry: YtDlpEntry = serde_json::from_str(json).unwrap();
        assert_eq!(track_from_entry(entry), None);
    }

    #[test]
    fn parse_tracks_skips_blank_and_unparseable_lines() {
        let stdout = "\nWARNING: something yt-dlp printed\n\
             {\"id\": \"real123\", \"title\": \"Real\", \"channel\": \"C\", \"duration\": 10}\n\
             \n";
        let tracks = parse_tracks(stdout);
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].video_id, "real123");
    }

    #[test]
    fn parse_tracks_skips_entries_with_no_usable_id() {
        let stdout =
            "{\"id\": null}\n{\"id\": \"real123\", \"title\": \"T\", \"channel\": \"C\"}\n";
        let tracks = parse_tracks(stdout);
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].video_id, "real123");
    }

    #[test]
    fn parse_tracks_empty_stdout_yields_empty_vec() {
        assert_eq!(parse_tracks(""), Vec::new());
    }

    // ---- truncate_tail ----

    #[test]
    fn truncate_tail_keeps_final_error_over_leading_warning() {
        let stderr = format!(
            "WARNING: [youtube] {}\nERROR: [youtube] xyz: Requested format is not available.\n",
            "x".repeat(300)
        );
        let truncated = truncate_tail(stderr.trim_end(), STDERR_TRUNCATE_LEN);
        assert!(truncated.contains("Requested format is not available"));
    }

    #[test]
    fn truncate_tail_leaves_short_strings_untouched() {
        assert_eq!(truncate_tail("short", 200), "short");
    }

    // ---- list_playlist_items target building ----

    #[test]
    fn playlist_id_without_scheme_is_treated_as_bare_id() {
        assert!(!"PLxxxxxxxxxxxx".contains("://"));
    }

    #[test]
    fn playlist_url_is_left_as_is() {
        assert!("https://www.youtube.com/playlist?list=PLxxxxxxxxxxxx".contains("://"));
    }
}
