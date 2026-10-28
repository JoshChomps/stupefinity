use std::ptr::null_mut;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use stupefinity_protocol::{
    AuthState, Command, CoreEvent, EqProfile, FilterBand, LyricsData, PlaybackState,
    PlaylistSummary, RepeatMode, Snapshot, TrackInfo,
};
use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot, RwLock};
use tracing::error;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW, CreateSolidBrush,
    DeleteDC, DeleteObject, DrawTextW, EndPaint, FillRect, InvalidateRect, ScreenToClient,
    SelectObject, SetBkMode, SetTextColor, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_PITCH,
    DEFAULT_QUALITY, DT_CENTER, DT_LEFT, DT_RIGHT, DT_SINGLELINE, DT_VCENTER, DT_WORDBREAK,
    FF_DONTCARE, FW_BOLD, FW_NORMAL, OUT_DEFAULT_PRECIS, PAINTSTRUCT, SRCCOPY, TRANSPARENT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, RegisterHotKey, UnregisterHotKey, VK_BACK, VK_CONTROL, VK_DOWN, VK_ESCAPE,
    VK_LEFT, VK_MEDIA_NEXT_TRACK, VK_MEDIA_PLAY_PAUSE, VK_MEDIA_PREV_TRACK, VK_MEDIA_STOP,
    VK_RETURN, VK_RIGHT, VK_SPACE, VK_UP,
};
use windows_sys::Win32::UI::Shell::{
    ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE,
    NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, FindWindowW, GetClientRect, GetCursorPos, GetMessageW, KillTimer,
    LoadCursorW, LoadIconW, PostQuitMessage, RegisterClassW, SendMessageW, SetForegroundWindow,
    SetTimer, ShowWindow, TrackPopupMenuEx, TranslateMessage, CS_HREDRAW, CS_VREDRAW, HICON,
    IDC_ARROW, IDI_APPLICATION, MF_SEPARATOR, MF_STRING, MSG, SW_HIDE, SW_SHOW, SW_SHOWNORMAL,
    TPM_BOTTOMALIGN, TPM_LEFTALIGN, WM_APP, WM_CHAR, WM_CLOSE, WM_COMMAND, WM_DESTROY, WM_HOTKEY,
    WM_KEYDOWN, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_MOUSEWHEEL, WM_PAINT, WM_RBUTTONUP,
    WM_SHOWWINDOW, WM_TIMER, WM_USER, WNDCLASSW, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

use crate::ipc::ClientRequest;

const CLASS_NAME: &str = "StupefinityMainWindowClass";

const WM_TRAYICON: u32 = WM_USER + 101;
/// Sent by the core on shutdown so the window (and tray icon) is torn down on its own thread.
const WM_APP_QUIT: u32 = WM_APP + 1;
const TRAY_ICON_ID: u32 = 1;
const ID_TRAY_OPEN: usize = 2001;
const ID_TRAY_PLAY_PAUSE: usize = 2002;
const ID_TRAY_NEXT: usize = 2003;
const ID_TRAY_PREV: usize = 2004;
const ID_TRAY_EXIT: usize = 2005;

const HOTKEY_PLAY_PAUSE: i32 = 1;
const HOTKEY_NEXT: i32 = 2;
const HOTKEY_PREV: i32 = 3;
const HOTKEY_STOP: i32 = 4;

/// 5% of the u16 volume range.
const VOLUME_STEP: u16 = 3276;

// Layout shared by the renderers and the hit-testing in `handle_click`.
const TAB_PLAYER: usize = 0;
const TAB_SEARCH: usize = 1;
const TAB_LIBRARY: usize = 2;
const TAB_LYRICS: usize = 3;
const TAB_EQ: usize = 4;
const TAB_LABELS: [&str; 5] = [
    "1. Player",
    "2. Search",
    "3. Library",
    "4. Lyrics",
    "5. Equalizer",
];
const TAB_X: [i32; 5] = [190, 280, 370, 460, 550];
const TAB_W: [i32; 5] = [80, 80, 80, 80, 95];
const BOTTOM_BAR_H: i32 = 95;
/// Rows shown in the search and library lists.
const VISIBLE_ROWS: usize = 12;
const SEARCH_LIST_TOP: i32 = 150;
const SEARCH_ROW_H: i32 = 32;
const LIBRARY_LIST_TOP: i32 = 115;
const PLAYLIST_ROW_H: i32 = 32;
const PLAYLIST_TRACK_ROW_H: i32 = 30;
const PLAYLIST_TRACKS_LEFT: i32 = 330;
const EQ_PRESETS: [&str; 6] = [
    "Flat",
    "Harman 2019",
    "Bass Boost",
    "Treble Boost",
    "Vocal",
    "HD650",
];

fn tab_rect(i: usize) -> RECT {
    RECT {
        left: TAB_X[i],
        top: 15,
        right: TAB_X[i] + TAB_W[i],
        bottom: 55,
    }
}

fn auth_rect(w: i32) -> RECT {
    RECT {
        left: w - 160,
        top: 15,
        right: w - 20,
        bottom: 45,
    }
}

/// Which first-run step the logged-out screen shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Onboarding {
    Terms,
    ClientId,
    Login,
}

fn onboarding(snapshot: &Snapshot) -> Onboarding {
    if !snapshot.terms_accepted {
        Onboarding::Terms
    } else if snapshot.needs_client_id {
        Onboarding::ClientId
    } else {
        Onboarding::Login
    }
}

const TERMS_DOC: &str = include_str!("../../../docs/legal/TERMS.md");
const PRIVACY_DOC: &str = include_str!("../../../docs/legal/PRIVACY.md");
const DASHBOARD_URL: &str = "https://developer.spotify.com/dashboard";
/// Top of the Terms / Privacy buttons on the first-run screen.
const TERMS_LINKS_TOP: i32 = 350;
/// Top of the Save / Open Dashboard buttons on the client ID screen.
const SETUP_BUTTONS_TOP: i32 = 328;

fn setup_input_rect(w: i32) -> RECT {
    let cx = w / 2;
    RECT {
        left: cx - 220,
        top: 280,
        right: cx + 220,
        bottom: 316,
    }
}

/// Left and right halves of a 440 px wide button row starting at `top`.
fn pair_button_rects(w: i32, top: i32) -> (RECT, RECT) {
    let cx = w / 2;
    (
        RECT {
            left: cx - 220,
            top,
            right: cx - 6,
            bottom: top + 36,
        },
        RECT {
            left: cx + 6,
            top,
            right: cx + 220,
            bottom: top + 36,
        },
    )
}

fn change_client_id_rect(w: i32) -> RECT {
    let cx = w / 2;
    RECT {
        left: cx - 110,
        top: 425,
        right: cx + 110,
        bottom: 450,
    }
}

/// Something to open outside the app, decided under the UI lock and run after it.
enum External {
    Url(&'static str),
    Document(&'static str, &'static str),
}

/// Opens a web page, or writes a bundled document to the data folder and opens it, in the
/// default app. Must run without the UI lock: the shell can pump window messages.
fn open_external(target: External) {
    let path = match target {
        External::Url(url) => url.to_string(),
        External::Document(file_name, contents) => {
            let path = dirs::data_local_dir()
                .unwrap_or_else(|| std::path::PathBuf::from("."))
                .join("Stupefinity")
                .join(file_name);
            if let Err(e) = std::fs::write(&path, contents) {
                error!("Could not write {file_name}: {e}");
                return;
            }
            path.to_string_lossy().into_owned()
        }
    };
    let op = NativeUi::to_wide("open");
    let file = NativeUi::to_wide(&path);
    unsafe {
        ShellExecuteW(
            null_mut(),
            op.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        );
    }
}

/// The clipboard's text, if it holds any.
unsafe fn clipboard_text(hwnd: HWND) -> Option<String> {
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, OpenClipboard,
    };
    use windows_sys::Win32::System::Memory::{GlobalLock, GlobalUnlock};
    use windows_sys::Win32::System::Ole::CF_UNICODETEXT;
    if OpenClipboard(hwnd) == 0 {
        return None;
    }
    let mut text = None;
    let handle = GetClipboardData(u32::from(CF_UNICODETEXT));
    if !handle.is_null() {
        let ptr = GlobalLock(handle).cast::<u16>();
        if !ptr.is_null() {
            let len = (0..).take_while(|&i| *ptr.add(i) != 0).count();
            text = Some(String::from_utf16_lossy(std::slice::from_raw_parts(
                ptr, len,
            )));
            GlobalUnlock(handle);
        }
    }
    CloseClipboard();
    text
}

fn hero_login_rect(w: i32) -> RECT {
    let cx = w / 2;
    RECT {
        left: cx - 160,
        top: 275,
        right: cx + 160,
        bottom: 335,
    }
}

fn like_rect() -> RECT {
    RECT {
        left: 40,
        top: 365,
        right: 130,
        bottom: 395,
    }
}

fn search_button_rect(w: i32) -> RECT {
    RECT {
        left: w - 130,
        top: 75,
        right: w - 40,
        bottom: 115,
    }
}

fn eq_toggle_rect() -> RECT {
    RECT {
        left: 50,
        top: 80,
        right: 220,
        bottom: 120,
    }
}

fn eq_preset_rect(i: usize) -> RECT {
    let left = 50 + (i as i32) * 110;
    RECT {
        left,
        top: 140,
        right: left + 100,
        bottom: 175,
    }
}

fn hit(r: &RECT, x: i32, y: i32) -> bool {
    (r.left..r.right).contains(&x) && (r.top..r.bottom).contains(&y)
}

/// Index of the list row under `y`, if it is one of the visible, populated rows.
/// Whether a `WM_MOUSEWHEEL` (screen coordinates) happened over the Library tab's
/// playlist column rather than its track list.
fn wheel_over_playlist_list(hwnd: HWND, lparam: LPARAM) -> bool {
    let mut pt = POINT {
        x: i32::from((lparam & 0xFFFF) as u16 as i16),
        y: i32::from(((lparam >> 16) & 0xFFFF) as u16 as i16),
    };
    unsafe { ScreenToClient(hwnd, &mut pt) };
    pt.x < PLAYLIST_TRACKS_LEFT
}

fn row_at(y: i32, top: i32, row_h: i32, len: usize) -> Option<usize> {
    if y < top {
        return None;
    }
    let idx = ((y - top) / row_h) as usize;
    (idx < len.min(VISIBLE_ROWS)).then_some(idx)
}

fn next_repeat(mode: RepeatMode) -> RepeatMode {
    match mode {
        RepeatMode::Off => RepeatMode::Context,
        RepeatMode::Context => RepeatMode::Track,
        RepeatMode::Track => RepeatMode::Off,
    }
}

fn eq_preset_profile(name: &str) -> EqProfile {
    let band = |filter_type: &str, frequency, q, gain_db| FilterBand {
        filter_type: filter_type.to_string(),
        frequency,
        q,
        gain_db,
    };
    let (preamp_db, bands) = match name {
        "Harman 2019" => (
            -5.5,
            vec![
                band("LowShelf", 105.0, 0.71, 5.5),
                band("Peaking", 200.0, 1.0, -2.0),
                band("Peaking", 1250.0, 1.4, -1.5),
                band("Peaking", 3000.0, 1.4, 3.0),
                band("HighShelf", 10000.0, 0.71, -1.0),
            ],
        ),
        "Bass Boost" => (-4.0, vec![band("LowShelf", 105.0, 0.71, 6.0)]),
        "Treble Boost" => (-3.0, vec![band("HighShelf", 8000.0, 0.71, 4.5)]),
        "Vocal" => (
            -3.5,
            vec![
                band("HighPass", 80.0, 0.71, 0.0),
                band("Peaking", 1000.0, 1.0, 2.5),
                band("Peaking", 3000.0, 1.2, 3.5),
                band("HighShelf", 8000.0, 0.71, 1.5),
            ],
        ),
        "HD650" => (
            -6.0,
            vec![
                band("LowShelf", 80.0, 0.71, 6.0),
                band("Peaking", 200.0, 1.2, -1.5),
                band("Peaking", 1300.0, 2.0, -1.8),
                band("Peaking", 3000.0, 1.5, 2.0),
                band("Peaking", 5800.0, 3.0, -3.0),
            ],
        ),
        _ => (0.0, Vec::new()),
    };
    EqProfile {
        name: name.to_string(),
        preamp_db,
        bands,
        enabled: true,
    }
}

