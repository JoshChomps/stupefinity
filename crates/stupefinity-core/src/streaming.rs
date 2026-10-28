use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use tracing::warn;

/// Coordinates download progress and playback reading across threads.
pub struct StreamCoordinator {
    downloaded: AtomicU64,
    total_len: AtomicU64,
    finished: AtomicBool,
    failed: AtomicBool,
    lock: Mutex<()>,
    condvar: Condvar,
}

impl Default for StreamCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamCoordinator {
    pub fn new() -> Self {
        Self {
            downloaded: AtomicU64::new(0),
            total_len: AtomicU64::new(0),
            finished: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            lock: Mutex::new(()),
            condvar: Condvar::new(),
        }
    }

    pub fn set_total_len(&self, len: u64) {
        self.total_len.store(len, Ordering::Release);
    }

    pub fn add_downloaded(&self, count: u64) {
        self.downloaded.fetch_add(count, Ordering::Release);
        self.condvar.notify_all();
    }

    pub fn mark_finished(&self) {
        self.finished.store(true, Ordering::Release);
        self.condvar.notify_all();
    }

    pub fn mark_failed(&self) {
        self.failed.store(true, Ordering::Release);
        self.condvar.notify_all();
    }

    pub fn downloaded_bytes(&self) -> u64 {
        self.downloaded.load(Ordering::Acquire)
    }

    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }

    pub fn is_failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }
}

/// A `Read + Seek` stream adapter that enables progressive audio decoding
/// before the entire track finishes downloading, with zero memory bloat.
pub struct ProgressiveStreamReader {
    file: File,
    coord: Arc<StreamCoordinator>,
    pos: u64,
    /// Deleted when the reader is dropped.
    path: Option<PathBuf>,
}

impl ProgressiveStreamReader {
    /// Reads `file` while it downloads to `path`, which is deleted afterwards unless it was
    /// moved away first.
    pub fn new(file: File, coord: Arc<StreamCoordinator>, path: PathBuf) -> Self {
        Self {
            file,
            coord,
            pos: 0,
            path: Some(path),
        }
    }

    /// Reads a file that is already complete (and is kept).
    pub fn complete(file: File) -> io::Result<Self> {
        let len = file.metadata()?.len();
        let coord = Arc::new(StreamCoordinator::new());
        coord.set_total_len(len);
        coord.add_downloaded(len);
        coord.mark_finished();
        Ok(Self {
            file,
            coord,
            pos: 0,
            path: None,
        })
    }
}

impl Read for ProgressiveStreamReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        let start_wait = std::time::Instant::now();
        loop {
            let downloaded = self.coord.downloaded_bytes();
            if self.pos < downloaded {
                let max_readable = (downloaded - self.pos).min(buf.len() as u64) as usize;
                let n = self.file.read(&mut buf[..max_readable])?;
                self.pos += n as u64;
                return Ok(n);
            }

            if self.coord.is_finished() {
                return Ok(0); // True EOF
            }

            if self.coord.is_failed() {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "Progressive audio download failed",
                ));
            }

            if start_wait.elapsed() > Duration::from_secs(15) {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Progressive audio stream stalled for >15s",
                ));
            }

            if let Ok(guard) = self.coord.lock.lock() {
                let _ = self
                    .coord
                    .condvar
                    .wait_timeout(guard, Duration::from_millis(50));
            }
        }
    }
}

impl Seek for ProgressiveStreamReader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let new_pos = match pos {
            SeekFrom::Start(offset) => offset,
            SeekFrom::Current(offset) => {
                let p = self.pos as i64 + offset;
                if p < 0 {
                    return Err(io::Error::new(io::ErrorKind::InvalidInput, "Negative seek"));
                }
                p as u64
            }
            SeekFrom::End(offset) => {
                let total = self.coord.total_len.load(Ordering::Acquire);
                if total > 0 {
                    let p = total as i64 + offset;
                    if p < 0 {
                        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Negative seek"));
                    }
                    p as u64
                } else {
                    let start_wait = std::time::Instant::now();
                    while !self.coord.is_finished()
                        && !self.coord.is_failed()
                        && start_wait.elapsed() < Duration::from_secs(5)
                    {
                        if let Ok(guard) = self.coord.lock.lock() {
                            let _ = self
                                .coord
                                .condvar
                                .wait_timeout(guard, Duration::from_millis(50));
                        }
                    }
                    let len = self.file.metadata()?.len();
                    let p = len as i64 + offset;
                    if p < 0 {
                        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Negative seek"));
                    }
                    p as u64
                }
            }
        };

        self.file.seek(SeekFrom::Start(new_pos))?;
        self.pos = new_pos;
        Ok(self.pos)
    }
}

impl Drop for ProgressiveStreamReader {
    fn drop(&mut self) {
        let Some(path) = &self.path else {
            return;
        };
        if let Err(e) = std::fs::remove_file(path) {
            // Ignore NotFound if another task cleaned it up
            if e.kind() != io::ErrorKind::NotFound {
                warn!("Failed to clean up temporary audio file {path:?}: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn progressive_reading_and_seeking() {
        let temp_path =
            std::env::temp_dir().join(format!("stupefinity_test_prog_{}.tmp", std::process::id()));
        let mut writer_file = File::create(&temp_path).unwrap();
        let reader_file = File::open(&temp_path).unwrap();

        let coord = Arc::new(StreamCoordinator::new());
        coord.set_total_len(100);

        let mut reader =
            ProgressiveStreamReader::new(reader_file, coord.clone(), temp_path.clone());

        // Write first 10 bytes
        writer_file.write_all(b"0123456789").unwrap();
        writer_file.flush().unwrap();
        coord.add_downloaded(10);

        let mut buf = [0u8; 5];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 5);
        assert_eq!(&buf, b"01234");

        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 5);
        assert_eq!(&buf, b"56789");

        // Write remaining bytes and finish
        writer_file.write_all(b"abcdefghij").unwrap();
        writer_file.flush().unwrap();
        coord.add_downloaded(10);
        coord.mark_finished();

        let mut buf2 = [0u8; 10];
        let n = reader.read(&mut buf2).unwrap();
        assert_eq!(n, 10);
        assert_eq!(&buf2, b"abcdefghij");

        // Seek back to start
        reader.seek(SeekFrom::Start(0)).unwrap();
        let mut buf3 = [0u8; 4];
        reader.read_exact(&mut buf3).unwrap();
        assert_eq!(&buf3, b"0123");
    }
}
