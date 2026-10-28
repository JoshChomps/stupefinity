pub mod album_art;
pub mod api;
pub mod discord;
pub mod eq;
pub mod ipc;
pub mod lyrics;
pub mod platform;
pub mod player;
pub mod queue;
pub mod rules;
pub mod session;
pub mod settings;
pub mod smart_shuffle;
pub mod streaming;
pub mod ui;

use api::SpotifyApiClient;
use eq::EqualizerEngine;
use ipc::{ClientRequest, IpcServer};
use lyrics::LyricsEngine;
use platform::PlatformManager;
use player::AudioPlayer;
use queue::QueueManager;
use rules::RulesEngine;
use session::SessionManager;
use smart_shuffle::SmartShuffleEngine;
use std::collections::HashMap;
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use stupefinity_protocol::{
    AuthState, Command, CoreEvent, PlaybackState, QueueItem, Snapshot, TrackInfo,
};
use tokio::sync::{mpsc, oneshot, RwLock};
use tracing::{error, info, warn, Level};
use tracing_subscriber::FmtSubscriber;
use ui::NativeUi;

/// Version of docs/legal/TERMS.md and PRIVACY.md; raising it asks users to agree again.
const TERMS_VERSION: u32 = 1;
/// How long the browser login may take before the core gives up waiting.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
/// How long settings must stay unchanged before they are written to disk.
const SETTINGS_SAVE_DELAY: Duration = Duration::from_secs(1);
/// Most Liked Songs loaded into the Smart Shuffle pool.
const SMART_SHUFFLE_POOL_MAX: u32 = 2000;

/// A second way to play tracks that Spotify will not stream to the account. The public app
/// has none, so it plays through Spotify Premium only.
#[async_trait::async_trait]
pub trait FallbackSource: Send + Sync {
    /// Shown under the login button.
    fn welcome_note(&self) -> &'static str;
    /// Starts playing `play.track` in the background.
    fn play(&self, play: FallbackPlay);
    /// Deletes what it saved about the user's tracks (on logout).
    async fn forget(&self);
}

/// A track for a [`FallbackSource`] to play.
pub struct FallbackPlay {
    pub track: TrackInfo,
    /// The track after it, for downloading ahead.
    pub next: Option<TrackInfo>,
    pub skip_intro_ms: u64,
    player: AudioPlayer,
    state: Arc<RwLock<Snapshot>>,
    generation: u64,
    current: Arc<AtomicU64>,
    req_tx: mpsc::Sender<ClientRequest>,
}

impl FallbackPlay {
    /// False once another track was requested; work for this one should stop.
    pub fn is_current(&self) -> bool {
        self.current.load(Ordering::SeqCst) == self.generation
    }

    /// Hands a buffered stream to the audio player, keeping a pause made while it buffered.
    pub async fn begin(&self, reader: streaming::ProgressiveStreamReader) {
        self.player.play_reader(Box::new(reader));
        if self.skip_intro_ms > 0 {
            self.player.seek(self.skip_intro_ms);
        }
        let mut s = self.state.write().await;
        if s.playback_state == PlaybackState::Paused {
            self.player.pause();
        } else {
            s.playback_state = PlaybackState::Playing;
        }
    }

    /// Reports that the track could not play.
    pub async fn fail(&self, reason: String) {
        let generation = self.generation;
        let _ = self
            .req_tx
            .send(ClientRequest::PlaybackFailed { generation, reason })
            .await;
    }
}

/// Runs the app, playing through Spotify Premium only.
pub async fn run_app() -> anyhow::Result<()> {
    run(None).await
}

/// Runs the app with a fallback source, made once the data folder is ready.
pub async fn run_app_with(
    fallback: impl FnOnce() -> Arc<dyn FallbackSource> + 'static,
) -> anyhow::Result<()> {
    run(Some(Box::new(fallback))).await
}

type MakeFallback = Box<dyn FnOnce() -> Arc<dyn FallbackSource>>;

