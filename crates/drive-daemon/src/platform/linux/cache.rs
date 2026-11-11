//! The content cache: one sparse file per node (`<cache_dir>/<node id>`).
//!
//! A file's [`Entry`] exists while something uses it (open handles, a download, a pin); idle
//! entries are dropped so the daemon doesn't hold a descriptor per file ever read. Downloads
//! are sequential, so a partially downloaded cache file always holds a prefix of the content
//! and its length says how much of it is there.
//!
//! Content is *complete* when the whole file is in the cache: downloaded, created locally or
//! changed locally (dirty). Only complete content can be written to.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use drive_core::db::{Db, Node, NodeId, now_ms};
use drive_core::engine::local::{LocalItem, UploadSource};
use drive_core::remote::{Kind, read_all};
use fuser::Errno;
use futures::StreamExt;
use tokio::sync::watch;
use tracing::{debug, info, warn};

use super::Core;

/// A download that makes no progress for this long fails (the reader gets `EIO`).
pub const STALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Deadline for fetching a range directly.
pub const RANGE_TIMEOUT: Duration = Duration::from_secs(60);
/// Reads starting further than this beyond the downloaded prefix are fetched directly instead
/// of waiting for the sequential download to get there.
pub const FAR_AHEAD: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, Default)]
pub struct Progress {
    /// Length of the downloaded prefix.
    pub have: u64,
    pub complete: bool,
    /// Incremented whenever a download fails (or the content is discarded).
    pub failures: u64,
}

#[derive(Debug)]
pub struct State {
    pub complete: bool,
    /// Downloaded prefix (meaningful while not complete).
    pub have: u64,
    /// Size of the cloud version being downloaded.
    pub remote_size: u64,
    pub downloading: bool,
    /// Download generation: bumped to cancel a running download.
    pub run: u64,
    /// Open handles and internal users (downloads, pins).
    pub users: u32,
    pub writers: u32,
    /// Written since the engine was last told.
    pub dirty: bool,
    /// Current size and mtime once changed locally (`local`), overriding the database.
    pub size: u64,
    pub mtime_ms: i64,
    pub local: bool,
    /// The node was deleted or its content replaced; the cache file is gone.
    pub removed: bool,
}

pub struct Entry {
    pub id: NodeId,
    pub file: File,
    pub st: Mutex<State>,
    pub progress: watch::Sender<Progress>,
}

