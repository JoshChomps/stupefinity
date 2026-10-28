use reqwest::Client;
use serde_json::Value;
use std::collections::HashMap;
use stupefinity_protocol::{PlaylistSummary, TrackInfo};
use tokio::sync::RwLock;
use tracing::{info, warn};

const API_BASE: &str = "https://api.spotify.com/v1";

pub struct SpotifyApiClient {
    client: Client,
    /// Playlist ID -> tracks, kept for the lifetime of the process.
    cache: RwLock<HashMap<String, Vec<TrackInfo>>>,
}

impl Default for SpotifyApiClient {
    fn default() -> Self {
        Self::new()
    }
}

impl SpotifyApiClient {
    pub fn new() -> Self {
        Self {
            client: Client::builder().build().unwrap_or_default(),
            cache: RwLock::new(HashMap::new()),
        }
    }

    fn ensure_token(access_token: &str) -> anyhow::Result<()> {
        if access_token.is_empty() {
            return Err(anyhow::anyhow!("Not authenticated. Please log in first."));
        }
        Ok(())
    }

    /// GET a Web API URL (absolute, or relative to the API base) and parse the JSON body.
    async fn get_json(&self, access_token: &str, url: &str) -> anyhow::Result<Value> {
        Self::ensure_token(access_token)?;
        let url = if url.starts_with("https://") {
            url.to_string()
        } else {
            format!("{API_BASE}{url}")
        };
        let resp = send_spotify(self.client.get(&url).bearer_auth(access_token)).await?;
        Ok(resp.json().await?)
    }

    /// Forgets cached playlist, album and artist tracks (on logout).
    pub async fn clear_cache(&self) {
        self.cache.write().await.clear();
    }

    /// Retrieve the current user's playlists.
    pub async fn get_playlists(&self, access_token: &str) -> anyhow::Result<Vec<PlaylistSummary>> {
        let mut results = Vec::new();
        let mut next = Some("/me/playlists?limit=50".to_string());
        while let Some(url) = next {
            let json = self.get_json(access_token, &url).await?;
            next = json["next"].as_str().map(str::to_string);
            let Some(items) = json["items"].as_array() else {
                break;
            };
            for item in items {
                let id = item["id"].as_str().unwrap_or("").to_string();
                let name = item["name"].as_str().unwrap_or("Untitled").to_string();
                let description = item["description"].as_str().map(|s| s.to_string());
                let track_count = item["tracks"]["total"].as_u64().unwrap_or(0) as u32;
                let image_url = item["images"]
                    .as_array()
                    .and_then(|imgs| imgs.first())
                    .and_then(|img| img["url"].as_str())
                    .map(|s| s.to_string());
                let owner_name = item["owner"]["display_name"]
                    .as_str()
                    .unwrap_or("Spotify")
                    .to_string();

                results.push(PlaylistSummary {
                    id,
                    name,
                    description,
                    track_count,
                    image_url,
                    owner_name,
                });
            }
        }

        info!("Fetched {} playlists from Spotify.", results.len());
        Ok(results)
    }

    /// Retrieve all tracks for a given playlist ID (following pagination), cached per playlist.
    pub async fn get_playlist_tracks(
        &self,
        access_token: &str,
        playlist_id: &str,
    ) -> anyhow::Result<Vec<TrackInfo>> {
        Self::ensure_token(access_token)?;
        if let Some(tracks) = self.cache.read().await.get(playlist_id) {
            return Ok(tracks.clone());
        }

        let mut tracks = Vec::new();
        let mut next = Some(format!("/playlists/{playlist_id}/tracks?limit=100"));
        while let Some(url) = next {
            let json = self.get_json(access_token, &url).await?;
            if let Some(items) = json["items"].as_array() {
                tracks.extend(items.iter().filter_map(|item| {
                    // Newer API payloads name the entry `item` instead of `track`.
                    Self::parse_track_json(&item["track"])
                        .or_else(|| Self::parse_track_json(&item["item"]))
                }));
            }
            next = json["next"].as_str().map(str::to_string);
        }

        self.cache
            .write()
            .await
            .insert(playlist_id.to_string(), tracks.clone());

        info!(
            "Fetched {} tracks for playlist {}.",
            tracks.len(),
            playlist_id
        );
        Ok(tracks)
    }

