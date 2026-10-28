use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use stupefinity_protocol::{EqProfile, RepeatMode, SmartShuffleConfig};
use tracing::{info, warn};

fn default_volume() -> u16 {
    u16::MAX
}

/// User settings persisted across application launches in `%LOCALAPPDATA%\Stupefinity\settings.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppSettings {
    #[serde(default = "default_volume")]
    pub volume: u16,
    #[serde(default)]
    pub shuffle: bool,
    #[serde(default)]
    pub repeat: RepeatMode,
    #[serde(default)]
    pub smart_shuffle: SmartShuffleConfig,
    #[serde(default)]
    pub eq_profile: EqProfile,
    #[serde(default)]
    pub eq_enabled: bool,
    #[serde(default)]
    pub discord_client_id: Option<String>,
    /// Client ID of your own Spotify app (developer.spotify.com/dashboard).
    #[serde(default)]
    pub spotify_client_id: Option<String>,
    /// Version of the Terms of Use and Privacy Policy last agreed to (0: never).
    #[serde(default)]
    pub accepted_terms_version: u32,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            volume: default_volume(),
            shuffle: false,
            repeat: RepeatMode::Off,
            smart_shuffle: SmartShuffleConfig::default(),
            eq_profile: EqProfile::default(),
            eq_enabled: false,
            discord_client_id: None,
            spotify_client_id: None,
            accepted_terms_version: 0,
        }
    }
}

pub struct SettingsManager {
    pub settings: AppSettings,
    settings_file: PathBuf,
}

impl Default for SettingsManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SettingsManager {
    pub fn new() -> Self {
        let settings_file = dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Stupefinity")
            .join("settings.json");

        let settings = Self::load_from(&settings_file);
        Self {
            settings,
            settings_file,
        }
    }

    /// Load settings from a specific path with fallback to defaults.
    pub fn load_from(path: &Path) -> AppSettings {
        if path.exists() {
            match std::fs::read_to_string(path) {
                // Notepad and PowerShell may add a byte-order mark when the file is edited by hand.
                Ok(content) => match serde_json::from_str::<AppSettings>(
                    content.trim_start_matches('\u{feff}'),
                ) {
                    Ok(s) => {
                        info!("Loaded settings from {:?}", path);
                        return s;
                    }
                    Err(e) => warn!(
                        "Corrupt settings file {:?}, reverting to defaults: {e}",
                        path
                    ),
                },
                Err(e) => warn!("Failed to read settings file {:?}: {e}", path),
            }
        }
        AppSettings::default()
    }

    /// Save current settings to disk.
    pub fn save(&self) {
        if let Some(parent) = self.settings_file.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match serde_json::to_string_pretty(&self.settings) {
            Ok(serialized) => {
                if let Err(e) = std::fs::write(&self.settings_file, serialized) {
                    warn!("Failed to save settings to {:?}: {e}", self.settings_file);
                } else {
                    info!("Saved settings to {:?}", self.settings_file);
                }
            }
            Err(e) => warn!("Failed to serialize settings: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_settings_sane() {
        let s = AppSettings::default();
        assert_eq!(s.volume, u16::MAX);
        assert!(!s.shuffle);
        assert_eq!(s.repeat, RepeatMode::Off);
        assert!(!s.smart_shuffle.enabled);
        assert!(!s.eq_enabled);
    }

    #[test]
    fn roundtrip_serialization() {
        let original = AppSettings {
            volume: 32768,
            shuffle: true,
            repeat: RepeatMode::Track,
            eq_enabled: true,
            ..Default::default()
        };

        let json = serde_json::to_string(&original).unwrap();
        let loaded: AppSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(original, loaded);
    }

    #[test]
    fn partial_json_falls_back_to_defaults() {
        let json = r#"{"volume": 12345}"#;
        let s: AppSettings = serde_json::from_str(json).unwrap();
        assert_eq!(s.volume, 12345);
        assert!(!s.shuffle);
        assert_eq!(s.repeat, RepeatMode::Off);
    }

    #[test]
    fn corrupt_file_falls_back_to_defaults() {
        let temp = std::env::temp_dir().join(format!(
            "stupefinity_test_settings_{}.json",
            std::process::id()
        ));
        std::fs::write(&temp, "{ not valid json").unwrap();
        let s = SettingsManager::load_from(&temp);
        assert_eq!(s, AppSettings::default());
        let _ = std::fs::remove_file(temp);
    }

    #[test]
    fn loads_a_file_with_a_byte_order_mark() {
        let temp = std::env::temp_dir().join(format!(
            "stupefinity_test_settings_bom_{}.json",
            std::process::id()
        ));
        std::fs::write(&temp, "\u{feff}{\"volume\": 4321}").unwrap();
        assert_eq!(SettingsManager::load_from(&temp).volume, 4321);
        let _ = std::fs::remove_file(temp);
    }
}
