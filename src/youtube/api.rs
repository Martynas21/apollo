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

/// Default for [`YouTubeClient::playlist_track_limit`] — see
/// `Config::playlist_track_limit` (`PLAYLIST_TRACK_LIMIT` env var) to
/// override it.
const DEFAULT_PLAYLIST_LIMIT: usize = 500;

/// Timeout for a single `yt-dlp` search or video-lookup call.
const YT_DLP_TIMEOUT: Duration = Duration::from_secs(30);

/// Timeout for `yt-dlp` calls that may process many more entries than a
/// single search or lookup: playlist listing and batch id hydration.
const YT_DLP_BATCH_TIMEOUT: Duration = Duration::from_secs(90);

/// Recognized `YouTube` hosts accepted for a playlist URL passed to
/// `list_playlist_items`.
const YOUTUBE_HOSTS: [&str; 5] = [
    "www.youtube.com",
    "youtube.com",
    "m.youtube.com",
    "music.youtube.com",
    "youtu.be",
];

/// Max length accepted for a bare (schemeless) playlist id passed to
/// `list_playlist_items`. Real `YouTube` playlist ids are well under this.
const MAX_PLAYLIST_ID_LEN: usize = 64;

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
    /// User-supplied input (currently: `list_playlist_items`'s playlist
    /// URL/id) failed validation before a `yt-dlp` process was even spawned
    /// — e.g. not a recognized `YouTube` host, or a bare id shaped like a
    /// `yt-dlp` command-line flag rather than a real playlist id.
    InvalidInput(String),
    /// The `yt-dlp` process didn't finish within its allotted timeout and
    /// was killed.
    Timeout,
    /// The requested video has no known duration — in practice, an
    /// in-progress livestream (see [`Track::duration`]). Rejected rather
    /// than queued: songbird's HLS path for a live broadcast never stops
    /// downloading segments if the track is skipped/stopped before the
    /// broadcast itself ends, leaking a background task for as long as the
    /// stream stays live.
    LiveStreamNotSupported,
}

impl std::fmt::Display for YouTubeApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::YtDlpMissing => write!(f, "yt-dlp is not installed or not on PATH"),
            Self::YtDlpFailed(message) => write!(f, "yt-dlp failed: {message}"),
            Self::InvalidInput(message) => write!(f, "invalid input: {message}"),
            Self::Timeout => write!(f, "yt-dlp timed out"),
            Self::LiveStreamNotSupported => {
                write!(
                    f,
                    "that's a livestream still in progress — try again once it's finished"
                )
            }
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
    /// Present on every entry of a `--flat-playlist` listing (not a search),
    /// carrying the *playlist's* title rather than this entry's own — see
    /// [`first_playlist_title`].
    #[serde(default)]
    playlist_title: Option<String>,
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
        // `Duration::from_secs_f64` panics on a negative, non-finite, or
        // overflowing value; `try_from_secs_f64` rejects the same inputs
        // without panicking, so a malformed `duration` from yt-dlp's JSON
        // just becomes `None` instead of crashing the caller.
        duration: entry
            .duration
            .and_then(|secs| Duration::try_from_secs_f64(secs).ok()),
    })
}

/// Parses `yt-dlp -j`'s (one-JSON-object-per-line) stdout into [`YtDlpEntry`]
/// values, skipping any line that isn't valid JSON.
fn parse_entries(stdout: &str) -> impl Iterator<Item = YtDlpEntry> + '_ {
    stdout.lines().filter_map(|line| {
        let line = line.trim();
        if line.is_empty() {
            None
        } else {
            serde_json::from_str(line).ok()
        }
    })
}