async fn run(make_fallback: Option<MakeFallback>) -> anyhow::Result<()> {
    migrate_data_dir();
    // Logs go to a file (rewritten each launch), since release builds have no console.
    let log_path = cache_dir().parent().map_or_else(
        || PathBuf::from("stupefinity.log"),
        |d| d.join("stupefinity.log"),
    );
    let builder = FmtSubscriber::builder().with_max_level(Level::INFO);
    let _ = match std::fs::File::create(&log_path) {
        Ok(file) => tracing::subscriber::set_global_default(
            builder
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(file))
                .finish(),
        ),
        Err(_) => tracing::subscriber::set_global_default(builder.finish()),
    };

    info!("=========================================");
    info!("        STUPEFINITY (Native Windows 11)      ");
    info!("=========================================");

    // 1. Single instance enforcement
    let mut platform_mgr = PlatformManager::new();
    if !platform_mgr.enforce_single_instance() {
        info!("Another instance of Stupefinity is already running. Focusing existing instance.");
        NativeUi::focus_existing();
        return Ok(());
    }

    remove_stale_streams();
    let settings_mgr = settings::SettingsManager::new();
    let initial_settings = settings_mgr.settings.clone();
    session::set_client_id(initial_settings.spotify_client_id.clone());
    let fallback = make_fallback.map(|make| make());
    if let Some(f) = &fallback {
        ui::set_welcome_note(f.welcome_note());
    }

    // Set up before the session: a Premium login builds librespot's output, which uses it.
    let eq = eq::shared();
    if let Ok(mut engine) = eq.lock() {
        engine.set_profile(initial_settings.eq_profile.clone());
        engine.set_enabled(initial_settings.eq_enabled);
    }

    // 2. Initialize Spotify session manager (non-blocking deferred auth)
    let session = Arc::new(RwLock::new(SessionManager::new().await?));
    let startup_error = premium_required(&mut *session.write().await, fallback.is_some());
    let initial_auth = session.read().await.auth_state();
    info!("Initial Auth State: {:?}", initial_auth);

    let state = Arc::new(RwLock::new(Snapshot {
        auth_state: initial_auth,
        login_error: startup_error,
        terms_accepted: initial_settings.accepted_terms_version >= TERMS_VERSION,
        needs_client_id: !session::has_client_id(),
        volume: initial_settings.volume,
        shuffle: initial_settings.shuffle,
        repeat: initial_settings.repeat,
        smart_shuffle_enabled: initial_settings.smart_shuffle.enabled,
        eq_enabled: initial_settings.eq_enabled,
        is_window_visible: true,
        ..Default::default()
    }));
    let (req_tx, req_rx) = mpsc::channel::<ClientRequest>(64);
    let (finished_tx, finished_rx) = mpsc::unbounded_channel();

    // 3. Native Win32 window, system tray and global media keys
    NativeUi::start(state.clone(), req_tx.clone());

    // 4. Named Pipe IPC server (for CLI commands)
    let ipc_server = IpcServer::new(state.clone(), req_tx.clone());
    tokio::spawn(async move {
        if let Err(e) = ipc_server.run().await {
            error!("IPC server error: {e}");
        }
    });

    discord::spawn(state.clone(), initial_settings.discord_client_id.clone());
    spawn_token_refresher(session.clone());

    let player = AudioPlayer::new(finished_tx, eq.clone());
    player.set_volume(f32::from(initial_settings.volume) / f32::from(u16::MAX));

    let mut queue = QueueManager::new();
    queue.set_shuffle(initial_settings.shuffle);
    queue.repeat = initial_settings.repeat;

    let mut smart_shuffle_engine = SmartShuffleEngine::new();
    smart_shuffle_engine.config = initial_settings.smart_shuffle;

    let api = Arc::new(SpotifyApiClient::new());
    let smart_shuffle = Arc::new(RwLock::new(smart_shuffle_engine));

    // Spawn initial event forwarders & background loaders if already authenticated
    if session.read().await.is_authenticated {
        spawn_smart_shuffle_loader(session.clone(), api.clone(), smart_shuffle.clone());
        spawn_librespot_event_forwarder(session.clone(), req_tx.clone());
    }

    let mut core = Core {
        state,
        session,
        api,
        fallback,
        lyrics: Arc::new(LyricsEngine::new()),
        rules: RulesEngine::new(),
        settings: settings_mgr,
        smart_shuffle,
        eq,
        player,
        queue,
        context_tracks: HashMap::new(),
        queued_tracks: HashMap::new(),
        settings_dirty_since: None,
        generation: Arc::new(AtomicU64::new(0)),
        skip_streak: 0,
        req_tx,
        is_playing_librespot: false,
        last_advance: std::time::Instant::now() - Duration::from_secs(10),
    };

    info!("Stupefinity native client initialized successfully.");
    core.run(req_rx, finished_rx).await;

    NativeUi::shutdown();
    Ok(())
}

fn spawn_librespot_event_forwarder(
    session: Arc<RwLock<SessionManager>>,
    req_tx: mpsc::Sender<ClientRequest>,
) {
    tokio::spawn(async move {
        let player = session.read().await.player.clone();
        if let Some(p) = player {
            let mut channel = p.get_player_event_channel();
            while let Some(event) = channel.recv().await {
                if req_tx
                    .send(ClientRequest::LibrespotEvent(event))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
    });
}

fn spawn_smart_shuffle_loader(
    session: Arc<RwLock<SessionManager>>,
    api: Arc<SpotifyApiClient>,
    smart_shuffle: Arc<RwLock<SmartShuffleEngine>>,
) {
    tokio::spawn(async move {
        let token = session.read().await.access_token.clone();
        if token.is_empty() {
            return;
        }
        let mut pool = Vec::new();
        while (pool.len() as u32) < SMART_SHUFFLE_POOL_MAX {
            match api.get_liked_songs(&token, pool.len() as u32, 50).await {
                Ok(page) if !page.is_empty() => {
                    let last_page = page.len() < 50;
                    pool.extend(page);
                    if last_page {
                        break;
                    }
                }
                Ok(_) => break,
                Err(e) => {
                    warn!("Loading Liked Songs for Smart Shuffle stopped early: {e}");
                    break;
                }
            }
        }
        if !pool.is_empty() {
            smart_shuffle.write().await.update_candidate_pool(pool);
        }
    });
}

/// Keeps the Spotify access token fresh for long-running sessions (tokens last one hour).
fn spawn_token_refresher(session: Arc<RwLock<SessionManager>>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            interval.tick().await;
            let due = session.read().await.refresh_due();
            if let Some(refresh_token) = due {
                match SessionManager::refresh(&refresh_token).await {
                    Ok(tokens) => session.write().await.update_tokens(tokens),
                    Err(e) => warn!("{e}"),
                }
            }
        }
    });
}

