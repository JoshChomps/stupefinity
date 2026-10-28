use rand::seq::SliceRandom;
use rand::thread_rng;
use std::collections::HashSet;
use stupefinity_protocol::{SmartShuffleConfig, TrackInfo};
use tracing::info;

pub struct SmartShuffleEngine {
    pub config: SmartShuffleConfig,
    liked_pool: Vec<TrackInfo>,
    played_history: HashSet<String>,
    counter: u32,
}

impl Default for SmartShuffleEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl SmartShuffleEngine {
    pub fn new() -> Self {
        Self {
            config: SmartShuffleConfig::default(),
            liked_pool: Vec::new(),
            played_history: HashSet::new(),
            counter: 0,
        }
    }

    /// Update the candidate pool (e.g. from Liked Songs).
    pub fn update_candidate_pool(&mut self, tracks: Vec<TrackInfo>) {
        info!(
            "Smart Shuffle candidate pool updated with {} tracks.",
            tracks.len()
        );
        self.liked_pool = tracks;
    }

    /// Record a played track so it is not immediately re-recommended.
    pub fn record_play(&mut self, uri: &str) {
        self.played_history.insert(uri.to_string());
    }

    /// Evaluates if the next track should be a smart suggestion.
    /// Returns a recommended track if ratio condition is met and candidates exist.
    pub fn maybe_suggest_track(&mut self, active_playlist_uris: &[String]) -> Option<TrackInfo> {
        if !self.config.enabled || self.liked_pool.is_empty() {
            return None;
        }

        self.counter += 1;
        if self.counter < self.config.ratio {
            return None;
        }

        // Reset counter
        self.counter = 0;

        let active_set: HashSet<&str> = active_playlist_uris.iter().map(|s| s.as_str()).collect();

        // Filter candidates: not in active playlist and not recently played
        let mut candidates: Vec<&TrackInfo> = self
            .liked_pool
            .iter()
            .filter(|t| {
                !active_set.contains(t.uri.as_str()) && !self.played_history.contains(&t.uri)
            })
            .collect();

        if candidates.is_empty() {
            // Reset history if exhausted
            self.played_history.clear();
            candidates = self
                .liked_pool
                .iter()
                .filter(|t| !active_set.contains(t.uri.as_str()))
                .collect();
        }

        let mut rng = thread_rng();
        if let Some(&chosen) = candidates.choose(&mut rng) {
            info!(
                "Smart Shuffle: Recommending '{}' by {}",
                chosen.title,
                chosen.artists.join(", ")
            );
            self.played_history.insert(chosen.uri.clone());
            Some(chosen.clone())
        } else {
            None
        }
    }
}
