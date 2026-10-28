use serde::{Deserialize, Serialize};

/// Standard Windows Named Pipe path for Stupefinity IPC.
pub const DEFAULT_PIPE_NAME: &str = r"\\.\pipe\stupefinity-ipc";

/// Authentication status of the core daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum AuthState {
    #[default]
    Unauthenticated,
    Authenticating,
    Authenticated {
        user_id: String,
        display_name: String,
        is_premium: bool,
    },
}

/// Commands sent from the UI or CLI to the resident Core daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Command {
    // Playback Controls
    Play,
    Pause,
    PlayPauseToggle,
    Next,
    Prev,
    Seek(u32),
    LoadContext {
        uri: String,
        start_index: usize,
    },
    Enqueue(String),
    RemoveFromQueue(usize),
    SetVolume(u16),
    SetShuffle(bool),
    SetRepeat(RepeatMode),

    // Authentication
    Login,
    Logout,
    /// Agree to the current Terms of Use and Privacy Policy.
    AcceptTerms,
    /// The client ID of the user's own Spotify app; empty clears it.
    SetClientId(String),

    // Data & Library
    GetPlaylists,
    GetPlaylistTracks {
        playlist_id: String,
    },
    GetLikedSongs {
        offset: u32,
        limit: u32,
    },
    Search {
        query: String,
    },
    LikeTrack {
        uri: String,
    },
    UnlikeTrack {
        uri: String,
    },

    // Advanced Audio & Features
    GetLyrics,
    SetRules(LocalRulesConfig),
    SetSmartShuffle(SmartShuffleConfig),
    SetEq(EqProfile),
    ToggleEq(bool),

    // Window / Lifecycle Management
    WindowShown,
    WindowHidden,
    GetSnapshot,
    Shutdown,
}

/// The repeat mode of playback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum RepeatMode {
    #[default]
    Off,
    Context,
    Track,
}

/// Current playback state of the audio engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum PlaybackState {
    #[default]
    Stopped,
    Playing,
    Paused,
    Buffering,
}

/// Metadata description of a track.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TrackInfo {
    pub uri: String,
    pub title: String,
    pub artists: Vec<String>,
    pub album: String,
    pub duration_ms: u32,
    pub cover_url: Option<String>,
    pub is_liked: bool,
}

/// Lightweight representation of a user's playlist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PlaylistSummary {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub track_count: u32,
    pub image_url: Option<String>,
    pub owner_name: String,
}

/// Single entry in the queue window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueItem {
    pub uri: String,
    pub title: String,
    pub artist: String,
    pub duration_ms: u32,
    pub is_user_queued: bool,
}

/// Single line in synced lyrics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct LyricLine {
    pub time_ms: u32,
    pub text: String,
}

/// Full lyrics data for a track.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct LyricsData {
    pub uri: String,
    pub track_title: String,
    pub artist_name: String,
    pub plain_lyrics: Option<String>,
    pub synced_lyrics: Vec<LyricLine>,
}

/// Local skip and filtering rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct LocalRulesConfig {
    pub blocked_tracks: Vec<String>,
    pub blocked_artists: Vec<String>,
    pub min_duration_secs: u32,
    pub skip_intro_secs: u32,
    pub enabled: bool,
}

/// Smart Shuffle settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SmartShuffleConfig {
    pub enabled: bool,
    pub ratio: u32,
    pub local_only: bool,
}

impl Default for SmartShuffleConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            ratio: 4,
            local_only: true,
        }
    }
}

/// Equalizer filter band.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FilterBand {
    pub filter_type: String,
    pub frequency: f32,
    pub q: f32,
    pub gain_db: f32,
}

/// Equalizer profile definition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct EqProfile {
    pub name: String,
    pub preamp_db: f32,
    pub bands: Vec<FilterBand>,
    pub enabled: bool,
}

/// Lightweight snapshot sent from Core to UI/CLI representing the player state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Snapshot {
    pub auth_state: AuthState,
    /// Why the last login attempt failed, shown on the welcome screen.
    #[serde(default)]
    pub login_error: Option<String>,
    /// The current Terms of Use and Privacy Policy were agreed to.
    #[serde(default)]
    pub terms_accepted: bool,
    /// No Spotify client ID is set yet, so login is not possible.
    #[serde(default)]
    pub needs_client_id: bool,
    pub playback_state: PlaybackState,
    pub current_track: Option<TrackInfo>,
    pub position_ms: u32,
    pub duration_ms: u32,
    pub volume: u16,
    pub shuffle: bool,
    pub repeat: RepeatMode,
    pub queue_window: Vec<QueueItem>,
    pub is_window_visible: bool,
    pub smart_shuffle_enabled: bool,
    pub eq_enabled: bool,
}

/// Messages emitted from Core to attached UI or CLI clients.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CoreEvent {
    State(Snapshot),
    Playlists(Vec<PlaylistSummary>),
    PlaylistTracks {
        playlist_id: String,
        tracks: Vec<TrackInfo>,
    },
    LikedSongs(Vec<TrackInfo>),
    SearchResults {
        query: String,
        tracks: Vec<TrackInfo>,
    },
    Lyrics(LyricsData),
    Rules(LocalRulesConfig),
    SmartShuffle(SmartShuffleConfig),
    Error(String),
}
