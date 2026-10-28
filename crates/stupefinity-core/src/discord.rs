use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use stupefinity_protocol::{PlaybackState, Snapshot};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};
use tokio::sync::RwLock;
use tokio::time::timeout;

const DISCORD_PIPE: &str = r"\\.\pipe\discord-ipc-0";
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const IO_TIMEOUT: Duration = Duration::from_secs(2);
/// Discord frames are small; refuse anything larger rather than allocating it.
const MAX_FRAME_LEN: usize = 64 * 1024;

const OP_HANDSHAKE: u32 = 0;
const OP_FRAME: u32 = 1;

/// Spawns a background task that mirrors the player state into Discord Rich Presence.
/// It never touches the main event loop, so a slow or absent Discord cannot stall playback.
/// Presence needs a Discord application ID (`discord_client_id` in settings.json, or the
/// `DISCORD_CLIENT_ID` environment variable); without one it stays off.
pub fn spawn(state: Arc<RwLock<Snapshot>>, custom_client_id: Option<String>) {
    let Some(client_id) = custom_client_id
        .filter(|id| !id.trim().is_empty())
        .or_else(|| std::env::var("DISCORD_CLIENT_ID").ok())
    else {
        tracing::info!("No Discord application ID configured; Rich Presence is off.");
        return;
    };

    tokio::spawn(async move {
        let mut pipe: Option<NamedPipeClient> = None;
        let mut last_sent: Option<Value> = None;
        let mut ticker = tokio::time::interval(POLL_INTERVAL);

        loop {
            ticker.tick().await;
            let activity = activity_for(&*state.read().await);
            if last_sent.as_ref() == Some(&activity) {
                continue;
            }

            if pipe.is_none() {
                pipe = connect(&client_id).await;
            }
            let Some(p) = pipe.as_mut() else { continue };

            let payload = json!({
                "cmd": "SET_ACTIVITY",
                "args": { "pid": std::process::id(), "activity": activity },
                "nonce": unix_now().to_string(),
            });
            if request(p, OP_FRAME, &payload).await.is_ok() {
                last_sent = Some(activity);
            } else {
                // Discord restarted or rejected us; reconnect on the next change.
                pipe = None;
            }
        }
    });
}

/// Opens the Discord IPC pipe and performs the handshake.
async fn connect(client_id: &str) -> Option<NamedPipeClient> {
    let mut pipe = ClientOptions::new().open(DISCORD_PIPE).ok()?;
    let handshake = json!({ "v": 1, "client_id": client_id });
    request(&mut pipe, OP_HANDSHAKE, &handshake).await.ok()?;
    Some(pipe)
}

/// Writes one frame and reads Discord's reply (which must be drained to keep the pipe flowing).
async fn request(pipe: &mut NamedPipeClient, opcode: u32, payload: &Value) -> anyhow::Result<()> {
    let body = payload.to_string().into_bytes();
    let mut frame = Vec::with_capacity(8 + body.len());
    frame.extend_from_slice(&opcode.to_le_bytes());
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend_from_slice(&body);

    timeout(IO_TIMEOUT, async {
        pipe.write_all(&frame).await?;
        let mut header = [0u8; 8];
        pipe.read_exact(&mut header).await?;
        let reply_op = u32::from_le_bytes(header[0..4].try_into()?);
        let len = u32::from_le_bytes(header[4..8].try_into()?) as usize;
        if len > MAX_FRAME_LEN {
            anyhow::bail!("Discord frame too large ({len} bytes)");
        }
        let mut reply = vec![0u8; len];
        pipe.read_exact(&mut reply).await?;
        // Opcode 2 is CLOSE: Discord refused the handshake or the frame.
        if reply_op == 2 {
            anyhow::bail!(
                "Discord closed the connection: {}",
                String::from_utf8_lossy(&reply)
            );
        }
        Ok(())
    })
    .await?
}

fn activity_for(s: &Snapshot) -> Value {
    let Some(t) = &s.current_track else {
        return Value::Null;
    };
    let artists = t.artists.join(", ");
    let large_image = t.cover_url.as_deref().unwrap_or("spotify");
    match s.playback_state {
        PlaybackState::Playing => {
            // Rounded to 10 s so normal progress does not count as a change.
            let start = unix_now().saturating_sub(u64::from(s.position_ms / 1000)) / 10 * 10;
            json!({
                "details": t.title,
                "state": format!("by {artists}"),
                "timestamps": { "start": start, "end": start + u64::from(t.duration_ms / 1000) },
                "assets": {
                    "large_image": large_image,
                    "large_text": t.album,
                    "small_image": "play",
                    "small_text": "Playing on Stupefinity"
                }
            })
        }
        PlaybackState::Paused => json!({
            "details": t.title,
            "state": format!("by {artists} (Paused)"),
            "assets": {
                "large_image": large_image,
                "large_text": t.album,
                "small_image": "pause",
                "small_text": "Paused"
            }
        }),
        _ => Value::Null,
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
