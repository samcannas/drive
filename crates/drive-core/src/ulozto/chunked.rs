//! Resumable, chunked uploads (<https://uloz.to/upload-resumable-api-beta>).
//!
//! Flow, as in uloz.to's reference script:
//! 1. `POST /v6/upload/link` → batch `private_slug`, `upload_signature`, resumable host.
//!    Called again with the `private_slug` it renews the signature of an existing batch,
//!    which `POST {host}/v1/chunked-file-upload/{slug}` then extends.
//! 2. `POST {host}/v1/chunked-file-upload` registers the file (name, size, chunk size, CRC32).
//! 3. `POST {host}/v1/chunked-file-upload/{slug}/{file}/{n}` uploads chunk `n` (any order).
//! 4. `GET  …/{slug}/{file}/status` lists `missing_chunks` and, once the server has assembled
//!    and CRC-checked the file, `status: finished` with the file's slug.
//! 5. `GET /v5/file/{slug}/hash-verify` confirms our MD5, then the file's properties are set and
//!    the batch is committed, exactly like single-request uploads.
//!
//! What we add on top:
//! * a preparation pass hashes the whole file (MD5, SHA-256 for rclone's metadata, CRC32 for
//!   the server's check) and every chunk (MD5). Each chunk is re-hashed right before it is
//!   sent; a mismatch means the file changed, and the session is abandoned. This keeps the
//!   crypt layer's resumed nonce safe: a block is never encrypted twice with different data.
//! * all state needed to continue lives in the [`ResumeState`], checkpointed after every
//!   step, so an upload continues after a restart with only the missing chunks;
//! * several chunks upload in parallel, adapted by [`AdaptiveLimit`];
//! * every chunk has a stall watchdog and its own retries.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use md5::Digest as _;
use reqwest::{Method, StatusCode, header};
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use tracing::{debug, info, warn};

use super::{UlozClient, api, map_status, meta, net, transfer_err};
use crate::remote::RemoteError;
use crate::remote::resumable::{ResumableUpload, ResumeState, UploadPhase, read_range};

/// Resume-state key of this layer.
pub const LAYER: &str = "ulozto";
const MIB: u64 = 1024 * 1024;
/// Chunk sizes the server accepts.
pub const CHUNK_SIZES: [u64; 3] = [4 * MIB, 32 * MIB, 128 * MIB];
/// Chunks held in memory at once, across one upload (each chunk is checked before sending).
const MEMORY_BUDGET: u64 = 160 * MIB;
/// Times a single chunk is retried before the upload gives up (the engine retries later).
const CHUNK_RETRIES: u32 = 4;

