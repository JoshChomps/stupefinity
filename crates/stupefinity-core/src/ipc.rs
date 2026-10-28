use std::sync::Arc;
use stupefinity_protocol::{Command, CoreEvent, Snapshot, TrackInfo, DEFAULT_PIPE_NAME};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::sync::{mpsc, oneshot, RwLock};
use tracing::{error, info, warn};

/// Largest IPC frame accepted from a client. Commands are tiny; this only guards
/// against a garbage length prefix triggering a huge allocation.
const MAX_FRAME_LEN: usize = 1024 * 1024;

pub enum ClientRequest {
    Command(Command),
    Query {
        cmd: Command,
        resp_tx: oneshot::Sender<CoreEvent>,
    },
    /// Answered once every request queued before it has been handled by the core loop.
    Sync(oneshot::Sender<()>),
    /// Internal: tracks of a context fetched in the background, ready to be queued.
    ContextLoaded {
        uri: String,
        tracks: Vec<TrackInfo>,
        start_index: usize,
    },
    /// Internal: metadata for a track that was requested for playback.
    TrackReady {
        generation: u64,
        track: TrackInfo,
    },
    /// Internal: a background task failed in a way the user should hear about.
    PlaybackFailed {
        generation: u64,
        reason: String,
    },
    /// Internal: metadata for a track the user queued from outside the loaded context.
    QueuedTrackInfo(TrackInfo),
    /// Internal: librespot playback event.
    LibrespotEvent(librespot::playback::player::PlayerEvent),
}

pub struct IpcServer {
    pipe_name: String,
    state: Arc<RwLock<Snapshot>>,
    req_sender: mpsc::Sender<ClientRequest>,
}

impl IpcServer {
    pub fn new(state: Arc<RwLock<Snapshot>>, req_sender: mpsc::Sender<ClientRequest>) -> Self {
        Self {
            pipe_name: DEFAULT_PIPE_NAME.to_string(),
            state,
            req_sender,
        }
    }

    /// Run the named pipe listener loop in a background Tokio task.
    pub async fn run(self) -> anyhow::Result<()> {
        info!("IPC Named Pipe server listening on {}", self.pipe_name);

        // `first_pipe_instance` stops another process from squatting on our pipe name.
        let mut server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&self.pipe_name)?;

        loop {
            if let Err(e) = server.connect().await {
                warn!("Named pipe client connection failed: {e}");
                server = self.create_instance().await;
                continue;
            }

            // Create the next instance before serving this one, so there is never a
            // window in which clients find no pipe to connect to.
            let client = std::mem::replace(&mut server, self.create_instance().await);
            let state = self.state.clone();
            let req_tx = self.req_sender.clone();
            tokio::spawn(async move {
                if let Err(e) = Self::handle_client(client, state, req_tx).await {
                    warn!("Error handling IPC client: {e}");
                }
            });
        }
    }

    async fn create_instance(&self) -> NamedPipeServer {
        loop {
            match ServerOptions::new().create(&self.pipe_name) {
                Ok(s) => return s,
                Err(e) => {
                    error!("Failed to create named pipe instance: {e}");
                    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                }
            }
        }
    }

    async fn handle_client(
        mut stream: NamedPipeServer,
        state: Arc<RwLock<Snapshot>>,
        req_tx: mpsc::Sender<ClientRequest>,
    ) -> anyhow::Result<()> {
        let mut len_buf = [0u8; 4];
        while stream.read_exact(&mut len_buf).await.is_ok() {
            let len = u32::from_le_bytes(len_buf) as usize;
            if len > MAX_FRAME_LEN {
                anyhow::bail!("IPC frame of {len} bytes exceeds limit");
            }
            let mut buf = vec![0u8; len];
            stream.read_exact(&mut buf).await?;

            let resp_event = match serde_json::from_slice::<Command>(&buf) {
                Ok(cmd) => {
                    info!("IPC received command: {:?}", cmd);
                    Self::dispatch(cmd, &state, &req_tx).await
                }
                Err(e) => {
                    warn!("Failed to deserialize incoming IPC command: {e}");
                    CoreEvent::Error(format!("Invalid command: {e}"))
                }
            };

            let resp_bytes = serde_json::to_vec(&resp_event)?;
            stream
                .write_all(&(resp_bytes.len() as u32).to_le_bytes())
                .await?;
            stream.write_all(&resp_bytes).await?;
            stream.flush().await?;
        }

        Ok(())
    }

    async fn dispatch(
        cmd: Command,
        state: &RwLock<Snapshot>,
        req_tx: &mpsc::Sender<ClientRequest>,
    ) -> CoreEvent {
        const CORE_GONE: &str = "Core is shutting down";
        match cmd {
            // Query operations with dedicated response events
            Command::GetPlaylists
            | Command::GetPlaylistTracks { .. }
            | Command::GetLikedSongs { .. }
            | Command::Search { .. }
            | Command::GetLyrics => {
                let (tx, rx) = oneshot::channel();
                if req_tx
                    .send(ClientRequest::Query { cmd, resp_tx: tx })
                    .await
                    .is_err()
                {
                    return CoreEvent::Error(CORE_GONE.into());
                }
                rx.await
                    .unwrap_or_else(|_| CoreEvent::Error("Query was dropped by the core".into()))
            }
            Command::GetSnapshot => CoreEvent::State(state.read().await.clone()),
            // The core stops right after handling this, so there is no later state to wait for.
            Command::Shutdown => {
                let _ = req_tx.send(ClientRequest::Command(Command::Shutdown)).await;
                CoreEvent::State(state.read().await.clone())
            }
            // Playback or state-altering commands: reply with the state after it was applied.
            other_cmd => {
                let (tx, rx) = oneshot::channel();
                let sent = req_tx.send(ClientRequest::Command(other_cmd)).await.is_ok()
                    && req_tx.send(ClientRequest::Sync(tx)).await.is_ok();
                if !sent || rx.await.is_err() {
                    return CoreEvent::Error(CORE_GONE.into());
                }
                CoreEvent::State(state.read().await.clone())
            }
        }
    }
}