struct CachedFonts {
    font_title: windows_sys::Win32::Graphics::Gdi::HFONT,
    font_sub: windows_sys::Win32::Graphics::Gdi::HFONT,
    font_body: windows_sys::Win32::Graphics::Gdi::HFONT,
    font_small: windows_sys::Win32::Graphics::Gdi::HFONT,
}

unsafe impl Send for CachedFonts {}
unsafe impl Sync for CachedFonts {}

static CACHED_FONTS: OnceLock<CachedFonts> = OnceLock::new();
static SMTC: OnceLock<Mutex<Option<souvlaki::MediaControls>>> = OnceLock::new();

/// What the media controls were last told: track URI, state, position and when.
type SmtcSent = (Option<String>, PlaybackState, u32, Instant);
static SMTC_SENT: Mutex<Option<SmtcSent>> = Mutex::new(None);

/// Mirrors the player into the Windows media controls. Only changes are sent: a new track
/// (metadata, which also reloads the cover art), a new state, or a seek. Windows advances
/// the position on its own while playing.
fn update_smtc(snapshot: &Snapshot) {
    let uri = snapshot.current_track.as_ref().map(|t| t.uri.clone());
    let mut sent = SMTC_SENT.lock().unwrap_or_else(|e| e.into_inner());
    let track_changed = sent.as_ref().is_none_or(|s| s.0 != uri);
    let playback_changed = track_changed
        || sent.as_ref().is_some_and(|(_, state, pos, at)| {
            let expected = if *state == PlaybackState::Playing {
                *pos + at.elapsed().as_millis() as u32
            } else {
                *pos
            };
            *state != snapshot.playback_state || expected.abs_diff(snapshot.position_ms) > 2000
        });
    if !playback_changed {
        return;
    }
    *sent = Some((
        uri,
        snapshot.playback_state,
        snapshot.position_ms,
        Instant::now(),
    ));
    drop(sent);

    if let Some(smtc_lock) = SMTC.get() {
        if let Ok(mut guard) = smtc_lock.lock() {
            if let Some(controls) = guard.as_mut() {
                if let Some(track) = snapshot.current_track.as_ref().filter(|_| track_changed) {
                    let artist_str = track.artists.join(", ");
                    let meta = souvlaki::MediaMetadata {
                        title: Some(&track.title),
                        album: Some(&track.album),
                        artist: Some(&artist_str),
                        cover_url: track.cover_url.as_deref(),
                        duration: Some(Duration::from_millis(track.duration_ms as u64)),
                    };
                    let _ = controls.set_metadata(meta);
                }
                let pb = match snapshot.playback_state {
                    PlaybackState::Playing => souvlaki::MediaPlayback::Playing {
                        progress: Some(souvlaki::MediaPosition(Duration::from_millis(
                            snapshot.position_ms as u64,
                        ))),
                    },
                    PlaybackState::Paused => souvlaki::MediaPlayback::Paused {
                        progress: Some(souvlaki::MediaPosition(Duration::from_millis(
                            snapshot.position_ms as u64,
                        ))),
                    },
                    _ => souvlaki::MediaPlayback::Stopped,
                };
                let _ = controls.set_playback(pb);
            }
        }
    }
}

static UI: OnceLock<Mutex<UiState>> = OnceLock::new();
static MAIN_HWND: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(null_mut());

/// Locks the shared UI state (tolerating poisoning: a panic must not wedge the window).
fn ui_state() -> Option<MutexGuard<'static, UiState>> {
    UI.get()
        .map(|m| m.lock().unwrap_or_else(|e| e.into_inner()))
}

fn invalidate_main_window() {
    let hwnd = MAIN_HWND.load(Ordering::Acquire);
    if !hwnd.is_null() {
        unsafe { InvalidateRect(hwnd, null_mut(), 0) };
    }
}

pub struct UiState {
    pub active_tab: usize,
    /// The Spotify client ID being typed or pasted during first-run setup.
    pub client_id_input: String,
    /// Why the typed client ID was rejected.
    pub setup_error: Option<&'static str>,
    pub search_input: String,
    pub search_results: Vec<TrackInfo>,
    pub search_selected_idx: usize,
    pub search_scroll_offset: usize,
    pub playlists: Vec<PlaylistSummary>,
    /// First playlist shown in the Library tab's left column.
    pub playlist_list_offset: usize,
    pub selected_playlist_idx: Option<usize>,
    pub playlist_tracks: Vec<TrackInfo>,
    pub playlist_scroll_offset: usize,
    pub lyrics: Option<LyricsData>,
    /// Track URI the latest lyrics request was made for, so each track is fetched once.
    pub lyrics_requested_for: Option<String>,
    pub eq_preset_idx: usize,
    pub req_tx: mpsc::Sender<ClientRequest>,
    pub snapshot: Arc<RwLock<Snapshot>>,
    /// Last snapshot read, used when the core holds the state lock during a repaint.
    pub last_snapshot: Snapshot,
    /// The window thread is not a Tokio thread, so async work is spawned through this handle.
    runtime: Handle,
}

impl UiState {
    /// Saves the typed client ID, or explains why it is not one.
    fn submit_client_id(&mut self) {
        let id = self.client_id_input.trim().to_string();
        if crate::session::valid_client_id(&id) {
            self.send_cmd(Command::SetClientId(id));
            self.client_id_input.clear();
            self.setup_error = None;
        } else {
            self.setup_error = Some(
                "That doesn't look like a Client ID: it is 32 letters and digits, shown on your app's page in the dashboard.",
            );
        }
    }

    pub fn send_cmd(&self, cmd: Command) {
        let _ = self.req_tx.try_send(ClientRequest::Command(cmd));
    }

    /// Sends a query to the core; `on_resp` runs with the UI state locked, then the window repaints.
    pub fn query(
        &self,
        cmd: Command,
        on_resp: impl FnOnce(&mut UiState, CoreEvent) + Send + 'static,
    ) {
        let (tx, rx) = oneshot::channel();
        if self
            .req_tx
            .try_send(ClientRequest::Query { cmd, resp_tx: tx })
            .is_err()
        {
            return;
        }
        self.runtime.spawn(async move {
            if let Ok(resp) = rx.await {
                if let Some(mut ui) = ui_state() {
                    on_resp(&mut ui, resp);
                }
                invalidate_main_window();
            }
        });
    }

    /// Refreshes and returns the latest core snapshot without blocking the window thread.
    pub fn refresh_snapshot(&mut self) -> Snapshot {
        if let Ok(s) = self.snapshot.try_read() {
            self.last_snapshot = s.clone();
        }
        self.last_snapshot.clone()
    }

    pub fn select_tab(&mut self, tab: usize) {
        self.active_tab = tab;
        match tab {
            TAB_LIBRARY if self.playlists.is_empty() => {
                self.query(Command::GetPlaylists, |ui, evt| {
                    if let CoreEvent::Playlists(list) = evt {
                        ui.playlists = list;
                    }
                });
            }
            TAB_LYRICS => self.fetch_lyrics_if_stale(),
            _ => {}
        }
    }

    /// Requests lyrics when the lyrics tab is open and the track changed since the last request.
    fn fetch_lyrics_if_stale(&mut self) {
        if self.active_tab != TAB_LYRICS {
            return;
        }
        let Some(uri) = self
            .last_snapshot
            .current_track
            .as_ref()
            .map(|t| t.uri.clone())
        else {
            return;
        };
        if self.lyrics_requested_for.as_ref() == Some(&uri) {
            return;
        }
        self.lyrics_requested_for = Some(uri);
        self.lyrics = None;
        self.query(Command::GetLyrics, |ui, evt| {
            if let CoreEvent::Lyrics(data) = evt {
                // Ignore a late reply for a track that is no longer playing.
                if ui.lyrics_requested_for.as_ref() == Some(&data.uri) {
                    ui.lyrics = Some(data);
                }
            }
        });
    }

    fn run_search(&self) {
        if self.search_input.is_empty() {
            return;
        }
        let query = self.search_input.clone();
        self.query(Command::Search { query }, |ui, evt| {
            if let CoreEvent::SearchResults { tracks, .. } = evt {
                ui.search_results = tracks;
                ui.search_selected_idx = 0;
                ui.search_scroll_offset = 0;
            }
        });
    }
}

pub struct NativeUi;

impl NativeUi {
    /// Starts the window thread. Must be called from within the Tokio runtime.
    pub fn start(snapshot: Arc<RwLock<Snapshot>>, req_tx: mpsc::Sender<ClientRequest>) {
        let state = UiState {
            active_tab: TAB_PLAYER,
            client_id_input: String::new(),
            setup_error: None,
            search_input: String::new(),
            search_results: Vec::new(),
            search_selected_idx: 0,
            search_scroll_offset: 0,
            playlists: Vec::new(),
            selected_playlist_idx: None,
            playlist_tracks: Vec::new(),
            playlist_scroll_offset: 0,
            playlist_list_offset: 0,
            lyrics: None,
            lyrics_requested_for: None,
            eq_preset_idx: 0,
            req_tx,
            snapshot,
            last_snapshot: Snapshot::default(),
            runtime: Handle::current(),
        };

        if UI.set(Mutex::new(state)).is_err() {
            error!("NativeUi::start called twice; ignoring.");
            return;
        }

        std::thread::spawn(|| unsafe {
            Self::run_window_loop();
        });
    }

    /// Brings the window of an already running instance to the front.
    pub fn focus_existing() {
        let class_name = Self::to_wide(CLASS_NAME);
        unsafe {
            let hwnd = FindWindowW(class_name.as_ptr(), std::ptr::null());
            if !hwnd.is_null() {
                ShowWindow(hwnd, SW_SHOW);
                SetForegroundWindow(hwnd);
            }
        }
    }

    /// Destroys the window (removing the tray icon) and waits until it is gone.
    pub fn shutdown() {
        let hwnd = MAIN_HWND.load(Ordering::Acquire);
        if !hwnd.is_null() {
            unsafe { SendMessageW(hwnd, WM_APP_QUIT, 0, 0) };
        }
    }

    fn to_wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    unsafe fn run_window_loop() {
        let instance = GetModuleHandleW(null_mut());
        let class_name = Self::to_wide(CLASS_NAME);
        let bg_brush = CreateSolidBrush(0x00121212);
        let icon: HICON = LoadIconW(null_mut(), IDI_APPLICATION);

        let wnd_class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: icon,
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: bg_brush,
            lpszMenuName: null_mut(),
            lpszClassName: class_name.as_ptr(),
        };

        RegisterClassW(&wnd_class);