pub fn chunk_size_for(len: u64) -> u64 {
    if len <= 256 * MIB {
        CHUNK_SIZES[0]
    } else if len <= 16 * 1024 * MIB {
        CHUNK_SIZES[1]
    } else {
        CHUNK_SIZES[2]
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChunkedState {
    pub v: u8,
    pub len: u64,
    pub chunk_size: u64,
    pub md5: String,
    pub sha256: String,
    pub crc32: u32,
    /// MD5 of every chunk, in order.
    pub chunk_md5: Vec<String>,
    pub batch: Option<Batch>,
    /// Set once the server has assembled the file.
    pub file_slug: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Batch {
    pub private_slug: String,
    pub base_url: String,
    pub batch_file_id: u32,
    pub registered: bool,
    /// Chunks the server has confirmed (for showing progress before the first status call).
    pub confirmed_chunks: u32,
}

impl ChunkedState {
    pub fn chunk_count(&self) -> u32 {
        self.len.div_ceil(self.chunk_size).max(1) as u32
    }
    pub fn chunk_len(&self, n: u32) -> u64 {
        let start = (n as u64 - 1) * self.chunk_size;
        self.chunk_size.min(self.len - start)
    }
    fn bytes_of(&self, chunks: impl IntoIterator<Item = u32>) -> u64 {
        chunks.into_iter().map(|n| self.chunk_len(n)).sum()
    }
}

/// Bytes confirmed by the server for a saved state (for showing progress of a queued upload).
pub fn confirmed_bytes(state: &ResumeState) -> Option<(u64, u64)> {
    let s: ChunkedState = state.get(LAYER)?;
    let b = s.batch.as_ref()?;
    let done = if s.file_slug.is_some() {
        s.len
    } else {
        s.bytes_of(1..=b.confirmed_chunks.min(s.chunk_count()))
    };
    Some((done, s.len))
}

/// The file slug of an upload the server already assembled (it may even be committed): such a
/// file belongs to the pending upload, not to anyone else.
pub fn finished_slug(state: &ResumeState) -> Option<String> {
    state.get::<ChunkedState>(LAYER)?.file_slug
}

// ---- adaptive concurrency ------------------------------------------------------------------

/// AIMD limit on chunk streams in flight (shared by all uploads of one account):
/// additive increase after a streak of successes, as long as throughput keeps improving;
/// halved on throttling, server errors and stalls.
pub struct AdaptiveLimit {
    inner: std::sync::Mutex<LimitState>,
    notify: Notify,
    max: usize,
}

struct LimitState {
    limit: usize,
    in_use: usize,
    streak: usize,
    /// Completed (time, bytes) in the recent window, for throughput.
    window: VecDeque<(Instant, u64)>,
    /// Throughput measured just before the last increase.
    before_increase: Option<(usize, f64)>,
    no_increase_until: Option<Instant>,
}

const RATE_WINDOW: Duration = Duration::from_secs(30);

impl AdaptiveLimit {
    pub fn new(start: usize, max: usize) -> Self {
        Self {
            inner: std::sync::Mutex::new(LimitState {
                limit: start.clamp(1, max.max(1)),
                in_use: 0,
                streak: 0,
                window: VecDeque::new(),
                before_increase: None,
                no_increase_until: None,
            }),
            notify: Notify::new(),
            max: max.max(1),
        }
    }

    pub fn limit(&self) -> usize {
        self.inner.lock().unwrap().limit
    }

    /// Waits for a free stream. `cap` further limits this caller (memory budget).
    pub async fn acquire(&self, held_by_caller: usize, cap: usize) -> LimitPermit<'_> {
        loop {
            let notified = self.notify.notified();
            {
                let mut s = self.inner.lock().unwrap();
                if s.in_use < s.limit && held_by_caller < cap.max(1) {
                    s.in_use += 1;
                    return LimitPermit { limit: self };
                }
            }
            notified.await;
        }
    }

    fn rate(window: &VecDeque<(Instant, u64)>, now: Instant) -> f64 {
        let bytes: u64 = window.iter().map(|(_, b)| b).sum();
        let span = window
            .front()
            .map(|(t, _)| now.duration_since(*t))
            .unwrap_or_default();
        bytes as f64 / span.as_secs_f64().max(1.0)
    }

    pub fn success(&self, bytes: u64, now: Instant) {
        let mut s = self.inner.lock().unwrap();
        s.window.push_back((now, bytes));
        while s
            .window
            .front()
            .is_some_and(|(t, _)| now.duration_since(*t) > RATE_WINDOW)
        {
            s.window.pop_front();
        }
        s.streak += 1;
        if s.streak < s.limit * 2 || s.limit >= self.max {
            return;
        }
        if s.no_increase_until.is_some_and(|t| now < t) {
            return;
        }
        let rate = Self::rate(&s.window, now);
        if let Some((prev_limit, prev_rate)) = s.before_increase.take()
            && s.limit > prev_limit
            && rate < prev_rate * 1.05
        {
            // More streams didn't help: go back and stop probing for a while.
            debug!(
                "chunk streams {} → {prev_limit} (no throughput gain)",
                s.limit
            );
            s.limit = prev_limit;
            s.no_increase_until = Some(now + Duration::from_secs(600));
            s.streak = 0;
            return;
        }
        s.before_increase = Some((s.limit, rate));
        s.limit += 1;
        s.streak = 0;
        debug!("chunk streams → {}", s.limit);
        drop(s);
        self.notify.notify_waiters();
    }

    pub fn backoff(&self, now: Instant) {
        let mut s = self.inner.lock().unwrap();
        let new = (s.limit / 2).max(1);
        if new != s.limit {
            debug!("chunk streams {} → {new} (server pushing back)", s.limit);
        }
        s.limit = new;
        s.streak = 0;
        s.before_increase = None;
        s.no_increase_until = Some(now + Duration::from_secs(120));
    }
}

pub struct LimitPermit<'a> {
    limit: &'a AdaptiveLimit,
}

impl Drop for LimitPermit<'_> {
    fn drop(&mut self) {
        self.limit.inner.lock().unwrap().in_use -= 1;
        self.limit.notify.notify_waiters();
    }
}

// ---- the upload ------------------------------------------------------------------------------

/// What the chunk senders need (an immutable snapshot of the state).
struct Plan {
    chunk_size: u64,
    len: u64,
    chunk_md5: Vec<String>,
}

enum ServerState {
    NotRegistered,
    InProgress(Vec<u32>),
    Finished(String),
    Failed(String),
}

/// Why a chunked attempt stopped short of a file.
enum Stop {
    /// Start over with a new batch (the old one expired or broke).
    NewBatch(String),
    Fail(RemoteError),
}

impl From<RemoteError> for Stop {
    fn from(e: RemoteError) -> Self {
        Stop::Fail(e)
    }
}

impl UlozClient {
    /// Whether chunked uploads are currently usable (not refused recently).
    pub fn chunked_available(&self) -> bool {
        self.chunked_unavailable_until
            .lock()
            .unwrap()
            .is_none_or(|t| Instant::now() >= t)
    }

    fn mark_chunked_unavailable(&self, why: &str) {
        warn!("chunked uploads unavailable ({why}); using single-request uploads for an hour");
        *self.chunked_unavailable_until.lock().unwrap() =
            Some(Instant::now() + Duration::from_secs(3600));
    }

