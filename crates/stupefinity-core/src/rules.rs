use std::path::PathBuf;
use stupefinity_protocol::{LocalRulesConfig, TrackInfo};
use tracing::info;

pub struct RulesEngine {
    pub config: LocalRulesConfig,
    config_file: PathBuf,
}

impl Default for RulesEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl RulesEngine {
    pub fn new() -> Self {
        let config_file = dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Stupefinity")
            .join("rules.json");

        let config = if config_file.exists() {
            std::fs::read_to_string(&config_file)
                .ok()
                .and_then(|c| serde_json::from_str(c.trim_start_matches('\u{feff}')).ok())
                .unwrap_or_default()
        } else {
            LocalRulesConfig::default()
        };

        Self {
            config,
            config_file,
        }
    }

    /// Evaluates if a track should be automatically skipped according to local rules.
    pub fn should_skip(&self, track: &TrackInfo) -> bool {
        if !self.config.enabled {
            return false;
        }

        // 1. Check blocked track URI
        if self
            .config
            .blocked_tracks
            .iter()
            .any(|uri| uri == &track.uri)
        {
            info!(
                "Rules Engine: Skipped track '{}' (matches blocked URI)",
                track.title
            );
            return true;
        }

        // 2. Check blocked artist name
        for artist in &track.artists {
            if self
                .config
                .blocked_artists
                .iter()
                .any(|b| b.eq_ignore_ascii_case(artist))
            {
                info!(
                    "Rules Engine: Skipped track '{}' (blocked artist '{}')",
                    track.title, artist
                );
                return true;
            }
        }

        // 3. Check min duration (filter skits and interludes)
        if self.config.min_duration_secs > 0
            && (track.duration_ms / 1000) < self.config.min_duration_secs
        {
            info!(
                "Rules Engine: Skipped track '{}' (duration {}s is below minimum {}s)",
                track.title,
                track.duration_ms / 1000,
                self.config.min_duration_secs
            );
            return true;
        }

        false
    }

    /// Save updated rules to disk.
    pub fn save(&self) {
        if let Ok(serialized) = serde_json::to_string_pretty(&self.config) {
            let _ = std::fs::write(&self.config_file, serialized);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine(config: LocalRulesConfig) -> RulesEngine {
        RulesEngine {
            config,
            config_file: PathBuf::new(),
        }
    }

    fn track(uri: &str, artist: &str, secs: u32) -> TrackInfo {
        TrackInfo {
            uri: uri.into(),
            artists: vec![artist.into()],
            duration_ms: secs * 1000,
            ..Default::default()
        }
    }

    #[test]
    fn disabled_rules_never_skip() {
        let r = engine(LocalRulesConfig {
            blocked_tracks: vec!["a".into()],
            ..Default::default()
        });
        assert!(!r.should_skip(&track("a", "x", 100)));
    }

    #[test]
    fn skips_blocked_uri_artist_and_short_tracks() {
        let r = engine(LocalRulesConfig {
            blocked_tracks: vec!["blocked".into()],
            blocked_artists: vec!["Nickelback".into()],
            min_duration_secs: 60,
            enabled: true,
            ..Default::default()
        });
        assert!(r.should_skip(&track("blocked", "x", 100)));
        assert!(r.should_skip(&track("a", "NICKELBACK", 100)));
        assert!(r.should_skip(&track("a", "x", 30)));
        assert!(!r.should_skip(&track("a", "x", 100)));
    }
}