        let title = Self::to_wide("Stupefinity");
        let hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            120,
            80,
            960,
            640,
            null_mut(),
            null_mut(),
            instance,
            null_mut(),
        );

        if hwnd.is_null() {
            error!("Failed to create Stupefinity main window!");
            return;
        }

        MAIN_HWND.store(hwnd, Ordering::Release);

        // Windows SMTC Integration via souvlaki
        let config = souvlaki::PlatformConfig {
            dbus_name: "stupefinity",
            display_name: "Stupefinity",
            hwnd: Some(hwnd as *mut _),
        };
        let mut smtc_ready = false;
        if let Ok(mut controls) = souvlaki::MediaControls::new(config) {
            smtc_ready = true;
            let _ = controls.attach(move |event: souvlaki::MediaControlEvent| {
                if let Some(ui) = ui_state() {
                    match event {
                        souvlaki::MediaControlEvent::Play => ui.send_cmd(Command::Play),
                        souvlaki::MediaControlEvent::Pause => ui.send_cmd(Command::Pause),
                        souvlaki::MediaControlEvent::Toggle => {
                            ui.send_cmd(Command::PlayPauseToggle)
                        }
                        souvlaki::MediaControlEvent::Next => ui.send_cmd(Command::Next),
                        souvlaki::MediaControlEvent::Previous => ui.send_cmd(Command::Prev),
                        souvlaki::MediaControlEvent::Stop => ui.send_cmd(Command::Pause),
                        _ => {}
                    }
                }
            });
            let _ = SMTC.set(Mutex::new(Some(controls)));
        }

        // The media controls already receive the media keys; grabbing them as hotkeys too
        // would take them from every other app, so that is only a fallback.
        if !smtc_ready {
            RegisterHotKey(hwnd, HOTKEY_PLAY_PAUSE, 0, VK_MEDIA_PLAY_PAUSE as u32);
            RegisterHotKey(hwnd, HOTKEY_NEXT, 0, VK_MEDIA_NEXT_TRACK as u32);
            RegisterHotKey(hwnd, HOTKEY_PREV, 0, VK_MEDIA_PREV_TRACK as u32);
            RegisterHotKey(hwnd, HOTKEY_STOP, 0, VK_MEDIA_STOP as u32);
        }

        // Register System Tray Icon
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = TRAY_ICON_ID;
        nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        nid.uCallbackMessage = WM_TRAYICON;
        nid.hIcon = icon;
        let tip_wide = Self::to_wide("Stupefinity");
        for (i, &ch) in tip_wide.iter().enumerate().take(nid.szTip.len() - 1) {
            nid.szTip[i] = ch;
        }
        Shell_NotifyIconW(NIM_ADD, &nid);

        // 200ms Scrubber Timer
        SetTimer(hwnd, 1, 200, None);

        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);

        // Windows Message Pump
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        DeleteObject(bg_brush);
    }
}

/// Shows the window and brings it to the front (WM_SHOWWINDOW notifies the core).
unsafe fn show_window(hwnd: HWND) {
    ShowWindow(hwnd, SW_SHOW);
    SetForegroundWindow(hwnd);
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let hdc = BeginPaint(hwnd, &mut ps);

            let mut rect: RECT = std::mem::zeroed();
            GetClientRect(hwnd, &mut rect);
            let w = rect.right;
            let h = rect.bottom;

            // Double Buffering: allocate memory DC and bitmap
            let mem_dc = CreateCompatibleDC(hdc);
            let mem_bmp = CreateCompatibleBitmap(hdc, w, h);
            let old_bmp = SelectObject(mem_dc, mem_bmp);

            if let Some(mut ui) = ui_state() {
                let snapshot = ui.refresh_snapshot();
                render_ui(mem_dc, &rect, &ui, &snapshot);
            }

            // Blit memory DC onto physical screen DC
            BitBlt(hdc, 0, 0, w, h, mem_dc, 0, 0, SRCCOPY);

            // Clean up double buffer
            SelectObject(mem_dc, old_bmp);
            DeleteObject(mem_bmp);
            DeleteDC(mem_dc);

            EndPaint(hwnd, &ps);
            0
        }

        WM_TIMER => {
            let snap = ui_state().map(|mut ui| {
                ui.fetch_lyrics_if_stale();
                (ui.refresh_snapshot(), ui.runtime.clone())
            });
            // Outside the UI lock: the media controls can be slow to update.
            if let Some((snap, runtime)) = snap {
                update_smtc(&snap);
                let cover = snap
                    .current_track
                    .as_ref()
                    .and_then(|t| t.cover_url.as_deref());
                crate::album_art::request(cover, &runtime, invalidate_main_window);
            }
            InvalidateRect(hwnd, null_mut(), 0);
            0
        }

        WM_MOUSEWHEEL => {
            let delta = ((wparam >> 16) & 0xFFFF) as i16;
            if let Some(mut ui) = ui_state() {
                if ui.active_tab == TAB_SEARCH {
                    if delta < 0 {
                        let max_offset = ui.search_results.len().saturating_sub(VISIBLE_ROWS);
                        ui.search_scroll_offset = (ui.search_scroll_offset + 3).min(max_offset);
                    } else if delta > 0 {
                        ui.search_scroll_offset = ui.search_scroll_offset.saturating_sub(3);
                    }
                } else if ui.active_tab == TAB_LIBRARY && wheel_over_playlist_list(hwnd, lparam) {
                    if delta < 0 {
                        let max_offset = ui.playlists.len().saturating_sub(VISIBLE_ROWS);
                        ui.playlist_list_offset = (ui.playlist_list_offset + 3).min(max_offset);
                    } else if delta > 0 {
                        ui.playlist_list_offset = ui.playlist_list_offset.saturating_sub(3);
                    }
                } else if ui.active_tab == TAB_LIBRARY {
                    if delta < 0 {
                        let max_offset = ui.playlist_tracks.len().saturating_sub(VISIBLE_ROWS);
                        ui.playlist_scroll_offset = (ui.playlist_scroll_offset + 3).min(max_offset);
                    } else if delta > 0 {
                        ui.playlist_scroll_offset = ui.playlist_scroll_offset.saturating_sub(3);
                    }
                }
            }
            InvalidateRect(hwnd, null_mut(), 0);
            0
        }

        WM_SHOWWINDOW => {
            if let Some(ui) = ui_state() {
                ui.send_cmd(if wparam != 0 {
                    Command::WindowShown
                } else {
                    Command::WindowHidden
                });
            }
            0
        }

        WM_HOTKEY => {
            if let Some(ui) = ui_state() {
                match wparam as i32 {
                    HOTKEY_PLAY_PAUSE => ui.send_cmd(Command::PlayPauseToggle),
                    HOTKEY_NEXT => ui.send_cmd(Command::Next),
                    HOTKEY_PREV => ui.send_cmd(Command::Prev),
                    HOTKEY_STOP => ui.send_cmd(Command::Pause),
                    _ => {}
                }
            }
            0
        }

        WM_TRAYICON => {
            let event = lparam as u32;
            if event == WM_LBUTTONDBLCLK {
                show_window(hwnd);
            } else if event == WM_RBUTTONUP {
                let mut cursor = POINT { x: 0, y: 0 };
                GetCursorPos(&mut cursor);

                let hmenu = CreatePopupMenu();
                let s_open = NativeUi::to_wide("Open Stupefinity");
                let s_play = NativeUi::to_wide("Play / Pause");
                let s_next = NativeUi::to_wide("Next Track");
                let s_prev = NativeUi::to_wide("Previous Track");
                let s_exit = NativeUi::to_wide("Exit");

                AppendMenuW(hmenu, MF_STRING, ID_TRAY_OPEN, s_open.as_ptr());
                AppendMenuW(hmenu, MF_SEPARATOR, 0, null_mut());
                AppendMenuW(hmenu, MF_STRING, ID_TRAY_PLAY_PAUSE, s_play.as_ptr());
                AppendMenuW(hmenu, MF_STRING, ID_TRAY_NEXT, s_next.as_ptr());
                AppendMenuW(hmenu, MF_STRING, ID_TRAY_PREV, s_prev.as_ptr());
                AppendMenuW(hmenu, MF_SEPARATOR, 0, null_mut());
                AppendMenuW(hmenu, MF_STRING, ID_TRAY_EXIT, s_exit.as_ptr());

                SetForegroundWindow(hwnd);
                TrackPopupMenuEx(
                    hmenu,
                    TPM_BOTTOMALIGN | TPM_LEFTALIGN,
                    cursor.x,
                    cursor.y,
                    hwnd,
                    null_mut(),
                );
                DestroyMenu(hmenu);
            }
            0
        }

        WM_COMMAND => {
            let id = wparam & 0xFFFF;
            if id == ID_TRAY_OPEN {
                show_window(hwnd);
            } else if let Some(ui) = ui_state() {
                match id {
                    ID_TRAY_PLAY_PAUSE => ui.send_cmd(Command::PlayPauseToggle),
                    ID_TRAY_NEXT => ui.send_cmd(Command::Next),
                    ID_TRAY_PREV => ui.send_cmd(Command::Prev),
                    // The core stops playback, then tears this window down via WM_APP_QUIT.
                    ID_TRAY_EXIT => ui.send_cmd(Command::Shutdown),
                    _ => {}
                }
            }
            0
        }

        WM_LBUTTONDOWN => {
            let x = (lparam & 0xFFFF) as i16 as i32;
            let y = ((lparam >> 16) & 0xFFFF) as i16 as i32;

            let mut rect: RECT = std::mem::zeroed();
            GetClientRect(hwnd, &mut rect);

            handle_click(x, y, &rect);
            InvalidateRect(hwnd, null_mut(), 0);
            0
        }

        WM_KEYDOWN => {
            let is_ctrl = (GetKeyState(VK_CONTROL as i32) as u16 & 0x8000) != 0;
            handle_key_down(hwnd, wparam as u16, is_ctrl);
            InvalidateRect(hwnd, null_mut(), 0);
            0
        }

        WM_CHAR => {
            // Surrogate halves (emoji) are not valid chars on their own and are dropped.
            if let Some(ch) = char::from_u32(wparam as u32).filter(|c| !c.is_control()) {
                if let Some(mut ui) = ui_state() {
                    let snap = &ui.last_snapshot;
                    let typing_id = snap.auth_state == AuthState::Unauthenticated
                        && onboarding(snap) == Onboarding::ClientId;
                    if typing_id {
                        if ch.is_ascii_alphanumeric() && ui.client_id_input.len() < 64 {
                            ui.client_id_input.push(ch);
                            ui.setup_error = None;
                        }
                        InvalidateRect(hwnd, null_mut(), 0);
                    } else if ui.active_tab == TAB_SEARCH {
                        ui.search_input.push(ch);
                        InvalidateRect(hwnd, null_mut(), 0);
                    }
                }
            }
            0
        }

        WM_CLOSE => {
            // Minimize to system tray instead of exiting
            ShowWindow(hwnd, SW_HIDE);
            0
        }

        WM_APP_QUIT => {
            DestroyWindow(hwnd);
            0
        }

        WM_DESTROY => {
            if let Some(smtc) = SMTC.get() {
                if let Ok(mut guard) = smtc.lock() {
                    *guard = None;
                }
            }
            MAIN_HWND.store(null_mut(), Ordering::Release);
            KillTimer(hwnd, 1);
            UnregisterHotKey(hwnd, HOTKEY_PLAY_PAUSE);
            UnregisterHotKey(hwnd, HOTKEY_NEXT);
            UnregisterHotKey(hwnd, HOTKEY_PREV);
            UnregisterHotKey(hwnd, HOTKEY_STOP);
            let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
            nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
            nid.hWnd = hwnd;
            nid.uID = TRAY_ICON_ID;
            Shell_NotifyIconW(NIM_DELETE, &nid);
            PostQuitMessage(0);
            0
        }

        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

// -------------------------------------------------------------------------------------------------
// Interaction Handlers
// -------------------------------------------------------------------------------------------------