    /// Uploads `up.source` as `name` in `folder` with resumable chunks.
    pub async fn upload_chunked(
        &self,
        folder: &str,
        name: &str,
        up: &ResumableUpload,
    ) -> Result<api::File, RemoteError> {
        let len = up.source.len();
        let mut state = up.state.clone();
        let chunk_size = self.cfg.chunk_size.unwrap_or_else(|| chunk_size_for(len));
        let saved: Option<ChunkedState> = state.get(LAYER);
        up.progress.set_phase(UploadPhase::Preparing);
        let fresh = self.prepare(up, chunk_size).await?;
        let mut st = match saved {
            Some(s)
                if s.v == 1 && s.len == len && s.chunk_size == chunk_size && s.md5 == fresh.md5 =>
            {
                info!("resuming upload of {name}");
                ChunkedState {
                    chunk_md5: fresh.chunk_md5,
                    ..s
                }
            }
            Some(s) => {
                // The file changed since that session started: it can't be continued.
                debug!("previous upload session of {name} doesn't match the file; starting over");
                self.discard_batch(&s).await;
                fresh
            }
            None => fresh,
        };
        state.set(LAYER, &st);
        (up.checkpoint)(&state);

        let mut restarts = 0;
        loop {
            match self.run_batch(folder, name, up, &mut st, &mut state).await {
                Ok(file) => return Ok(file),
                Err(Stop::NewBatch(why)) if restarts < 3 => {
                    restarts += 1;
                    info!("starting a new upload batch for {name} ({why})");
                    st.batch = None;
                    st.file_slug = None;
                    state.set(LAYER, &st);
                    (up.checkpoint)(&state);
                }
                Err(Stop::NewBatch(why)) => {
                    return Err(RemoteError::Server(format!(
                        "upload batch kept failing: {why}"
                    )));
                }
                Err(Stop::Fail(e)) => {
                    up.progress.in_flight.store(0, Ordering::Relaxed);
                    if matches!(e, RemoteError::Integrity(_)) {
                        self.discard_batch(&st).await;
                    }
                    return Err(e);
                }
            }
        }
    }

    /// Hashes the file: whole-file MD5/SHA-256/CRC32 and per-chunk MD5.
    async fn prepare(
        &self,
        up: &ResumableUpload,
        chunk_size: u64,
    ) -> Result<ChunkedState, RemoteError> {
        let len = up.source.len();
        let mut md5 = md5::Md5::new();
        let mut sha = sha2::Sha256::new();
        let mut crc = crc32fast::Hasher::new();
        let mut chunk_md5 = Vec::new();
        let mut offset = 0;
        while offset < len || (len == 0 && chunk_md5.is_empty()) {
            if up.cancel.is_cancelled() {
                return Err(RemoteError::Cancelled);
            }
            let n = chunk_size.min(len - offset) as usize;
            let buf = read_range(&up.source, offset, n).await.map_err(io_err)?;
            md5.update(&buf);
            sha.update(&buf);
            crc.update(&buf);
            chunk_md5.push(hex::encode(md5::Md5::digest(&buf)));
            offset += n as u64;
            if len == 0 {
                break;
            }
        }
        Ok(ChunkedState {
            v: 1,
            len,
            chunk_size,
            md5: hex::encode(md5.finalize()),
            sha256: hex::encode(sha.finalize()),
            crc32: crc.finalize(),
            chunk_md5,
            batch: None,
            file_slug: None,
        })
    }

    async fn new_link(
        &self,
        private_slug: Option<&str>,
    ) -> Result<api::CreateChunkedLinkResponse, RemoteError> {
        let s = self.session().await?;
        self.call(
            Method::POST,
            "/v6/upload/link",
            &[],
            Some(&api::CreateChunkedLinkRequest {
                user_login: &s.login,
                private_slug,
            }),
        )
        .await
    }