impl Entry {
    /// Reads up to `len` bytes at `offset` from the cache file (short at end of file).
    pub fn read_at(&self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let mut buf = vec![0u8; len];
        let mut done = 0;
        while done < len {
            match self.file.read_at(&mut buf[done..], offset + done as u64) {
                Ok(0) => break,
                Ok(n) => done += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        buf.truncate(done);
        Ok(buf)
    }
}

pub struct Cache {
    dir: PathBuf,
    entries: Mutex<HashMap<NodeId, Arc<Entry>>>,
    /// Stamp of the last local write per file, to tell whether a file changed during an upload.
    last_write: Mutex<HashMap<NodeId, u64>>,
    upload_marks: Mutex<HashMap<NodeId, u64>>,
    write_counter: AtomicU64,
}

impl Cache {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            entries: Mutex::new(HashMap::new()),
            last_write: Mutex::new(HashMap::new()),
            upload_marks: Mutex::new(HashMap::new()),
            write_counter: AtomicU64::new(1),
        }
    }

    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }

    fn path(&self, id: NodeId) -> PathBuf {
        self.dir.join(id.to_string())
    }

    /// Brings the cache directory and the database in line after a (re)start: partial
    /// downloads and orphans are deleted, hydration flags fixed.
    pub fn reconcile(&self, db: &Db) -> io::Result<()> {
        let nodes: HashMap<NodeId, Node> = db
            .all_nodes()
            .map_err(io::Error::other)?
            .into_iter()
            .filter(|n| n.kind == Kind::File && !n.deleted)
            .map(|n| (n.id, n))
            .collect();
        let mut present = std::collections::HashSet::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(id) = name.to_str().and_then(|s| s.parse::<NodeId>().ok()) else {
                continue;
            };
            let len = entry.metadata()?.len();
            let keep = match nodes.get(&id) {
                Some(n) if n.dirty || n.remote_id.is_none() => true,
                Some(n) if n.hydrated && len == n.size => true,
                _ => false,
            };
            if keep {
                present.insert(id);
            } else {
                debug!("dropping stale cache file {id}");
                let _ = std::fs::remove_file(entry.path());
            }
        }
        let mut fixed = 0;
        for n in nodes.values() {
            if n.hydrated && n.remote_id.is_some() && !n.dirty && !present.contains(&n.id) {
                db.set_hydrated(n.id, false).map_err(io::Error::other)?;
                fixed += 1;
            } else if n.remote_id.is_none() && !present.contains(&n.id) && n.size > 0 {
                warn!(
                    "the only copy of node {} ({} bytes) is missing from the cache",
                    n.id, n.size
                );
            }
        }
        if fixed > 0 {
            info!("{fixed} files are no longer downloaded");
        }
        Ok(())
    }

    pub fn get(&self, id: NodeId) -> Option<Arc<Entry>> {
        self.entries.lock().unwrap().get(&id).cloned()
    }

    /// Gets (or sets up) the entry for a file and registers a user. `fresh` starts with empty
    /// content (a file just created).
    pub fn acquire(&self, node: &Node, write: bool, fresh: bool) -> io::Result<Arc<Entry>> {
        let mut map = self.entries.lock().unwrap();
        if let Some(e) = map.get(&node.id) {
            let mut st = e.st.lock().unwrap();
            if fresh {
                drop(st);
                // A brand-new node can't have users; drop whatever stale entry is there.
                if let Some(old) = map.remove(&node.id) {
                    old.st.lock().unwrap().removed = true;
                }
            } else {
                st.users += 1;
                if write {
                    st.writers += 1;
                }
                drop(st);
                return Ok(e.clone());
            }
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.path(node.id))?;
        let len = file.metadata()?.len();
        let local = node.dirty || node.remote_id.is_none();
        let (complete, have, size) = if fresh {
            file.set_len(0)?;
            (true, 0, 0)
        } else if local {
            // The cache holds the only copy: whatever is there is the content.
            (true, len, len)
        } else if node.size == 0 {
            (true, 0, 0)
        } else if node.hydrated && len == node.size {
            (true, len, len)
        } else if len <= node.size {
            // A prefix from an earlier read in this session (downloads are sequential).
            (false, len, node.size)
        } else {
            file.set_len(0)?;
            (false, 0, node.size)
        };
        let entry = Arc::new(Entry {
            id: node.id,
            file,
            st: Mutex::new(State {
                complete,
                have,
                remote_size: node.size,
                downloading: false,
                run: 0,
                users: 1,
                writers: u32::from(write),
                dirty: false,
                size,
                mtime_ms: node.mtime_ms,
                local: fresh || (local && len != node.size),
                removed: false,
            }),
            progress: watch::Sender::new(Progress {
                have,
                complete,
                failures: 0,
            }),
        });
        map.insert(node.id, entry.clone());
        Ok(entry)
    }

    /// Unregisters a user; drops the entry once nothing uses it.
    pub fn release(&self, entry: &Arc<Entry>, write: bool) {
        {
            let mut st = entry.st.lock().unwrap();
            st.users = st.users.saturating_sub(1);
            if write {
                st.writers = st.writers.saturating_sub(1);
            }
        }
        self.maybe_close(entry);
    }

    pub fn maybe_close(&self, entry: &Arc<Entry>) {
        let mut map = self.entries.lock().unwrap();
        let idle = {
            let st = entry.st.lock().unwrap();
            st.users == 0 && !st.downloading
        };
        if idle && map.get(&entry.id).is_some_and(|e| Arc::ptr_eq(e, entry)) {
            map.remove(&entry.id);
        }
    }

    /// The node is gone or its content was replaced in the cloud: forget the cached bytes.
    /// Open handles keep the old (unlinked) file.
    pub fn discard(&self, id: NodeId) {
        let mut map = self.entries.lock().unwrap();
        if let Some(e) = map.remove(&id) {
            let mut st = e.st.lock().unwrap();
            st.removed = true;
            st.run += 1;
            st.downloading = false;
            drop(st);
            e.progress.send_modify(|p| p.failures += 1);
        }
        let _ = std::fs::remove_file(self.path(id));
        drop(map);
        self.last_write.lock().unwrap().remove(&id);
        self.upload_marks.lock().unwrap().remove(&id);
    }

    /// Deletes a clean file's cached copy unless it is in use.
    pub fn evict(&self, id: NodeId) -> io::Result<bool> {
        let map = self.entries.lock().unwrap();
        if map.contains_key(&id) {
            return Ok(false);
        }
        match std::fs::remove_file(self.path(id)) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(true),
            Err(e) => Err(e),
        }
    }

    pub fn note_write(&self, id: NodeId) {
        let stamp = self.write_counter.fetch_add(1, Ordering::Relaxed);
        self.last_write.lock().unwrap().insert(id, stamp);
    }

    pub fn open_for_upload(&self, item: &LocalItem) -> io::Result<UploadSource> {
        if let Some(e) = self.get(item.id) {
            let st = e.st.lock().unwrap();
            if st.writers > 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "the file is open for writing",
                ));
            }
        }
        let file = File::open(self.path(item.id))?;
        let size = file.metadata()?.len();
        let stamp = self
            .last_write
            .lock()
            .unwrap()
            .get(&item.id)
            .copied()
            .unwrap_or(0);
        self.upload_marks.lock().unwrap().insert(item.id, stamp);
        Ok(UploadSource {
            file,
            size,
            mtime_ms: item.mtime_ms,
        })
    }

    /// Whether the file is unchanged since `open_for_upload`.
    pub fn uploaded(&self, id: NodeId) -> bool {
        let mark = self.upload_marks.lock().unwrap().remove(&id);
        let last = self
            .last_write
            .lock()
            .unwrap()
            .get(&id)
            .copied()
            .unwrap_or(0);
        let unchanged = mark.is_none_or(|m| m == last);
        let writing = self.get(id).is_some_and(|e| e.st.lock().unwrap().dirty);
        unchanged && !writing
    }
}