    /// Retrieve all tracks for an album by ID (following pagination), cached per album.
    pub async fn get_album_tracks(
        &self,
        access_token: &str,
        album_id: &str,
    ) -> anyhow::Result<Vec<TrackInfo>> {
        Self::ensure_token(access_token)?;
        let cache_key = format!("album:{album_id}");
        if let Some(tracks) = self.cache.read().await.get(&cache_key) {
            return Ok(tracks.clone());
        }

        let album_json = self
            .get_json(access_token, &format!("/albums/{album_id}"))
            .await?;
        let album_name = album_json["name"]
            .as_str()
            .unwrap_or("Unknown Album")
            .to_string();
        let cover_url = album_json["images"]
            .as_array()
            .and_then(|imgs| cover_image(imgs));

        let mut tracks = Vec::new();
        let mut next = Some(format!("/albums/{album_id}/tracks?limit=50"));
        while let Some(url) = next {
            let json = self.get_json(access_token, &url).await?;
            if let Some(items) = json["items"].as_array() {
                for item in items {
                    let uri = match item["uri"].as_str() {
                        Some(u) => u.to_string(),
                        None => continue,
                    };
                    let title = match item["name"].as_str() {
                        Some(n) => n.to_string(),
                        None => continue,
                    };
                    let duration_ms = item["duration_ms"].as_u64().unwrap_or(0) as u32;
                    let artists = item["artists"]
                        .as_array()
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|a| a["name"].as_str().map(|s| s.to_string()))
                                .collect()
                        })
                        .unwrap_or_default();

                    tracks.push(TrackInfo {
                        uri,
                        title,
                        artists,
                        album: album_name.clone(),
                        duration_ms,
                        cover_url: cover_url.clone(),
                        is_liked: false,
                    });
                }
            }
            next = json["next"].as_str().map(str::to_string);
        }

        self.cache.write().await.insert(cache_key, tracks.clone());

        info!("Fetched {} tracks for album {}.", tracks.len(), album_id);
        Ok(tracks)
    }

    /// Retrieve top tracks for an artist by ID, cached per artist.
    pub async fn get_artist_top_tracks(
        &self,
        access_token: &str,
        artist_id: &str,
    ) -> anyhow::Result<Vec<TrackInfo>> {
        Self::ensure_token(access_token)?;
        let cache_key = format!("artist:{artist_id}");
        if let Some(tracks) = self.cache.read().await.get(&cache_key) {
            return Ok(tracks.clone());
        }

        let json = self
            .get_json(
                access_token,
                &format!("/artists/{artist_id}/top-tracks?market=from_token"),
            )
            .await?;
        let tracks: Vec<TrackInfo> = json["tracks"]
            .as_array()
            .map(|items| items.iter().filter_map(Self::parse_track_json).collect())
            .unwrap_or_default();

        self.cache.write().await.insert(cache_key, tracks.clone());

        info!(
            "Fetched {} top tracks for artist {}.",
            tracks.len(),
            artist_id
        );
        Ok(tracks)
    }

    /// Retrieve metadata for a single track by URI or ID.
    pub async fn get_track(&self, access_token: &str, uri: &str) -> anyhow::Result<TrackInfo> {
        let id = Self::id_from_uri(uri);
        let json = self
            .get_json(access_token, &format!("/tracks/{id}"))
            .await?;
        Self::parse_track_json(&json)
            .ok_or_else(|| anyhow::anyhow!("Unexpected track payload for {uri}"))
    }

    /// Retrieve the user's Liked Songs library.
    pub async fn get_liked_songs(
        &self,
        access_token: &str,
        offset: u32,
        limit: u32,
    ) -> anyhow::Result<Vec<TrackInfo>> {
        let url = format!("/me/tracks?offset={}&limit={}", offset, limit.min(50));
        let json = self.get_json(access_token, &url).await?;
        let mut tracks = Vec::new();

        if let Some(items) = json["items"].as_array() {
            for item in items {
                if let Some(mut track) = Self::parse_track_json(&item["track"]) {
                    track.is_liked = true;
                    tracks.push(track);
                }
            }
        }

        info!("Fetched {} Liked Songs from Spotify.", tracks.len());
        Ok(tracks)
    }

    /// Search the Spotify catalog (capped at 10 results).
    pub async fn search(&self, access_token: &str, query: &str) -> anyhow::Result<Vec<TrackInfo>> {
        let url = format!(
            "/search?q={}&type=track&limit=10",
            urlencoding::encode(query)
        );
        let json = self.get_json(access_token, &url).await?;
        let tracks: Vec<TrackInfo> = json["tracks"]["items"]
            .as_array()
            .map(|items| items.iter().filter_map(Self::parse_track_json).collect())
            .unwrap_or_default();

        info!("Search for '{}' returned {} results.", query, tracks.len());
        Ok(tracks)
    }

    /// Save (like) a track to current user's library.
    pub async fn like_track(&self, access_token: &str, uri: &str) -> anyhow::Result<()> {
        Self::ensure_token(access_token)?;
        let url = format!("{API_BASE}/me/tracks?ids={}", Self::id_from_uri(uri));
        self.client
            .put(&url)
            .bearer_auth(access_token)
            .header("Content-Length", 0)
            .send()
            .await?
            .error_for_status()?;
        info!("Liked track: {}", uri);
        Ok(())
    }

    /// Remove (unlike) a track from current user's library.
    pub async fn unlike_track(&self, access_token: &str, uri: &str) -> anyhow::Result<()> {
        Self::ensure_token(access_token)?;
        let url = format!("{API_BASE}/me/tracks?ids={}", Self::id_from_uri(uri));
        self.client
            .delete(&url)
            .bearer_auth(access_token)
            .send()
            .await?
            .error_for_status()?;
        info!("Unliked track: {}", uri);
        Ok(())
    }

    /// Check whether a track is saved (liked) in the current user's library.
    pub async fn is_track_liked(&self, access_token: &str, uri: &str) -> anyhow::Result<bool> {
        Self::ensure_token(access_token)?;
        let id = Self::id_from_uri(uri);
        let json = self
            .get_json(access_token, &format!("/me/tracks/contains?ids={id}"))
            .await?;
        let is_liked = json
            .as_array()
            .and_then(|arr| arr.first())
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        Ok(is_liked)
    }

    /// `spotify:track:abc` -> `abc`; a bare ID is returned unchanged.
    fn id_from_uri(uri: &str) -> &str {
        uri.rsplit(':').next().unwrap_or(uri)
    }

    /// Helper to parse standard Spotify Track JSON into TrackInfo.
    fn parse_track_json(track: &Value) -> Option<TrackInfo> {
        let uri = track["uri"].as_str()?.to_string();
        let title = track["name"].as_str()?.to_string();
        let duration_ms = track["duration_ms"].as_u64()? as u32;

        let artists = track["artists"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|a| a["name"].as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let album = track["album"]["name"]
            .as_str()
            .unwrap_or("Unknown Album")
            .to_string();

        let cover_url = track["album"]["images"]
            .as_array()
            .and_then(|imgs| cover_image(imgs));

        Some(TrackInfo {
            uri,
            title,
            artists,
            album,
            duration_ms,
            cover_url,
            is_liked: false,
        })
    }
}