    /// One attempt with one batch: get/renew the batch, register, send missing chunks, wait
    /// for assembly, verify and commit.
    async fn run_batch(
        &self,
        folder: &str,
        name: &str,
        up: &ResumableUpload,
        st: &mut ChunkedState,
        state: &mut ResumeState,
    ) -> Result<api::File, Stop> {
        let save = |st: &ChunkedState, state: &mut ResumeState| {
            state.set(LAYER, st);
            (up.checkpoint)(state);
        };

        // 0. Already assembled by an earlier attempt (maybe even committed, with the reply or
        //    our record of it lost): don't touch the batch, just verify and commit/adopt.
        if let (Some(slug), Some(batch)) = (st.file_slug.clone(), st.batch.clone()) {
            up.progress.set_phase(UploadPhase::Verifying);
            up.progress
                .confirmed
                .store(up.outer(st.len), Ordering::Relaxed);
            return self
                .verify_and_commit(folder, name, up, st, &batch, &slug)
                .await;
        }

        // 1. A batch with a valid signature.
        let (signature, mut valid_until) = match &st.batch {
            None => {
                let link = match self.new_link(None).await {
                    Ok(l) => l,
                    Err(e @ (RemoteError::NotFound(_) | RemoteError::Other(_))) => {
                        self.mark_chunked_unavailable(&e.to_string());
                        return Err(Stop::Fail(RemoteError::Other(format!(
                            "chunked uploads unavailable: {e}"
                        ))));
                    }
                    Err(e) => return Err(e.into()),
                };
                st.batch = Some(Batch {
                    private_slug: link.private_slug.clone(),
                    base_url: link
                        .upload_resumable_base_url
                        .trim_end_matches('/')
                        .to_string(),
                    batch_file_id: 1,
                    registered: false,
                    confirmed_chunks: 0,
                });
                save(st, state);
                (link.upload_signature, expiry(link.validity_interval))
            }
            Some(b) => {
                let link = match self.new_link(Some(&b.private_slug)).await {
                    Ok(l) => l,
                    Err(e) if e.is_transient() || matches!(e, RemoteError::Auth(_)) => {
                        return Err(e.into());
                    }
                    Err(e) => {
                        return Err(Stop::NewBatch(format!("renewing the batch failed: {e}")));
                    }
                };
                let base = link
                    .upload_resumable_base_url
                    .trim_end_matches('/')
                    .to_string();
                if let Err(e) = self
                    .extend(&base, &b.private_slug, &link.upload_signature)
                    .await
                {
                    if e.is_transient() {
                        return Err(e.into());
                    }
                    return Err(Stop::NewBatch(format!("extending the batch failed: {e}")));
                }
                if let Some(b) = st.batch.as_mut() {
                    b.base_url = base;
                }
                save(st, state);
                (link.upload_signature, expiry(link.validity_interval))
            }
        };
        let batch = st.batch.clone().expect("batch set above");

        // 2. Where is the file on the server?
        let missing = match self.status(&batch).await? {
            ServerState::Finished(slug) => {
                st.file_slug = Some(slug);
                save(st, state);
                Vec::new()
            }
            ServerState::Failed(s) if s == "bad-crc32" => {
                return Err(Stop::Fail(RemoteError::Integrity(
                    "the server's CRC32 of the uploaded file didn't match".into(),
                )));
            }
            ServerState::Failed(s) => {
                return Err(Stop::NewBatch(format!("server reported status {s}")));
            }
            ServerState::NotRegistered => {
                let req = api::RegisterChunkedRequest {
                    batch_file_id: batch.batch_file_id,
                    upload_signature: &signature,
                    name,
                    filesize: st.len,
                    chunksize: st.chunk_size,
                    crc32: Some(st.crc32),
                };
                match self.register(&batch.base_url, &req).await {
                    Ok(()) => {}
                    Err(e) if e.is_transient() => return Err(e.into()),
                    Err(e)
                        if !batch.registered
                            && st.batch.as_ref().is_some_and(|b| b.confirmed_chunks == 0) =>
                    {
                        // A brand-new batch that can't register: the API isn't usable for us.
                        self.mark_chunked_unavailable(&e.to_string());
                        return Err(Stop::Fail(RemoteError::Other(format!(
                            "chunked uploads unavailable: {e}"
                        ))));
                    }
                    Err(e) => return Err(Stop::NewBatch(format!("registering failed: {e}"))),
                }
                if let Some(b) = st.batch.as_mut() {
                    b.registered = true;
                    b.confirmed_chunks = 0;
                }
                save(st, state);
                (1..=st.chunk_count()).collect()
            }
            ServerState::InProgress(missing) => missing,
        };

        // 3. Send what's missing.
        let missing_set: std::collections::HashSet<u32> = missing.iter().copied().collect();
        let confirmed = st.bytes_of((1..=st.chunk_count()).filter(|n| !missing_set.contains(n)));
        up.progress
            .confirmed
            .store(up.outer(confirmed), Ordering::Relaxed);
        up.progress.in_flight.store(0, Ordering::Relaxed);
        if st.file_slug.is_none() && !missing.is_empty() {
            up.progress.set_phase(UploadPhase::Sending);
            self.send_chunks(up, st, state, &batch, &missing, &mut valid_until)
                .await?;
        }

        // 4. Wait for the server to assemble (and CRC-check) the file.
        up.progress.set_phase(UploadPhase::Verifying);
        let slug = match st.file_slug.clone() {
            Some(s) => s,
            None => {
                let slug = self.wait_finished(up, &batch).await?;
                st.file_slug = Some(slug.clone());
                save(st, state);
                slug
            }
        };
        up.progress
            .confirmed
            .store(up.outer(st.len), Ordering::Relaxed);
        up.progress.in_flight.store(0, Ordering::Relaxed);

        self.verify_and_commit(folder, name, up, st, &batch, &slug)
            .await
    }