fn handle_click(x: i32, y: i32, rect: &RECT) {
    let Some(mut ui) = ui_state() else { return };
    let snapshot = ui.refresh_snapshot();

    // 1. Header: tabs and the auth button
    if let Some(tab) = (0..TAB_LABELS.len()).find(|&i| hit(&tab_rect(i), x, y)) {
        ui.select_tab(tab);
        return;
    }
    if hit(&auth_rect(rect.right), x, y) {
        match &snapshot.auth_state {
            AuthState::Unauthenticated if onboarding(&snapshot) == Onboarding::Login => {
                ui.send_cmd(Command::Login)
            }
            AuthState::Unauthenticated => {}
            AuthState::Authenticated { .. } => ui.send_cmd(Command::Logout),
            AuthState::Authenticating => {}
        }
        return;
    }

    // 2. Logged out: first-run setup, then the login button
    if snapshot.auth_state == AuthState::Unauthenticated {
        let w = rect.right;
        let mut external = None;
        match onboarding(&snapshot) {
            Onboarding::Terms => {
                let (terms, privacy) = pair_button_rects(w, TERMS_LINKS_TOP);
                if hit(&hero_login_rect(w), x, y) {
                    ui.send_cmd(Command::AcceptTerms);
                } else if hit(&terms, x, y) {
                    external = Some(External::Document(
                        "Stupefinity Terms of Use.txt",
                        TERMS_DOC,
                    ));
                } else if hit(&privacy, x, y) {
                    external = Some(External::Document(
                        "Stupefinity Privacy Policy.txt",
                        PRIVACY_DOC,
                    ));
                }
            }
            Onboarding::ClientId => {
                let (save, dashboard) = pair_button_rects(w, SETUP_BUTTONS_TOP);
                if hit(&save, x, y) {
                    ui.submit_client_id();
                } else if hit(&dashboard, x, y) {
                    external = Some(External::Url(DASHBOARD_URL));
                }
            }
            Onboarding::Login => {
                if hit(&hero_login_rect(w), x, y) {
                    ui.send_cmd(Command::Login);
                } else if hit(&change_client_id_rect(w), x, y) {
                    ui.send_cmd(Command::SetClientId(String::new()));
                }
            }
        }
        drop(ui);
        if let Some(target) = external {
            open_external(target);
        }
        return;
    }

    // 3. Bottom Player Bar
    if y >= rect.bottom - BOTTOM_BAR_H {
        handle_bottom_bar_click(&ui, &snapshot, x, y, rect);
        return;
    }

    // 4. Tab Specific Content Interactions
    match ui.active_tab {
        TAB_PLAYER => {
            if hit(&like_rect(), x, y) {
                if let Some(track) = &snapshot.current_track {
                    let uri = track.uri.clone();
                    ui.send_cmd(if track.is_liked {
                        Command::UnlikeTrack { uri }
                    } else {
                        Command::LikeTrack { uri }
                    });
                }
            } else if (330..rect.right - 40).contains(&x) {
                let queue_start_y = 255;
                let queue_row_h = 28;
                let count = snapshot.queue_window.len().min(6);
                if let Some(row) = row_at(y, queue_start_y, queue_row_h, count) {
                    if let Some(item) = snapshot.queue_window.get(row) {
                        if item.is_user_queued {
                            let uq_idx = snapshot.queue_window[..=row]
                                .iter()
                                .filter(|it| it.is_user_queued)
                                .count()
                                .saturating_sub(1);
                            ui.send_cmd(Command::RemoveFromQueue(uq_idx));
                        }
                    }
                }
            }
        }
        TAB_SEARCH => {
            if hit(&search_button_rect(rect.right), x, y) {
                ui.run_search();
            } else if let Some(row) = row_at(y, SEARCH_LIST_TOP, SEARCH_ROW_H, VISIBLE_ROWS) {
                let idx = row + ui.search_scroll_offset;
                if idx < ui.search_results.len() {
                    let uri = ui.search_results[idx].uri.clone();
                    if (rect.right - 80..rect.right - 40).contains(&x) {
                        ui.send_cmd(Command::Enqueue(uri));
                    } else {
                        ui.search_selected_idx = idx;
                        ui.send_cmd(Command::LoadContext {
                            uri,
                            start_index: 0,
                        });
                    }
                }
            }
        }
        TAB_LIBRARY => {
            if (40..300).contains(&x) {
                let shown = ui.playlists.len().saturating_sub(ui.playlist_list_offset);
                if let Some(row) =
                    row_at(y, LIBRARY_LIST_TOP, PLAYLIST_ROW_H, shown.min(VISIBLE_ROWS))
                {
                    let idx = row + ui.playlist_list_offset;
                    ui.selected_playlist_idx = Some(idx);
                    ui.playlist_tracks.clear();
                    ui.playlist_scroll_offset = 0;
                    let playlist_id = ui.playlists[idx].id.clone();
                    ui.query(Command::GetPlaylistTracks { playlist_id }, |ui, evt| {
                        if let CoreEvent::PlaylistTracks { tracks, .. } = evt {
                            ui.playlist_tracks = tracks;
                        }
                    });
                }
            } else if (PLAYLIST_TRACKS_LEFT..rect.right - 40).contains(&x) {
                let selected = ui.selected_playlist_idx.and_then(|i| ui.playlists.get(i));
                if let (Some(playlist), Some(row)) = (
                    selected,
                    row_at(y, LIBRARY_LIST_TOP, PLAYLIST_TRACK_ROW_H, VISIBLE_ROWS),
                ) {
                    let idx = row + ui.playlist_scroll_offset;
                    if idx < ui.playlist_tracks.len() {
                        let uri = ui.playlist_tracks[idx].uri.clone();
                        if (rect.right - 80..rect.right - 40).contains(&x) {
                            ui.send_cmd(Command::Enqueue(uri));
                        } else {
                            ui.send_cmd(Command::LoadContext {
                                uri: format!("spotify:playlist:{}", playlist.id),
                                start_index: idx,
                            });
                        }
                    }
                }
            }
        }
        TAB_EQ => {
            if hit(&eq_toggle_rect(), x, y) {
                ui.send_cmd(Command::ToggleEq(!snapshot.eq_enabled));
            } else if let Some(i) = (0..EQ_PRESETS.len()).find(|&i| hit(&eq_preset_rect(i), x, y)) {
                ui.eq_preset_idx = i;
                ui.send_cmd(Command::SetEq(eq_preset_profile(EQ_PRESETS[i])));
            }
        }
        _ => {}
    }
}

fn handle_bottom_bar_click(ui: &UiState, snapshot: &Snapshot, x: i32, y: i32, rect: &RECT) {
    // Scrubber Bar (y: rect.bottom - 85..rect.bottom - 65)
    let scrub_x1 = 90;
    let scrub_x2 = rect.right - 90;
    if (rect.bottom - 85..rect.bottom - 65).contains(&y) && (scrub_x1..scrub_x2).contains(&x) {
        let ratio = (x - scrub_x1) as f32 / (scrub_x2 - scrub_x1) as f32;
        ui.send_cmd(Command::Seek((ratio * snapshot.duration_ms as f32) as u32));
        return;
    }

    // Transport Controls, laid out around the horizontal center
    let cx = rect.right / 2;
    let cmd = if (cx - 110..cx - 65).contains(&x) {
        Command::Prev
    } else if (cx - 55..cx + 10).contains(&x) {
        Command::PlayPauseToggle
    } else if (cx + 20..cx + 65).contains(&x) {
        Command::Next
    } else if (cx + 75..cx + 125).contains(&x) {
        Command::SetShuffle(!snapshot.shuffle)
    } else if (cx + 135..cx + 195).contains(&x) {
        Command::SetRepeat(next_repeat(snapshot.repeat))
    } else if (rect.right - 190..rect.right - 90).contains(&x) {
        let ratio = (x - (rect.right - 190)) as f32 / 100.0;
        Command::SetVolume((ratio.clamp(0.0, 1.0) * f32::from(u16::MAX)) as u16)
    } else {
        return;
    };
    ui.send_cmd(cmd);
}

unsafe fn handle_key_down(hwnd: HWND, key: u16, is_ctrl: bool) {
    // Escape: Minimize to System Tray. Handled before locking the UI state because
    // ShowWindow synchronously re-enters wnd_proc (WM_SHOWWINDOW), which locks it too.
    if key == VK_ESCAPE {
        ShowWindow(hwnd, SW_HIDE);
        return;
    }

    // Read before taking the UI lock (clipboard calls must not run under it).
    let pasted = if key == 0x56 && is_ctrl {
        unsafe { clipboard_text(hwnd) }
    } else {
        None
    };
    let Some(mut ui) = ui_state() else { return };
    let snapshot = ui.refresh_snapshot();
    let setup = (snapshot.auth_state == AuthState::Unauthenticated).then(|| onboarding(&snapshot));
    let typing_id = setup == Some(Onboarding::ClientId);
    // While a text box has focus, letter/digit/space keys are text (see WM_CHAR).
    let typing = (ui.active_tab == TAB_SEARCH || typing_id) && !is_ctrl;

    match key {
        VK_SPACE if !typing => ui.send_cmd(Command::PlayPauseToggle),

        // Right Arrow: Next Track (Ctrl) or Seek +5s
        VK_RIGHT if is_ctrl => ui.send_cmd(Command::Next),
        VK_RIGHT => ui.send_cmd(Command::Seek(
            (snapshot.position_ms + 5000).min(snapshot.duration_ms),
        )),

        // Left Arrow: Prev Track (Ctrl) or Seek -5s
        VK_LEFT if is_ctrl => ui.send_cmd(Command::Prev),
        VK_LEFT => ui.send_cmd(Command::Seek(snapshot.position_ms.saturating_sub(5000))),

        VK_UP => ui.send_cmd(Command::SetVolume(
            snapshot.volume.saturating_add(VOLUME_STEP),
        )),
        VK_DOWN => ui.send_cmd(Command::SetVolume(
            snapshot.volume.saturating_sub(VOLUME_STEP),
        )),

        // Return / Enter: Login (if unauthenticated), or Submit Search
        VK_RETURN => match setup {
            Some(Onboarding::Terms) => ui.send_cmd(Command::AcceptTerms),
            Some(Onboarding::ClientId) => ui.submit_client_id(),
            Some(Onboarding::Login) => ui.send_cmd(Command::Login),
            None if ui.active_tab == TAB_SEARCH => ui.run_search(),
            None => {}
        },

        VK_BACK if typing_id => {
            ui.client_id_input.pop();
        }
        VK_BACK if ui.active_tab == TAB_SEARCH => {
            ui.search_input.pop();
        }

        // Ctrl+V: paste into the focused text box
        0x56 if is_ctrl => {
            let text: String = pasted.unwrap_or_default().trim().to_string();
            if typing_id {
                ui.client_id_input = text.chars().take(64).collect();
                ui.setup_error = None;
            } else if ui.active_tab == TAB_SEARCH {
                ui.search_input.push_str(&text.replace(['\r', '\n'], " "));
            }
        }

        // Tabs 1 - 5
        0x31..=0x35 if !typing => ui.select_tab(usize::from(key - 0x31)),

        // Ctrl+F: Search
        0x46 if is_ctrl => ui.select_tab(TAB_SEARCH),

        // 'S': Toggle Shuffle
        0x53 if !typing => ui.send_cmd(Command::SetShuffle(!snapshot.shuffle)),

        // 'R': Cycle Repeat
        0x52 if !typing => ui.send_cmd(Command::SetRepeat(next_repeat(snapshot.repeat))),

        // 'L': Login or Lyrics
        0x4C if !typing => {
            if setup == Some(Onboarding::Login) {
                ui.send_cmd(Command::Login);
            } else if setup.is_none() {
                ui.select_tab(TAB_LYRICS);
            }
        }

        _ => {}
    }
}

// -------------------------------------------------------------------------------------------------
// Rendering Engine (Win32 GDI)
// -------------------------------------------------------------------------------------------------

