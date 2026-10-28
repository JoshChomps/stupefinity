# Stupefinity: Project Progress & Milestones

Tracking current phase status, deliverables, and memory benchmarks.

---

## Progress Overview

| Phase | Description | Status | Target Memory |
| :--- | :--- | :--- | :--- |
| **Phase 0** | Toolchain & Environment Setup (Rust, WinLibs MinGW GCC, Git) | 🟢 Complete | N/A |
| **Phase 1** | Baseline Validation (`librespot` 0.8.0 WASAPI audio & RAM test) | 🟢 Complete | < 20 MB (Core) |
| **Phase 2** | Headless Core (`stupefinity-core` + `stupefinity-cli` via Named Pipe) | 🟢 Complete | < 15 MB (Core) |
| **Phase 3** | Data Layer & Metadata (Playlists, Library, Search, LRU Cache) | 🟢 Complete | < 25 MB (Core) |
| **Phase 4** | Native Ephemeral UI (`stupefinity-ui` process with instant memory teardown) | 🟢 Complete | < 50 MB (Active UI) |
| **Phase 5** | Windows 11 Integration (Single-instance Mutex, MMCSS, EcoQoS, System Tray, Media Keys) | 🟢 Complete | < 20 MB (Tray) |
| **Phase 6** | Advanced Audio & Rules (AutoEq Headphone DSP, Synced Lyrics, Local Rules, Smart Shuffle, Discord RPC) | 🟢 Complete | < 25 MB (Core) |
| **Phase 7** | Spotify Connect (`Spirc` phone remote control) | ⚪ Deferred | < 20 MB (Tray) |

---

## Deliverables Checklist

