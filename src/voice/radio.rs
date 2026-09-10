use std::time::Duration;

use crate::youtube::ytdlp::{STDERR_TRUNCATE_LEN, YtDlp, YtDlpError, truncate_tail};

const MIX_LISTING_LIMIT: usize = 20;

const MIX_LISTING_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum RadioError {
    #[error("yt-dlp is not installed or not on PATH")]
    YtDlpMissing,
    #[error("yt-dlp failed to list mix: {0}")]
    YtDlpFailed(String),
    #[error("mix listing returned no usable entries")]
    Empty,
    #[error("yt-dlp timed out")]
    Timeout,
}

fn map_ytdlp_error(err: YtDlpError) -> RadioError {
    match err {
        YtDlpError::Missing => RadioError::YtDlpMissing,
        YtDlpError::Timeout => RadioError::Timeout,
        YtDlpError::Spawn(message) => {
            RadioError::YtDlpFailed(truncate_tail(&message, STDERR_TRUNCATE_LEN))
        }
        YtDlpError::Failed(stderr) => {
            RadioError::YtDlpFailed(truncate_tail(stderr.trim_end(), STDERR_TRUNCATE_LEN))
        }
    }
}

#[derive(serde::Deserialize)]
struct FlatPlaylistEntry {
    id: Option<String>,
}

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

fn mix_ids_from_stdout(stdout: &str) -> Result<Vec<String>, RadioError> {
    let ids = parse_flat_playlist_ids(stdout);
    if ids.is_empty() {
        Err(RadioError::Empty)
    } else {
        Ok(ids)
    }
}

pub async fn list_mix_video_ids(
    seed_video_id: &str,
    cookies_file: Option<&str>,
) -> Result<Vec<String>, RadioError> {
    let url = format!("https://www.youtube.com/watch?v={seed_video_id}&list=RD{seed_video_id}");
    let ytdlp = YtDlp::new(cookies_file.map(str::to_string));
    let limit = MIX_LISTING_LIMIT.to_string();
    let args = [
        "-j",
        "--flat-playlist",
        "--no-warnings",
        "--playlist-end",
        limit.as_str(),
        url.as_str(),
    ];

    let stdout = ytdlp
        .run(&args, MIX_LISTING_TIMEOUT)
        .await
        .map_err(map_ytdlp_error)?;

    mix_ids_from_stdout(&stdout)
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

    #[test]
    fn mix_ids_from_stdout_returns_parsed_ids() {
        let stdout = "{\"id\": \"abc123\"}\n{\"id\": \"def456\"}\n";
        assert_eq!(
            mix_ids_from_stdout(stdout).unwrap(),
            vec!["abc123".to_string(), "def456".to_string()]
        );
    }

    #[test]
    fn mix_ids_from_stdout_errors_when_no_usable_entries() {
        assert!(matches!(mix_ids_from_stdout(""), Err(RadioError::Empty)));
    }

    #[test]
    fn map_ytdlp_error_missing_maps_to_missing() {
        assert!(matches!(
            map_ytdlp_error(YtDlpError::Missing),
            RadioError::YtDlpMissing
        ));
    }

    #[test]
    fn map_ytdlp_error_timeout_maps_to_timeout() {
        assert!(matches!(
            map_ytdlp_error(YtDlpError::Timeout),
            RadioError::Timeout
        ));
    }

    #[test]
    fn map_ytdlp_error_spawn_and_failed_both_truncate_the_tail() {
        let long = "x".repeat(500);
        for err in [YtDlpError::Spawn(long.clone()), YtDlpError::Failed(long)] {
            match map_ytdlp_error(err) {
                RadioError::YtDlpFailed(message) => {
                    assert!(message.starts_with("..."));
                    assert!(message.len() <= STDERR_TRUNCATE_LEN + 3);
                }
                other => panic!("expected YtDlpFailed, got {other:?}"),
            }
        }
    }
}
