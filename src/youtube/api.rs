//! Thin `YouTube` Data API v3 client.
//!
//! Callers are expected to obtain a valid access token themselves (see
//! `crate::youtube::oauth::get_valid_access_token`) and pass it in — this
//! module has no opinion on token storage or refresh, only on talking to
//! the API and mapping its responses into [`Track`]/[`Playlist`].
//!
//! `list_playlist_items` follows `nextPageToken` to fetch a whole playlist,
//! not just its first 50 items. `list_playlists` and `search` stay
//! single-page (`maxResults=50`/`25`) — an account realistically has under
//! 50 playlists, and search intentionally shows only the top handful of
//! hits, not everything that matched.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::time::Duration;

use serde::Deserialize;

const API_BASE: &str = "https://www.googleapis.com/youtube/v3";

/// A single playable video, as resolved from a playlist, liked-videos list,
/// uploads list, or search results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    pub video_id: String,
    pub title: String,
    pub channel: String,
    /// `None` when `YouTube` didn't report a usable duration (e.g. an
    /// in-progress livestream, which the API reports as `P0D`).
    pub duration: Option<Duration>,
}

/// A playlist owned by (or otherwise visible to) the linked account.
#[derive(Debug, Clone, PartialEq, Eq)]
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
            Self::QuotaExceeded => write!(f, "YouTube API quota exceeded"),
            Self::RateLimited => write!(f, "YouTube API rate limited"),
            Self::Unauthorized => write!(
                f,
                "YouTube API request unauthorized (token invalid/expired)"
            ),
            Self::Api { status, message } => {
                write!(f, "YouTube API error (HTTP {status}): {message}")
            }
            Self::Transport(message) => {
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
#[serde(rename_all = "camelCase")]
struct PlaylistItemsResponse {
    #[serde(default)]
    items: Vec<PlaylistItemEntry>,
    #[serde(default)]
    next_page_token: Option<String>,
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
    items: Vec<PlaylistItemEntry>,
    durations: &HashMap<String, Option<Duration>>,
) -> Vec<Track> {
    items
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

/// Like [`map_video_with_snippet`], but maps every item instead of just the
/// first — used by [`YouTubeClient::hydrate_videos`]'s multi-id lookup.
fn map_video_with_snippet_list(resp: VideoWithSnippetResponse) -> Vec<Track> {
    resp.items
        .into_iter()
        .map(|item| Track {
            video_id: item.id,
            title: item.snippet.title,
            channel: item.snippet.channel_title,
            duration: parse_iso8601_duration(&item.content_details.duration),
        })
        .collect()
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
/// returned by `YouTube` for video/content durations.
///
/// `"P0D"` is `YouTube`'s convention for "no fixed duration" (e.g. an
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
    /// `API_BASE` in production; overridden in tests to point at a local
    /// mock server so request counts/paths can be asserted without hitting
    /// the real API.
    base_url: String,
}

#[allow(dead_code)]
impl YouTubeClient {
    pub fn new(http: oauth2::reqwest::Client) -> Self {
        Self {
            http,
            base_url: API_BASE.to_string(),
        }
    }

    #[cfg(test)]
    fn with_base_url(http: oauth2::reqwest::Client, base_url: String) -> Self {
        Self { http, base_url }
    }

    pub async fn list_playlists(
        &self,
        access_token: &str,
    ) -> Result<Vec<Playlist>, YouTubeApiError> {
        let base = &self.base_url;
        let url = format!("{base}/playlists?part=snippet,contentDetails&mine=true&maxResults=50");
        let resp: PlaylistsResponse = self.get_json(&url, access_token).await?;
        Ok(map_playlists(resp))
    }

    /// Generic: lists items of any playlist ID, including the special
    /// `LL` (liked videos) playlist. Follows `nextPageToken` across as many
    /// pages as it takes to fetch the whole playlist.
    pub async fn list_playlist_items(
        &self,
        access_token: &str,
        playlist_id: &str,
    ) -> Result<Vec<Track>, YouTubeApiError> {
        let mut items = Vec::new();
        let mut page_token: Option<String> = None;
        let base = &self.base_url;
        loop {
            let mut url = format!(
                "{base}/playlistItems?part=snippet,contentDetails&playlistId={playlist_id}&maxResults=50"
            );
            if let Some(token) = &page_token {
                url.push_str("&pageToken=");
                url.push_str(token);
            }

            let resp: PlaylistItemsResponse = self.get_json(&url, access_token).await?;
            items.extend(resp.items);
            page_token = resp.next_page_token;
            if page_token.is_none() {
                break;
            }
        }

        let video_ids: Vec<&str> = items
            .iter()
            .map(|item| item.content_details.video_id.as_str())
            .collect();
        let durations = self.fetch_durations(access_token, &video_ids).await?;

        Ok(map_playlist_items(items, &durations))
    }

    /// Resolves the signed-in user's uploads playlist ID (via
    /// `channels.list?part=contentDetails&mine=true`), so callers can feed
    /// it into `list_playlist_items` to browse the user's own uploads.
    pub async fn uploads_playlist_id(&self, access_token: &str) -> Result<String, YouTubeApiError> {
        let base = &self.base_url;
        let url = format!("{base}/channels?part=contentDetails&mine=true");
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
        let base = &self.base_url;
        let url = format!("{base}/search?part=snippet&type=video&q={encoded_query}&maxResults=25");
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
        let base = &self.base_url;
        let url = format!("{base}/videos?part=snippet,contentDetails&id={video_id}");
        let resp: VideoWithSnippetResponse = self.get_json(&url, access_token).await?;
        map_video_with_snippet(resp).ok_or_else(|| YouTubeApiError::Api {
            status: 404,
            message: "video not found".to_string(),
        })
    }

    /// Looks up multiple videos by id in one request per 50-id chunk (used
    /// by radio mode to hydrate a batch of bare video ids from a `yt-dlp`
    /// Mix listing into full `Track`s — see
    /// `crate::voice::radio::list_mix_video_ids` — without an N+1 request
    /// per candidate). Unlike `fetch_durations`, requests `snippet` too,
    /// since callers need title/channel, not just duration.
    ///
    /// Ids that don't resolve (e.g. deleted/private since the Mix was
    /// generated) are silently omitted from the result rather than erroring
    /// the whole batch. Order is not guaranteed to match `video_ids`'s
    /// input order.
    pub async fn hydrate_videos(
        &self,
        access_token: &str,
        video_ids: &[&str],
    ) -> Result<Vec<Track>, YouTubeApiError> {
        let mut tracks = Vec::new();
        let base = &self.base_url;
        for chunk in video_ids.chunks(50) {
            let ids = chunk.join(",");
            let url = format!("{base}/videos?part=snippet,contentDetails&id={ids}");
            let resp: VideoWithSnippetResponse = self.get_json(&url, access_token).await?;
            tracks.extend(map_video_with_snippet_list(resp));
        }
        Ok(tracks)
    }

    /// `videos.list`'s `id` filter takes a comma-separated list; chunking
    /// keeps each request in line with this module's other `maxResults=50`
    /// calls rather than sending an unbounded id list for a large playlist.
    async fn fetch_durations(
        &self,
        access_token: &str,
        video_ids: &[&str],
    ) -> Result<HashMap<String, Option<Duration>>, YouTubeApiError> {
        let mut durations = HashMap::new();
        let base = &self.base_url;
        for chunk in video_ids.chunks(50) {
            let ids = chunk.join(",");
            let url = format!("{base}/videos?part=contentDetails&id={ids}");
            let resp: VideosResponse = self.get_json(&url, access_token).await?;
            durations.extend(map_video_durations(resp));
        }
        Ok(durations)
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
            _ => {
                let _ = write!(out, "%{byte:02X}");
            }
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
            Some(Duration::from_hours(26))
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

        let tracks = map_playlist_items(resp.items, &durations);
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
    fn maps_video_with_snippet_list_response() {
        let json = r#"{
            "items": [
                {
                    "id": "abc123",
                    "snippet": { "title": "First", "channelTitle": "Channel A" },
                    "contentDetails": { "duration": "PT1M0S" }
                },
                {
                    "id": "def456",
                    "snippet": { "title": "Second", "channelTitle": "Channel B" },
                    "contentDetails": { "duration": "PT2M0S" }
                }
            ]
        }"#;
        let resp: VideoWithSnippetResponse = serde_json::from_str(json).unwrap();
        let tracks = map_video_with_snippet_list(resp);
        assert_eq!(
            tracks,
            vec![
                Track {
                    video_id: "abc123".to_string(),
                    title: "First".to_string(),
                    channel: "Channel A".to_string(),
                    duration: Some(Duration::from_secs(60)),
                },
                Track {
                    video_id: "def456".to_string(),
                    title: "Second".to_string(),
                    channel: "Channel B".to_string(),
                    duration: Some(Duration::from_secs(120)),
                },
            ]
        );
    }

    #[test]
    fn maps_video_with_snippet_list_response_omits_missing_ids_without_erroring() {
        // Simulates one of several requested ids having been deleted/made
        // private since the mix was generated — `videos.list` just leaves
        // it out of `items` rather than erroring.
        let json = r#"{
            "items": [
                {
                    "id": "abc123",
                    "snippet": { "title": "Still Here", "channelTitle": "Channel A" },
                    "contentDetails": { "duration": "PT1M0S" }
                }
            ]
        }"#;
        let resp: VideoWithSnippetResponse = serde_json::from_str(json).unwrap();
        assert_eq!(map_video_with_snippet_list(resp).len(), 1);
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

/// Guards against a future change accidentally turning a batched call
/// pattern into one YouTube API request per item (an "N+1" regression) —
/// the kind of change that wouldn't fail any of the pure mapping-function
/// tests above but would quietly multiply real quota usage per `/search`,
/// `/add_to_queue`, or playlist browse. Each test counts requests against a
/// local mock server via [`YouTubeClient::with_base_url`] rather than
/// measuring wall time or memory: request *count* is what actually burns
/// YouTube's per-project quota, and it stays deterministic where timing-
/// or allocation-based assertions would be flaky.
#[cfg(test)]
mod request_count_tests {
    use wiremock::matchers::{method, path, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    async fn mock_client() -> (YouTubeClient, MockServer) {
        let server = MockServer::start().await;
        let client = YouTubeClient::with_base_url(oauth2::reqwest::Client::new(), server.uri());
        (client, server)
    }

    #[tokio::test]
    async fn search_fetches_durations_in_one_batched_request_not_one_per_result() {
        let (client, server) = mock_client().await;

        let ids: Vec<String> = (0..3).map(|i| format!("vid{i}")).collect();
        let search_items: Vec<_> = ids
            .iter()
            .map(|id| {
                serde_json::json!({
                    "id": { "videoId": id },
                    "snippet": { "title": format!("Title {id}"), "channelTitle": "Channel" }
                })
            })
            .collect();
        Mock::given(method("GET"))
            .and(path("/search"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "items": search_items })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let duration_items: Vec<_> = ids
            .iter()
            .map(|id| serde_json::json!({ "id": id, "contentDetails": { "duration": "PT1M0S" } }))
            .collect();
        Mock::given(method("GET"))
            .and(path("/videos"))
            .and(query_param("part", "contentDetails"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "items": duration_items })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let tracks = client.search("token", "lofi").await.unwrap();
        assert_eq!(tracks.len(), 3);

        server.verify().await;
    }

    #[tokio::test]
    async fn hydrate_videos_fetches_multiple_ids_in_one_batched_request() {
        let (client, server) = mock_client().await;

        Mock::given(method("GET"))
            .and(path("/videos"))
            .and(query_param("part", "snippet,contentDetails"))
            .and(query_param("id", "vid0,vid1,vid2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "items": [
                    { "id": "vid0", "snippet": { "title": "T0", "channelTitle": "C" }, "contentDetails": { "duration": "PT1M0S" } },
                    { "id": "vid1", "snippet": { "title": "T1", "channelTitle": "C" }, "contentDetails": { "duration": "PT1M0S" } },
                    { "id": "vid2", "snippet": { "title": "T2", "channelTitle": "C" }, "contentDetails": { "duration": "PT1M0S" } },
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let tracks = client
            .hydrate_videos("token", &["vid0", "vid1", "vid2"])
            .await
            .unwrap();
        assert_eq!(tracks.len(), 3);

        server.verify().await;
    }

    #[tokio::test]
    async fn get_video_makes_a_single_request_not_a_separate_duration_lookup() {
        let (client, server) = mock_client().await;

        Mock::given(method("GET"))
            .and(path("/videos"))
            .and(query_param("part", "snippet,contentDetails"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "items": [{
                    "id": "abc123",
                    "snippet": { "title": "Some Video", "channelTitle": "Some Channel" },
                    "contentDetails": { "duration": "PT3M33S" }
                }]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let track = client.get_video("token", "abc123").await.unwrap();
        assert_eq!(track.video_id, "abc123");

        server.verify().await;
    }

    #[tokio::test]
    async fn list_playlist_items_batches_durations_by_fifty_regardless_of_page_count() {
        let (client, server) = mock_client().await;

        // 60 items split across two `playlistItems` pages (50 + 10) — chosen
        // so the 50-item page boundary lands exactly on `fetch_durations`'s
        // own chunk size, the case most likely to silently regress into one
        // `videos` request per page (or per item) instead of per 50 ids.
        let page1_ids: Vec<String> = (0..50).map(|i| format!("p1-{i}")).collect();
        let page2_ids: Vec<String> = (0..10).map(|i| format!("p2-{i}")).collect();

        let items_json = |ids: &[String]| -> serde_json::Value {
            serde_json::json!(
                ids.iter()
                    .map(|id| serde_json::json!({
                        "snippet": { "title": format!("Track {id}"), "channelTitle": "Channel" },
                        "contentDetails": { "videoId": id }
                    }))
                    .collect::<Vec<_>>()
            )
        };

        Mock::given(method("GET"))
            .and(path("/playlistItems"))
            .and(query_param_is_missing("pageToken"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "items": items_json(&page1_ids),
                "nextPageToken": "PAGE2",
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/playlistItems"))
            .and(query_param("pageToken", "PAGE2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "items": items_json(&page2_ids),
            })))
            .expect(1)
            .mount(&server)
            .await;

        let durations_json = |ids: &[String]| -> serde_json::Value {
            serde_json::json!({
                "items": ids.iter()
                    .map(|id| serde_json::json!({ "id": id, "contentDetails": { "duration": "PT1M0S" } }))
                    .collect::<Vec<_>>()
            })
        };

        Mock::given(method("GET"))
            .and(path("/videos"))
            .and(query_param("id", page1_ids.join(",")))
            .respond_with(ResponseTemplate::new(200).set_body_json(durations_json(&page1_ids)))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/videos"))
            .and(query_param("id", page2_ids.join(",")))
            .respond_with(ResponseTemplate::new(200).set_body_json(durations_json(&page2_ids)))
            .expect(1)
            .mount(&server)
            .await;

        let tracks = client
            .list_playlist_items("token", "PLsomeplaylist")
            .await
            .unwrap();
        assert_eq!(tracks.len(), 60);

        // Exactly 4 requests total (2 pages + 2 duration batches) — asserted
        // by `expect(1)` on each mock above; `verify` fails loudly on either
        // a missed or an extra/unmatched call.
        server.verify().await;
    }
}
