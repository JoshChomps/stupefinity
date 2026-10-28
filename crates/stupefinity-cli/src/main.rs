use clap::{Parser, Subcommand};
use stupefinity_protocol::{Command, CoreEvent, DEFAULT_PIPE_NAME};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};

#[derive(Parser, Debug)]
#[command(
    name = "stupefinity",
    about = "Command-line controller for the Stupefinity music player"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Start playback
    Play,
    /// Pause playback
    Pause,
    /// Toggle play / pause
    Toggle,
    /// Skip to next track
    Next,
    /// Skip to previous track
    Prev,
    /// Seek to position in seconds
    Seek { seconds: u32 },
    /// Load and play a Spotify URI (track, album, playlist)
    PlayUri {
        uri: String,
        #[arg(default_value_t = 0)]
        start: usize,
    },
    /// Add a track to the upcoming queue
    Enqueue { uri: String },
    /// Set volume (0 - 65535)
    Volume { level: u16 },
    /// Log in to Spotify via browser PKCE
    Login,
    /// Clear credentials and log out
    Logout,
    /// List user playlists
    Playlists,
    /// List tracks in a playlist
    PlaylistTracks { playlist_id: String },
    /// List Liked Songs
    Liked {
        #[arg(default_value_t = 0)]
        offset: u32,
        #[arg(default_value_t = 20)]
        limit: u32,
    },
    /// Search Spotify catalog
    Search { query: String },
    /// Save track to Liked Songs
    Like { uri: String },
    /// Remove track from Liked Songs
    Unlike { uri: String },
    /// Fetch synced lyrics for currently playing track
    Lyrics,
    /// Toggle Smart Shuffle
    SmartShuffle {
        #[arg(default_value_t = 4)]
        ratio: u32,
    },
    /// Enable or disable the Equalizer (`eq true` / `eq false`)
    Eq {
        #[arg(default_value_t = true, action = clap::ArgAction::Set)]
        enable: bool,
    },
    /// Print current player snapshot
    Status,
    /// Gracefully shutdown the core daemon
    Stop,
}