/// Longest rate-limit wait (Retry-After) sat out before giving up with an error.
const MAX_RATE_LIMIT_WAIT_SECS: u64 = 90;

/// Sends a Spotify Web API request, waiting out short rate limits (429) as Spotify asks.
/// Errors for other failure statuses.
pub(crate) async fn send_spotify(
    req: reqwest::RequestBuilder,
) -> anyhow::Result<reqwest::Response> {
    for _ in 0..4 {
        let attempt = req
            .try_clone()
            .ok_or_else(|| anyhow::anyhow!("request cannot be retried"))?;
        let resp = attempt.send().await?;
        if resp.status() != reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Ok(resp.error_for_status()?);
        }
        let wait = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(2);
        if wait > MAX_RATE_LIMIT_WAIT_SECS {
            let when = if wait >= 2 * 3600 {
                format!("{} hours", wait.div_ceil(3600))
            } else {
                format!("{} min", wait.div_ceil(60))
            };
            anyhow::bail!(
                "Spotify is rate-limiting this app's client ID; try again in about {when}, or \
                 set your own \"spotify_client_id\" (see the README)"
            );
        }
        warn!("Spotify rate limit hit; retrying in {wait}s.");
        tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
    }
    anyhow::bail!("Spotify is still rate-limiting requests; try again shortly")
}