/// Parses `yt-dlp -j`'s stdout into [`Track`]s, skipping any entry that isn't
/// valid JSON, maps to no usable track (e.g. deleted-video placeholder
/// entries), or has no known duration — silently, none of these are treated
/// as an error. A missing duration means `yt-dlp` couldn't report one, most
/// commonly because the video is an in-progress livestream (see
/// [`Track::duration`]); those are dropped rather than surfaced because
/// every caller of this (`search`, `list_playlist_items`, `hydrate_videos`)
/// already tolerates a track going missing from its results, and queuing one
/// leaks a background download if it's skipped before the broadcast itself
/// ends (songbird's HLS path never notices — see the removed
/// `vendor/stream_lib` patch this replaced). `get_video` needs to tell a
/// caller-picked livestream apart from a missing video, so it checks
/// [`Track::duration`] itself instead of going through this.
fn parse_tracks(stdout: &str) -> Vec<Track> {
    parse_entries(stdout)
        .filter_map(track_from_entry)
        .filter(|track| track.duration.is_some())
        .collect()
}

/// Pulls the playlist's own title out of a `--flat-playlist` listing's
/// stdout — every entry carries it under `playlist_title`, so the first
/// entry that has one (usually the first line) settles it. `None` for a
/// search listing (no such field at all) or an empty/unparseable playlist.
fn first_playlist_title(stdout: &str) -> Option<String> {
    stdout.lines().find_map(|line| {
        let line = line.trim();
        if line.is_empty() {
            return None;
        }
        serde_json::from_str::<YtDlpEntry>(line)
            .ok()
            .and_then(|entry| entry.playlist_title)
            .filter(|title| !title.is_empty())
    })
}

/// A playlist listing: its tracks, plus its own title if `yt-dlp` reported
/// one — used by the saved-playlists picker to default an import's name to
/// the real playlist title instead of the raw URL when the user leaves the
/// name field blank.
pub struct PlaylistListing {
    pub title: Option<String>,
    pub tracks: Vec<Track>,
}

/// Validates `playlist_url_or_id` and builds the single argv token passed to
/// `yt-dlp` as its playlist target.
///
/// This is the fix for a `yt-dlp` argument-injection bug: `playlist_url_or_id`
/// comes straight from raw user text (the playlist-import modal and
/// `/play`'s playlist branch), and without validation, a bare id starting
/// with `-` (e.g. `--exec=<cmd> http://x`) is parsed by `yt-dlp` as a
/// command-line flag rather than a positional argument — `--exec` runs an
/// arbitrary shell command. Rejecting a leading `-` is the critical check;
/// the URL/host/charset checks narrow this further to only what a real
/// `YouTube` playlist reference looks like.
fn build_playlist_target(playlist_url_or_id: &str) -> Result<String, YouTubeApiError> {
    if playlist_url_or_id.contains("://") {
        let url = url::Url::parse(playlist_url_or_id)
            .map_err(|_e| YouTubeApiError::InvalidInput("not a valid URL".to_string()))?;
        if url.scheme() != "http" && url.scheme() != "https" {
            return Err(YouTubeApiError::InvalidInput(
                "only http/https URLs are accepted".to_string(),
            ));
        }
        let host = url
            .host_str()
            .ok_or_else(|| YouTubeApiError::InvalidInput("URL has no host".to_string()))?;
        if !YOUTUBE_HOSTS.contains(&host) {
            return Err(YouTubeApiError::InvalidInput(
                "URL is not a recognized YouTube host".to_string(),
            ));
        }
        Ok(playlist_url_or_id.to_string())
    } else {
        let valid_id = !playlist_url_or_id.is_empty()
            && playlist_url_or_id.len() <= MAX_PLAYLIST_ID_LEN
            && !playlist_url_or_id.starts_with('-')
            && playlist_url_or_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !valid_id {
            return Err(YouTubeApiError::InvalidInput(
                "not a valid YouTube playlist ID".to_string(),
            ));
        }
        Ok(format!(
            "https://www.youtube.com/playlist?list={playlist_url_or_id}"
        ))
    }
}

/// `yt-dlp`-backed `YouTube` client: search, single-video lookup, and
/// playlist listing. Cheap to clone — holds only the (optional) cookies
/// file path and the playlist track cap, no connection state.
#[derive(Debug, Clone)]
pub struct YouTubeClient {
    /// Path to a Netscape-format cookies file, passed to `yt-dlp` as
    /// `--cookies` when set — see `Config::yt_dlp_cookies_file`.
    cookies_file: Option<String>,
    /// Max tracks returned by `list_playlist_items`. `YouTube` playlists can
    /// run up to 5000 entries; without a cap a very large (or deliberately
    /// crafted) playlist produces an unbounded `Vec<Track>`. Passed to
    /// `yt-dlp` itself via `--playlist-end` (so it stops fetching early) and
    /// enforced again on the parsed result as a defense-in-depth backstop.
    /// See `Config::playlist_track_limit`.
    playlist_track_limit: usize,
}