impl Core {
    /// Starts downloading the rest of the file into the cache, unless that is under way.
    pub fn start_download(self: &Arc<Self>, entry: &Arc<Entry>) {
        let run = {
            let mut st = entry.st.lock().unwrap();
            if st.complete || st.downloading || st.removed {
                return;
            }
            st.downloading = true;
            st.run += 1;
            st.users += 1;
            st.run
        };
        let me = self.clone();
        let e = entry.clone();
        self.rt.spawn(async move { me.download(e, run).await });
    }

    async fn download(self: Arc<Self>, e: Arc<Entry>, run: u64) {
        let result = self.download_inner(&e, run).await;
        let ok = {
            let mut st = e.st.lock().unwrap();
            st.users = st.users.saturating_sub(1);
            let current = st.run == run;
            if current {
                st.downloading = false;
            }
            match &result {
                Ok(done) => *done && current && !st.removed,
                Err(err) => {
                    if current {
                        warn!("downloading node {} failed: {err}", e.id);
                        drop(st);
                        e.progress.send_modify(|p| p.failures += 1);
                    }
                    false
                }
            }
        };
        // Close first: once the file counts as downloaded it may be evicted right away.
        self.cache.maybe_close(&e);
        if ok && let Some(engine) = self.engine.upgrade() {
            engine.mark_hydrated(e.id, true);
            self.inval.inode_attrs(fuser::INodeNo(e.id as u64));
        }
    }