    /// Checks the assembled file's MD5 on the server, then sets its properties and commits.
    async fn verify_and_commit(
        &self,
        folder: &str,
        name: &str,
        up: &ResumableUpload,
        st: &ChunkedState,
        batch: &Batch,
        slug: &str,
    ) -> Result<api::File, Stop> {
        let slug = slug.to_string();
        let token = format!("{}:{}", batch.private_slug, batch.batch_file_id);
        match self.hash_verify(&slug, &token, &st.md5, st.crc32).await {
            Ok(Some(false)) => {
                return Err(Stop::Fail(RemoteError::Integrity(
                    "the server's MD5 of the uploaded file didn't match".into(),
                )));
            }
            Ok(Some(true)) => debug!("server confirmed the MD5 of {name}"),
            Ok(None) => {
                debug!("hash verification unavailable for {name}; CRC32 was checked on assembly")
            }
            Err(e) if e.is_transient() => return Err(e.into()),
            Err(e) => debug!("hash verification failed ({e}); CRC32 was checked on assembly"),
        }
        let description = meta::encode(&meta::RcloneMeta {
            md5: hex::decode(&st.md5).unwrap_or_default(),
            sha256: hex::decode(&st.sha256).unwrap_or_default(),
            mtime_micros: up.mtime_ms.saturating_mul(1000),
        });
        match self
            .finalize(
                &batch.private_slug,
                batch.batch_file_id,
                &slug,
                folder,
                name,
                &description,
            )
            .await
        {
            Ok(file) => Ok(file),
            Err(e) if e.is_transient() || matches!(e, RemoteError::Auth(_)) => Err(e.into()),
            Err(e) => {
                // An earlier attempt may have committed it and lost the reply: adopt that file
                // rather than uploading a duplicate.
                match self.file_private(&slug).await {
                    Ok(f) if !f.is_in_trash => {
                        info!("{name} was already committed; adopting it");
                        Ok(f)
                    }
                    _ => Err(Stop::NewBatch(format!("committing failed: {e}"))),
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn send_chunks(
        &self,
        up: &ResumableUpload,
        st: &mut ChunkedState,
        state: &mut ResumeState,
        batch: &Batch,
        missing: &[u32],
        valid_until: &mut Instant,
    ) -> Result<(), Stop> {
        let url_base = format!(
            "{}/v1/chunked-file-upload/{}/{}",
            batch.base_url, batch.private_slug, batch.batch_file_id
        );
        let cap = (MEMORY_BUDGET / st.chunk_size).max(1) as usize;
        let plan = Arc::new(Plan {
            chunk_size: st.chunk_size,
            len: st.len,
            chunk_md5: st.chunk_md5.clone(),
        });
        let mut queue: VecDeque<u32> = missing.iter().copied().collect();
        let mut running = FuturesUnordered::new();
        let mut confirmed_count = st.chunk_count() - missing.len() as u32;
        loop {
            // Keep the signature fresh on long uploads.
            if Instant::now() + Duration::from_secs(300) > *valid_until {
                let link = self.new_link(Some(&batch.private_slug)).await?;
                self.extend(&batch.base_url, &batch.private_slug, &link.upload_signature)
                    .await
                    .map_err(|e| {
                        if e.is_transient() {
                            Stop::Fail(e)
                        } else {
                            Stop::NewBatch(e.to_string())
                        }
                    })?;
                *valid_until = expiry(link.validity_interval);
            }
            // Start chunks while streams are available.
            while let Some(&n) = queue.front() {
                let permit = tokio::select! {
                    biased;
                    p = self.chunk_limit.acquire(running.len(), cap) => p,
                    _ = futures::future::ready(()), if !running.is_empty() => break,
                };
                queue.pop_front();
                running.push(self.send_one_chunk(up, plan.clone(), &url_base, n, permit));
            }
            if running.is_empty() {
                return Ok(());
            }
            let next = tokio::select! {
                r = running.next() => r,
                _ = up.cancel.cancelled() => {
                    drop(running);
                    up.progress.in_flight.store(0, Ordering::Relaxed);
                    return Err(Stop::Fail(RemoteError::Cancelled));
                }
            };
            match next {
                Some(Ok(n)) => {
                    confirmed_count += 1;
                    up.progress
                        .confirmed
                        .fetch_add(up.outer(st.chunk_len(n)), Ordering::Relaxed);
                    if let Some(b) = st.batch.as_mut() {
                        b.confirmed_chunks = confirmed_count;
                    }
                    state.set(LAYER, &*st);
                    (up.checkpoint)(state);
                }
                Some(Err(e)) => {
                    drop(running);
                    up.progress.in_flight.store(0, Ordering::Relaxed);
                    return Err(e);
                }
                None => return Ok(()),
            }
        }
    }

    async fn send_one_chunk(
        &self,
        up: &ResumableUpload,
        plan: Arc<Plan>,
        url_base: &str,
        n: u32,
        _permit: LimitPermit<'_>,
    ) -> Result<u32, Stop> {
        let offset = (n as u64 - 1) * plan.chunk_size;
        let len = plan.chunk_size.min(plan.len - offset) as usize;
        let data = read_range(&up.source, offset, len)
            .await
            .map_err(|e| Stop::Fail(io_err(e)))?;
        let expected = plan
            .chunk_md5
            .get(n as usize - 1)
            .map(String::as_str)
            .unwrap_or("");
        if hex::encode(md5::Md5::digest(&data)) != expected {
            return Err(Stop::Fail(RemoteError::Integrity(format!(
                "the file changed while uploading (chunk {n})"
            ))));
        }
        let data = Bytes::from(data);
        let url = format!("{url_base}/{n}");
        let mut attempt = 0;
        loop {
            attempt += 1;
            let started = Instant::now();
            match self.post_chunk(&url, data.clone(), up).await {
                Ok(()) => {
                    self.chunk_limit.success(len as u64, Instant::now());
                    debug!("chunk {n} sent in {:?}", started.elapsed());
                    return Ok(n);
                }
                Err(e) if attempt < CHUNK_RETRIES && (e.is_transient()) => {
                    self.chunk_limit.backoff(Instant::now());
                    let wait = Duration::from_millis(500 * (1 << attempt.min(5)));
                    debug!("chunk {n} failed ({e}); retrying in {wait:?}");
                    tokio::select! {
                        _ = tokio::time::sleep(wait) => {}
                        _ = up.cancel.cancelled() => return Err(Stop::Fail(RemoteError::Cancelled)),
                    }
                }
                Err(e) if e.is_transient() => return Err(Stop::Fail(e)),
                // 4xx on a chunk: the batch is no longer accepting data (expired/discarded).
                Err(e) => return Err(Stop::NewBatch(format!("chunk {n} refused: {e}"))),
            }
        }
    }

    /// Sends one chunk, paced by the meter and guarded by the stall watchdog.
    async fn post_chunk(
        &self,
        url: &str,
        data: Bytes,
        up: &ResumableUpload,
    ) -> Result<(), RemoteError> {
        const PIECE: usize = 256 * 1024;
        let total = data.len() as u64;
        let sent = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let (meter, progress, scale) = (up.meter.clone(), up.progress.clone(), up.scale);
        let counter = sent.clone();
        let pieces: Vec<Bytes> = (0..data.len())
            .step_by(PIECE)
            .map(|i| data.slice(i..(i + PIECE).min(data.len())))
            .collect();
        let body = futures::stream::iter(pieces).then(move |piece| {
            let (meter, progress, counter) = (meter.clone(), progress.clone(), counter.clone());
            async move {
                meter.consume(piece.len()).await;
                counter.fetch_add(piece.len() as u64, Ordering::Relaxed);
                progress.in_flight.fetch_add(
                    (piece.len() as f64 * scale).round() as u64,
                    Ordering::Relaxed,
                );
                Ok::<Bytes, std::io::Error>(piece)
            }
        });
        let send = self
            .upload
            .post(url)
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .header(header::CONTENT_LENGTH, total)
            .body(reqwest::Body::wrap_stream(body))
            .send();
        let undo = |progress: &crate::remote::resumable::Progress| {
            let n = (sent.load(Ordering::Relaxed) as f64 * scale).round() as u64;
            let _ = progress
                .in_flight
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                    Some(v.saturating_sub(n))
                });
        };
        tokio::pin!(send);
        let (mut seen, mut since) = (0u64, Instant::now());
        let resp = loop {
            tokio::select! {
                r = &mut send => break r.map_err(transfer_err),
                _ = tokio::time::sleep(Duration::from_millis(500).min(self.cfg.stall_timeout)) => {
                    let s = sent.load(Ordering::Relaxed);
                    if s != seen {
                        (seen, since) = (s, Instant::now());
                    }
                    let limit = if s >= total { self.cfg.response_wait } else { self.cfg.stall_timeout };
                    if since.elapsed() > limit {
                        break Err(RemoteError::Server(format!(
                            "chunk upload stalled ({s} of {total} bytes sent, no progress for {} s)",
                            limit.as_secs()
                        )));
                    }
                }
            }
        };
        let result = match resp {
            Ok(resp) => {
                let status = resp.status();
                let text = tokio::time::timeout(self.cfg.response_wait, resp.text())
                    .await
                    .map_err(|_| RemoteError::Server("no response after a chunk".into()))
                    .and_then(|r| r.map_err(transfer_err));
                match text {
                    Ok(text) => check_body(status, &text),
                    Err(e) => Err(e),
                }
            }
            Err(e) => Err(e),
        };
        // The chunk's bytes move from "in flight" to "confirmed" in the caller on success.
        undo(&up.progress);
        result
    }

    async fn status(&self, b: &Batch) -> Result<ServerState, Stop> {
        let url = format!(
            "{}/v1/chunked-file-upload/{}/{}/status",
            b.base_url, b.private_slug, b.batch_file_id
        );
        let resp = self
            .upload
            .get(&url)
            .timeout(Duration::from_secs(60))
            .send()
            .await
            .map_err(|e| Stop::Fail(transfer_err(e)))?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| Stop::Fail(transfer_err(e)))?;
        if status == StatusCode::BAD_REQUEST || status == StatusCode::NOT_FOUND {
            return Ok(ServerState::NotRegistered);
        }
        if !status.is_success() {
            let err: api::ApiError = serde_json::from_str(&text).unwrap_or_default();
            let e = map_status(status, &err, &text);
            return Err(if e.is_transient() {
                Stop::Fail(e)
            } else {
                Stop::NewBatch(format!("status: {e}"))
            });
        }
        let s: api::ChunkedStatus = serde_json::from_str(&text)
            .map_err(|e| Stop::Fail(RemoteError::Other(format!("bad upload status: {e}"))))?;
        Ok(match s.status.as_str() {
            "finished" => match s.irpc {
                Some(r) => ServerState::Finished(r.slug),
                None => ServerState::Failed("finished without a file".into()),
            },
            "pending" | "processing" | "" => ServerState::InProgress(s.missing_chunks),
            other => ServerState::Failed(other.to_string()),
        })
    }

    async fn wait_finished(&self, up: &ResumableUpload, b: &Batch) -> Result<String, Stop> {
        let deadline = Instant::now() + self.cfg.response_wait;
        let mut wait = self.cfg.status_poll;
        loop {
            match self.status(b).await? {
                ServerState::Finished(slug) => return Ok(slug),
                ServerState::Failed(s) if s == "bad-crc32" => {
                    return Err(Stop::Fail(RemoteError::Integrity(
                        "the server's CRC32 of the uploaded file didn't match".into(),
                    )));
                }
                ServerState::Failed(s) => {
                    return Err(Stop::NewBatch(format!("server reported status {s}")));
                }
                ServerState::NotRegistered => {
                    return Err(Stop::NewBatch(
                        "the upload disappeared from the server".into(),
                    ));
                }
                ServerState::InProgress(missing) if !missing.is_empty() => {
                    // Lost chunks (e.g. a reply that never arrived); the next attempt resends them.
                    return Err(Stop::Fail(RemoteError::Server(format!(
                        "the server is missing {} chunk(s)",
                        missing.len()
                    ))));
                }
                ServerState::InProgress(_) => {}
            }
            if Instant::now() > deadline {
                return Err(Stop::Fail(RemoteError::Server(
                    "the server didn't finish assembling the file".into(),
                )));
            }
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = up.cancel.cancelled() => return Err(Stop::Fail(RemoteError::Cancelled)),
            }
            wait = (wait * 2).min(Duration::from_secs(10));
        }
    }