impl Default for YouTubeClient {
    fn default() -> Self {
        Self {
            cookies_file: None,
            playlist_track_limit: DEFAULT_PLAYLIST_LIMIT,
        }
    }
}

impl YouTubeClient {
    pub fn new(cookies_file: Option<String>, playlist_track_limit: usize) -> Self {
        Self {
            cookies_file,
            playlist_track_limit,
        }
    }

    /// Runs `yt-dlp -j <extra_args...> <target>` and returns its stdout on
    /// success. Killed and reported as [`YouTubeApiError::Timeout`] if it
    /// doesn't finish within `timeout`.
    async fn run(
        &self,
        extra_args: &[&str],
        target: &str,
        timeout: Duration,
    ) -> Result<String, YouTubeApiError> {
        let mut command = Command::new("yt-dlp");
        command.kill_on_drop(true);
        command.arg("-j").args(extra_args);
        if let Some(cookies_file) = &self.cookies_file {
            command.args(["--cookies", cookies_file]);
        }
        command.arg(target);

        let output = tokio::time::timeout(timeout, command.output())
            .await
            .map_err(|_elapsed| YouTubeApiError::Timeout)?
            .map_err(|e| {
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
        // The `ytsearch{N}:` prefix is a fixed literal, so the resulting
        // target can never start with `-` regardless of `query`'s content —
        // it can't be mistaken by yt-dlp for a flag.
        let target = format!("ytsearch{SEARCH_LIMIT}:{query}");
        let stdout = self
            .run(
                &["--flat-playlist", "--no-warnings"],
                &target,
                YT_DLP_TIMEOUT,
            )
            .await?;
        Ok(parse_tracks(&stdout))
    }

    /// Looks up a single video by ID (used by `/play` for a direct video
    /// ID/URL, and by `/add_to_queue`'s picker re-lookup).
    pub async fn get_video(&self, video_id: &str) -> Result<Track, YouTubeApiError> {
        // As with `search`, the `https://www.youtube.com/watch?v=` prefix is
        // a fixed literal: the resulting target always starts with `h`, so
        // it can't be mistaken by yt-dlp for a flag no matter what
        // `video_id` contains.
        let url = format!("https://www.youtube.com/watch?v={video_id}");
        let stdout = self.run(&["--no-playlist"], &url, YT_DLP_TIMEOUT).await?;
        let track = parse_entries(&stdout)
            .next()
            .and_then(track_from_entry)
            .ok_or_else(|| {
                YouTubeApiError::YtDlpFailed("no metadata returned for video".to_string())
            })?;
        if track.duration.is_none() {
            return Err(YouTubeApiError::LiveStreamNotSupported);
        }
        Ok(track)
    }

    /// Lists every track in a playlist, given either a full playlist URL or
    /// a bare playlist ID (e.g. `PLxxxxxxxxxxxx`), alongside the playlist's
    /// own title if `yt-dlp` reported one. Rejects anything that doesn't
    /// validate as a `YouTube` playlist URL/id (see
    /// [`YouTubeApiError::InvalidInput`]), and returns at most
    /// [`Self::playlist_track_limit`] tracks.
    pub async fn list_playlist_items(
        &self,
        playlist_url_or_id: &str,
    ) -> Result<PlaylistListing, YouTubeApiError> {
        let target = build_playlist_target(playlist_url_or_id)?;
        let playlist_end = self.playlist_track_limit.to_string();
        let stdout = self
            .run(
                &[
                    "--flat-playlist",
                    "--no-warnings",
                    "--playlist-end",
                    &playlist_end,
                ],
                &target,
                YT_DLP_BATCH_TIMEOUT,
            )
            .await?;
        let mut tracks = parse_tracks(&stdout);
        tracks.truncate(self.playlist_track_limit);
        Ok(PlaylistListing {
            title: first_playlist_title(&stdout),
            tracks,
        })
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
        command.kill_on_drop(true);
        command.args(["-j", "--no-playlist", "--ignore-errors"]);
        if let Some(cookies_file) = &self.cookies_file {
            command.args(["--cookies", cookies_file]);
        }
        for id in video_ids {
            command.arg(format!("https://www.youtube.com/watch?v={id}"));
        }

        let output = tokio::time::timeout(YT_DLP_BATCH_TIMEOUT, command.output())
            .await
            .map_err(|_elapsed| YouTubeApiError::Timeout)?
            .map_err(|e| {
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
    fn negative_duration_maps_to_none_instead_of_panicking() {
        let json = r#"{"id": "x", "title": "t", "channel": "c", "duration": -1}"#;
        let entry: YtDlpEntry = serde_json::from_str(json).unwrap();
        assert_eq!(track_from_entry(entry).unwrap().duration, None);
    }

    #[test]
    fn nan_duration_maps_to_none_instead_of_panicking() {
        let entry = YtDlpEntry {
            id: Some("x".to_string()),
            title: Some("t".to_string()),
            channel: Some("c".to_string()),
            uploader: None,
            duration: Some(f64::NAN),
            playlist_title: None,
        };
        assert_eq!(track_from_entry(entry).unwrap().duration, None);
    }

    #[test]
    fn infinite_duration_maps_to_none_instead_of_panicking() {
        let entry = YtDlpEntry {
            id: Some("x".to_string()),
            title: Some("t".to_string()),
            channel: Some("c".to_string()),
            uploader: None,
            duration: Some(f64::INFINITY),
            playlist_title: None,
        };
        assert_eq!(track_from_entry(entry).unwrap().duration, None);
    }

    #[test]
    fn overflowing_duration_maps_to_none_instead_of_panicking() {
        let entry = YtDlpEntry {
            id: Some("x".to_string()),
            title: Some("t".to_string()),
            channel: Some("c".to_string()),
            uploader: None,
            duration: Some(f64::MAX),
            playlist_title: None,
        };
        assert_eq!(track_from_entry(entry).unwrap().duration, None);
    }

    #[test]
    fn normal_duration_still_parses() {
        let json = r#"{"id": "x", "title": "t", "channel": "c", "duration": 42.5}"#;
        let entry: YtDlpEntry = serde_json::from_str(json).unwrap();
        assert_eq!(
            track_from_entry(entry).unwrap().duration,
            Some(Duration::from_secs_f64(42.5))
        );
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
        let stdout = "{\"id\": null}\n\
             {\"id\": \"real123\", \"title\": \"T\", \"channel\": \"C\", \"duration\": 10}\n";
        let tracks = parse_tracks(stdout);
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].video_id, "real123");
    }

    #[test]
    fn parse_tracks_empty_stdout_yields_empty_vec() {
        assert_eq!(parse_tracks(""), Vec::new());
    }

    #[test]
    fn parse_tracks_drops_entries_with_no_known_duration() {
        let stdout = "{\"id\": \"live1\", \"title\": \"Live\", \"channel\": \"C\", \"duration\": null}\n\
             {\"id\": \"vod1\", \"title\": \"VOD\", \"channel\": \"C\", \"duration\": 213}\n";
        let tracks = parse_tracks(stdout);
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].video_id, "vod1");
    }

    // ---- first_playlist_title ----

    #[test]
    fn first_playlist_title_reads_it_from_any_entry() {
        let stdout = "{\"id\": \"a\", \"playlist_title\": \"Chill Mix\"}\n\
             {\"id\": \"b\", \"playlist_title\": \"Chill Mix\"}\n";
        assert_eq!(first_playlist_title(stdout), Some("Chill Mix".to_string()));
    }

    #[test]
    fn first_playlist_title_none_when_absent() {
        // e.g. a search listing, which has no playlist_title field at all.
        let stdout = "{\"id\": \"a\", \"title\": \"Some Video\"}\n";
        assert_eq!(first_playlist_title(stdout), None);
    }

    #[test]
    fn first_playlist_title_none_when_empty_string() {
        let stdout = "{\"id\": \"a\", \"playlist_title\": \"\"}\n";
        assert_eq!(first_playlist_title(stdout), None);
    }

    #[test]
    fn first_playlist_title_skips_blank_and_unparseable_lines() {
        let stdout = "\nnot json\n{\"id\": \"a\", \"playlist_title\": \"Chill Mix\"}\n";
        assert_eq!(first_playlist_title(stdout), Some("Chill Mix".to_string()));
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

    // ---- build_playlist_target ----

    #[test]
    fn accepts_bare_playlist_id() {
        let target = build_playlist_target("PLxxxxxxxxxxxx").unwrap();
        assert_eq!(
            target,
            "https://www.youtube.com/playlist?list=PLxxxxxxxxxxxx"
        );
    }

    #[test]
    fn accepts_bare_id_with_underscores_and_hyphens_in_the_middle() {
        assert!(build_playlist_target("PL_abc-123_XYZ").is_ok());
    }

    #[test]
    fn accepts_full_playlist_url() {
        let url = "https://www.youtube.com/playlist?list=PLxxxxxxxxxxxx";
        assert_eq!(build_playlist_target(url).unwrap(), url);
    }

    #[test]
    fn accepts_all_recognized_youtube_hosts() {
        for host in YOUTUBE_HOSTS {
            let url = format!("https://{host}/playlist?list=PLxxxxxxxxxxxx");
            assert!(
                build_playlist_target(&url).is_ok(),
                "expected {url} to be accepted"
            );
        }
    }

    #[test]
    fn accepts_plain_http_scheme() {
        assert!(build_playlist_target("http://www.youtube.com/playlist?list=PLxxx").is_ok());
    }

    #[test]
    fn rejects_bare_id_starting_with_dash() {
        // The actual root cause of the injection: a leading `-`/`--` makes
        // yt-dlp treat the token as a flag rather than a positional arg.
        let err = build_playlist_target("--exec=touch /tmp/pwned").unwrap_err();
        assert!(matches!(err, YouTubeApiError::InvalidInput(_)));
    }

    #[test]
    fn rejects_injection_style_url_with_extra_flag_token() {
        // `"http://x"` here exists only to pass a naive `contains("://")`
        // check; the host isn't YouTube at all, so this must be rejected.
        let err = build_playlist_target("--exec=<cmd> http://x").unwrap_err();
        assert!(matches!(err, YouTubeApiError::InvalidInput(_)));
    }

    #[test]
    fn rejects_non_youtube_host() {
        let err =
            build_playlist_target("https://evil.example.com/playlist?list=PLxxx").unwrap_err();
        assert!(matches!(err, YouTubeApiError::InvalidInput(_)));
    }

    #[test]
    fn rejects_non_http_scheme() {
        let err = build_playlist_target("file:///etc/passwd").unwrap_err();
        assert!(matches!(err, YouTubeApiError::InvalidInput(_)));
    }

    #[test]
    fn rejects_empty_bare_id() {
        assert!(build_playlist_target("").is_err());
    }

    #[test]
    fn rejects_overlong_bare_id() {
        let long_id = "a".repeat(MAX_PLAYLIST_ID_LEN + 1);
        assert!(build_playlist_target(&long_id).is_err());
    }

    #[test]
    fn rejects_bare_id_with_disallowed_characters() {
        assert!(build_playlist_target("PL abc/xyz").is_err());
        assert!(build_playlist_target("PL;rm -rf").is_err());
    }

    #[test]
    fn rejects_garbage_url() {
        assert!(build_playlist_target("not a url://at all").is_err());
    }

    // ---- playlist track limit truncation ----

    #[test]
    fn track_list_is_truncated_to_playlist_limit() {
        let limit = DEFAULT_PLAYLIST_LIMIT;
        let mut tracks: Vec<Track> = (0..limit + 50)
            .map(|i| Track {
                video_id: format!("id{i}"),
                title: String::new(),
                channel: String::new(),
                duration: None,
            })
            .collect();
        tracks.truncate(limit);
        assert_eq!(tracks.len(), limit);
        assert_eq!(tracks[0].video_id, "id0");
    }
}
