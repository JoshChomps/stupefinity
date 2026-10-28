use crate::eq::{EqualizerEngine, EqualizerSource};
use crate::platform::PlatformManager;
use rodio::source::SeekError;
use rodio::{Decoder, OutputStreamBuilder, Sink, Source};
use std::io::{Cursor, Read, Seek};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{
    channel, sync_channel, Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError,
};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{error, info, warn};

/// How often the audio thread samples the sink for position / end-of-track.
const POLL_INTERVAL: Duration = Duration::from_millis(200);
/// Samples per chunk handed from a decoder thread to the output (a multiple of 1, 2 and 4
/// channels, so chunks always end on a frame boundary).
const CHUNK_SAMPLES: usize = 4096;
/// Chunks decoded ahead of playback: about 0.75 s of 44.1 kHz stereo.
const CHUNKS_AHEAD: usize = 8;

pub trait ReadSeekSendSync: Read + Seek + Send + Sync + 'static {}
impl<T: Read + Seek + Send + Sync + 'static> ReadSeekSendSync for T {}

enum AudioMessage {
    PlayReader(Box<dyn ReadSeekSendSync>),
    /// A reader finished probing on its own thread. `load` identifies the request, so a
    /// probe that finishes after a newer track was requested is dropped.
    Ready(u64, DecodeAhead),
    LoadFailed(u64),
    Pause,
    Resume,
    Seek(u64),
    SetVolume(f32),
    Stop,
}

#[derive(Clone)]
pub struct AudioPlayer {
    sender: Sender<AudioMessage>,
    position_ms: Arc<AtomicU32>,
}