unsafe fn render_ui(
    hdc: windows_sys::Win32::Graphics::Gdi::HDC,
    rect: &RECT,
    ui: &UiState,
    snapshot: &Snapshot,
) {
    let w = rect.right;

    // 1. Dark Background (#121212)
    let bg_brush = CreateSolidBrush(0x00121212);
    FillRect(hdc, rect, bg_brush);
    DeleteObject(bg_brush);

    SetBkMode(hdc, TRANSPARENT as i32);

    // Use cached GDI fonts (created once to prevent allocation churn)
    let fonts = CACHED_FONTS.get_or_init(|| CachedFonts {
        font_title: create_gdi_font(22, true),
        font_sub: create_gdi_font(15, true),
        font_body: create_gdi_font(13, false),
        font_small: create_gdi_font(11, false),
    });
    let font_title = fonts.font_title;
    let font_sub = fonts.font_sub;
    let font_body = fonts.font_body;
    let font_small = fonts.font_small;

    // 2. Top Header Bar (y: 0..60)
    let header_brush = CreateSolidBrush(0x00181818);
    let header_rect = RECT {
        left: 0,
        top: 0,
        right: w,
        bottom: 60,
    };
    FillRect(hdc, &header_rect, header_brush);
    DeleteObject(header_brush);

    // Brand Logo: STUPEFINITY (Spotify Green #1DB954)
    SelectObject(hdc, font_title);
    SetTextColor(hdc, 0x0054B91D);
    let mut brand_text = NativeUi::to_wide("STUPEFINITY");
    let mut brand_rect = RECT {
        left: 20,
        top: 10,
        right: 180,
        bottom: 50,
    };
    DrawTextW(
        hdc,
        brand_text.as_mut_ptr(),
        -1,
        &mut brand_rect,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );

    // Navigation Tabs: [ 1. Player ] [ 2. Search ] [ 3. Library ] [ 4. Lyrics ] [ 5. Equalizer ]
    SelectObject(hdc, font_sub);
    for (i, &name) in TAB_LABELS.iter().enumerate() {
        let is_active = ui.active_tab == i;
        let mut t_rect = tab_rect(i);
        let (x1, x2) = (t_rect.left, t_rect.right);

        if is_active {
            SetTextColor(hdc, 0x0054B91D); // Green text
            let tab_indicator = CreateSolidBrush(0x0054B91D);
            let ind_rect = RECT {
                left: x1 + 5,
                top: 55,
                right: x2 - 5,
                bottom: 59,
            };
            FillRect(hdc, &ind_rect, tab_indicator);
            DeleteObject(tab_indicator);
        } else {
            SetTextColor(hdc, 0x00A0A0A0); // Grey text
        }

        let mut tab_text = NativeUi::to_wide(name);
        DrawTextW(
            hdc,
            tab_text.as_mut_ptr(),
            -1,
            &mut t_rect,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
        );
    }

    // Top-Right Auth Pill / Button
    SelectObject(hdc, font_body);
    let (auth_str, btn_color) = match &snapshot.auth_state {
        AuthState::Unauthenticated => ("🎵 Log In".to_string(), 0x0054B91D),
        AuthState::Authenticating => ("Authenticating...".to_string(), 0x0000A5FF),
        AuthState::Authenticated {
            display_name,
            is_premium,
            ..
        } => {
            let tier = if *is_premium { "Premium" } else { "Free" };
            (format!("👤 {} ({})", display_name, tier), 0x00888888)
        }
    };

    let auth_brush = CreateSolidBrush(0x00242424);
    let mut auth_btn_rect = auth_rect(w);
    FillRect(hdc, &auth_btn_rect, auth_brush);
    DeleteObject(auth_brush);

    SetTextColor(hdc, btn_color);
    let mut auth_text = NativeUi::to_wide(&auth_str);
    DrawTextW(
        hdc,
        auth_text.as_mut_ptr(),
        -1,
        &mut auth_btn_rect,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );

    // 3. Main Content Viewport (y: 65..h - 95)
    match snapshot.auth_state {
        AuthState::Unauthenticated | AuthState::Authenticating => {
            render_unauthenticated_view(hdc, rect, snapshot, ui, font_title, font_sub, font_body);
        }
        _ => match ui.active_tab {
            TAB_PLAYER => render_player_view(
                hdc, rect, snapshot, font_title, font_sub, font_body, font_small,
            ),
            TAB_SEARCH => render_search_view(hdc, rect, ui, font_sub, font_body),
            TAB_LIBRARY => render_library_view(hdc, rect, ui, font_sub, font_body),
            TAB_LYRICS => {
                render_lyrics_view(hdc, rect, ui, snapshot, font_title, font_sub, font_body)
            }
            TAB_EQ => render_eq_view(hdc, ui, snapshot, font_sub, font_body),
            _ => {}
        },
    }

    // 4. Bottom Persistent Player Bar (y: h - 95..h)
    render_bottom_bar(hdc, rect, snapshot, font_body, font_sub, font_small);
}

// -------------------------------------------------------------------------------------------------
// Sub-view Renderers
// -------------------------------------------------------------------------------------------------

/// Shown under the login button. Not affiliated: Spotify's terms forbid implying endorsement.
static WELCOME_NOTE: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();

/// Replaces the text under the login button; call before the window opens.
pub fn set_welcome_note(note: &'static str) {
    let _ = WELCOME_NOTE.set(note);
}

fn welcome_note() -> &'static str {
    WELCOME_NOTE.get().copied().unwrap_or(
        "Log in with your Spotify Premium account. Stupefinity is not affiliated with Spotify.",
    )
}

const TERMS_INTRO: &str = "Stupefinity is an independent app that works with your Spotify account. It is not made, endorsed or supported by Spotify. Before you start, please read and agree to the Terms of Use and Privacy Policy.";

const SETUP_STEPS: &str = "1.  Open the Spotify Developer Dashboard and create an app.\n2.  Add the redirect URI  http://127.0.0.1:8888/login  and tick Web API.\n3.  Copy the app's Client ID, paste it below (Ctrl+V) and press Enter.";

/// Draws `text` in `rect` with the selected font.
unsafe fn draw_text(
    hdc: windows_sys::Win32::Graphics::Gdi::HDC,
    text: &str,
    mut rect: RECT,
    color: u32,
    flags: u32,
) {
    SetTextColor(hdc, color);
    let mut wide = NativeUi::to_wide(text);
    DrawTextW(hdc, wide.as_mut_ptr(), -1, &mut rect, flags);
}

/// A filled (primary) or dark (secondary) button with centered text.
unsafe fn draw_button(
    hdc: windows_sys::Win32::Graphics::Gdi::HDC,
    rect: RECT,
    label: &str,
    primary: bool,
) {
    let brush = CreateSolidBrush(if primary { 0x0054B91D } else { 0x00303030 });
    FillRect(hdc, &rect, brush);
    DeleteObject(brush);
    draw_text(
        hdc,
        label,
        rect,
        0x00FFFFFF,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );
}

#[allow(clippy::too_many_arguments)]
unsafe fn render_unauthenticated_view(
    hdc: windows_sys::Win32::Graphics::Gdi::HDC,
    rect: &RECT,
    snapshot: &Snapshot,
    ui: &UiState,
    font_title: windows_sys::Win32::Graphics::Gdi::HFONT,
    font_sub: windows_sys::Win32::Graphics::Gdi::HFONT,
    font_body: windows_sys::Win32::Graphics::Gdi::HFONT,
) {
    let w = rect.right;
    let cx = w / 2;
    let line = DT_CENTER | DT_VCENTER | DT_SINGLELINE;
    let row = |top: i32, bottom: i32, half: i32| RECT {
        left: cx - half,
        top,
        right: cx + half,
        bottom,
    };

    SelectObject(hdc, font_title);
    draw_text(
        hdc,
        "WELCOME TO STUPEFINITY",
        row(120, 160, 250),
        0x0054B91D,
        line,
    );

    match onboarding(snapshot) {
        Onboarding::Terms => {
            SelectObject(hdc, font_sub);
            draw_text(
                hdc,
                "Native Windows 11 music player  •  Target: ≤ 10 MB RAM",
                row(170, 200, 300),
                0x00CCCCCC,
                line,
            );
            SelectObject(hdc, font_body);
            draw_text(
                hdc,
                TERMS_INTRO,
                row(208, 268, 330),
                0x00999999,
                DT_CENTER | DT_WORDBREAK,
            );
            SelectObject(hdc, font_sub);
            draw_button(hdc, hero_login_rect(w), "I Agree  [Enter]", true);
            SelectObject(hdc, font_body);
            let (terms, privacy) = pair_button_rects(w, TERMS_LINKS_TOP);
            draw_button(hdc, terms, "Read the Terms of Use", false);
            draw_button(hdc, privacy, "Read the Privacy Policy", false);
        }
        Onboarding::ClientId => {
            SelectObject(hdc, font_sub);
            draw_text(
                hdc,
                "One-time setup: connect your own Spotify app",
                row(170, 200, 300),
                0x00CCCCCC,
                line,
            );
            SelectObject(hdc, font_body);
            draw_text(
                hdc,
                SETUP_STEPS,
                RECT {
                    left: cx - 260,
                    top: 205,
                    right: cx + 300,
                    bottom: 272,
                },
                0x00AAAAAA,
                DT_LEFT | DT_WORDBREAK,
            );

            let input = setup_input_rect(w);
            let brush = CreateSolidBrush(0x00202020);
            FillRect(hdc, &input, brush);
            DeleteObject(brush);
            let (text, color) = if ui.client_id_input.is_empty() {
                ("Paste your Client ID here (Ctrl+V)".to_string(), 0x00666666)
            } else {
                (format!("{}|", ui.client_id_input), 0x00FFFFFF)
            };
            draw_text(hdc, &text, input, color, line);

            let (save, dashboard) = pair_button_rects(w, SETUP_BUTTONS_TOP);
            draw_button(hdc, save, "Save  [Enter]", true);
            draw_button(hdc, dashboard, "Open the Developer Dashboard", false);
            if let Some(err) = ui.setup_error {
                draw_text(
                    hdc,
                    err,
                    row(375, 415, 330),
                    0x005050FF,
                    DT_CENTER | DT_WORDBREAK,
                );
            }
        }
        Onboarding::Login => {
            SelectObject(hdc, font_sub);
            draw_text(
                hdc,
                "Native Windows 11 music player  •  Target: ≤ 10 MB RAM",
                row(170, 200, 300),
                0x00CCCCCC,
                line,
            );
            SelectObject(hdc, font_body);
            draw_text(
                hdc,
                "No Chromium, no webview: native WASAPI audio with a built-in equalizer.",
                row(210, 240, 320),
                0x00888888,
                line,
            );

            let waiting = snapshot.auth_state == AuthState::Authenticating;
            SelectObject(hdc, font_sub);
            draw_button(
                hdc,
                hero_login_rect(w),
                if waiting {
                    "Waiting for your browser..."
                } else {
                    "🎵  Log In  [Enter]"
                },
                true,
            );

            SelectObject(hdc, font_body);
            let (note, color) = match (&snapshot.login_error, waiting) {
                (Some(err), false) => (err.as_str(), 0x005050FF),
                (_, true) => (
                    "Finish logging in on the page that opened in your browser, then come back here.",
                    0x00777777,
                ),
                (None, false) => (welcome_note(), 0x00777777),
            };
            draw_text(
                hdc,
                note,
                row(355, 415, 380),
                color,
                DT_CENTER | DT_WORDBREAK,
            );
            if !waiting {
                draw_text(
                    hdc,
                    "Change client ID",
                    change_client_id_rect(w),
                    0x00666666,
                    line,
                );
            }
        }
    }
}

