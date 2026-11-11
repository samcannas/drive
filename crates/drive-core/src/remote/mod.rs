//! Storage backends. The engine only ever talks to a [`Remote`].
//!
//! * [`ulozto::UlozRemote`](crate::ulozto::UlozRemote) — the real uloz.to account (raw bytes).
//! * [`crypt::CryptRemote`] — wraps any remote with rclone-compatible encryption.
//! * [`mem::MemRemote`] — in-memory backend with fault injection, for tests.
//!
//! Entries are identified by backend ids that stay stable across renames and moves (uloz.to
//! slugs). Files are immutable: replacing content produces a new id.

pub mod crypt;
pub mod mem;
pub mod resumable;

use std::io;
use std::pin::Pin;

use async_trait::async_trait;
use bytes::Bytes;
use futures::Stream;
use serde::{Deserialize, Serialize};

pub type ByteStream = Pin<Box<dyn Stream<Item = io::Result<Bytes>> + Send + 'static>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Kind {
    File,
    Folder,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteEntry {
    pub id: String,
    pub parent_id: String,
    pub kind: Kind,
    pub name: String,
    /// Size in bytes as seen through this remote (plaintext size for a crypt remote).
    pub size: u64,
    /// Modification time in Unix milliseconds (0 if unknown).
    pub mtime_ms: i64,
}

/// Full listing of everything below the remote's root.
#[derive(Debug, Clone, Default)]
pub struct RemoteTree {
    pub root_id: String,
    pub entries: Vec<RemoteEntry>,
    /// Raw names that could not be decoded (e.g. foreign files in an encrypted vault).
    pub skipped: Vec<String>,
}

pub struct UploadBody {
    pub stream: ByteStream,
    pub size: u64,
    pub mtime_ms: i64,
}

impl UploadBody {
    pub fn from_bytes(data: impl Into<Bytes>, mtime_ms: i64) -> Self {
        let data: Bytes = data.into();
        let size = data.len() as u64;
        Self {
            stream: Box::pin(futures::stream::once(async move { Ok(data) })),
            size,
            mtime_ms,
        }
    }
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum RemoteError {
    #[error("sign-in failed: {0}")]
    Auth(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("uloz.to is rate limiting requests")]
    RateLimited,
    #[error("network error: {0}")]
    Network(String),
    #[error("server error: {0}")]
    Server(String),
    #[error("storage quota exceeded")]
    Quota,
    #[error("download needs a premium uloz.to account: {0}")]
    DownloadNotAllowed(String),
    #[error("invalid name: {0}")]
    InvalidName(String),
    #[error("data integrity check failed: {0}")]
    Integrity(String),
    /// The caller stopped the transfer (pause, shutdown). Resume state is kept.
    #[error("cancelled")]
    Cancelled,
    #[error("{0}")]
    Other(String),
}

impl RemoteError {
    /// Whether retrying the same operation later may succeed.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            RemoteError::RateLimited
                | RemoteError::Network(_)
                | RemoteError::Server(_)
                | RemoteError::Integrity(_)
        )
    }
}

impl From<RemoteError> for io::Error {
    fn from(e: RemoteError) -> Self {
        io::Error::other(e)
    }
}

#[async_trait]
pub trait Remote: Send + Sync + 'static {
    /// Lists everything below the root (recursively).
    async fn scan(&self) -> Result<RemoteTree, RemoteError>;
    async fn create_folder(&self, parent_id: &str, name: &str) -> Result<RemoteEntry, RemoteError>;
    /// Creates a new file. Existing files with the same name are left alone (the engine
    /// trashes the previous version itself once the upload succeeded).
    async fn upload(
        &self,
        parent_id: &str,
        name: &str,
        body: UploadBody,
    ) -> Result<RemoteEntry, RemoteError>;
    /// Reads `len` bytes (or to EOF) starting at `offset`.
    async fn read(
        &self,
        file: &RemoteEntry,
        offset: u64,
        len: Option<u64>,
    ) -> Result<ByteStream, RemoteError>;
    /// Moves and/or renames. Returns the entry as it is afterwards.
    async fn rename(
        &self,
        entry: &RemoteEntry,
        new_parent_id: &str,
        new_name: &str,
    ) -> Result<RemoteEntry, RemoteError>;
    /// Moves to the provider's trash (recoverable), or deletes if there is no trash.
    async fn trash(&self, entry: &RemoteEntry) -> Result<(), RemoteError>;
    /// Whether a file still exists (not deleted or trashed). Used before uploading an edit to
    /// make sure the version it was based on is still the current one.
    async fn exists(&self, entry: &RemoteEntry) -> Result<bool, RemoteError>;

    /// Uploads from a random-access source, picking up where an earlier attempt stopped when
    /// `upload.state` holds that attempt's resume state. Backends that can't resume read the
    /// source from the start (the default).
    async fn upload_resumable(
        &self,
        parent_id: &str,
        name: &str,
        upload: resumable::ResumableUpload,
    ) -> Result<RemoteEntry, RemoteError> {
        resumable::upload_whole(self, parent_id, name, upload).await
    }

    /// Best-effort cleanup of an abandoned resumable upload (its partial data on the server).
    async fn discard_resumable(&self, _state: &resumable::ResumeState) {}

    /// Smallest upload (in this remote's bytes) that [`Remote::upload_resumable`] can resume;
    /// `None` if it never resumes.
    fn resumable_min_size(&self) -> Option<u64> {
        None
    }
}

/// Collects a stream into memory (tests and small reads).
pub async fn read_all(mut s: ByteStream) -> io::Result<Vec<u8>> {
    use futures::StreamExt;
    let mut out = Vec::new();
    while let Some(chunk) = s.next().await {
        out.extend_from_slice(&chunk?);
    }
    Ok(out)
}