impl AudioPlayer {
    /// Spawns the dedicated audio thread. `on_finished` receives a message each time
    /// a track plays through to its end or fails to decode (not when it is stopped or
    /// replaced).
    pub fn new(on_finished: UnboundedSender<()>, eq: Arc<Mutex<EqualizerEngine>>) -> Self {
        let (tx, rx) = channel::<AudioMessage>();
        let position_ms = Arc::new(AtomicU32::new(0));
        let position = position_ms.clone();
        let loopback = tx.clone();

        thread::spawn(move || {
            PlatformManager::enable_mmcss_audio();

            let mut stream_handle = match OutputStreamBuilder::open_default_stream() {
                Ok(res) => res,
                Err(e) => {
                    error!("Failed to open WASAPI default output stream: {e}");
                    return;
                }
            };
            stream_handle.log_on_drop(false);

            let sink = Sink::connect_new(stream_handle.mixer());
            // True while a decoded track is loaded into the sink.
            let mut active = false;
            // Increments per track request; the latest one is the only one allowed to play.
            let mut load = 0u64;
            // Requested while the track was still probing; applied once it is ready.
            let mut pending_seek: Option<u64> = None;
            let mut paused = false;

            loop {
                match rx.recv_timeout(POLL_INTERVAL) {
                    Ok(AudioMessage::PlayReader(reader)) => {
                        load += 1;
                        sink.stop();
                        active = false;
                        pending_seek = None;
                        paused = false;
                        position.store(0, Ordering::Relaxed);
                        // Probing reads the stream header, which may still be downloading,
                        // so it runs off this thread to keep pause/stop responsive.
                        let (tx, id) = (loopback.clone(), load);
                        thread::spawn(move || {
                            let msg = match Decoder::new(reader) {
                                Ok(decoder) => AudioMessage::Ready(id, DecodeAhead::spawn(decoder)),
                                Err(e) => {
                                    error!("Failed to decode audio stream: {e}");
                                    AudioMessage::LoadFailed(id)
                                }
                            };
                            let _ = tx.send(msg);
                        });
                    }
                    Ok(AudioMessage::Ready(id, source)) if id == load => {
                        sink.append(EqualizerSource::new(source, eq.clone()));
                        if let Some(ms) = pending_seek.take() {
                            if let Err(e) = sink.try_seek(Duration::from_millis(ms)) {
                                error!("Seek error: {e}");
                            }
                        }
                        if !paused {
                            sink.play();
                        }
                        active = true;
                        info!("Audio playback began on WASAPI output sink with equalizer.");
                    }
                    Ok(AudioMessage::LoadFailed(id)) if id == load => {
                        let _ = on_finished.send(());
                    }
                    Ok(AudioMessage::Ready(..) | AudioMessage::LoadFailed(_)) => {}
                    Ok(AudioMessage::Pause) => {
                        paused = true;
                        sink.pause();
                    }
                    Ok(AudioMessage::Resume) => {
                        paused = false;
                        sink.play();
                    }
                    Ok(AudioMessage::Seek(ms)) => {
                        if active {
                            if let Err(e) = sink.try_seek(Duration::from_millis(ms)) {
                                error!("Seek error: {e}");
                            }
                        } else {
                            pending_seek = Some(ms);
                        }
                    }
                    Ok(AudioMessage::SetVolume(vol)) => sink.set_volume(vol.clamp(0.0, 1.0)),
                    Ok(AudioMessage::Stop) => {
                        load += 1;
                        sink.stop();
                        active = false;
                        pending_seek = None;
                        position.store(0, Ordering::Relaxed);
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }

                if active {
                    if sink.empty() {
                        active = false;
                        position.store(0, Ordering::Relaxed);
                        let _ = on_finished.send(());
                    } else {
                        position.store(sink.get_pos().as_millis() as u32, Ordering::Relaxed);
                    }
                }
            }
            info!("Audio thread exiting.");
        });

        Self {
            sender: tx,
            position_ms,
        }
    }

    /// Current playback position of the loaded track, in milliseconds.
    pub fn position_ms(&self) -> u32 {
        self.position_ms.load(Ordering::Relaxed)
    }

    pub fn play_bytes(&self, bytes: Vec<u8>) {
        self.play_reader(Box::new(Cursor::new(bytes)));
    }

    pub fn play_reader(&self, reader: Box<dyn ReadSeekSendSync>) {
        let _ = self.sender.send(AudioMessage::PlayReader(reader));
    }

    pub fn pause(&self) {
        let _ = self.sender.send(AudioMessage::Pause);
    }

    pub fn resume(&self) {
        let _ = self.sender.send(AudioMessage::Resume);
    }

    pub fn seek(&self, ms: u64) {
        let _ = self.sender.send(AudioMessage::Seek(ms));
    }

    pub fn set_volume(&self, linear_vol: f32) {
        let _ = self.sender.send(AudioMessage::SetVolume(linear_vol));
    }

    pub fn stop(&self) {
        let _ = self.sender.send(AudioMessage::Stop);
    }
}

/// A chunk of decoded samples, tagged with the seek it belongs to. `None` marks the end.
type Chunk = (u64, Option<Vec<f32>>);

/// Decodes on its own thread, a few chunks ahead of playback. The audio output only ever
/// takes ready chunks, so a slow download (or a seek into audio that has not arrived yet)
/// makes it play silence until the decoder catches up, instead of stalling the output.
struct DecodeAhead {
    chunks: Receiver<Chunk>,
    seeks: Sender<(u64, Duration)>,
    /// Bumped on every seek; chunks decoded before the latest seek are discarded.
    epoch: u64,
    current: Vec<f32>,
    pos: usize,
    /// Samples of silence still owed, so an underrun always covers whole frames.
    silence: usize,
    finished: bool,
    channels: u16,
    sample_rate: u32,
    total_duration: Option<Duration>,
}

impl DecodeAhead {
    fn spawn<S>(decoder: S) -> Self
    where
        S: Source<Item = f32> + Send + 'static,
    {
        let (channels, sample_rate) = (decoder.channels(), decoder.sample_rate());
        let total_duration = decoder.total_duration();
        let (chunk_tx, chunks) = sync_channel(CHUNKS_AHEAD);
        let (seeks, seek_rx) = channel();
        thread::spawn(move || decode_loop(decoder, &chunk_tx, &seek_rx));
        Self {
            chunks,
            seeks,
            epoch: 0,
            current: Vec::new(),
            pos: 0,
            silence: 0,
            finished: false,
            channels,
            sample_rate,
            total_duration,
        }
    }
}

/// Runs until the `DecodeAhead` that owns the other end of both channels is dropped.
fn decode_loop<S: Source<Item = f32>>(
    mut decoder: S,
    chunks: &SyncSender<Chunk>,
    seeks: &Receiver<(u64, Duration)>,
) {
    let mut epoch = 0;
    let mut ended = false;
    loop {
        // Only the latest pending seek matters.
        let mut seek = None;
        loop {
            match seeks.try_recv() {
                Ok(s) => seek = Some(s),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
        if seek.is_none() && ended {
            // Nothing left to decode until a seek restarts playback.
            match seeks.recv() {
                Ok(s) => seek = Some(s),
                Err(_) => return,
            }
        }
        if let Some((e, pos)) = seek {
            epoch = e;
            if let Err(err) = decoder.try_seek(pos) {
                warn!("Seek failed: {err}");
            }
        }

        let chunk: Vec<f32> = decoder.by_ref().take(CHUNK_SAMPLES).collect();
        ended = chunk.len() < CHUNK_SAMPLES;
        if !chunk.is_empty() && chunks.send((epoch, Some(chunk))).is_err() {
            return;
        }
        if ended && chunks.send((epoch, None)).is_err() {
            return;
        }
    }
}

impl Iterator for DecodeAhead {
    type Item = f32;

    #[inline]
    fn next(&mut self) -> Option<f32> {
        loop {
            if self.silence > 0 {
                self.silence -= 1;
                return Some(0.0);
            }
            if let Some(&sample) = self.current.get(self.pos) {
                self.pos += 1;
                return Some(sample);
            }
            if self.finished {
                return None;
            }
            match self.chunks.try_recv() {
                Ok((epoch, _)) if epoch != self.epoch => {}
                Ok((_, Some(chunk))) => {
                    self.current = chunk;
                    self.pos = 0;
                }
                Ok((_, None)) | Err(TryRecvError::Disconnected) => self.finished = true,
                Err(TryRecvError::Empty) => self.silence = usize::from(self.channels.max(1)),
            }
        }
    }
}

impl Source for DecodeAhead {
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn total_duration(&self) -> Option<Duration> {
        self.total_duration
    }

    /// Hands the seek to the decoder thread and returns at once; playback resumes from the
    /// new position as soon as its first chunk is decoded.
    fn try_seek(&mut self, pos: Duration) -> Result<(), SeekError> {
        self.epoch += 1;
        self.current.clear();
        self.pos = 0;
        self.silence = 0;
        self.finished = false;
        let _ = self.seeks.send((self.epoch, pos));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// Counts up from a seekable position, one sample per tick, mono.
    struct Ramp {
        at: u32,
        len: u32,
    }

    impl Iterator for Ramp {
        type Item = f32;
        fn next(&mut self) -> Option<f32> {
            (self.at < self.len).then(|| {
                self.at += 1;
                (self.at - 1) as f32
            })
        }
    }

    impl Source for Ramp {
        fn current_span_len(&self) -> Option<usize> {
            None
        }
        fn channels(&self) -> u16 {
            1
        }
        fn sample_rate(&self) -> u32 {
            1000
        }
        fn total_duration(&self) -> Option<Duration> {
            None
        }
        fn try_seek(&mut self, pos: Duration) -> Result<(), SeekError> {
            self.at = pos.as_millis() as u32;
            Ok(())
        }
    }

    /// Next non-silent sample, waiting for the decoder thread if needed.
    fn next_real(src: &mut DecodeAhead) -> Option<f32> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match src.next() {
                Some(s) if s == 0.0 && Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(1))
                }
                other => return other,
            }
        }
    }

    #[test]
    fn plays_all_samples_in_order_then_ends() {
        let mut src = DecodeAhead::spawn(Ramp { at: 1, len: 10_000 });
        for expected in 1..10_000 {
            assert_eq!(next_real(&mut src), Some(expected as f32));
        }
        assert_eq!(next_real(&mut src), None);
    }

    #[test]
    fn seek_discards_audio_decoded_before_it() {
        let mut src = DecodeAhead::spawn(Ramp {
            at: 1,
            len: 100_000,
        });
        assert_eq!(next_real(&mut src), Some(1.0));
        src.try_seek(Duration::from_millis(50_000)).unwrap();
        assert_eq!(next_real(&mut src), Some(50_000.0));
        assert_eq!(next_real(&mut src), Some(50_001.0));
    }

    #[test]
    fn seek_after_the_end_restarts_playback() {
        let mut src = DecodeAhead::spawn(Ramp { at: 1, len: 10 });
        while next_real(&mut src).is_some() {}
        src.try_seek(Duration::from_millis(5)).unwrap();
        assert_eq!(next_real(&mut src), Some(5.0));
    }
}
