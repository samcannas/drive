//! Resumable uploads: the pieces shared by every layer.
//!
//! An upload reads from an [`UploadSource`] (random access, so any byte range can be sent
//! again or skipped) and carries a [`ResumeState`]: an opaque map in which each layer keeps
//! what it needs to pick up an interrupted upload (the crypt layer its file nonce, uloz.to its
//! upload batch). Layers call [`ResumableUpload::checkpoint`] whenever their part changes; the
//! engine persists every checkpoint in its database, so an upload survives a dropped
//! connection, a pause, a crash or a reboot.

use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::{Remote, RemoteEntry, RemoteError, UploadBody};

/// Random-access bytes to upload. `read_at` blocks (it reads files); call it from a blocking
/// task.
pub trait UploadSource: Send + Sync + 'static {
    fn len(&self) -> u64;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Fills `buf` with the bytes at `offset`. Errors if fewer bytes are available.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()>;
}

/// A source backed by an open file.
pub struct FileSource {
    file: std::fs::File,
    len: u64,
}

impl FileSource {
    pub fn new(file: std::fs::File, len: u64) -> Self {
        Self { file, len }
    }
}

impl UploadSource for FileSource {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        if offset + buf.len() as u64 > self.len {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "read past the end of the file",
            ));
        }
        read_exact_at(&self.file, offset, buf)
    }
}

#[cfg(unix)]
fn read_exact_at(file: &std::fs::File, offset: u64, buf: &mut [u8]) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(buf, offset)
}

#[cfg(windows)]
fn read_exact_at(file: &std::fs::File, mut offset: u64, mut buf: &mut [u8]) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_read(buf, offset) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "file got shorter",
                ));
            }
            Ok(n) => {
                buf = &mut buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// An in-memory source (tests, small uploads).
pub struct BytesSource(pub Bytes);

impl UploadSource for BytesSource {
    fn len(&self) -> u64 {
        self.0.len() as u64
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let start = offset as usize;
        let end = start + buf.len();
        if end > self.0.len() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "read past the end",
            ));
        }
        buf.copy_from_slice(&self.0[start..end]);
        Ok(())
    }
}

/// Reads `len` bytes at `offset` on a blocking thread.
pub async fn read_range(
    source: &Arc<dyn UploadSource>,
    offset: u64,
    len: usize,
) -> io::Result<Vec<u8>> {
    let src = source.clone();
    tokio::task::spawn_blocking(move || {
        let mut buf = vec![0u8; len];
        src.read_at(offset, &mut buf)?;
        Ok(buf)
    })
    .await
    .map_err(io::Error::other)?
}

/// What a backend can tell about a saved upload session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResumeInfo {
    /// Bytes (of the backend's own source) the server has confirmed.
    pub confirmed: u64,
    pub total: u64,
    /// The server already assembled the file under this id (it may even be committed): a
    /// remote file with this id belongs to the pending upload.
    pub assembled_id: Option<String>,
}

/// Per-layer resume state, persisted between attempts.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ResumeState(pub BTreeMap<String, serde_json::Value>);