/// Opens the daemon pipe, retrying briefly while all pipe instances are busy.
async fn connect() -> std::io::Result<NamedPipeClient> {
    const ERROR_PIPE_BUSY: i32 = 231;
    let mut attempts = 0;
    loop {
        match ClientOptions::new().open(DEFAULT_PIPE_NAME) {
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && attempts < 20 => {
                attempts += 1;
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            result => return result,
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let cmd = match cli.command {
        Commands::Play => Command::Play,
        Commands::Pause => Command::Pause,
        Commands::Toggle => Command::PlayPauseToggle,
        Commands::Next => Command::Next,
        Commands::Prev => Command::Prev,
        Commands::Seek { seconds } => Command::Seek(seconds * 1000),
        Commands::PlayUri { uri, start } => Command::LoadContext {
            uri,
            start_index: start,
        },
        Commands::Enqueue { uri } => Command::Enqueue(uri),
        Commands::Volume { level } => Command::SetVolume(level),
        Commands::Login => Command::Login,
        Commands::Logout => Command::Logout,
        Commands::Playlists => Command::GetPlaylists,
        Commands::PlaylistTracks { playlist_id } => Command::GetPlaylistTracks { playlist_id },
        Commands::Liked { offset, limit } => Command::GetLikedSongs { offset, limit },
        Commands::Search { query } => Command::Search { query },
        Commands::Like { uri } => Command::LikeTrack { uri },
        Commands::Unlike { uri } => Command::UnlikeTrack { uri },
        Commands::Lyrics => Command::GetLyrics,
        Commands::SmartShuffle { ratio } => {
            Command::SetSmartShuffle(stupefinity_protocol::SmartShuffleConfig {
                enabled: true,
                ratio,
                local_only: true,
            })
        }
        Commands::Eq { enable } => Command::ToggleEq(enable),
        Commands::Status => Command::GetSnapshot,
        Commands::Stop => Command::Shutdown,
    };

    println!(
        "Connecting to Stupefinity daemon at {}...",
        DEFAULT_PIPE_NAME
    );
    let mut client = connect().await.unwrap_or_else(|e| {
        eprintln!("Error connecting to Stupefinity core daemon: {e}");
        eprintln!("Make sure stupefinity.exe is running.");
        std::process::exit(1);
    });

    let serialized = serde_json::to_vec(&cmd)?;
    let len = (serialized.len() as u32).to_le_bytes();
    client.write_all(&len).await?;
    client.write_all(&serialized).await?;
    client.flush().await?;

    // Read response
    let mut resp_len_buf = [0u8; 4];
    if client.read_exact(&mut resp_len_buf).await.is_ok() {
        let resp_len = u32::from_le_bytes(resp_len_buf) as usize;
        let mut resp_buf = vec![0u8; resp_len];
        client.read_exact(&mut resp_buf).await?;

        if let Ok(event) = serde_json::from_slice::<CoreEvent>(&resp_buf) {
            match event {
                CoreEvent::State(snap) => {
                    println!("\n=== Stupefinity Player Status ===");
                    println!("Auth:          {:?}", snap.auth_state);
                    println!("Playback:      {:?}", snap.playback_state);
                    if let Some(t) = snap.current_track {
                        println!("Track:         {} by {}", t.title, t.artists.join(", "));
                        println!("Album:         {}", t.album);
                        println!(
                            "Progress:      {}s / {}s",
                            snap.position_ms / 1000,
                            snap.duration_ms / 1000
                        );
                    } else {
                        println!("Track:         (None)");
                    }
                    println!("Volume:        {}/65535", snap.volume);
                    println!("Shuffle:       {}", snap.shuffle);
                    println!("Smart Shuffle: {}", snap.smart_shuffle_enabled);
                    println!("EQ Enabled:    {}", snap.eq_enabled);
                    println!("Repeat:        {:?}", snap.repeat);
                }
                CoreEvent::Playlists(playlists) => {
                    println!("\n=== Your Playlists ({}) ===", playlists.len());
                    for (i, p) in playlists.iter().enumerate() {
                        println!(
                            "{}. {} ({} tracks) - ID: {}",
                            i + 1,
                            p.name,
                            p.track_count,
                            p.id
                        );
                    }
                }
                CoreEvent::PlaylistTracks {
                    playlist_id,
                    tracks,
                } => {
                    println!(
                        "\n=== Playlist {} Tracks ({}) ===",
                        playlist_id,
                        tracks.len()
                    );
                    for (i, t) in tracks.iter().enumerate() {
                        println!(
                            "{}. {} - {} ({}:{:02})",
                            i + 1,
                            t.title,
                            t.artists.join(", "),
                            t.duration_ms / 1000 / 60,
                            (t.duration_ms / 1000) % 60
                        );
                    }
                }
                CoreEvent::LikedSongs(tracks) => {
                    println!("\n=== Liked Songs ({}) ===", tracks.len());
                    for (i, t) in tracks.iter().enumerate() {
                        println!(
                            "{}. {} - {} ({}:{:02})",
                            i + 1,
                            t.title,
                            t.artists.join(", "),
                            t.duration_ms / 1000 / 60,
                            (t.duration_ms / 1000) % 60
                        );
                    }
                }
                CoreEvent::SearchResults { query, tracks } => {
                    println!(
                        "\n=== Search Results for '{}' ({}) ===",
                        query,
                        tracks.len()
                    );
                    for (i, t) in tracks.iter().enumerate() {
                        println!(
                            "{}. {} - {} [{}]",
                            i + 1,
                            t.title,
                            t.artists.join(", "),
                            t.album
                        );
                    }
                }
                CoreEvent::Lyrics(lyrics) => {
                    println!(
                        "\n=== Synced Lyrics: {} - {} ===",
                        lyrics.artist_name, lyrics.track_title
                    );
                    if !lyrics.synced_lyrics.is_empty() {
                        for line in &lyrics.synced_lyrics {
                            let total_sec = line.time_ms / 1000;
                            println!(
                                "[{:02}:{:02}] {}",
                                total_sec / 60,
                                total_sec % 60,
                                line.text
                            );
                        }
                    } else if let Some(plain) = lyrics.plain_lyrics {
                        println!("{}", plain);
                    } else {
                        println!("No lyrics found for this track.");
                    }
                }
                CoreEvent::Rules(r) => {
                    println!("\n=== Local Rules ===");
                    println!("Min Duration: {}s", r.min_duration_secs);
                    println!("Blocked Artists: {:?}", r.blocked_artists);
                    println!("Blocked Tracks: {:?}", r.blocked_tracks);
                }
                CoreEvent::SmartShuffle(s) => {
                    println!("\n=== Smart Shuffle ===");
                    println!("Enabled: {}", s.enabled);
                    println!("Ratio: 1 in {}", s.ratio);
                }
                CoreEvent::Error(err) => {
                    eprintln!("Error from daemon: {}", err);
                }
            }
        } else {
            let resp_str = String::from_utf8_lossy(&resp_buf);
            println!("Response: {}", resp_str);
        }
    } else {
        println!("Command sent successfully.");
    }

    Ok(())
}