    async fn register(
        &self,
        base: &str,
        req: &api::RegisterChunkedRequest<'_>,
    ) -> Result<(), RemoteError> {
        let resp = self
            .upload
            .post(format!("{base}/v1/chunked-file-upload"))
            .json(req)
            .timeout(Duration::from_secs(60))
            .send()
            .await
            .map_err(transfer_err)?;
        let status = resp.status();
        let text = resp.text().await.map_err(transfer_err)?;
        check_body(status, &text)
    }

    async fn extend(
        &self,
        base: &str,
        private_slug: &str,
        signature: &str,
    ) -> Result<(), RemoteError> {
        let resp = self
            .upload
            .post(format!("{base}/v1/chunked-file-upload/{private_slug}"))
            .json(&api::ExtendChunkedRequest {
                upload_signature: signature,
            })
            .timeout(Duration::from_secs(60))
            .send()
            .await
            .map_err(transfer_err)?;
        let status = resp.status();
        let text = resp.text().await.map_err(transfer_err)?;
        check_body(status, &text)
    }

    /// `Ok(None)` when the server can't tell (endpoint unavailable for this file).
    async fn hash_verify(
        &self,
        slug: &str,
        upload_token: &str,
        md5: &str,
        crc32: u32,
    ) -> Result<Option<bool>, RemoteError> {
        let s = self.session().await?;
        let resp = self
            .api
            .get(format!("{}/v5/file/{slug}/hash-verify", self.cfg.api_root))
            .query(&[("md5", md5.to_string()), ("crc32", crc32.to_string())])
            .header("X-Auth-Token", &self.cfg.app_token)
            .header("X-User-Token", &s.token)
            .header("X-Upload-Token", upload_token)
            .send()
            .await
            .map_err(net)?;
        let status = resp.status();
        let text = resp.text().await.map_err(net)?;
        if status.is_success() {
            let v: api::HashVerifyResponse = serde_json::from_str(&text).unwrap_or_default();
            return Ok(v.md5.or(v.crc32));
        }
        let err: api::ApiError = serde_json::from_str(&text).unwrap_or_default();
        let e = map_status(status, &err, &text);
        if e.is_transient() { Err(e) } else { Ok(None) }
    }

