use rand::seq::SliceRandom;
use rand::{thread_rng, Rng};
use std::collections::VecDeque;
use stupefinity_protocol::RepeatMode;

#[derive(Debug, Clone)]
pub struct QueueManager {
    pub context_uri: Option<String>,
    pub tracks: Vec<String>,
    pub order: Vec<usize>,
    pub pos: usize,
    pub user_queue: VecDeque<String>,
    pub repeat: RepeatMode,
    pub shuffle: bool,
}

impl Default for QueueManager {
    fn default() -> Self {
        Self::new()
    }
}

impl QueueManager {
    pub fn new() -> Self {
        Self {
            context_uri: None,
            tracks: Vec::new(),
            order: Vec::new(),
            pos: 0,
            user_queue: VecDeque::new(),
            repeat: RepeatMode::Off,
            shuffle: false,
        }
    }

    /// Load a new playlist/album context.
    pub fn load_context(&mut self, context_uri: String, tracks: Vec<String>, start_index: usize) {
        let count = tracks.len();
        self.context_uri = Some(context_uri);
        self.tracks = tracks;
        self.order = (0..count).collect();
        self.pos = start_index.min(count.saturating_sub(1));

        if self.shuffle {
            self.apply_shuffle();
        }
    }

    /// Returns the upcoming URIs in playback order up to `limit` items,
    /// tagged with `is_user_queued: true` for user queue items and `false` for context items.
    pub fn upcoming_uris(&self, limit: usize) -> Vec<(String, bool)> {
        let mut result = Vec::with_capacity(limit);

        // 1. User queue first
        for u in &self.user_queue {
            if result.len() >= limit {
                return result;
            }
            result.push((u.clone(), true));
        }

        if self.order.is_empty() {
            return result;
        }

        // 2. Context tracks following current position
        let mut idx = self.pos + 1;
        let total = self.order.len();
        let mut visited = 0;

        while result.len() < limit && visited < total.saturating_sub(1) {
            if idx < total {
                let track_idx = self.order[idx];
                if let Some(uri) = self.tracks.get(track_idx) {
                    result.push((uri.clone(), false));
                }
                idx += 1;
                visited += 1;
            } else if self.repeat == RepeatMode::Context {
                idx = 0;
            } else {
                break;
            }
        }

        result
    }

    /// Enqueue a track manually into the priority user queue.
    pub fn enqueue(&mut self, track_uri: String) {
        self.user_queue.push_back(track_uri);
    }

    /// Remove a track from the manual user queue.
    pub fn remove_from_user_queue(&mut self, index: usize) -> Option<String> {
        if index < self.user_queue.len() {
            self.user_queue.remove(index)
        } else {
            None
        }
    }

    /// Get current active track URI.
    pub fn current_track(&self) -> Option<String> {
        if self.order.is_empty() || self.pos >= self.order.len() {
            None
        } else {
            let idx = self.order[self.pos];
            self.tracks.get(idx).cloned()
        }
    }

    /// Advance to the next track.
    /// User queue plays first; otherwise advances context pos.
    pub fn advance(&mut self) -> Option<String> {
        // Priority 1: User queue
        if let Some(user_track) = self.user_queue.pop_front() {
            return Some(user_track);
        }

        if self.order.is_empty() {
            return None;
        }

        match self.repeat {
            RepeatMode::Track => self.current_track(),
            RepeatMode::Context => {
                if self.shuffle && self.pos + 1 >= self.order.len() {
                    self.reshuffle_pass();
                } else {
                    self.pos = (self.pos + 1) % self.order.len();
                }
                self.current_track()
            }
            RepeatMode::Off => {
                if self.pos + 1 < self.order.len() {
                    self.pos += 1;
                    self.current_track()
                } else {
                    None // End of context
                }
            }
        }
    }

    /// Skip to previous track.
    pub fn prev(&mut self, current_position_secs: u32) -> Option<String> {
        if current_position_secs > 3 {
            // Restart current track if played more than 3 seconds
            return self.current_track();
        }

        if self.pos > 0 {
            self.pos -= 1;
        } else if self.repeat == RepeatMode::Context && !self.order.is_empty() {
            self.pos = self.order.len() - 1;
        }
        self.current_track()
    }