unsafe fn render_player_view(
    hdc: windows_sys::Win32::Graphics::Gdi::HDC,
    rect: &RECT,
    snapshot: &Snapshot,
    font_title: windows_sys::Win32::Graphics::Gdi::HFONT,
    font_sub: windows_sys::Win32::Graphics::Gdi::HFONT,
    font_body: windows_sys::Win32::Graphics::Gdi::HFONT,
    font_small: windows_sys::Win32::Graphics::Gdi::HFONT,
) {
    let w = rect.right;

    // Left Column: Album Art Box (x: 40..300, y: 90..350)
    let art_brush = CreateSolidBrush(0x00202020);
    let mut art_rect = RECT {
        left: 40,
        top: 90,
        right: 300,
        bottom: 350,
    };
    FillRect(hdc, &art_rect, art_brush);
    DeleteObject(art_brush);

    let cover = snapshot
        .current_track
        .as_ref()
        .and_then(|t| t.cover_url.as_deref());
    if !crate::album_art::draw(hdc, cover, art_rect.left, art_rect.top) {
        SelectObject(hdc, font_title);
        SetTextColor(hdc, 0x0054B91D);
        let mut art_icon = NativeUi::to_wide("🎵");
        DrawTextW(
            hdc,
            art_icon.as_mut_ptr(),
            -1,
            &mut art_rect,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
        );
    }

    // Liked Song Pill (x: 40..130, y: 365..395)
    let is_liked = snapshot
        .current_track
        .as_ref()
        .map(|t| t.is_liked)
        .unwrap_or(false);
    let like_brush = CreateSolidBrush(0x00262626);
    let mut like_btn = like_rect();
    FillRect(hdc, &like_btn, like_brush);
    DeleteObject(like_brush);

    SelectObject(hdc, font_body);
    SetTextColor(hdc, if is_liked { 0x0054B91D } else { 0x00AAAAAA });
    let mut like_str = NativeUi::to_wide(if is_liked { "♥ Liked" } else { "♡ Like" });
    DrawTextW(
        hdc,
        like_str.as_mut_ptr(),
        -1,
        &mut like_btn,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );

    // Audio Engine Characteristics (x: 40..320, y: 410..440)
    SelectObject(hdc, font_small);
    SetTextColor(hdc, 0x00777777);
    let mut audio_info = NativeUi::to_wide("WASAPI Bit-Perfect • 44.1kHz • AutoEq Direct Form II");
    let mut a_rect = RECT {
        left: 40,
        top: 410,
        right: 320,
        bottom: 440,
    };
    DrawTextW(
        hdc,
        audio_info.as_mut_ptr(),
        -1,
        &mut a_rect,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );

    // Right Column: Track Details
    let rx1 = 330;
    let rx2 = w - 40;

    let (title, artists, album) = if let Some(t) = &snapshot.current_track {
        (t.title.clone(), t.artists.join(", "), t.album.clone())
    } else {
        (
            "No track playing".to_string(),
            "Select a song or playlist to start".to_string(),
            "Stupefinity Library".to_string(),
        )
    };

    // Track Title (Large bold)
    SelectObject(hdc, font_title);
    SetTextColor(hdc, 0x00FFFFFF);
    let mut t_title = NativeUi::to_wide(&title);
    let mut r_title = RECT {
        left: rx1,
        top: 90,
        right: rx2,
        bottom: 135,
    };
    DrawTextW(
        hdc,
        t_title.as_mut_ptr(),
        -1,
        &mut r_title,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );

    // Artist Name (Green bold)
    SelectObject(hdc, font_sub);
    SetTextColor(hdc, 0x0054B91D);
    let mut t_artist = NativeUi::to_wide(&artists);
    let mut r_artist = RECT {
        left: rx1,
        top: 140,
        right: rx2,
        bottom: 170,
    };
    DrawTextW(
        hdc,
        t_artist.as_mut_ptr(),
        -1,
        &mut r_artist,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );

    // Album Name
    SelectObject(hdc, font_body);
    SetTextColor(hdc, 0x00999999);
    let mut t_album = NativeUi::to_wide(&album);
    let mut r_album = RECT {
        left: rx1,
        top: 175,
        right: rx2,
        bottom: 200,
    };
    DrawTextW(
        hdc,
        t_album.as_mut_ptr(),
        -1,
        &mut r_album,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );

    // Separator line
    let sep_brush = CreateSolidBrush(0x00282828);
    let sep_rect = RECT {
        left: rx1,
        top: 215,
        right: rx2,
        bottom: 216,
    };
    FillRect(hdc, &sep_rect, sep_brush);
    DeleteObject(sep_brush);

    // Up Next Queue Header
    SelectObject(hdc, font_sub);
    SetTextColor(hdc, 0x00CCCCCC);
    let mut q_header = NativeUi::to_wide("Up Next in Queue");
    let mut q_rect = RECT {
        left: rx1,
        top: 225,
        right: rx2,
        bottom: 250,
    };
    DrawTextW(
        hdc,
        q_header.as_mut_ptr(),
        -1,
        &mut q_rect,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );

    let queue_start_y = 255;
    let queue_row_h = 28;
    if snapshot.queue_window.is_empty() {
        SelectObject(hdc, font_body);
        SetTextColor(hdc, 0x00777777);
        let mut q_empty =
            NativeUi::to_wide("Queue ready. Press [Ctrl+F] or select 'Search' tab to find songs.");
        let mut qe_rect = RECT {
            left: rx1,
            top: queue_start_y,
            right: rx2,
            bottom: queue_start_y + 30,
        };
        DrawTextW(
            hdc,
            q_empty.as_mut_ptr(),
            -1,
            &mut qe_rect,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        );
    } else {
        SelectObject(hdc, font_body);
        for (i, item) in snapshot.queue_window.iter().take(6).enumerate() {
            let qy1 = queue_start_y + (i as i32) * queue_row_h;
            let qy2 = qy1 + queue_row_h;

            let is_user = item.is_user_queued;
            let dur_sec = item.duration_ms / 1000;
            let dur_str = if dur_sec > 0 {
                format!("{:02}:{:02}", dur_sec / 60, dur_sec % 60)
            } else {
                String::new()
            };

            let prefix = if is_user { "★ " } else { "  " };
            let title_disp = truncate_str(&item.title, 26);
            let artist_disp = truncate_str(&item.artist, 20);

            // Row background highlight for user-queued tracks
            if is_user {
                let uq_brush = CreateSolidBrush(0x00222222);
                let uq_rect = RECT {
                    left: rx1,
                    top: qy1,
                    right: rx2,
                    bottom: qy2 - 2,
                };
                FillRect(hdc, &uq_rect, uq_brush);
                DeleteObject(uq_brush);
            }

            SetTextColor(hdc, if is_user { 0x0054B91D } else { 0x00DDDDDD });
            let label = format!(
                "{}{:<3} {:<28} {:<20}",
                prefix,
                i + 1,
                title_disp,
                artist_disp
            );
            let mut label_w = NativeUi::to_wide(&label);
            let mut label_r = RECT {
                left: rx1 + 5,
                top: qy1,
                right: rx2 - 70,
                bottom: qy2,
            };
            DrawTextW(
                hdc,
                label_w.as_mut_ptr(),
                -1,
                &mut label_r,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE,
            );

            // Duration
            SetTextColor(hdc, 0x00888888);
            let mut dur_w = NativeUi::to_wide(&dur_str);
            let mut dur_r = RECT {
                left: rx2 - 65,
                top: qy1,
                right: rx2 - 25,
                bottom: qy2,
            };
            DrawTextW(
                hdc,
                dur_w.as_mut_ptr(),
                -1,
                &mut dur_r,
                DT_RIGHT | DT_VCENTER | DT_SINGLELINE,
            );

            // If user-queued, draw a remove button '✕'
            if is_user {
                SetTextColor(hdc, 0x000055FF);
                let mut rem_w = NativeUi::to_wide("✕");
                let mut rem_r = RECT {
                    left: rx2 - 20,
                    top: qy1,
                    right: rx2,
                    bottom: qy2,
                };
                DrawTextW(
                    hdc,
                    rem_w.as_mut_ptr(),
                    -1,
                    &mut rem_r,
                    DT_CENTER | DT_VCENTER | DT_SINGLELINE,
                );
            }
        }
    }

    // Keyboard Shortcuts Cheat-sheet at bottom
    SelectObject(hdc, font_small);
    SetTextColor(hdc, 0x00666666);
    let mut shortcuts = NativeUi::to_wide("Space: Play/Pause  •  Ctrl+→/←: Next/Prev  •  →/←: Seek ±5s  •  ↑/↓: Volume  •  Ctrl+F: Search  •  1-5: Tabs  •  Esc: Tray");
    let mut sc_rect = RECT {
        left: 40,
        top: rect.bottom - 130,
        right: w - 40,
        bottom: rect.bottom - 105,
    };
    DrawTextW(
        hdc,
        shortcuts.as_mut_ptr(),
        -1,
        &mut sc_rect,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );
}

unsafe fn render_search_view(
    hdc: windows_sys::Win32::Graphics::Gdi::HDC,
    rect: &RECT,
    ui: &UiState,
    font_sub: windows_sys::Win32::Graphics::Gdi::HFONT,
    font_body: windows_sys::Win32::Graphics::Gdi::HFONT,
) {
    let w = rect.right;

    // Search Input Bar (x: 40..w - 140, y: 75..115)
    let bar_brush = CreateSolidBrush(0x00222222);
    let in_rect = RECT {
        left: 40,
        top: 75,
        right: w - 140,
        bottom: 115,
    };
    FillRect(hdc, &in_rect, bar_brush);
    DeleteObject(bar_brush);

    SelectObject(hdc, font_body);
    SetTextColor(hdc, 0x00FFFFFF);
    let display_query = if ui.search_input.is_empty() {
        "🔍 Type artist, track, or album (e.g. Bohemian Rhapsody)...".to_string()
    } else {
        format!("🔍 {}|", ui.search_input)
    };
    let mut dq_text = NativeUi::to_wide(&display_query);
    let mut dq_rect = RECT {
        left: 55,
        top: 75,
        right: w - 150,
        bottom: 115,
    };
    DrawTextW(
        hdc,
        dq_text.as_mut_ptr(),
        -1,
        &mut dq_rect,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );

    // Search Button (x: w - 130..w - 40, y: 75..115)
    let s_btn_brush = CreateSolidBrush(0x0054B91D);
    let mut s_btn_rect = search_button_rect(w);
    FillRect(hdc, &s_btn_rect, s_btn_brush);
    DeleteObject(s_btn_brush);

    SelectObject(hdc, font_sub);
    SetTextColor(hdc, 0x00FFFFFF);
    let mut s_btn_text = NativeUi::to_wide("Search");
    DrawTextW(
        hdc,
        s_btn_text.as_mut_ptr(),
        -1,
        &mut s_btn_rect,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );

    // Results Header
    SelectObject(hdc, font_sub);
    SetTextColor(hdc, 0x00888888);
    let mut hdr_text = NativeUi::to_wide("#   Title                                             Artist                      Duration");
    let mut hdr_rect = RECT {
        left: 40,
        top: 125,
        right: w - 40,
        bottom: 145,
    };
    DrawTextW(
        hdc,
        hdr_text.as_mut_ptr(),
        -1,
        &mut hdr_rect,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );

    // Results List
    SelectObject(hdc, font_body);
    let start_y = SEARCH_LIST_TOP;
    if ui.search_results.is_empty() {
        SetTextColor(hdc, 0x00666666);
        let mut no_res = NativeUi::to_wide(
            "No search results. Type a query above and click Search or press Enter.",
        );
        let mut nr_rect = RECT {
            left: 40,
            top: start_y,
            right: w - 40,
            bottom: start_y + 40,
        };
        DrawTextW(
            hdc,
            no_res.as_mut_ptr(),
            -1,
            &mut nr_rect,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        );
    } else {
        for (i, t) in ui
            .search_results
            .iter()
            .skip(ui.search_scroll_offset)
            .take(VISIBLE_ROWS)
            .enumerate()
        {
            let actual_idx = i + ui.search_scroll_offset;
            let ry1 = start_y + (i as i32) * SEARCH_ROW_H;
            let ry2 = ry1 + 30;

            if ui.search_selected_idx == actual_idx {
                let sel_brush = CreateSolidBrush(0x00282828);
                let sel_rect = RECT {
                    left: 40,
                    top: ry1,
                    right: w - 40,
                    bottom: ry2,
                };
                FillRect(hdc, &sel_rect, sel_brush);
                DeleteObject(sel_brush);
            }

            SetTextColor(hdc, 0x00FFFFFF);
            let dur_sec = t.duration_ms / 1000;
            let row_str = format!(
                "{:<3} {:<45} {:<25} {:02}:{:02}",
                actual_idx + 1,
                truncate_str(&t.title, 40),
                truncate_str(&t.artists.join(", "), 22),
                dur_sec / 60,
                dur_sec % 60
            );
            let mut row_text = NativeUi::to_wide(&row_str);
            let mut row_rect = RECT {
                left: 50,
                top: ry1,
                right: w - 85,
                bottom: ry2,
            };
            DrawTextW(
                hdc,
                row_text.as_mut_ptr(),
                -1,
                &mut row_rect,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE,
            );

            // Queue button [+Q]
            SetTextColor(hdc, 0x0054B91D);
            let mut q_btn_text = NativeUi::to_wide("+Q");
            let mut q_btn_rect = RECT {
                left: w - 80,
                top: ry1,
                right: w - 45,
                bottom: ry2,
            };
            DrawTextW(
                hdc,
                q_btn_text.as_mut_ptr(),
                -1,
                &mut q_btn_rect,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE,
            );
        }
    }
}

