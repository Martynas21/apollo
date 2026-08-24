//! Thin YouTube Data API v3 client.
//!
//! Callers are expected to obtain a valid access token themselves (see
//! `crate::youtube::oauth::get_valid_access_token`) and pass it in — this
//! module has no opinion on token storage or refresh, only on talking to
//! the API and mapping its responses into [`Track`]/[`Playlist`].
//!
//! None of these endpoints paginate past the first page (`maxResults=50`
//! for playlists/playlist items, `25` for search) — following
//! `nextPageToken` is out of scope for this MVP pass.

use std::collections::HashMap;
use std::time::Duration;

use serde::Deserialize;

const API_BASE: &str = "https://www.googleapis.com/youtube/v3";

/// Special playlist ID YouTube reserves for a user's liked videos.
pub const LIKED_VIDEOS_PLAYLIST_ID: &str = "LL";

/// A single playable video, as resolved from a playlist, liked-videos list,
/// uploads list, or search results.
#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    pub video_id: String,
    pub title: String,
    pub channel: String,
    /// `None` when YouTube didn't report a usable duration (e.g. an
    /// in-progress livestream, which the API reports as `P0D`).
    pub duration: Option<Duration>,
}

/// A playlist owned by (or otherwise visible to) the linked account.
#[derive(Debug, Clone, PartialEq)]
pub struct Playlist {
    pub id: String,
    pub title: String,
    pub item_count: Option<u32>,
}

#[derive(Debug)]
pub enum YouTubeApiError {
    /// HTTP 403 with a quota-related reason (`quotaExceeded`, `dailyLimitExceeded`).
    QuotaExceeded,
    /// HTTP 429, or a 403 that isn't quota-related but looks like backoff-worthy throttling.
    RateLimited,
    /// HTTP 401 — the access token is invalid/expired. Caller (a later phase) should
    /// prompt a re-link; this client has no opinion on how.
    Unauthorized,
    /// Any other non-2xx response from the API, with the status and a short message
    /// extracted from the response body if possible.
    Api { status: u16, message: String },
    /// Transport-level failure (DNS, connection, timeout, TLS, JSON decode of a malformed body, etc).
    Transport(String),
}

impl std::fmt::Display for YouTubeApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            YouTubeApiError::QuotaExceeded => write!(f, "YouTube API quota exceeded"),
            YouTubeApiError::RateLimited => write!(f, "YouTube API rate limited"),
            YouTubeApiError::Unauthorized => write!(
                f,
                "YouTube API request unauthorized (token invalid/expired)"
            ),
            YouTubeApiError::Api { status, message } => {
                write!(f, "YouTube API error (HTTP {status}): {message}")
            }
            YouTubeApiError::Transport(message) => {
                write!(f, "YouTube API transport error: {message}")
            }
        }
    }
}

impl std::error::Error for YouTubeApiError {}

/// Truncation length for error message bodies embedded in `YouTubeApiError::Api`.
const ERROR_BODY_TRUNCATE_LEN: usize = 200;

fn truncate(s: &str, max_len: usize) -> String {
    match s.char_indices().nth(max_len) {
        Some((byte_idx, _)) => format!("{}...", &s[..byte_idx]),
        None => s.to_string(),
    }
}

// ---- Response shapes (private, deserialize-only) --------------------------