    /// Toggle shuffle mode.
    pub fn set_shuffle(&mut self, enabled: bool) {
        if self.shuffle == enabled {
            return;
        }
        self.shuffle = enabled;

        if enabled {
            self.apply_shuffle();
        } else {
            // Restore identity order, but keep the current track active
            if let Some(&original_idx) = self.order.get(self.pos) {
                self.pos = original_idx;
            }
            self.order = (0..self.tracks.len()).collect();
        }
    }

    /// Shuffles the context: the active track stays playing and leads the new order, and
    /// every other track follows in a uniformly random order (Fisher-Yates, driven by the
    /// OS-seeded ChaCha generator behind `thread_rng`). Unlike Spotify's shuffle there is
    /// no weighting or spreading of artists, and nothing earlier in the list is left out.
    fn apply_shuffle(&mut self) {
        if self.order.is_empty() {
            return;
        }
        let current = self.order.get(self.pos).copied().unwrap_or(0);
        let mut rest: Vec<usize> = (0..self.tracks.len()).filter(|&i| i != current).collect();
        rest.shuffle(&mut thread_rng());
        self.order = std::iter::once(current).chain(rest).collect();
        self.pos = 0;
    }

    /// Starts another pass of a repeating shuffled context in a fresh random order, never
    /// opening with the track that just ended.
    fn reshuffle_pass(&mut self) {
        let last = self.order[self.pos];
        let mut rng = thread_rng();
        self.order.shuffle(&mut rng);
        if self.order.len() > 1 && self.order[0] == last {
            let swap_with = rng.gen_range(1..self.order.len());
            self.order.swap(0, swap_with);
        }
        self.pos = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue(n: usize) -> QueueManager {
        let mut q = QueueManager::new();
        let tracks = (0..n).map(|i| format!("spotify:track:{i}")).collect();
        q.load_context("spotify:playlist:x".into(), tracks, 0);
        q
    }

    #[test]
    fn next_stops_at_end_without_repeat() {
        let mut q = queue(2);
        assert_eq!(q.advance().as_deref(), Some("spotify:track:1"));
        assert_eq!(q.advance(), None);
    }

    #[test]
    fn repeat_context_wraps_both_ways() {
        let mut q = queue(3);
        q.repeat = RepeatMode::Context;
        assert_eq!(q.prev(0).as_deref(), Some("spotify:track:2"));
        assert_eq!(q.advance().as_deref(), Some("spotify:track:0"));
    }

    #[test]
    fn repeat_track_replays_current() {
        let mut q = queue(3);
        q.repeat = RepeatMode::Track;
        assert_eq!(q.advance().as_deref(), Some("spotify:track:0"));
    }

    #[test]
    fn user_queue_takes_priority() {
        let mut q = queue(3);
        q.enqueue("spotify:track:user".into());
        assert_eq!(q.advance().as_deref(), Some("spotify:track:user"));
        assert_eq!(q.advance().as_deref(), Some("spotify:track:1"));
    }

    #[test]
    fn prev_restarts_after_three_seconds() {
        let mut q = queue(3);
        q.advance();
        assert_eq!(q.prev(10).as_deref(), Some("spotify:track:1"));
        assert_eq!(q.prev(0).as_deref(), Some("spotify:track:0"));
    }

    #[test]
    fn shuffle_round_trip_keeps_current_track() {
        let mut q = queue(50);
        q.advance();
        q.advance();
        q.set_shuffle(true);
        assert_eq!(q.current_track().as_deref(), Some("spotify:track:2"));
        let mut sorted = q.order.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..50).collect::<Vec<_>>());
        q.set_shuffle(false);
        assert_eq!(q.current_track().as_deref(), Some("spotify:track:2"));
        assert_eq!(q.pos, 2);
    }

    #[test]
    fn unshuffle_with_duplicate_tracks_keeps_position() {
        let mut q = QueueManager::new();
        let dup = "spotify:track:dup".to_string();
        q.load_context("ctx".into(), vec![dup.clone(), "a".into(), dup], 2);
        q.set_shuffle(true);
        q.set_shuffle(false);
        assert_eq!(q.pos, 2);
    }

    /// Plays a context to its end, returning every track heard (including the first).
    fn play_through(q: &mut QueueManager) -> Vec<String> {
        let mut heard: Vec<String> = q.current_track().into_iter().collect();
        while let Some(t) = q.advance() {
            heard.push(t);
        }
        heard
    }

    #[test]
    fn shuffle_from_the_middle_still_plays_every_track_once() {
        let mut q = QueueManager::new();
        q.set_shuffle(true);
        let tracks: Vec<String> = (0..100).map(|i| format!("t{i}")).collect();
        q.load_context("ctx".into(), tracks.clone(), 80);
        assert_eq!(q.current_track().as_deref(), Some("t80"));
        let mut heard = play_through(&mut q);
        assert_eq!(heard.len(), 100);
        heard.sort();
        let mut expected = tracks;
        expected.sort();
        assert_eq!(heard, expected);
    }

    #[test]
    fn enabling_shuffle_mid_context_covers_the_whole_context() {
        let mut q = queue(30);
        for _ in 0..10 {
            q.advance();
        }
        q.set_shuffle(true);
        assert_eq!(q.current_track().as_deref(), Some("spotify:track:10"));
        assert_eq!(play_through(&mut q).len(), 30);
    }

    #[test]
    fn repeat_shuffle_reshuffles_each_pass_without_back_to_back_repeats() {
        let mut q = queue(20);
        q.repeat = RepeatMode::Context;
        q.set_shuffle(true);
        // Finish the first pass, which started on the playing track.
        let mut previous = q.current_track().unwrap();
        for _ in 0..19 {
            previous = q.advance().unwrap();
        }
        let mut passes = Vec::new();
        for _ in 0..5 {
            let pass: Vec<String> = (0..20).map(|_| q.advance().unwrap()).collect();
            assert_ne!(pass[0], previous, "a new pass repeated the last track");
            previous = pass[19].clone();
            let mut sorted = pass.clone();
            sorted.sort();
            sorted.dedup();
            assert_eq!(sorted.len(), 20, "a pass must play every track once");
            passes.push(pass);
        }
        assert!(
            passes.windows(2).any(|w| w[0] != w[1]),
            "passes were not reshuffled"
        );
    }

    #[test]
    fn shuffle_is_uniform() {
        // Where track 1 lands after shuffling 4 tracks from track 0: each of the three
        // slots after the playing track should be equally likely.
        let mut counts = [0u32; 4];
        let trials = 30_000;
        for _ in 0..trials {
            let mut q = queue(4);
            q.set_shuffle(true);
            counts[q.order.iter().position(|&i| i == 1).unwrap()] += 1;
        }
        assert_eq!(counts[0], 0);
        for &c in &counts[1..] {
            let share = f64::from(c) / f64::from(trials);
            assert!((share - 1.0 / 3.0).abs() < 0.02, "slot counts {counts:?}");
        }
    }

    #[test]
    fn upcoming_uris_returns_user_queue_first_then_context() {
        let mut q = queue(5); // tracks: 0, 1, 2, 3, 4
        q.enqueue("spotify:track:user1".into());
        q.enqueue("spotify:track:user2".into());

        // Currently at track 0, upcoming should be user1, user2, track 1, track 2
        let upcoming = q.upcoming_uris(4);
        assert_eq!(upcoming.len(), 4);
        assert_eq!(upcoming[0], ("spotify:track:user1".to_string(), true));
        assert_eq!(upcoming[1], ("spotify:track:user2".to_string(), true));
        assert_eq!(upcoming[2], ("spotify:track:1".to_string(), false));
        assert_eq!(upcoming[3], ("spotify:track:2".to_string(), false));
    }
}
