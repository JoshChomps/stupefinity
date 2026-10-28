use reqwest::Client;
use serde::Deserialize;
use std::path::PathBuf;
use stupefinity_protocol::{LyricLine, LyricsData};
use tracing::{info, warn};

#[derive(Debug, Deserialize)]
struct LrclibResponse {
    #[serde(rename = "plainLyrics")]
    plain_lyrics: Option<String>,
    #[serde(rename = "syncedLyrics")]
    synced_lyrics: Option<String>,
}

pub struct LyricsEngine {
    client: Client,
    cache_dir: PathBuf,
}

impl Default for LyricsEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl LyricsEngine {
    pub fn new() -> Self {
        let cache_dir = dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Stupefinity")
            .join("cache")
            .join("lyrics");
        let _ = std::fs::create_dir_all(&cache_dir);

        Self {
            client: Client::builder()
                .user_agent(concat!("Stupefinity/", env!("CARGO_PKG_VERSION")))
                .build()
                .unwrap_or_default(),
            cache_dir,
        }
    }

    /// Fetches synced lyrics from cache or LRCLIB REST API.
    pub async fn get_lyrics(
        &self,
        uri: &str,
        title: &str,
        artist: &str,
        album: &str,
        duration_secs: u32,
    ) -> anyhow::Result<LyricsData> {
        let sanitized_id = uri.replace([':', '/'], "_");
        let cache_file = self.cache_dir.join(format!("{sanitized_id}.json"));

        // 1. Check local file cache
        if cache_file.exists() {
            if let Ok(content) = std::fs::read_to_string(&cache_file) {
                if let Ok(data) = serde_json::from_str::<LyricsData>(&content) {
                    info!("Loaded lyrics from cache for: {} - {}", artist, title);
                    return Ok(data);
                }
            }
        }

        info!(
            "Fetching synced lyrics from LRCLIB for: {} - {}",
            artist, title
        );
        let url = format!(
            "https://lrclib.net/api/get?artist_name={}&track_name={}&album_name={}&duration={}",
            urlencoding::encode(artist),
            urlencoding::encode(title),
            urlencoding::encode(album),
            duration_secs
        );

        let resp = match self.client.get(&url).send().await {
            Ok(r) if r.status().is_success() => r,
            Ok(r) => {
                warn!("LRCLIB returned status {}", r.status());
                return Err(anyhow::anyhow!("Lyrics not found"));
            }
            Err(e) => return Err(anyhow::anyhow!("Failed to connect to LRCLIB: {e}")),
        };

        let lrclib_data: LrclibResponse = resp.json().await?;
        let synced_lines = lrclib_data
            .synced_lyrics
            .as_deref()
            .map(Self::parse_lrc)
            .unwrap_or_default();

        let lyrics_data = LyricsData {
            uri: uri.to_string(),
            track_title: title.to_string(),
            artist_name: artist.to_string(),
            plain_lyrics: lrclib_data.plain_lyrics,
            synced_lyrics: synced_lines,
        };

        // Cache result
        if let Ok(serialized) = serde_json::to_string(&lyrics_data) {
            let _ = std::fs::write(&cache_file, serialized);
        }

        Ok(lyrics_data)
    }

    /// Parses LRC text into time-ordered lines. A line may carry several timestamps
    /// (`[00:12.00][01:30.00] chorus`); metadata tags such as `[ar:Artist]` are skipped.
    fn parse_lrc(raw: &str) -> Vec<LyricLine> {
        let mut lines = Vec::new();
        for line in raw.lines() {
            let mut rest = line.trim();
            let mut stamps = Vec::new();
            while let Some(after_open) = rest.strip_prefix('[') {
                let Some(close) = after_open.find(']') else {
                    break;
                };
                match Self::parse_timestamp(&after_open[..close]) {
                    Some(ms) => stamps.push(ms),
                    None => break,
                }
                rest = &after_open[close + 1..];
            }
            let text = rest.trim();
            lines.extend(stamps.into_iter().map(|time_ms| LyricLine {
                time_ms,
                text: text.to_string(),
            }));
        }
        lines.sort_by_key(|l| l.time_ms);
        lines
    }

    /// Parses `mm:ss.xx` into milliseconds.
    fn parse_timestamp(stamp: &str) -> Option<u32> {
        let (minutes, seconds) = stamp.split_once(':')?;
        let minutes: u32 = minutes.parse().ok()?;
        let seconds: f32 = seconds.parse().ok()?;
        Some(minutes * 60_000 + (seconds * 1000.0).round() as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lrc_with_tags_and_repeated_timestamps() {
        let raw = "[ar:Someone]
[00:01.50] first
[00:10.00][00:03.25]chorus
no stamp";
        let lines = LyricsEngine::parse_lrc(raw);
        let got: Vec<(u32, &str)> = lines.iter().map(|l| (l.time_ms, l.text.as_str())).collect();
        assert_eq!(
            got,
            vec![(1500, "first"), (3250, "chorus"), (10_000, "chorus")]
        );
    }
}