/// Moves data saved under the app's former name (Stupefy) to the current folder, once.
fn migrate_data_dir() {
    let Some(base) = dirs::data_local_dir() else {
        return;
    };
    let (old, new) = (base.join("Stupefy"), base.join("Stupefinity"));
    if old.is_dir() && !new.exists() {
        let _ = std::fs::rename(old, new);
    }
}

/// `%LOCALAPPDATA%\Stupefinity\cache`, created if missing.
pub fn cache_dir() -> PathBuf {
    let dir = dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Stupefinity")
        .join("cache");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Deletes files left behind by earlier versions (per-play streams and lookup files).
fn remove_stale_streams() {
    let Ok(entries) = std::fs::read_dir(cache_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let stream = name.starts_with("stream_") && name.ends_with(".tmp");
        let info = name.starts_with("info_") && name.ends_with(".json");
        if stream || info {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Without a fallback source only Spotify Premium can play, so a Free account is logged out
/// again; returns the reason to show.
fn premium_required(session: &mut SessionManager, has_fallback: bool) -> Option<String> {
    if has_fallback || !session.is_authenticated || session.is_premium {
        return None;
    }
    session.logout();
    Some("Stupefinity needs a Spotify Premium account to play music.".to_string())
}

/// Owns all playback state. Only the main loop touches it, so slow work (network,
/// downloads) is spawned and reports back through `ClientRequest` messages.
struct Core {
    state: Arc<RwLock<Snapshot>>,
    session: Arc<RwLock<SessionManager>>,
    api: Arc<SpotifyApiClient>,
    fallback: Option<Arc<dyn FallbackSource>>,
    lyrics: Arc<LyricsEngine>,
    rules: RulesEngine,
    settings: settings::SettingsManager,
    smart_shuffle: Arc<RwLock<SmartShuffleEngine>>,
    eq: Arc<std::sync::Mutex<EqualizerEngine>>,
    player: AudioPlayer,
    queue: QueueManager,
    /// Metadata for the tracks of the loaded context, so playing them needs no API lookup.
    context_tracks: HashMap<String, TrackInfo>,
    /// Metadata for user-queued tracks from outside the context (e.g. search results).
    queued_tracks: HashMap<String, TrackInfo>,
    /// Set when settings changed but are not saved yet; rapid changes (volume drags) are
    /// written once they settle.
    settings_dirty_since: Option<std::time::Instant>,
    /// Incremented for every track change; background tasks for an older track drop
    /// their results instead of playing over the current one.
    generation: Arc<AtomicU64>,
    /// Consecutive tracks skipped by rules or failures, to stop instead of looping forever.
    skip_streak: usize,
    req_tx: mpsc::Sender<ClientRequest>,
    is_playing_librespot: bool,
    last_advance: std::time::Instant,
}

impl Core {
    async fn run(
        &mut self,
        mut req_rx: mpsc::Receiver<ClientRequest>,
        mut finished_rx: mpsc::UnboundedReceiver<()>,
    ) {
        let mut ticker = tokio::time::interval(Duration::from_millis(500));
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if self
                        .settings_dirty_since
                        .is_some_and(|t| t.elapsed() >= SETTINGS_SAVE_DELAY)
                    {
                        self.settings_dirty_since = None;
                        self.settings.save();
                    }
                    let mut s = self.state.write().await;
                    if s.playback_state == PlaybackState::Playing {
                        if self.is_playing_librespot {
                            s.position_ms = (s.position_ms + 500).min(s.duration_ms);
                        } else {
                            s.position_ms = self.player.position_ms().min(s.duration_ms);
                        }
                    }
                }
                Some(()) = finished_rx.recv() => {
                    if self.last_advance.elapsed() < Duration::from_millis(300) {
                        info!("Ignoring finished signal within race window of prior advance.");
                    } else {
                        info!("Track finished; advancing.");
                        self.last_advance = std::time::Instant::now();
                        self.next_track().await;
                    }
                }
                Some(req) = req_rx.recv() => {
                    if self.handle_request(req).await.is_break() {
                        break;
                    }
                }
            }
        }
    }

    fn is_current(&self, generation: u64) -> bool {
        self.generation.load(Ordering::SeqCst) == generation
    }

    async fn access_token(&self) -> String {
        self.session.read().await.access_token.clone()
    }

    async fn handle_request(&mut self, req: ClientRequest) -> ControlFlow<()> {
        match req {
            ClientRequest::Command(cmd) => return self.handle_command(cmd).await,
            ClientRequest::Query { cmd, resp_tx } => self.spawn_query(cmd, resp_tx).await,
            ClientRequest::Sync(done) => {
                let _ = done.send(());
            }
            ClientRequest::ContextLoaded {
                uri,
                tracks,
                start_index,
            } => {
                info!("Context {} loaded with {} tracks.", uri, tracks.len());
                let uris = tracks.iter().map(|t| t.uri.clone()).collect();
                self.context_tracks = tracks.into_iter().map(|t| (t.uri.clone(), t)).collect();
                self.queue.load_context(uri, uris, start_index);
                self.update_queue_window().await;
                if let Some(current) = self.queue.current_track() {
                    self.play_uri(current).await;
                }
            }
            ClientRequest::TrackReady { generation, track } => {
                if self.is_current(generation) {
                    self.start_track(generation, track).await;
                }
            }
            ClientRequest::PlaybackFailed { generation, reason } => {
                if self.is_current(generation) {
                    error!("Playback failed: {reason}");
                    self.skip_or_stop().await;
                }
            }
            ClientRequest::QueuedTrackInfo(track) => {
                self.queued_tracks.insert(track.uri.clone(), track);
                self.update_queue_window().await;
            }
            ClientRequest::LibrespotEvent(event) => {
                self.handle_librespot_event(event).await;
            }
        }
        ControlFlow::Continue(())
    }

    async fn handle_librespot_event(&mut self, event: librespot::playback::player::PlayerEvent) {
        use librespot::playback::player::PlayerEvent;
        match event {
            PlayerEvent::EndOfTrack { .. } => {
                info!("Librespot track finished; advancing.");
                self.next_track().await;
            }
            PlayerEvent::Playing { position_ms, .. } => {
                let mut s = self.state.write().await;
                s.playback_state = PlaybackState::Playing;
                s.position_ms = position_ms;
            }
            PlayerEvent::Paused { position_ms, .. } => {
                let mut s = self.state.write().await;
                s.playback_state = PlaybackState::Paused;
                s.position_ms = position_ms;
            }
            PlayerEvent::PositionCorrection { position_ms, .. } => {
                let mut s = self.state.write().await;
                s.position_ms = position_ms;
            }
            PlayerEvent::Unavailable { track_id, .. } => {
                warn!(
                    "Librespot track {:?} unavailable on CDN; trying the fallback",
                    track_id
                );
                self.is_playing_librespot = false;
                let track = self.state.read().await.current_track.clone();
                if let Some(t) = track {
                    let gen = self.generation.load(Ordering::SeqCst);
                    self.play_fallback(gen, t).await;
                } else {
                    self.skip_or_stop().await;
                }
            }
            _ => {}
        }
    }

    async fn update_queue_window(&self) {
        let upcoming = self.queue.upcoming_uris(10);
        let mut window = Vec::with_capacity(upcoming.len());

        for (uri, is_user_queued) in upcoming {
            if let Some(t) = self.track_info(&uri) {
                window.push(QueueItem {
                    uri: uri.clone(),
                    title: t.title.clone(),
                    artist: t.artists.join(", "),
                    duration_ms: t.duration_ms,
                    is_user_queued,
                });
            } else {
                window.push(QueueItem {
                    uri: uri.clone(),
                    title: uri.clone(),
                    artist: String::new(),
                    duration_ms: 0,
                    is_user_queued,
                });
            }
        }

        self.state.write().await.queue_window = window;
    }

    async fn handle_command(&mut self, cmd: Command) -> ControlFlow<()> {
        info!("Executing command: {:?}", cmd);
        match cmd {
            Command::Login => self.login().await,
            Command::AcceptTerms => {
                self.settings.settings.accepted_terms_version = TERMS_VERSION;
                self.settings.save();
                self.state.write().await.terms_accepted = true;
            }
            Command::SetClientId(id) => {
                let mut s = self.state.write().await;
                if s.auth_state != AuthState::Unauthenticated {
                    warn!("Log out before changing the Spotify client ID.");
                } else if id.trim().is_empty() || session::valid_client_id(id.trim()) {
                    let id = Some(id.trim().to_ascii_lowercase()).filter(|i| !i.is_empty());
                    session::set_client_id(id.clone());
                    s.needs_client_id = id.is_none();
                    s.login_error = None;
                    drop(s);
                    self.settings.settings.spotify_client_id = id;
                    self.settings.save();
                } else {
                    warn!("Ignoring an invalid Spotify client ID.");
                }
            }
            Command::Logout => {
                if self.is_playing_librespot {
                    if let Some(p) = self.session.read().await.player.as_ref() {
                        p.stop();
                    }
                    self.is_playing_librespot = false;
                }
                // Spotify's terms: disconnecting deletes the user's Spotify data.
                self.session.write().await.logout();
                self.api.clear_cache().await;
                self.smart_shuffle
                    .write()
                    .await
                    .update_candidate_pool(Vec::new());
                if let Some(f) = &self.fallback {
                    f.forget().await;
                }
                self.generation.fetch_add(1, Ordering::SeqCst);
                self.player.stop();
                self.queue = QueueManager::new();
                self.context_tracks.clear();
                self.queued_tracks.clear();
                self.update_queue_window().await;
                let mut s = self.state.write().await;
                s.auth_state = AuthState::Unauthenticated;
                s.playback_state = PlaybackState::Stopped;
                s.current_track = None;
                s.position_ms = 0;
                s.duration_ms = 0;
            }
            Command::Play => self.play().await,
            Command::Pause => self.pause().await,
            Command::PlayPauseToggle => {
                let playback = self.state.read().await.playback_state;
                match playback {
                    PlaybackState::Playing | PlaybackState::Buffering => self.pause().await,
                    _ => self.play().await,
                }
            }
            Command::Seek(ms) => {
                let mut s = self.state.write().await;
                s.position_ms = ms.min(s.duration_ms);
                if self.is_playing_librespot {
                    if let Some(p) = self.session.read().await.player.as_ref() {
                        p.seek(s.position_ms);
                    }
                } else {
                    self.player.seek(u64::from(s.position_ms));
                }
            }
            Command::LoadContext { uri, start_index } => self.load_context(uri, start_index).await,
            Command::Next => {
                self.last_advance = std::time::Instant::now();
                self.next_track().await;
            }
            Command::Prev => {
                self.last_advance = std::time::Instant::now();
                let pos_secs = self.state.read().await.position_ms / 1000;
                if let Some(uri) = self.queue.prev(pos_secs) {
                    self.play_uri(uri).await;
                }
            }
            Command::Enqueue(uri) => {
                info!("Enqueued track: {}", uri);
                if self.track_info(&uri).is_none() {
                    let api = self.api.clone();
                    let token = self.access_token().await;
                    let req_tx = self.req_tx.clone();
                    let uri = uri.clone();
                    tokio::spawn(async move {
                        match api.get_track(&token, &uri).await {
                            Ok(track) => {
                                let _ = req_tx.send(ClientRequest::QueuedTrackInfo(track)).await;
                            }
                            Err(e) => warn!("Could not fetch details of queued track {uri}: {e}"),
                        }
                    });
                }
                self.queue.enqueue(uri);
                self.update_queue_window().await;
            }
            Command::RemoveFromQueue(idx) => {
                if let Some(removed) = self.queue.remove_from_user_queue(idx) {
                    info!("Removed track from user queue: {}", removed);
                    self.update_queue_window().await;
                }
            }
            Command::SetVolume(vol) => {
                let linear = f32::from(vol) / f32::from(u16::MAX);
                self.player.set_volume(linear);
                if let Some(m) = self.session.read().await.mixer.as_ref() {
                    use librespot::playback::mixer::Mixer;
                    m.set_volume(vol);
                }
                self.state.write().await.volume = vol;
                self.settings.settings.volume = vol;
                self.settings_dirty_since = Some(std::time::Instant::now());
            }
            Command::SetShuffle(shuffle) => {
                self.queue.set_shuffle(shuffle);
                self.state.write().await.shuffle = shuffle;
                self.settings.settings.shuffle = shuffle;
                self.settings.save();
                self.update_queue_window().await;
            }
            Command::SetRepeat(repeat) => {
                self.queue.repeat = repeat;
                self.state.write().await.repeat = repeat;
                self.settings.settings.repeat = repeat;
                self.settings.save();
                self.update_queue_window().await;
            }
            Command::SetRules(rules) => {
                self.rules.config = rules;
                self.rules.save();
                info!("Updated local rules configuration.");
            }
            Command::SetSmartShuffle(cfg) => {
                self.state.write().await.smart_shuffle_enabled = cfg.enabled;
                info!("Smart Shuffle configuration updated: {:?}", cfg);
                self.smart_shuffle.write().await.config = cfg.clone();
                self.settings.settings.smart_shuffle = cfg;
                self.settings.save();
            }
            Command::SetEq(profile) => {
                if let Ok(mut engine) = self.eq.lock() {
                    engine.set_profile(profile.clone());
                }
                self.settings.settings.eq_profile = profile;
                self.settings.save();
            }
            Command::ToggleEq(enable) => {
                if let Ok(mut engine) = self.eq.lock() {
                    engine.set_enabled(enable);
                }
                self.state.write().await.eq_enabled = enable;
                self.settings.settings.eq_enabled = enable;
                self.settings.save();
            }
            Command::LikeTrack { uri } => self.set_liked(uri, true).await,
            Command::UnlikeTrack { uri } => self.set_liked(uri, false).await,
            Command::WindowShown => {
                PlatformManager::set_eco_qos(false);
                self.state.write().await.is_window_visible = true;
            }
            Command::WindowHidden => {
                PlatformManager::set_eco_qos(true);
                PlatformManager::trim_working_set();
                self.state.write().await.is_window_visible = false;
            }
            Command::Shutdown => {
                info!("Shutdown signal received.");
                if self.is_playing_librespot {
                    if let Some(p) = self.session.read().await.player.as_ref() {
                        p.stop();
                    }
                }
                self.player.stop();
                if self.settings_dirty_since.take().is_some() {
                    self.settings.save();
                }
                return ControlFlow::Break(());
            }
            Command::GetPlaylists
            | Command::GetPlaylistTracks { .. }
            | Command::GetLikedSongs { .. }
            | Command::Search { .. }
            | Command::GetLyrics
            | Command::GetSnapshot => warn!("Query command sent without a reply channel; ignored."),
        }
        ControlFlow::Continue(())
    }

    async fn login(&mut self) {
        {
            let mut s = self.state.write().await;
            if s.auth_state != AuthState::Unauthenticated || !s.terms_accepted || s.needs_client_id
            {
                return;
            }
            s.auth_state = AuthState::Authenticating;
            s.login_error = None;
        }
        let session = self.session.clone();
        let state = self.state.clone();
        let api = self.api.clone();
        let smart_shuffle = self.smart_shuffle.clone();
        let has_fallback = self.fallback.is_some();
        let req_tx = self.req_tx.clone();

        tokio::spawn(async move {
            let cache_dir = session.read().await.cache_dir();
            let auth = match tokio::time::timeout(
                LOGIN_TIMEOUT,
                SessionManager::interactive_login(cache_dir),
            )
            .await
            {
                Ok(Ok(account)) => {
                    let mut mgr = session.write().await;
                    mgr.set_account(account);
                    if let Some(reason) = premium_required(&mut mgr, has_fallback) {
                        drop(mgr);
                        state.write().await.login_error = Some(reason);
                        AuthState::Unauthenticated
                    } else {
                        let auth = mgr.auth_state();
                        drop(mgr);
                        spawn_smart_shuffle_loader(session.clone(), api, smart_shuffle);
                        spawn_librespot_event_forwarder(session.clone(), req_tx);
                        auth
                    }
                }
                Ok(Err(e)) => {
                    error!("Login failed: {e:#}");
                    state.write().await.login_error = Some(format!("Login failed: {e:#}"));
                    AuthState::Unauthenticated
                }
                Err(_) => {
                    error!("Login timed out after {}s.", LOGIN_TIMEOUT.as_secs());
                    state.write().await.login_error =
                        Some("Login timed out. Press Log In to try again.".to_string());
                    AuthState::Unauthenticated
                }
            };
            state.write().await.auth_state = auth;
        });
    }

    async fn play(&mut self) {
        let playback = self.state.read().await.playback_state;
        match playback {
            PlaybackState::Paused => {
                if self.is_playing_librespot {
                    if let Some(p) = self.session.read().await.player.as_ref() {
                        p.play();
                    }
                } else {
                    self.player.resume();
                }
                self.state.write().await.playback_state = PlaybackState::Playing;
            }
            PlaybackState::Stopped => {
                if let Some(uri) = self.queue.current_track() {
                    self.play_uri(uri).await;
                }
            }
            PlaybackState::Playing | PlaybackState::Buffering => {}
        }
    }

    async fn pause(&mut self) {
        let mut s = self.state.write().await;
        if matches!(
            s.playback_state,
            PlaybackState::Playing | PlaybackState::Buffering
        ) {
            if self.is_playing_librespot {
                if let Some(p) = self.session.read().await.player.as_ref() {
                    p.pause();
                }
            } else {
                self.player.pause();
            }
            s.playback_state = PlaybackState::Paused;
        }
    }

    async fn load_context(&mut self, uri: String, start_index: usize) {
        info!("Loading context: {} starting at {}", uri, start_index);
        if let Some(playlist_id) = uri.strip_prefix("spotify:playlist:") {
            let api = self.api.clone();
            let token = self.access_token().await;
            let req_tx = self.req_tx.clone();
            let playlist_id = playlist_id.to_string();
            tokio::spawn(async move {
                match api.get_playlist_tracks(&token, &playlist_id).await {
                    Ok(tracks) => {
                        let _ = req_tx
                            .send(ClientRequest::ContextLoaded {
                                uri,
                                tracks,
                                start_index,
                            })
                            .await;
                    }
                    Err(e) => error!("Failed to load playlist {playlist_id}: {e}"),
                }
            });
        } else if let Some(album_id) = uri.strip_prefix("spotify:album:") {
            let api = self.api.clone();
            let token = self.access_token().await;
            let req_tx = self.req_tx.clone();
            let album_id = album_id.to_string();
            tokio::spawn(async move {
                match api.get_album_tracks(&token, &album_id).await {
                    Ok(tracks) => {
                        let _ = req_tx
                            .send(ClientRequest::ContextLoaded {
                                uri,
                                tracks,
                                start_index,
                            })
                            .await;
                    }
                    Err(e) => error!("Failed to load album {album_id}: {e}"),
                }
            });
        } else if let Some(artist_id) = uri.strip_prefix("spotify:artist:") {
            let api = self.api.clone();
            let token = self.access_token().await;
            let req_tx = self.req_tx.clone();
            let artist_id = artist_id.to_string();
            tokio::spawn(async move {
                match api.get_artist_top_tracks(&token, &artist_id).await {
                    Ok(tracks) => {
                        let _ = req_tx
                            .send(ClientRequest::ContextLoaded {
                                uri,
                                tracks,
                                start_index,
                            })
                            .await;
                    }
                    Err(e) => error!("Failed to load artist top tracks for {artist_id}: {e}"),
                }
            });
        } else if uri.starts_with("spotify:track:") {
            self.context_tracks.clear();
            self.queue.load_context(uri.clone(), vec![uri.clone()], 0);
            self.update_queue_window().await;
            self.play_uri(uri).await;
        } else {
            warn!("Unsupported context URI: {uri}");
        }
    }

    async fn next_track(&mut self) {
        let suggestion = self
            .smart_shuffle
            .write()
            .await
            .maybe_suggest_track(&self.queue.tracks);
        if let Some(track) = suggestion {
            info!(
                "Smart Shuffle suggested track: {} ({})",
                track.title, track.uri
            );
            let generation = self.begin_track().await;
            self.start_track(generation, track).await;
            return;
        }

        match self.queue.advance() {
            Some(uri) => {
                self.update_queue_window().await;
                self.play_uri(uri).await;
            }
            None => {
                info!("End of context reached.");
                self.update_queue_window().await;
                self.stop_playback().await;
            }
        }
    }

    async fn stop_playback(&mut self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        if self.is_playing_librespot {
            if let Some(p) = self.session.read().await.player.as_ref() {
                p.stop();
            }
            self.is_playing_librespot = false;
        }
        self.player.stop();
        let mut s = self.state.write().await;
        s.playback_state = PlaybackState::Stopped;
        s.position_ms = 0;
    }

    /// Called when a track is skipped by rules or fails to play.
    async fn skip_or_stop(&mut self) {
        self.skip_streak += 1;
        let playable = self.queue.tracks.len() + self.queue.user_queue.len();
        if self.skip_streak > playable.max(1) {
            warn!("Every track in the queue was skipped or failed; stopping playback.");
            self.skip_streak = 0;
            self.stop_playback().await;
        } else {
            self.next_track().await;
        }
    }

    /// Invalidates any in-flight track work and silences the old track.
    async fn begin_track(&mut self) -> u64 {
        self.last_advance = std::time::Instant::now();
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        if self.is_playing_librespot {
            if let Some(p) = self.session.read().await.player.as_ref() {
                p.stop();
            }
            self.is_playing_librespot = false;
        }
        self.player.stop();
        let mut s = self.state.write().await;
        s.playback_state = PlaybackState::Buffering;
        s.position_ms = 0;
        generation
    }

    /// Plays a track by URI, fetching its metadata first if it is not in the loaded context.
    async fn play_uri(&mut self, uri: String) {
        let generation = self.begin_track().await;
        if let Some(track) = self.track_info(&uri).cloned() {
            self.start_track(generation, track).await;
            return;
        }

        let api = self.api.clone();
        let token = self.access_token().await;
        let req_tx = self.req_tx.clone();
        tokio::spawn(async move {
            let msg = match api.get_track(&token, &uri).await {
                Ok(track) => ClientRequest::TrackReady { generation, track },
                Err(e) => ClientRequest::PlaybackFailed {
                    generation,
                    reason: format!("metadata lookup for {uri} failed: {e}"),
                },
            };
            let _ = req_tx.send(msg).await;
        });
    }

    /// Applies rules, publishes the track and starts resolving its audio in the background.
    async fn start_track(&mut self, generation: u64, track: TrackInfo) {
        if self.rules.should_skip(&track) {
            info!("Local rules engine auto-skipped track: {}", track.title);
            return Box::pin(self.skip_or_stop()).await;
        }
        self.skip_streak = 0;

        {
            let mut s = self.state.write().await;
            s.current_track = Some(track.clone());
            s.duration_ms = track.duration_ms;
            s.position_ms = 0;
            s.playback_state = PlaybackState::Buffering;
        }
        self.smart_shuffle.write().await.record_play(&track.uri);

        let artist = track.artists.first().cloned().unwrap_or_default();
        let duration_secs = track.duration_ms / 1000;

        // Warm the lyrics cache without delaying audio.
        let lyrics = self.lyrics.clone();
        let t = track.clone();
        let a = artist.clone();
        tokio::spawn(async move {
            let _ = lyrics
                .get_lyrics(&t.uri, &t.title, &a, &t.album, duration_secs)
                .await;
        });

        // Check if track is liked in library
        let api = self.api.clone();
        let token = self.access_token().await;
        let track_uri = track.uri.clone();
        let state = self.state.clone();
        tokio::spawn(async move {
            if let Ok(is_liked) = api.is_track_liked(&token, &track_uri).await {
                let mut s = state.write().await;
                if let Some(cur) = s.current_track.as_mut().filter(|t| t.uri == track_uri) {
                    cur.is_liked = is_liked;
                }
            }
        });

        let is_premium = self.session.read().await.is_premium;
        let librespot_player = self.session.read().await.player.clone();

        // 1. If Premium account has connected librespot player, stream from Spotify CDN
        if is_premium && librespot_player.is_some() {
            if let Ok(spotify_uri) = librespot::core::SpotifyUri::from_uri(&track.uri) {
                info!(
                    "Playing '{}' via Spotify CDN (Premium account)",
                    track.title
                );
                self.is_playing_librespot = true;
                let skip_intro_ms = if self.rules.config.enabled {
                    self.rules.config.skip_intro_secs * 1000
                } else {
                    0
                };
                if let Some(p) = librespot_player {
                    p.load(spotify_uri, true, skip_intro_ms);
                }
                return;
            }
        }

        // 2. Free account, or Premium without a librespot player
        self.is_playing_librespot = false;
        self.play_fallback(generation, track).await;
    }

    /// Plays a track Spotify will not stream to this account (Free accounts, or a Premium
    /// track librespot reports unavailable) from the fallback source, if there is one.
    async fn play_fallback(&self, generation: u64, track: TrackInfo) {
        let Some(source) = &self.fallback else {
            let reason = format!("'{}' can only play with Spotify Premium", track.title);
            let _ = self
                .req_tx
                .send(ClientRequest::PlaybackFailed { generation, reason })
                .await;
            return;
        };
        let skip_intro_ms = if self.rules.config.enabled {
            u64::from(self.rules.config.skip_intro_secs) * 1000
        } else {
            0
        };
        source.play(FallbackPlay {
            next: self.upcoming_track(),
            track,
            skip_intro_ms,
            player: self.player.clone(),
            state: self.state.clone(),
            generation,
            current: self.generation.clone(),
            req_tx: self.req_tx.clone(),
        });
    }

    /// The track that plays after the current one, when its metadata is known.
    fn upcoming_track(&self) -> Option<TrackInfo> {
        let (uri, _) = self.queue.upcoming_uris(1).into_iter().next()?;
        self.track_info(&uri).cloned()
    }

    /// Known metadata for a queued or context track.
    fn track_info(&self, uri: &str) -> Option<&TrackInfo> {
        self.context_tracks
            .get(uri)
            .or_else(|| self.queued_tracks.get(uri))
    }

    async fn set_liked(&self, uri: String, liked: bool) {
        let token = self.access_token().await;
        let api = self.api.clone();
        let state = self.state.clone();
        tokio::spawn(async move {
            let result = if liked {
                api.like_track(&token, &uri).await
            } else {
                api.unlike_track(&token, &uri).await
            };
            match result {
                Ok(()) => {
                    if let Some(t) = state
                        .write()
                        .await
                        .current_track
                        .as_mut()
                        .filter(|t| t.uri == uri)
                    {
                        t.is_liked = liked;
                    }
                }
                Err(e) => error!("Failed to update like state of {}: {e}", uri),
            }
        });
    }

    async fn spawn_query(&self, cmd: Command, resp_tx: oneshot::Sender<CoreEvent>) {
        let token = self.access_token().await;
        let api = self.api.clone();
        let lyrics = self.lyrics.clone();
        let smart_shuffle = self.smart_shuffle.clone();
        let current_track = self.state.read().await.current_track.clone();

        tokio::spawn(async move {
            let to_event = |e: anyhow::Error| CoreEvent::Error(e.to_string());
            let resp = match cmd {
                Command::GetPlaylists => api
                    .get_playlists(&token)
                    .await
                    .map_or_else(to_event, CoreEvent::Playlists),
                Command::GetPlaylistTracks { playlist_id } => api
                    .get_playlist_tracks(&token, &playlist_id)
                    .await
                    .map_or_else(to_event, |tracks| CoreEvent::PlaylistTracks {
                        playlist_id,
                        tracks,
                    }),
                Command::GetLikedSongs { offset, limit } => {
                    match api.get_liked_songs(&token, offset, limit).await {
                        Ok(tracks) => {
                            smart_shuffle
                                .write()
                                .await
                                .update_candidate_pool(tracks.clone());
                            CoreEvent::LikedSongs(tracks)
                        }
                        Err(e) => to_event(e),
                    }
                }
                Command::Search { query } => {
                    api.search(&token, &query)
                        .await
                        .map_or_else(to_event, |tracks| CoreEvent::SearchResults {
                            query,
                            tracks,
                        })
                }
                Command::GetLyrics => match current_track {
                    Some(t) => {
                        let artist = t.artists.first().cloned().unwrap_or_default();
                        lyrics
                            .get_lyrics(&t.uri, &t.title, &artist, &t.album, t.duration_ms / 1000)
                            .await
                            .map_or_else(to_event, CoreEvent::Lyrics)
                    }
                    None => CoreEvent::Error("No track currently playing".to_string()),
                },
                _ => CoreEvent::Error("Unsupported query".to_string()),
            };
            let _ = resp_tx.send(resp);
        });
    }
}