unsafe fn render_library_view(
    hdc: windows_sys::Win32::Graphics::Gdi::HDC,
    rect: &RECT,
    ui: &UiState,
    font_sub: windows_sys::Win32::Graphics::Gdi::HFONT,
    font_body: windows_sys::Win32::Graphics::Gdi::HFONT,
) {
    let w = rect.right;

    // Left Panel: Playlists Header
    SelectObject(hdc, font_sub);
    SetTextColor(hdc, 0x0054B91D);
    let mut p_hdr = NativeUi::to_wide("Your Playlists");
    let mut p_rect = RECT {
        left: 40,
        top: 80,
        right: 300,
        bottom: 105,
    };
    DrawTextW(
        hdc,
        p_hdr.as_mut_ptr(),
        -1,
        &mut p_rect,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );

    SelectObject(hdc, font_body);
    let start_y = LIBRARY_LIST_TOP;
    for (i, p) in ui
        .playlists
        .iter()
        .enumerate()
        .skip(ui.playlist_list_offset)
        .take(VISIBLE_ROWS)
    {
        let py1 = start_y + ((i - ui.playlist_list_offset) as i32) * PLAYLIST_ROW_H;
        let py2 = py1 + 30;

        let is_sel = ui.selected_playlist_idx == Some(i);
        if is_sel {
            let sel_brush = CreateSolidBrush(0x00282828);
            let sel_rect = RECT {
                left: 40,
                top: py1,
                right: 300,
                bottom: py2,
            };
            FillRect(hdc, &sel_rect, sel_brush);
            DeleteObject(sel_brush);
        }

        SetTextColor(hdc, if is_sel { 0x0054B91D } else { 0x00CCCCCC });
        let row_str = format!(
            "📁 {} ({} tracks)",
            truncate_str(&p.name, 22),
            p.track_count
        );
        let mut row_text = NativeUi::to_wide(&row_str);
        let mut row_rect = RECT {
            left: 50,
            top: py1,
            right: 300,
            bottom: py2,
        };
        DrawTextW(
            hdc,
            row_text.as_mut_ptr(),
            -1,
            &mut row_rect,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        );
    }

    // Right Panel: Playlist Tracks
    SelectObject(hdc, font_sub);
    SetTextColor(hdc, 0x00CCCCCC);
    let right_hdr = if let Some(idx) = ui.selected_playlist_idx {
        if idx < ui.playlists.len() {
            format!("Tracks in {}", ui.playlists[idx].name)
        } else {
            "Tracks".to_string()
        }
    } else {
        "Select a playlist on the left to view tracks".to_string()
    };
    let mut rh_text = NativeUi::to_wide(&right_hdr);
    let mut rh_rect = RECT {
        left: PLAYLIST_TRACKS_LEFT,
        top: 80,
        right: w - 40,
        bottom: 105,
    };
    DrawTextW(
        hdc,
        rh_text.as_mut_ptr(),
        -1,
        &mut rh_rect,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );

    SelectObject(hdc, font_body);
    for (i, t) in ui
        .playlist_tracks
        .iter()
        .skip(ui.playlist_scroll_offset)
        .take(VISIBLE_ROWS)
        .enumerate()
    {
        let actual_idx = i + ui.playlist_scroll_offset;
        let ty1 = start_y + (i as i32) * PLAYLIST_TRACK_ROW_H;
        let ty2 = ty1 + 28;

        SetTextColor(hdc, 0x00FFFFFF);
        let dur_sec = t.duration_ms / 1000;
        let row_str = format!(
            "{:<3} {:<40} {:<20} {:02}:{:02}",
            actual_idx + 1,
            truncate_str(&t.title, 35),
            truncate_str(&t.artists.join(", "), 18),
            dur_sec / 60,
            dur_sec % 60
        );
        let mut row_text = NativeUi::to_wide(&row_str);
        let mut row_rect = RECT {
            left: PLAYLIST_TRACKS_LEFT,
            top: ty1,
            right: w - 85,
            bottom: ty2,
        };
        DrawTextW(
            hdc,
            row_text.as_mut_ptr(),
            -1,
            &mut row_rect,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        );

        // Queue button [+Q]
        SetTextColor(hdc, 0x0054B91D);
        let mut q_btn_text = NativeUi::to_wide("+Q");
        let mut q_btn_rect = RECT {
            left: w - 80,
            top: ty1,
            right: w - 45,
            bottom: ty2,
        };
        DrawTextW(
            hdc,
            q_btn_text.as_mut_ptr(),
            -1,
            &mut q_btn_rect,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
        );
    }
}

unsafe fn render_lyrics_view(
    hdc: windows_sys::Win32::Graphics::Gdi::HDC,
    rect: &RECT,
    ui: &UiState,
    snapshot: &Snapshot,
    font_title: windows_sys::Win32::Graphics::Gdi::HFONT,
    font_sub: windows_sys::Win32::Graphics::Gdi::HFONT,
    font_body: windows_sys::Win32::Graphics::Gdi::HFONT,
) {
    let w = rect.right;
    let cx = w / 2;

    SelectObject(hdc, font_sub);
    SetTextColor(hdc, 0x0054B91D);
    let header_str = if let Some(t) = &snapshot.current_track {
        format!("Synced Lyrics: {} - {}", t.title, t.artists.join(", "))
    } else {
        "Synced Lyrics (LRCLIB)".to_string()
    };
    let mut h_text = NativeUi::to_wide(&header_str);
    let mut h_rect = RECT {
        left: 40,
        top: 75,
        right: w - 40,
        bottom: 100,
    };
    DrawTextW(
        hdc,
        h_text.as_mut_ptr(),
        -1,
        &mut h_rect,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );

    if let Some(lyrics) = &ui.lyrics {
        let cur_ms = snapshot.position_ms;
        // Find current matching line
        let active_idx = lyrics
            .synced_lyrics
            .iter()
            .position(|l| l.time_ms > cur_ms)
            .unwrap_or(lyrics.synced_lyrics.len())
            .saturating_sub(1);

        let center_y = 260;
        let line_height = 36;

        for (offset, line_idx) in ((active_idx.saturating_sub(4))..=(active_idx + 4)).enumerate() {
            if line_idx < lyrics.synced_lyrics.len() {
                let l = &lyrics.synced_lyrics[line_idx];
                let is_active = line_idx == active_idx;
                let y = (center_y - 4 * line_height) + (offset as i32) * line_height;

                if is_active {
                    SelectObject(hdc, font_title);
                    SetTextColor(hdc, 0x0054B91D); // Vibrant green
                    let line_str = format!("▶  {}", l.text);
                    let mut lt = NativeUi::to_wide(&line_str);
                    let mut lr = RECT {
                        left: 40,
                        top: y,
                        right: w - 40,
                        bottom: y + line_height,
                    };
                    DrawTextW(
                        hdc,
                        lt.as_mut_ptr(),
                        -1,
                        &mut lr,
                        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
                    );
                } else {
                    SelectObject(hdc, font_body);
                    SetTextColor(
                        hdc,
                        if line_idx < active_idx {
                            0x00555555
                        } else {
                            0x00999999
                        },
                    );
                    let mut lt = NativeUi::to_wide(&l.text);
                    let mut lr = RECT {
                        left: 40,
                        top: y,
                        right: w - 40,
                        bottom: y + line_height,
                    };
                    DrawTextW(
                        hdc,
                        lt.as_mut_ptr(),
                        -1,
                        &mut lr,
                        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
                    );
                }
            }
        }
    } else {
        SelectObject(hdc, font_body);
        SetTextColor(hdc, 0x00777777);
        let mut msg = NativeUi::to_wide("No synced lyrics available for the current track.");
        let mut mr = RECT {
            left: cx - 300,
            top: 240,
            right: cx + 300,
            bottom: 280,
        };
        DrawTextW(
            hdc,
            msg.as_mut_ptr(),
            -1,
            &mut mr,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
        );
    }
}

unsafe fn render_eq_view(
    hdc: windows_sys::Win32::Graphics::Gdi::HDC,
    ui: &UiState,
    snapshot: &Snapshot,
    font_sub: windows_sys::Win32::Graphics::Gdi::HFONT,
    font_body: windows_sys::Win32::Graphics::Gdi::HFONT,
) {
    // Master Toggle Button (x: 50..220, y: 80..120)
    let is_on = snapshot.eq_enabled;
    let eq_btn_brush = CreateSolidBrush(if is_on { 0x0054B91D } else { 0x00262626 });
    let mut eq_btn_rect = eq_toggle_rect();
    FillRect(hdc, &eq_btn_rect, eq_btn_brush);
    DeleteObject(eq_btn_brush);

    SelectObject(hdc, font_sub);
    SetTextColor(hdc, 0x00FFFFFF);
    let mut eq_toggle_text = NativeUi::to_wide(if is_on { "EQ: ENABLED" } else { "EQ: DISABLED" });
    DrawTextW(
        hdc,
        eq_toggle_text.as_mut_ptr(),
        -1,
        &mut eq_btn_rect,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );

    // Preset Buttons (y: 140..175)
    for (i, name) in EQ_PRESETS.iter().enumerate() {
        let is_sel = ui.eq_preset_idx == i;
        let p_brush = CreateSolidBrush(if is_sel { 0x0054B91D } else { 0x00222222 });
        let mut p_rect = eq_preset_rect(i);
        FillRect(hdc, &p_rect, p_brush);
        DeleteObject(p_brush);

        SelectObject(hdc, font_body);
        SetTextColor(hdc, 0x00FFFFFF);
        let mut pt = NativeUi::to_wide(name);
        DrawTextW(
            hdc,
            pt.as_mut_ptr(),
            -1,
            &mut p_rect,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
        );
    }

    // Graphical EQ Visualizer (y: 210..350)
    SelectObject(hdc, font_sub);
    SetTextColor(hdc, 0x00CCCCCC);
    let mut g_hdr = NativeUi::to_wide("AutoEq Biquad Frequency Curve (Direct Form II Transposed)");
    let mut gh_rect = RECT {
        left: 50,
        top: 195,
        right: 600,
        bottom: 220,
    };
    DrawTextW(
        hdc,
        g_hdr.as_mut_ptr(),
        -1,
        &mut gh_rect,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );

    let bands = [
        "31Hz", "62Hz", "125Hz", "250Hz", "500Hz", "1kHz", "2kHz", "4kHz", "8kHz", "16kHz",
    ];
    let bar_base_y = 330;
    for (i, b) in bands.iter().enumerate() {
        let bx = 60 + (i as i32) * 75;
        let bar_h = match ui.eq_preset_idx {
            1 => [45, 55, 50, 40, 35, 38, 42, 45, 40, 35][i], // Harman
            2 => [70, 65, 50, 35, 30, 30, 30, 30, 30, 30][i], // Bass Boost
            3 => [25, 25, 30, 30, 35, 40, 50, 65, 70, 75][i], // Treble Boost
            _ => 35,                                          // Flat
        };

        let bar_brush = CreateSolidBrush(0x0054B91D);
        let b_rect = RECT {
            left: bx,
            top: bar_base_y - bar_h,
            right: bx + 35,
            bottom: bar_base_y,
        };
        FillRect(hdc, &b_rect, bar_brush);
        DeleteObject(bar_brush);

        SelectObject(hdc, font_body);
        SetTextColor(hdc, 0x00888888);
        let mut bt = NativeUi::to_wide(b);
        let mut b_label_rect = RECT {
            left: bx - 10,
            top: bar_base_y + 5,
            right: bx + 45,
            bottom: bar_base_y + 25,
        };
        DrawTextW(
            hdc,
            bt.as_mut_ptr(),
            -1,
            &mut b_label_rect,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
        );
    }
}