impl ResumeState {
    pub fn get<T: DeserializeOwned>(&self, layer: &str) -> Option<T> {
        self.0
            .get(layer)
            .and_then(|v| serde_json::from_value(v.clone()).ok())
    }
    pub fn set<T: Serialize>(&mut self, layer: &str, value: &T) {
        if let Ok(v) = serde_json::to_value(value) {
            self.0.insert(layer.to_string(), v);
        }
    }
    pub fn remove(&mut self, layer: &str) {
        self.0.remove(layer);
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Where an upload is (shown in the transfer list).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum UploadPhase {
    Preparing = 0,
    Sending = 1,
    Verifying = 2,
}

/// Paces and counts the bytes an upload sends.
#[async_trait]
pub trait Meter: Send + Sync {
    /// Called before `bytes` more go out; may wait (bandwidth limit).
    async fn consume(&self, bytes: usize);
}

/// Live progress of an upload, readable while it runs. All byte counts are in units of the
/// *outermost* source (plaintext for an encrypted upload); inner layers scale.
#[derive(Debug, Default)]
pub struct Progress {
    /// Bytes the server has confirmed (survive a restart).
    pub confirmed: AtomicU64,
    /// Bytes sent in requests still in flight (lost if they fail).
    pub in_flight: AtomicU64,
    phase: AtomicU8,
}

impl Progress {
    pub fn done(&self) -> u64 {
        self.confirmed.load(Ordering::Relaxed) + self.in_flight.load(Ordering::Relaxed)
    }
    pub fn phase(&self) -> UploadPhase {
        match self.phase.load(Ordering::Relaxed) {
            0 => UploadPhase::Preparing,
            2 => UploadPhase::Verifying,
            _ => UploadPhase::Sending,
        }
    }
    pub fn set_phase(&self, p: UploadPhase) {
        self.phase.store(p as u8, Ordering::Relaxed);
    }
}

pub struct ResumableUpload {
    pub source: Arc<dyn UploadSource>,
    pub mtime_ms: i64,
    /// State saved by an earlier attempt (empty for a new upload).
    pub state: ResumeState,
    /// Persists the state; called by the layers whenever it changes.
    pub checkpoint: Arc<dyn Fn(&ResumeState) + Send + Sync>,
    pub meter: Arc<dyn Meter>,
    pub progress: Arc<Progress>,
    /// Ratio of outer (reported) bytes to this layer's source bytes, for progress.
    pub scale: f64,
    pub cancel: CancellationToken,
}

impl ResumableUpload {
    /// Converts a byte count of this layer's source into reported (outer) bytes.
    pub fn outer(&self, inner_bytes: u64) -> u64 {
        (inner_bytes as f64 * self.scale).round() as u64
    }
}

/// Uploads the whole source in one request (no resume). Used by backends without resumable
/// uploads and as the fallback for small files.
pub async fn upload_whole<R: Remote + ?Sized>(
    remote: &R,
    parent_id: &str,
    name: &str,
    up: ResumableUpload,
) -> Result<RemoteEntry, RemoteError> {
    let body = whole_body(&up);
    let cancel = up.cancel.clone();
    tokio::select! {
        r = remote.upload(parent_id, name, body) => {
            if r.is_ok() {
                let total = up.outer(up.source.len());
                up.progress.in_flight.store(0, Ordering::Relaxed);
                up.progress.confirmed.store(total, Ordering::Relaxed);
            } else {
                up.progress.in_flight.store(0, Ordering::Relaxed);
            }
            r
        }
        _ = cancel.cancelled() => {
            up.progress.in_flight.store(0, Ordering::Relaxed);
            Err(RemoteError::Cancelled)
        }
    }
}

/// A streaming body that reads the source in 256 KiB pieces, paced by the meter.
pub fn whole_body(up: &ResumableUpload) -> UploadBody {
    const PIECE: u64 = 256 * 1024;
    let source = up.source.clone();
    let meter = up.meter.clone();
    let progress = up.progress.clone();
    let scale = up.scale;
    let len = source.len();
    progress.set_phase(UploadPhase::Sending);
    let stream = futures::stream::unfold(0u64, move |offset| {
        let source = source.clone();
        let meter = meter.clone();
        let progress = progress.clone();
        async move {
            if offset >= len {
                return None;
            }
            let n = PIECE.min(len - offset) as usize;
            meter.consume(n).await;
            match read_range(&source, offset, n).await {
                Ok(buf) => {
                    progress.in_flight.store(
                        ((offset + n as u64) as f64 * scale).round() as u64,
                        Ordering::Relaxed,
                    );
                    Some((Ok(Bytes::from(buf)), offset + n as u64))
                }
                Err(e) => Some((Err(e), len)),
            }
        }
    });
    UploadBody {
        stream: Box::pin(stream),
        size: len,
        mtime_ms: up.mtime_ms,
    }
}

/// A meter that does nothing (tests).
pub struct NoMeter;

#[async_trait]
impl Meter for NoMeter {
    async fn consume(&self, _bytes: usize) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_source_reads_ranges() {
        let mut f = tempfile::tempfile().unwrap();
        use std::io::Write;
        let data: Vec<u8> = (0..10_000u32).map(|i| i as u8).collect();
        f.write_all(&data).unwrap();
        let src = FileSource::new(f, data.len() as u64);
        let mut buf = vec![0u8; 100];
        src.read_at(9_900, &mut buf).unwrap();
        assert_eq!(buf, &data[9_900..]);
        assert!(src.read_at(9_950, &mut buf).is_err());
    }

    #[test]
    fn resume_state_layers() {
        let mut s = ResumeState::default();
        s.set("crypt", &serde_json::json!({"nonce": "abc"}));
        let back: serde_json::Value = s.get("crypt").unwrap();
        assert_eq!(back["nonce"], "abc");
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<ResumeState>(&json).unwrap(), s);
    }
}