#[derive(Debug, Deserialize)]
struct PlaylistsResponse {
    #[serde(default)]
    items: Vec<PlaylistItem>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlaylistItem {
    id: String,
    snippet: PlaylistSnippet,
    #[serde(default)]
    content_details: Option<PlaylistContentDetails>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlaylistSnippet {
    title: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlaylistContentDetails {
    item_count: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct PlaylistItemsResponse {
    #[serde(default)]
    items: Vec<PlaylistItemEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlaylistItemEntry {
    snippet: PlaylistItemSnippet,
    content_details: PlaylistItemContentDetails,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlaylistItemSnippet {
    title: String,
    #[serde(default)]
    video_owner_channel_title: Option<String>,
    #[serde(default)]
    channel_title: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlaylistItemContentDetails {
    video_id: String,
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    #[serde(default)]
    items: Vec<SearchResultItem>,
}

#[derive(Debug, Deserialize)]
struct SearchResultItem {
    id: SearchResultId,
    snippet: SearchResultSnippet,
}

#[derive(Debug, Deserialize)]
struct SearchResultId {
    #[serde(rename = "videoId")]
    video_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchResultSnippet {
    title: String,
    channel_title: String,
}

#[derive(Debug, Deserialize)]
struct VideosResponse {
    #[serde(default)]
    items: Vec<VideoItem>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VideoItem {
    id: String,
    content_details: VideoContentDetails,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VideoContentDetails {
    duration: String,
}

/// Response shape for `videos.list?part=snippet,contentDetails`, used by
/// [`YouTubeClient::get_video`]. Separate from [`VideosResponse`] (used by
/// the duration-only batch lookup in `fetch_durations`) because that one
/// only ever requests `contentDetails`, not `snippet`.
#[derive(Debug, Deserialize)]
struct VideoWithSnippetResponse {
    #[serde(default)]
    items: Vec<VideoWithSnippetItem>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VideoWithSnippetItem {
    id: String,
    snippet: VideoSnippet,
    content_details: VideoContentDetails,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VideoSnippet {
    title: String,
    channel_title: String,
}

#[derive(Debug, Deserialize)]
struct ChannelsResponse {
    #[serde(default)]
    items: Vec<ChannelItem>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChannelItem {
    content_details: ChannelContentDetails,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChannelContentDetails {
    related_playlists: RelatedPlaylists,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RelatedPlaylists {
    uploads: String,
}

// ---- Pure mapping functions (unit-testable, no network) --------------------

fn map_playlists(resp: PlaylistsResponse) -> Vec<Playlist> {
    resp.items
        .into_iter()
        .map(|item| Playlist {
            id: item.id,
            title: item.snippet.title,
            item_count: item.content_details.and_then(|cd| cd.item_count),
        })
        .collect()
}

fn map_playlist_items(
    resp: PlaylistItemsResponse,
    durations: &HashMap<String, Option<Duration>>,
) -> Vec<Track> {
    resp.items
        .into_iter()
        .map(|item| {
            let video_id = item.content_details.video_id;
            // `channelTitle` on a playlist item is the *playlist owner's*
            // channel, not the video uploader's; `videoOwnerChannelTitle` is
            // the actual uploader but can be absent (e.g. deleted/private
            // videos), hence the fallback.
            let channel = item
                .snippet
                .video_owner_channel_title
                .or(item.snippet.channel_title)
                .unwrap_or_default();
            let duration = durations.get(&video_id).copied().flatten();
            Track {
                video_id,
                title: item.snippet.title,
                channel,
                duration,
            }
        })
        .collect()
}

fn map_search_results(
    resp: SearchResponse,
    durations: &HashMap<String, Option<Duration>>,
) -> Vec<Track> {
    resp.items
        .into_iter()
        .filter_map(|item| {
            let video_id = item.id.video_id?;
            let duration = durations.get(&video_id).copied().flatten();
            Some(Track {
                video_id,
                title: item.snippet.title,
                channel: item.snippet.channel_title,
                duration,
            })
        })
        .collect()
}

fn map_video_with_snippet(resp: VideoWithSnippetResponse) -> Option<Track> {
    resp.items.into_iter().next().map(|item| Track {
        video_id: item.id,
        title: item.snippet.title,
        channel: item.snippet.channel_title,
        duration: parse_iso8601_duration(&item.content_details.duration),
    })
}

fn map_video_durations(resp: VideosResponse) -> HashMap<String, Option<Duration>> {
    resp.items
        .into_iter()
        .map(|item| {
            (
                item.id,
                parse_iso8601_duration(&item.content_details.duration),
            )
        })
        .collect()
}

/// Parses a restricted-form ISO 8601 duration (`P[nD]T[nH][nM][nS]`) as
/// returned by YouTube for video/content durations.
///
/// `"P0D"` is YouTube's convention for "no fixed duration" (e.g. an
/// in-progress livestream) and is deliberately mapped to `None` rather than
/// `Some(Duration::ZERO)` — it means "unknown", not "zero-length". Any other
/// string that doesn't parse also returns `None` rather than panicking.
fn parse_iso8601_duration(s: &str) -> Option<Duration> {
    if s == "P0D" {
        return None;
    }

    let rest = s.strip_prefix('P')?;
    let (date_part, time_part) = match rest.split_once('T') {
        Some((d, t)) => (d, Some(t)),
        None => (rest, None),
    };

    let mut total_secs: u64 = 0;

    if !date_part.is_empty() {
        let days = date_part.strip_suffix('D')?.parse::<u64>().ok()?;
        total_secs += days * 86_400;
    }

    if let Some(time_part) = time_part {
        let mut remaining = time_part;

        if let Some(idx) = remaining.find('H') {
            let hours = remaining[..idx].parse::<u64>().ok()?;
            total_secs += hours * 3_600;
            remaining = &remaining[idx + 1..];
        }
        if let Some(idx) = remaining.find('M') {
            let minutes = remaining[..idx].parse::<u64>().ok()?;
            total_secs += minutes * 60;
            remaining = &remaining[idx + 1..];
        }
        if let Some(idx) = remaining.find('S') {
            let seconds = remaining[..idx].parse::<u64>().ok()?;
            total_secs += seconds;
            remaining = &remaining[idx + 1..];
        }
        if !remaining.is_empty() {
            return None;
        }
    } else if date_part.is_empty() {
        // Bare "P" with neither a date nor a time component.
        return None;
    }

    Some(Duration::from_secs(total_secs))
}

// ---- Client ----------------------------------------------------------------

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct YouTubeClient {
    http: oauth2::reqwest::Client,
}

#[allow(dead_code)]
impl YouTubeClient {
    pub fn new(http: oauth2::reqwest::Client) -> Self {
        Self { http }
    }

    pub async fn list_playlists(
        &self,
        access_token: &str,
    ) -> Result<Vec<Playlist>, YouTubeApiError> {
        let url =
            format!("{API_BASE}/playlists?part=snippet,contentDetails&mine=true&maxResults=50");
        let resp: PlaylistsResponse = self.get_json(&url, access_token).await?;
        Ok(map_playlists(resp))
    }

    /// Generic: lists items of any playlist ID, including the special
    /// `LL` (liked videos) playlist.
    pub async fn list_playlist_items(
        &self,
        access_token: &str,
        playlist_id: &str,
    ) -> Result<Vec<Track>, YouTubeApiError> {
        let url = format!(
            "{API_BASE}/playlistItems?part=snippet,contentDetails&playlistId={playlist_id}&maxResults=50"
        );
        let resp: PlaylistItemsResponse = self.get_json(&url, access_token).await?;

        let video_ids: Vec<&str> = resp
            .items
            .iter()
            .map(|item| item.content_details.video_id.as_str())
            .collect();
        let durations = self.fetch_durations(access_token, &video_ids).await?;

        Ok(map_playlist_items(resp, &durations))
    }

    pub async fn list_liked_videos(
        &self,
        access_token: &str,
    ) -> Result<Vec<Track>, YouTubeApiError> {
        self.list_playlist_items(access_token, LIKED_VIDEOS_PLAYLIST_ID)
            .await
    }

    /// Resolves the signed-in user's uploads playlist ID (via
    /// `channels.list?part=contentDetails&mine=true`), so callers can feed
    /// it into `list_playlist_items` to browse the user's own uploads.
    pub async fn uploads_playlist_id(&self, access_token: &str) -> Result<String, YouTubeApiError> {
        let url = format!("{API_BASE}/channels?part=contentDetails&mine=true");
        let resp: ChannelsResponse = self.get_json(&url, access_token).await?;
        resp.items
            .into_iter()
            .next()
            .map(|item| item.content_details.related_playlists.uploads)
            .ok_or_else(|| YouTubeApiError::Api {
                status: 200,
                message: "channels.list?mine=true returned no channel for this account".to_string(),
            })
    }

    pub async fn search(
        &self,
        access_token: &str,
        query: &str,
    ) -> Result<Vec<Track>, YouTubeApiError> {
        let encoded_query = urlencoding_encode(query);
        let url =
            format!("{API_BASE}/search?part=snippet&type=video&q={encoded_query}&maxResults=25");
        let resp: SearchResponse = self.get_json(&url, access_token).await?;

        let video_ids: Vec<&str> = resp
            .items
            .iter()
            .filter_map(|item| item.id.video_id.as_deref())
            .collect();
        let durations = self.fetch_durations(access_token, &video_ids).await?;

        Ok(map_search_results(resp, &durations))
    }

    /// Looks up a single video by ID (used by `/play` for a direct video
    /// ID/URL, where no search or playlist listing is involved).
    pub async fn get_video(
        &self,
        access_token: &str,
        video_id: &str,
    ) -> Result<Track, YouTubeApiError> {
        let url = format!("{API_BASE}/videos?part=snippet,contentDetails&id={video_id}");
        let resp: VideoWithSnippetResponse = self.get_json(&url, access_token).await?;
        map_video_with_snippet(resp).ok_or_else(|| YouTubeApiError::Api {
            status: 404,
            message: "video not found".to_string(),
        })
    }

    async fn fetch_durations(
        &self,
        access_token: &str,
        video_ids: &[&str],
    ) -> Result<HashMap<String, Option<Duration>>, YouTubeApiError> {
        if video_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let ids = video_ids.join(",");
        let url = format!("{API_BASE}/videos?part=contentDetails&id={ids}");
        let resp: VideosResponse = self.get_json(&url, access_token).await?;
        Ok(map_video_durations(resp))
    }

    async fn get_json<T: for<'de> Deserialize<'de>>(
        &self,
        url: &str,
        access_token: &str,
    ) -> Result<T, YouTubeApiError> {
        let response = self
            .http
            .get(url)
            .bearer_auth(access_token)
            .send()
            .await
            .map_err(|e| YouTubeApiError::Transport(e.to_string()))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_else(|_| String::new());
            return Err(map_error_response(status.as_u16(), &body));
        }

        let body = response
            .text()
            .await
            .map_err(|e| YouTubeApiError::Transport(e.to_string()))?;
        serde_json::from_str(&body).map_err(|e| YouTubeApiError::Transport(e.to_string()))
    }
}

fn map_error_response(status: u16, body: &str) -> YouTubeApiError {
    match status {
        401 => YouTubeApiError::Unauthorized,
        403 => {
            // YouTube's error responses put a machine-readable `reason`
            // field inside `error.errors[].reason`; checking for the known
            // quota-related reason strings directly in the raw body text is
            // robust enough without modeling the full error JSON shape.
            if body.contains("quotaExceeded") || body.contains("dailyLimitExceeded") {
                YouTubeApiError::QuotaExceeded
            } else {
                YouTubeApiError::Api {
                    status,
                    message: truncate(body, ERROR_BODY_TRUNCATE_LEN),
                }
            }
        }
        429 => YouTubeApiError::RateLimited,
        _ => YouTubeApiError::Api {
            status,
            message: truncate(body, ERROR_BODY_TRUNCATE_LEN),
        },
    }
}

/// Minimal percent-encoding for a search query string, sufficient for
/// embedding arbitrary user text in a URL query parameter without pulling in
/// a dedicated URL-encoding dependency.
fn urlencoding_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- ISO 8601 duration parsing ----

    #[test]
    fn parses_minutes_and_seconds() {
        assert_eq!(
            parse_iso8601_duration("PT4M13S"),
            Some(Duration::from_secs(253))
        );
    }

    #[test]
    fn parses_hours_minutes_seconds() {
        assert_eq!(
            parse_iso8601_duration("PT1H2M3S"),
            Some(Duration::from_secs(3723))
        );
    }

    #[test]
    fn parses_seconds_only() {
        assert_eq!(
            parse_iso8601_duration("PT30S"),
            Some(Duration::from_secs(30))
        );
    }

    #[test]
    fn zero_seconds_is_a_real_zero_duration() {
        assert_eq!(parse_iso8601_duration("PT0S"), Some(Duration::ZERO));
    }

    #[test]
    fn p0d_means_unknown_duration() {
        assert_eq!(parse_iso8601_duration("P0D"), None);
    }

    #[test]
    fn garbage_returns_none_not_panic() {
        assert_eq!(parse_iso8601_duration("garbage"), None);
    }

    #[test]
    fn parses_days_and_hours() {
        assert_eq!(
            parse_iso8601_duration("P1DT2H"),
            Some(Duration::from_secs(26 * 3_600))
        );
    }

    #[test]
    fn bare_p_returns_none() {
        assert_eq!(parse_iso8601_duration("P"), None);
    }

    // ---- JSON fixture -> mapped type tests ----

    #[test]
    fn maps_playlists_response() {
        let json = r#"{
            "items": [
                {
                    "id": "PLxxxxxxxxxxxx",
                    "snippet": { "title": "My Playlist", "channelTitle": "Some Channel" },
                    "contentDetails": { "itemCount": 42 }
                },
                {
                    "id": "PLyyyyyyyyyyyy",
                    "snippet": { "title": "No Count Playlist" }
                }
            ]
        }"#;
        let resp: PlaylistsResponse = serde_json::from_str(json).unwrap();
        let playlists = map_playlists(resp);
        assert_eq!(
            playlists,
            vec![
                Playlist {
                    id: "PLxxxxxxxxxxxx".to_string(),
                    title: "My Playlist".to_string(),
                    item_count: Some(42),
                },
                Playlist {
                    id: "PLyyyyyyyyyyyy".to_string(),
                    title: "No Count Playlist".to_string(),
                    item_count: None,
                },
            ]
        );
    }

    #[test]
    fn maps_playlist_items_response_with_channel_title_fallback() {
        let json = r#"{
            "items": [
                {
                    "snippet": {
                        "title": "Some Video",
                        "videoOwnerChannelTitle": "Uploader Channel",
                        "channelTitle": "Playlist Owner"
                    },
                    "contentDetails": { "videoId": "dQw4w9WgXcQ" }
                },
                {
                    "snippet": {
                        "title": "Deleted Video Slot",
                        "channelTitle": "Playlist Owner"
                    },
                    "contentDetails": { "videoId": "zzzzzzzzzzz" }
                }
            ]
        }"#;
        let resp: PlaylistItemsResponse = serde_json::from_str(json).unwrap();

        let mut durations = HashMap::new();
        durations.insert("dQw4w9WgXcQ".to_string(), Some(Duration::from_secs(213)));
        // "zzzzzzzzzzz" intentionally absent -> should map to None.

        let tracks = map_playlist_items(resp, &durations);
        assert_eq!(
            tracks,
            vec![
                Track {
                    video_id: "dQw4w9WgXcQ".to_string(),
                    title: "Some Video".to_string(),
                    channel: "Uploader Channel".to_string(),
                    duration: Some(Duration::from_secs(213)),
                },
                Track {
                    video_id: "zzzzzzzzzzz".to_string(),
                    title: "Deleted Video Slot".to_string(),
                    channel: "Playlist Owner".to_string(),
                    duration: None,
                },
            ]
        );
    }

    #[test]
    fn maps_search_response() {
        let json = r#"{
            "items": [
                {
                    "id": { "kind": "youtube#video", "videoId": "dQw4w9WgXcQ" },
                    "snippet": { "title": "Some Video", "channelTitle": "Uploader Channel" }
                }
            ]
        }"#;
        let resp: SearchResponse = serde_json::from_str(json).unwrap();

        let mut durations = HashMap::new();
        durations.insert("dQw4w9WgXcQ".to_string(), Some(Duration::from_secs(60)));

        let tracks = map_search_results(resp, &durations);
        assert_eq!(
            tracks,
            vec![Track {
                video_id: "dQw4w9WgXcQ".to_string(),
                title: "Some Video".to_string(),
                channel: "Uploader Channel".to_string(),
                duration: Some(Duration::from_secs(60)),
            }]
        );
    }

    #[test]
    fn maps_video_durations_response_including_p0d() {
        let json = r#"{
            "items": [
                { "id": "dQw4w9WgXcQ", "contentDetails": { "duration": "PT3M33S" } },
                { "id": "livestreamvid", "contentDetails": { "duration": "P0D" } }
            ]
        }"#;
        let resp: VideosResponse = serde_json::from_str(json).unwrap();
        let durations = map_video_durations(resp);
        assert_eq!(
            durations.get("dQw4w9WgXcQ"),
            Some(&Some(Duration::from_secs(213)))
        );
        assert_eq!(durations.get("livestreamvid"), Some(&None));
    }

    #[test]
    fn maps_video_with_snippet_response() {
        let json = r#"{
            "items": [
                {
                    "id": "dQw4w9WgXcQ",
                    "snippet": { "title": "Some Video", "channelTitle": "Uploader Channel" },
                    "contentDetails": { "duration": "PT3M33S" }
                }
            ]
        }"#;
        let resp: VideoWithSnippetResponse = serde_json::from_str(json).unwrap();
        let track = map_video_with_snippet(resp);
        assert_eq!(
            track,
            Some(Track {
                video_id: "dQw4w9WgXcQ".to_string(),
                title: "Some Video".to_string(),
                channel: "Uploader Channel".to_string(),
                duration: Some(Duration::from_secs(213)),
            })
        );
    }

    #[test]
    fn maps_video_with_snippet_response_empty_items() {
        let json = r#"{ "items": [] }"#;
        let resp: VideoWithSnippetResponse = serde_json::from_str(json).unwrap();
        assert_eq!(map_video_with_snippet(resp), None);
    }

    #[test]
    fn error_mapping_distinguishes_quota_from_other_403() {
        let quota_body = r#"{"error":{"errors":[{"reason":"quotaExceeded"}]}}"#;
        assert!(matches!(
            map_error_response(403, quota_body),
            YouTubeApiError::QuotaExceeded
        ));

        let daily_limit_body = r#"{"error":{"errors":[{"reason":"dailyLimitExceeded"}]}}"#;
        assert!(matches!(
            map_error_response(403, daily_limit_body),
            YouTubeApiError::QuotaExceeded
        ));

        let other_body = r#"{"error":{"errors":[{"reason":"forbidden"}]}}"#;
        match map_error_response(403, other_body) {
            YouTubeApiError::Api { status, .. } => assert_eq!(status, 403),
            other => panic!("expected Api error, got {other:?}"),
        }
    }

    #[test]
    fn error_mapping_handles_401_and_429() {
        assert!(matches!(
            map_error_response(401, ""),
            YouTubeApiError::Unauthorized
        ));
        assert!(matches!(
            map_error_response(429, ""),
            YouTubeApiError::RateLimited
        ));
    }

    #[test]
    fn error_message_is_truncated() {
        let long_body = "x".repeat(500);
        match map_error_response(500, &long_body) {
            YouTubeApiError::Api { status, message } => {
                assert_eq!(status, 500);
                assert!(message.len() <= ERROR_BODY_TRUNCATE_LEN + 3);
            }
            other => panic!("expected Api error, got {other:?}"),
        }
    }
}