    /// Sets name, folder and metadata of an uploaded file and commits its batch.
    pub(super) async fn finalize(
        &self,
        private_slug: &str,
        batch_file_id: u32,
        file_slug: &str,
        folder: &str,
        name: &str,
        description: &str,
    ) -> Result<api::File, RemoteError> {
        let s = self.session().await?;
        let mut tokens = HashMap::new();
        tokens.insert(file_slug, format!("{private_slug}:{batch_file_id}"));
        let req = api::BatchUpdateFilePropertiesRequest {
            name,
            folder_slug: folder,
            description,
            slugs: vec![file_slug],
            upload_tokens: tokens,
        };
        let files: Vec<api::File> = self
            .call(Method::PATCH, "/v8/file-list/private", &[], Some(&req))
            .await?;
        let _: serde::de::IgnoredAny = self
            .call(
                Method::PATCH,
                &format!("/v8/upload-batch/private/{private_slug}"),
                &[],
                Some(&api::CommitUploadBatchRequest {
                    status: "confirmed",
                    owner_login: &s.login,
                }),
            )
            .await?;
        let mut file = files
            .into_iter()
            .next()
            .ok_or_else(|| RemoteError::Other("upload commit returned no file".into()))?;
        if file.folder_slug.is_none() {
            file.folder_slug = Some(folder.to_string());
        }
        if file.description.as_deref().unwrap_or("").is_empty() {
            file.description = Some(description.to_string());
        }
        Ok(file)
    }