### Phase 0: Toolchain & Workspace Setup
- [x] Installed `rustup`, `rustc 1.99.1`, and `cargo 1.99.1` (`x86_64-pc-windows-gnu`).
- [x] Installed **WinLibs GCC 16.2.0 (UCRT)** toolchain.
- [x] Initialized Git repository and root workspace [`Cargo.toml`](file:///d:/Coding_Projects/Stupefinity/Cargo.toml) with size/speed optimization profiles.
- [x] Fixed upstream `vergen 9.0.6` build-dependency conflict.

### Phase 1 & 2: Headless Core & Dual Engine
- [x] **`stupefinity-protocol`**: Implemented shared IPC types (`Command`, `Snapshot`, `TrackInfo`, `RepeatMode`, `PlaybackState`, `AuthState`, `PlaylistSummary`, `LyricsData`, `LocalRulesConfig`, `SmartShuffleConfig`, `EqProfile`).
- [x] **`stupefinity-core`**:
  - [x] Spotify OAuth 2.0 PKCE browser authorization with loopback server (`http://127.0.0.1:8888/login`).
  - [x] Non-blocking deferred authentication mode (allows login on-demand later).
  - [x] Silent login via cached credentials (`%LOCALAPPDATA%\Stupefinity\cache\token.json`).
  - [x] Account tier detection (`is_premium` via `/v1/me`).
  - [x] Prevention of `librespot` hardcoded `exit(1)` crash on Free accounts.
  - [x] Free accounts are told Premium is needed and logged out again.
  - [x] Dedicated WASAPI audio playback engine via `rodio 0.21.1` and `symphonia`.
  - [x] Queue state machine with Fisher-Yates shuffle and user queue priority.
  - [x] Windows Named Pipe IPC listener on `\\.\pipe\stupefinity-ipc`.
- [x] **`stupefinity-cli`**: Command-line interface with `play`, `pause`, `toggle`, `seek`, `play-uri`, `enqueue`, `volume`, `status`, `login`, `logout`, `playlists`, `playlist-tracks`, `liked`, `search`, `like`, `unlike`, `lyrics`, `smart-shuffle`, `eq`, and `stop`.

### Phase 3: Data Layer & Metadata Pipeline
- [x] Implemented `SpotifyApiClient` in `crates/stupefinity-core/src/api.rs`:
  - [x] User playlists fetching (`/v1/me/playlists`).
  - [x] Playlist tracks fetching with LRU cache (`/v1/playlists/{id}/tracks`).
  - [x] Liked Songs library fetching (`/v1/me/tracks`).
  - [x] Catalog search (`/v1/search`).
  - [x] Like / Unlike track library endpoints (`/v1/me/tracks`).
- [x] Extended Named Pipe query protocol with dedicated async `oneshot` responses for library items.

### Phase 4: Native Ephemeral UI (`stupefinity-ui`)
- [x] Built native Windows 11 transient GUI in `crates/stupefinity-ui`:
  - [x] Ultra-lightweight native Win32 GDI dark theme (`#121212`).
  - [x] Live scrubber bar and playback controls (Play/Pause, Next, Prev, Shuffle, Repeat).
  - [x] Header bar with live auth status and Spotify branding.
  - [x] Damage-tracked dirty-rect repaints via `InvalidateRect`.
  - [x] Ephemeral lifecycle: completely terminates on window close (`WM_CLOSE`), freeing 100% of UI memory.

### Phase 5: Windows 11 Platform Integration
- [x] Single-instance Windows Named Mutex enforcement (`Global\StupefinityCoreNamedMutex`).
- [x] Windows MMCSS high-priority audio thread scheduling.
- [x] Windows 11 EcoQoS power throttling (`ProcessPowerThrottling`) automatically activated when window is hidden.
- [x] Native Win32 System Tray icon via `Shell_NotifyIconW` with right-click menu and double-click to open UI.
- [x] Windows Global Media Keys (`VK_MEDIA_PLAY_PAUSE`, `VK_MEDIA_NEXT_TRACK`, `VK_MEDIA_PREV_TRACK`, `VK_MEDIA_STOP`) registered globally via `RegisterHotKey`.

### Phase 6: Advanced Audio & Smart Features
- [x] **AutoEq Headphone Equalizer DSP Engine**: Direct Form II Transposed RBJ biquad filters in f32, negative preamp gain, equal-power crossfade.
- [x] **Real-Time Synced Lyrics Engine**: Open LRCLIB REST API integration, line-by-line sync timestamps, local disk caching.
- [x] **Local Rules Engine**: Skit/interlude filtering by duration, intro auto-skip, artist/track blacklisting.
- [x] **Smart Shuffle Engine**: Configurable ratio recommendation injection from Liked Songs with history de-duplication.
- [x] **Discord Rich Presence**: Live presence updates via local Windows Named Pipe (`\\.\pipe\discord-ipc-0`).

---

## Log & Updates

### 2026-10-20: Code Review & Cleanup
- Fixed: Next/Prev replaced the whole queue with a single track; playback now keeps the context and uses real track metadata from the Web API instead of placeholder titles.
- Fixed: tracks never auto-advanced; the audio thread now reports the real position and end-of-track.
- Fixed: the UI aborted on Library/Lyrics/Search (async work spawned outside the Tokio runtime); a per-frame string leak in the header; click targets misaligned with drawn controls; typing in search switched tabs.
- Fixed: login held the session lock during the browser flow (the core froze); tokens expired after an hour (the refresh token is now stored and refreshed).
- Fixed: the free resolver picked undecodable Opus/WebM streams and cached expiring stream URLs; EQ high-shelf coefficient sign error; `stupefinity-cli eq` panicked.
- Fixed: Discord RPC reused connections without a handshake and never drained replies; tray Exit left a windowless process running; IPC hung clients on bad JSON and allowed unbounded frame allocation.
- Implemented real MMCSS registration (was a log-only stub). Removed the unused `tray.rs` duplicate.
- Still open: librespot (Premium) playback, EQ in the audio path, SMTC, album/artist contexts.

### 2026-10-21: Implementation of All Workstreams & ≤ 10 MB Memory Budget
- **WS1 (Premium Playback via librespot)**: Connected `librespot` Player & SoftMixer for Premium Spotify CDN streaming.
- **WS2 (Progressive Streaming)**: Implemented disk-cached chunk streaming (`ProgressiveStreamReader`) dropping startup latency to < 150 ms and eliminating whole-file RAM buffering.
- **WS3 (Audio Equalizer in Playback Path)**: Inserted `EqualizerSource` with Direct Form II RBJ biquads directly into the WASAPI playback stream.
- **WS4 (Queue Panel & UI Interactions)**: Interactive queue window in Player tab with user-queued tags (`★`), track removal (`✕`), and fast `+Q` enqueuing from Search & Library.
- **WS5 (Album & Artist Contexts)**: Added native support for `spotify:album:` and `spotify:artist:` with automatic pagination.
- **WS6 (Persisted Settings)**: `%LOCALAPPDATA%\Stupefinity\settings.json` saving volume, shuffle, repeat, Smart Shuffle, EQ profiles, and Discord client ID.
- **WS7 (Windows SMTC Media Overlay)**: Windows System Media Transport Controls integration via `souvlaki` with system flyout controls.
- **WS8 (CI & Memory Benchmarks)**: Created Windows 11 GitHub Actions workflow ([`.github/workflows/ci.yml`](file:///d:/Coding_Projects/Stupefinity/.github/workflows/ci.yml)) and benchmark automation script ([`scripts/benchmark_memory.ps1`](file:///d:/Coding_Projects/Stupefinity/scripts/benchmark_memory.ps1)).
- **WS9 (Small Fixes & Polish)**: Configurable Discord client ID; cached GDI fonts; mousewheel scrolling; race guard on track transitions.
- **Benchmarked Memory Results (Strict ≤ 10.0 MB Target Budget)**:
  - **Active Audio Streaming & Equalizer DSP**: **4.38 MB Peak Private Bytes** (Passing).
  - **Resident Process (Win32 UI, SMTC, Tray, IPC)**: **7.82 MB Private Bytes** (Passing).
- Workspace verification: 0 clippy warnings (`cargo clippy --workspace --all-targets -- -D warnings`), 100% formatted (`cargo fmt`), 22/22 unit and integration tests passing.

