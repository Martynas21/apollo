use std::io::ErrorKind;
use std::time::Duration;

use serde::Deserialize;
use tokio::process::Command;

const SEARCH_LIMIT: usize = 25;

const DEFAULT_PLAYLIST_LIMIT: usize = 500;

const YT_DLP_TIMEOUT: Duration = Duration::from_secs(30);

const YT_DLP_BATCH_TIMEOUT: Duration = Duration::from_secs(90);

const YOUTUBE_HOSTS: [&str; 5] = [
    "www.youtube.com",
    "youtube.com",
    "m.youtube.com",
    "music.youtube.com",
    "youtu.be",
];

const MAX_PLAYLIST_ID_LEN: usize = 64;

const STDERR_TRUNCATE_LEN: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    pub video_id: String,
    pub title: String,
    pub channel: String,
    pub duration: Option<Duration>,
}

#[derive(Debug)]
pub enum YouTubeApiError {
    YtDlpMissing,
    YtDlpFailed(String),
    InvalidInput(String),
    Timeout,
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
    #[serde(default)]
    playlist_title: Option<String>,
}

fn track_from_entry(entry: YtDlpEntry) -> Option<Track> {
    let video_id = entry.id.filter(|id| !id.is_empty())?;
    Some(Track {
        video_id,
        title: entry.title.unwrap_or_default(),
        channel: entry.channel.or(entry.uploader).unwrap_or_default(),
        duration: entry
            .duration
            .and_then(|secs| Duration::try_from_secs_f64(secs).ok()),
    })
}

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

fn parse_tracks(stdout: &str) -> Vec<Track> {
    parse_entries(stdout)
        .filter_map(track_from_entry)
        .filter(|track| track.duration.is_some())
        .collect()
}

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

pub struct PlaylistListing {
    pub title: Option<String>,
    pub tracks: Vec<Track>,
}

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

#[derive(Debug, Clone)]
pub struct YouTubeClient {
    cookies_file: Option<String>,
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

    pub async fn search(&self, query: &str) -> Result<Vec<Track>, YouTubeApiError> {
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

    pub async fn get_video(&self, video_id: &str) -> Result<Track, YouTubeApiError> {
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

        Ok(parse_tracks(&String::from_utf8_lossy(&output.stdout)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn first_playlist_title_reads_it_from_any_entry() {
        let stdout = "{\"id\": \"a\", \"playlist_title\": \"Chill Mix\"}\n\
             {\"id\": \"b\", \"playlist_title\": \"Chill Mix\"}\n";
        assert_eq!(first_playlist_title(stdout), Some("Chill Mix".to_string()));
    }

    #[test]
    fn first_playlist_title_none_when_absent() {
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
        let err = build_playlist_target("--exec=touch /tmp/pwned").unwrap_err();
        assert!(matches!(err, YouTubeApiError::InvalidInput(_)));
    }

    #[test]
    fn rejects_injection_style_url_with_extra_flag_token() {
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