    /// Returns `Ok(true)` when the content is complete, `Ok(false)` when cancelled.
    async fn download_inner(&self, e: &Arc<Entry>, run: u64) -> io::Result<bool> {
        let (start, size) = {
            let st = e.st.lock().unwrap();
            (st.have, st.remote_size)
        };
        let engine = self
            .engine
            .upgrade()
            .ok_or_else(|| io::Error::other("shutting down"))?;
        let mut off = start;
        if start < size {
            let mut stream = tokio::time::timeout(STALL_TIMEOUT, engine.read(e.id, start, None))
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "the cloud did not answer"))?
                .map_err(io::Error::from)?;
            drop(engine);
            while off < size {
                let chunk = match tokio::time::timeout(STALL_TIMEOUT, stream.next()).await {
                    Err(_) => {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "the download stalled",
                        ));
                    }
                    Ok(None) => break,
                    Ok(Some(c)) => c?,
                };
                let n = (chunk.len() as u64).min(size - off) as usize;
                let mut st = e.st.lock().unwrap();
                if st.run != run || st.removed {
                    return Ok(false);
                }
                e.file.write_all_at(&chunk[..n], off)?;
                off += n as u64;
                st.have = off;
                if off == size {
                    st.complete = true;
                    st.size = size;
                }
                drop(st);
                e.progress.send_modify(|p| {
                    p.have = off;
                    p.complete = off == size;
                });
            }
            if off < size {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the download ended early",
                ));
            }
        }
        let mut st = e.st.lock().unwrap();
        if st.run != run || st.removed {
            return Ok(false);
        }
        st.complete = true;
        st.size = size;
        st.have = size;
        drop(st);
        e.progress.send_modify(|p| {
            p.have = size;
            p.complete = true;
        });
        Ok(true)
    }

    /// Waits until the first `need` bytes are in the cache (starting the download if needed).
    pub async fn wait_for(self: &Arc<Self>, e: &Arc<Entry>, need: u64) -> Result<(), Errno> {
        let mut rx = e.progress.subscribe();
        let failures = rx.borrow().failures;
        self.start_download(e);
        loop {
            {
                let p = rx.borrow_and_update();
                if p.complete || p.have >= need {
                    return Ok(());
                }
                if p.failures > failures {
                    return Err(Errno::EIO);
                }
            }
            if e.st.lock().unwrap().removed {
                return Err(Errno::EIO);
            }
            match tokio::time::timeout(STALL_TIMEOUT * 2, rx.changed()).await {
                Ok(Ok(())) => {}
                _ => return Err(Errno::EIO),
            }
        }
    }

    pub async fn wait_complete(self: &Arc<Self>, e: &Arc<Entry>) -> Result<(), Errno> {
        let size = e.st.lock().unwrap().remote_size;
        self.wait_for(e, size).await?;
        // `have == size` and `complete` are set together, but be precise.
        if e.st.lock().unwrap().complete {
            Ok(())
        } else {
            Err(Errno::EIO)
        }
    }

    /// Fetches a range straight from the cloud (reads far ahead of the download).
    pub async fn read_range(&self, id: NodeId, offset: u64, len: u64) -> Result<Vec<u8>, Errno> {
        let engine = self.engine()?;
        let fetch = async {
            let stream = engine
                .read(id, offset, Some(len))
                .await
                .map_err(io::Error::from)?;
            read_all(stream).await
        };
        match tokio::time::timeout(RANGE_TIMEOUT, fetch).await {
            Ok(Ok(mut data)) => {
                data.truncate(len as usize);
                Ok(data)
            }
            Ok(Err(e)) => {
                debug!("reading node {id} at {offset} failed: {e}");
                Err(Errno::EIO)
            }
            Err(_) => Err(Errno::EIO),
        }
    }

    /// Sets the content length (`size` 0, or the content must be complete). Marks the file
    /// as changed.
    pub fn truncate(&self, e: &Arc<Entry>, size: u64) -> Result<(), Errno> {
        let mut st = e.st.lock().unwrap();
        if !st.complete && size > 0 {
            return Err(Errno::EIO);
        }
        if st.downloading {
            // Cancel: the download task notices the new run and stops writing.
            st.run += 1;
            st.downloading = false;
        }
        e.file.set_len(size).map_err(|err| super::errno_of(&err))?;
        st.complete = true;
        st.have = size;
        st.size = size;
        st.dirty = true;
        st.local = true;
        st.mtime_ms = now_ms();
        drop(st);
        e.progress.send_modify(|p| {
            p.have = size;
            p.complete = true;
        });
        self.cache.note_write(e.id);
        Ok(())
    }

    /// Tells the engine about local changes (it debounces and uploads).
    pub fn report(&self, e: &Arc<Entry>) {
        let (size, mtime) = {
            let mut st = e.st.lock().unwrap();
            if !st.dirty || st.removed {
                return;
            }
            st.dirty = false;
            (st.size, st.mtime_ms)
        };
        let Some(engine) = self.engine.upgrade() else {
            return;
        };
        if let Err(err) = engine.modified(e.id, size, mtime) {
            warn!("recording a change of node {} failed: {err}", e.id);
        }
    }
}