unsafe fn render_bottom_bar(
    hdc: windows_sys::Win32::Graphics::Gdi::HDC,
    rect: &RECT,
    snapshot: &Snapshot,
    font_body: windows_sys::Win32::Graphics::Gdi::HFONT,
    font_sub: windows_sys::Win32::Graphics::Gdi::HFONT,
    font_small: windows_sys::Win32::Graphics::Gdi::HFONT,
) {
    let w = rect.right;
    let h = rect.bottom;

    // Elevated Background (#181818)
    let bar_brush = CreateSolidBrush(0x00181818);
    let bar_rect = RECT {
        left: 0,
        top: h - BOTTOM_BAR_H,
        right: w,
        bottom: h,
    };
    FillRect(hdc, &bar_rect, bar_brush);
    DeleteObject(bar_brush);

    // Top Border
    let border_brush = CreateSolidBrush(0x00282828);
    let border_rect = RECT {
        left: 0,
        top: h - BOTTOM_BAR_H,
        right: w,
        bottom: h - BOTTOM_BAR_H + 1,
    };
    FillRect(hdc, &border_rect, border_brush);
    DeleteObject(border_brush);

    // 1. Scrubber (Seek Bar)
    let cur_sec = snapshot.position_ms / 1000;
    let tot_sec = snapshot.duration_ms / 1000;

    // Left timestamp
    SelectObject(hdc, font_small);
    SetTextColor(hdc, 0x00AAAAAA);
    let t_left_str = format!("{:02}:{:02}", cur_sec / 60, cur_sec % 60);
    let mut tl_text = NativeUi::to_wide(&t_left_str);
    let mut tl_rect = RECT {
        left: 25,
        top: h - 85,
        right: 80,
        bottom: h - 68,
    };
    DrawTextW(
        hdc,
        tl_text.as_mut_ptr(),
        -1,
        &mut tl_rect,
        DT_RIGHT | DT_VCENTER | DT_SINGLELINE,
    );

    // Right timestamp
    let t_right_str = format!("{:02}:{:02}", tot_sec / 60, tot_sec % 60);
    let mut tr_text = NativeUi::to_wide(&t_right_str);
    let mut tr_rect = RECT {
        left: w - 80,
        top: h - 85,
        right: w - 25,
        bottom: h - 68,
    };
    DrawTextW(
        hdc,
        tr_text.as_mut_ptr(),
        -1,
        &mut tr_rect,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );

    // Track Bar
    let sx1 = 90;
    let sx2 = w - 90;
    let bar_y = h - 77;
    let track_bg = CreateSolidBrush(0x003A3A3A);
    let sb_rect = RECT {
        left: sx1,
        top: bar_y,
        right: sx2,
        bottom: bar_y + 5,
    };
    FillRect(hdc, &sb_rect, track_bg);
    DeleteObject(track_bg);

    // Filled progress
    let ratio = (snapshot.position_ms as f32 / snapshot.duration_ms.max(1) as f32).clamp(0.0, 1.0);
    let progress_x = sx1 + ((sx2 - sx1) as f32 * ratio) as i32;

    let fill_brush = CreateSolidBrush(0x0054B91D);
    let fill_rect = RECT {
        left: sx1,
        top: bar_y,
        right: progress_x,
        bottom: bar_y + 5,
    };
    FillRect(hdc, &fill_rect, fill_brush);
    DeleteObject(fill_brush);

    // Scrubber thumb circle
    let thumb_brush = CreateSolidBrush(0x00FFFFFF);
    let thumb_rect = RECT {
        left: progress_x - 4,
        top: bar_y - 2,
        right: progress_x + 5,
        bottom: bar_y + 7,
    };
    FillRect(hdc, &thumb_rect, thumb_brush);
    DeleteObject(thumb_brush);

    // 2. Left Mini-Info (Track & Artist)
    let (mini_title, mini_artist) = if let Some(t) = &snapshot.current_track {
        (t.title.clone(), t.artists.join(", "))
    } else {
        ("No track playing".to_string(), "Stupefinity".to_string())
    };

    SelectObject(hdc, font_body);
    SetTextColor(hdc, 0x00FFFFFF);
    let mut mt_text = NativeUi::to_wide(&truncate_str(&mini_title, 26));
    let mut mt_rect = RECT {
        left: 25,
        top: h - 55,
        right: 240,
        bottom: h - 35,
    };
    DrawTextW(
        hdc,
        mt_text.as_mut_ptr(),
        -1,
        &mut mt_rect,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );

    SelectObject(hdc, font_small);
    SetTextColor(hdc, 0x00888888);
    let mut ma_text = NativeUi::to_wide(&truncate_str(&mini_artist, 28));
    let mut ma_rect = RECT {
        left: 25,
        top: h - 35,
        right: 240,
        bottom: h - 15,
    };
    DrawTextW(
        hdc,
        ma_text.as_mut_ptr(),
        -1,
        &mut ma_rect,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );

    // 3. Center Transport Buttons
    let cx = w / 2;

    // Prev [⏮]
    SelectObject(hdc, font_sub);
    SetTextColor(hdc, 0x00CCCCCC);
    let mut prev_text = NativeUi::to_wide("⏮");
    let mut prev_rect = RECT {
        left: cx - 110,
        top: h - 55,
        right: cx - 65,
        bottom: h - 15,
    };
    DrawTextW(
        hdc,
        prev_text.as_mut_ptr(),
        -1,
        &mut prev_rect,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );

    // Play / Pause Circle Button
    let play_label = if snapshot.playback_state == PlaybackState::Playing {
        "⏸"
    } else {
        "▶"
    };
    let play_bg = CreateSolidBrush(0x0054B91D);
    let mut play_btn_rect = RECT {
        left: cx - 55,
        top: h - 55,
        right: cx + 10,
        bottom: h - 15,
    };
    FillRect(hdc, &play_btn_rect, play_bg);
    DeleteObject(play_bg);

    SelectObject(hdc, font_sub);
    SetTextColor(hdc, 0x00FFFFFF);
    let mut play_text = NativeUi::to_wide(play_label);
    DrawTextW(
        hdc,
        play_text.as_mut_ptr(),
        -1,
        &mut play_btn_rect,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );

    // Next [⏭]
    SetTextColor(hdc, 0x00CCCCCC);
    let mut next_text = NativeUi::to_wide("⏭");
    let mut next_rect = RECT {
        left: cx + 20,
        top: h - 55,
        right: cx + 65,
        bottom: h - 15,
    };
    DrawTextW(
        hdc,
        next_text.as_mut_ptr(),
        -1,
        &mut next_rect,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );

    // Shuffle [🔀]
    SelectObject(hdc, font_body);
    SetTextColor(
        hdc,
        if snapshot.shuffle {
            0x0054B91D
        } else {
            0x00777777
        },
    );
    let mut shuf_text = NativeUi::to_wide("🔀");
    let mut shuf_rect = RECT {
        left: cx + 75,
        top: h - 55,
        right: cx + 125,
        bottom: h - 15,
    };
    DrawTextW(
        hdc,
        shuf_text.as_mut_ptr(),
        -1,
        &mut shuf_rect,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );

    // Repeat [🔁]
    let rep_str = match snapshot.repeat {
        RepeatMode::Off => "🔁 Off",
        RepeatMode::Context => "🔁 All",
        RepeatMode::Track => "🔂 1",
    };
    SetTextColor(
        hdc,
        if snapshot.repeat != RepeatMode::Off {
            0x0054B91D
        } else {
            0x00777777
        },
    );
    let mut rep_text = NativeUi::to_wide(rep_str);
    let mut rep_rect = RECT {
        left: cx + 135,
        top: h - 55,
        right: cx + 195,
        bottom: h - 15,
    };
    DrawTextW(
        hdc,
        rep_text.as_mut_ptr(),
        -1,
        &mut rep_rect,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );

    // 4. Right Volume Slider
    let vx1 = w - 190;
    let vx2 = w - 90;
    let vy = h - 38;

    SelectObject(hdc, font_small);
    SetTextColor(hdc, 0x00888888);
    let mut v_icon = NativeUi::to_wide("🔉");
    let mut vi_rect = RECT {
        left: vx1 - 25,
        top: vy - 8,
        right: vx1 - 5,
        bottom: vy + 12,
    };
    DrawTextW(
        hdc,
        v_icon.as_mut_ptr(),
        -1,
        &mut vi_rect,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );

    // Volume Track
    let v_bg = CreateSolidBrush(0x003A3A3A);
    let v_track = RECT {
        left: vx1,
        top: vy,
        right: vx2,
        bottom: vy + 4,
    };
    FillRect(hdc, &v_track, v_bg);
    DeleteObject(v_bg);

    // Filled Volume
    let v_ratio = (snapshot.volume as f32 / 65535.0).clamp(0.0, 1.0);
    let v_fill_x = vx1 + ((vx2 - vx1) as f32 * v_ratio) as i32;
    let v_fill_brush = CreateSolidBrush(0x0054B91D);
    let v_fill_rect = RECT {
        left: vx1,
        top: vy,
        right: v_fill_x,
        bottom: vy + 4,
    };
    FillRect(hdc, &v_fill_rect, v_fill_brush);
    DeleteObject(v_fill_brush);

    // Volume Percentage Text
    let vol_pct = (v_ratio * 100.0).round() as u32;
    let v_pct_str = format!("{}%", vol_pct);
    let mut vp_text = NativeUi::to_wide(&v_pct_str);
    let mut vp_rect = RECT {
        left: vx2 + 10,
        top: vy - 8,
        right: w - 25,
        bottom: vy + 12,
    };
    DrawTextW(
        hdc,
        vp_text.as_mut_ptr(),
        -1,
        &mut vp_rect,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );
}

// -------------------------------------------------------------------------------------------------
// Helpers
// -------------------------------------------------------------------------------------------------

unsafe fn create_gdi_font(size: i32, bold: bool) -> windows_sys::Win32::Graphics::Gdi::HFONT {
    let face = NativeUi::to_wide("Segoe UI");
    CreateFontW(
        -size,
        0,
        0,
        0,
        if bold {
            FW_BOLD as i32
        } else {
            FW_NORMAL as i32
        },
        0,
        0,
        0,
        DEFAULT_CHARSET as u32,
        OUT_DEFAULT_PRECIS as u32,
        CLIP_DEFAULT_PRECIS as u32,
        DEFAULT_QUALITY as u32,
        DEFAULT_PITCH as u32 | FF_DONTCARE as u32,
        face.as_ptr(),
    )
}

fn truncate_str(s: &str, max_len: usize) -> String {
    if s.chars().count() > max_len {
        let mut truncated: String = s.chars().take(max_len.saturating_sub(3)).collect();
        truncated.push_str("...");
        truncated
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_run_asks_for_terms_then_client_id_then_login() {
        let mut s = Snapshot {
            needs_client_id: true,
            ..Default::default()
        };
        assert_eq!(onboarding(&s), Onboarding::Terms);
        s.terms_accepted = true;
        assert_eq!(onboarding(&s), Onboarding::ClientId);
        s.needs_client_id = false;
        assert_eq!(onboarding(&s), Onboarding::Login);
    }
}