    /// Details of one of the user's files.
    pub async fn file_private(&self, slug: &str) -> Result<api::File, RemoteError> {
        self.call::<api::File, ()>(Method::GET, &format!("/v7/file/{slug}/private"), &[], None)
            .await
    }

    /// Best effort: deletes a registered, unfinished upload from the resumable host.
    pub(super) async fn discard_batch(&self, st: &ChunkedState) {
        let Some(b) = &st.batch else { return };
        if st.file_slug.is_some() || !b.registered {
            return;
        }
        let url = format!(
            "{}/v1/chunked-file-upload/{}/{}",
            b.base_url, b.private_slug, b.batch_file_id
        );
        if let Err(e) = self
            .upload
            .delete(&url)
            .timeout(Duration::from_secs(30))
            .send()
            .await
        {
            debug!("discarding upload {} failed: {e}", b.private_slug);
        }
    }
}

fn io_err(e: std::io::Error) -> RemoteError {
    RemoteError::Other(format!("reading the file failed: {e}"))
}

fn expiry(interval: Option<i64>) -> Instant {
    let secs = interval.filter(|s| *s > 0).unwrap_or(3600) as u64;
    Instant::now() + Duration::from_secs(secs)
}

/// The resumable host reports errors both as HTTP status and as `return_code` in the body.
fn check_body(status: StatusCode, text: &str) -> Result<(), RemoteError> {
    #[derive(Deserialize, Default)]
    struct Body {
        #[serde(default)]
        return_code: Option<u16>,
        #[serde(default)]
        message: Option<String>,
    }
    let body: Body = serde_json::from_str(text).unwrap_or_default();
    let code = match body.return_code {
        Some(c) if c >= 400 && status.is_success() => {
            StatusCode::from_u16(c).unwrap_or(StatusCode::BAD_REQUEST)
        }
        _ => status,
    };
    if code.is_success() {
        return Ok(());
    }
    let err = api::ApiError {
        message: body.message.unwrap_or_default(),
        ..Default::default()
    };
    Err(map_status(code, &err, text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_sizes() {
        assert_eq!(chunk_size_for(5 * MIB), 4 * MIB);
        assert_eq!(chunk_size_for(256 * MIB), 4 * MIB);
        assert_eq!(chunk_size_for(1600 * MIB), 32 * MIB);
        assert_eq!(chunk_size_for(20 * 1024 * MIB), 128 * MIB);
    }

    #[test]
    fn chunk_lengths() {
        let st = ChunkedState {
            v: 1,
            len: 10,
            chunk_size: 4,
            md5: String::new(),
            sha256: String::new(),
            crc32: 0,
            chunk_md5: vec![],
            batch: None,
            file_slug: None,
        };
        assert_eq!(st.chunk_count(), 3);
        assert_eq!((st.chunk_len(1), st.chunk_len(3)), (4, 2));
        assert_eq!(st.bytes_of([1, 3]), 6);
    }

    #[test]
    fn aimd_grows_on_success_and_halves_on_pushback() {
        let lim = AdaptiveLimit::new(2, 6);
        let t0 = Instant::now();
        // Throughput keeps growing with more streams: the limit climbs.
        let mut t = t0;
        for i in 0..40u64 {
            t += Duration::from_millis(200);
            lim.success(1_000_000 * (1 + i / 4), t);
        }
        let grown = lim.limit();
        assert!(grown > 2, "limit should grow, is {grown}");
        lim.backoff(t);
        assert_eq!(lim.limit(), (grown / 2).max(1));
        // No increases during the cooldown after a backoff.
        for _ in 0..20 {
            t += Duration::from_millis(200);
            lim.success(1_000_000, t);
        }
        assert_eq!(lim.limit(), (grown / 2).max(1));
    }

    #[test]
    fn aimd_steps_back_without_gain() {
        let lim = AdaptiveLimit::new(2, 6);
        let mut t = Instant::now();
        // Constant throughput: one probe up, then back down and stop probing.
        for _ in 0..30 {
            t += Duration::from_secs(1);
            lim.success(1_000_000, t);
        }
        assert_eq!(lim.limit(), 2);
    }

    #[test]
    fn body_return_codes() {
        assert!(check_body(StatusCode::OK, r#"{"message":"Done","return_code":201}"#).is_ok());
        assert!(check_body(StatusCode::OK, r#"{"message":"expired","return_code":410}"#).is_err());
        assert!(matches!(
            check_body(StatusCode::SERVICE_UNAVAILABLE, ""),
            Err(RemoteError::Server(_))
        ));
    }

    #[test]
    fn confirmed_from_state() {
        let mut s = ResumeState::default();
        s.set(
            LAYER,
            &ChunkedState {
                v: 1,
                len: 10,
                chunk_size: 4,
                md5: String::new(),
                sha256: String::new(),
                crc32: 0,
                chunk_md5: vec![],
                batch: Some(Batch {
                    private_slug: "p".into(),
                    base_url: "b".into(),
                    batch_file_id: 1,
                    registered: true,
                    confirmed_chunks: 2,
                }),
                file_slug: None,
            },
        );
        assert_eq!(confirmed_bytes(&s), Some((8, 10)));
    }
}