/// The smallest cover image at least as large as the player's art box (Spotify lists
/// 640, 300 and 64 px), or the largest one when none is.
fn cover_image(images: &[serde_json::Value]) -> Option<String> {
    let min = u64::from(crate::album_art::ART_SIZE);
    let width = |img: &serde_json::Value| img["width"].as_u64().unwrap_or(0);
    images
        .iter()
        .filter(|img| width(img) >= min)
        .min_by_key(|img| width(img))
        .or_else(|| images.iter().max_by_key(|img| width(img)))
        .and_then(|img| img["url"].as_str())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_cover_size_closest_above_the_art_box() {
        let images: Vec<serde_json::Value> = serde_json::from_str(
            r#"[{"url":"big","width":640},{"url":"mid","width":300},{"url":"small","width":64}]"#,
        )
        .unwrap();
        assert_eq!(cover_image(&images).as_deref(), Some("mid"));
        assert_eq!(cover_image(&images[2..]).as_deref(), Some("small"));
        assert_eq!(cover_image(&[]), None);
    }

    #[test]
    fn extracts_id_from_various_uri_formats() {
        assert_eq!(
            SpotifyApiClient::id_from_uri("spotify:track:12345"),
            "12345"
        );
        assert_eq!(
            SpotifyApiClient::id_from_uri("spotify:album:abcde"),
            "abcde"
        );
        assert_eq!(
            SpotifyApiClient::id_from_uri("spotify:artist:xyz89"),
            "xyz89"
        );
        assert_eq!(
            SpotifyApiClient::id_from_uri("bare_id_only"),
            "bare_id_only"
        );
    }

    #[test]
    fn parses_valid_track_json() {
        let json: Value = serde_json::json!({
            "uri": "spotify:track:4cOdK2wGLETKBW3PvgPWqT",
            "name": "Never Gonna Give You Up",
            "duration_ms": 213573,
            "artists": [
                { "name": "Rick Astley" }
            ],
            "album": {
                "name": "Whenever You Need Somebody",
                "images": [
                    { "url": "https://i.scdn.co/image/ab67616d0000b2735755e164993798e0c9ef7d7a" }
                ]
            }
        });

        let track = SpotifyApiClient::parse_track_json(&json).expect("should parse");
        assert_eq!(track.uri, "spotify:track:4cOdK2wGLETKBW3PvgPWqT");
        assert_eq!(track.title, "Never Gonna Give You Up");
        assert_eq!(track.artists, vec!["Rick Astley".to_string()]);
        assert_eq!(track.album, "Whenever You Need Somebody");
        assert_eq!(track.duration_ms, 213573);
        assert!(track.cover_url.is_some());
        assert!(!track.is_liked);
    }
}
