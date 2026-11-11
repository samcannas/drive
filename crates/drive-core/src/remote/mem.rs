//! In-memory [`Remote`] that behaves like uloz.to (stable ids, immutable files, duplicate
//! names allowed, trash) with fault injection. Used by engine tests and the dev backend.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;

use super::{ByteStream, Kind, Remote, RemoteEntry, RemoteError, RemoteTree, UploadBody};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct Item {
    parent: String,
    kind: Kind,
    name: String,
    #[serde(skip)]
    data: Bytes,
    mtime_ms: i64,
    trashed: bool,
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Index {
    next: u64,
    items: HashMap<String, Item>,
}

#[derive(Default)]
pub struct MemRemote {
    items: Mutex<HashMap<String, Item>>,
    next: AtomicU64,
    /// Operations fail with a transient error while this is > 0 (decremented per failure).
    fail_next: AtomicU64,
    /// Every operation fails as offline while set.
    offline: std::sync::atomic::AtomicBool,
    calls: Mutex<Vec<String>>,
    /// When set, the remote is persisted to this directory (`index.json` + `blobs/<id>`).
    dir: Option<std::path::PathBuf>,
    /// Resumable-upload simulation: partial uploads by session id.
    sessions: Mutex<HashMap<String, Vec<u8>>>,
    /// The next resumable upload fails (as a dropped connection) once it has this many bytes.
    fail_upload_at: Mutex<Option<u64>>,
    /// Delay per resumable chunk (to observe uploads in progress).
    chunk_delay_ms: AtomicU64,
    /// Total bytes received by resumable uploads (to check nothing is re-sent).
    received: AtomicU64,
}

/// Chunk size of the resumable simulation.
pub const MEM_CHUNK: u64 = 64 * 1024;
/// Uploads at least this large are resumable.
pub const MEM_RESUMABLE_MIN: u64 = 128 * 1024;

#[derive(serde::Serialize, serde::Deserialize)]
struct MemResume {
    session: String,
    len: u64,
}

pub const ROOT: &str = "root";

impl MemRemote {
    pub fn new() -> Self {
        Self::default()
    }

    /// A remote persisted in `dir` — a stand-in for uloz.to during development
    /// (`DRIVE_DEV_REMOTE=<dir>`).
    pub fn persistent(dir: impl Into<std::path::PathBuf>) -> std::io::Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(dir.join("blobs"))?;
        let index: Index = match std::fs::read(dir.join("index.json")) {
            Ok(b) => serde_json::from_slice(&b).map_err(std::io::Error::other)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Index::default(),
            Err(e) => return Err(e),
        };
        let mut items = index.items;
        for (id, it) in items.iter_mut() {
            if it.kind == Kind::File {
                it.data = std::fs::read(dir.join("blobs").join(id))
                    .map(Bytes::from)
                    .unwrap_or_default();
            }
        }
        Ok(Self {
            items: Mutex::new(items),
            next: AtomicU64::new(index.next),
            dir: Some(dir),
            ..Default::default()
        })
    }

    fn persist(&self, new_blob: Option<(&str, &[u8])>) {
        let Some(dir) = &self.dir else { return };
        if let Some((id, data)) = new_blob {
            let _ = std::fs::write(dir.join("blobs").join(id), data);
        }
        let index = Index {
            next: self.next.load(Ordering::SeqCst),
            items: self.items.lock().unwrap().clone(),
        };
        let tmp = dir.join("index.json.tmp");
        if std::fs::write(&tmp, serde_json::to_vec(&index).unwrap_or_default()).is_ok() {
            let _ = std::fs::rename(&tmp, dir.join("index.json"));
        }
    }

    /// Makes the next resumable upload break off once it has received `bytes`.
    pub fn fail_upload_at(&self, bytes: u64) {
        *self.fail_upload_at.lock().unwrap() = Some(bytes);
    }

    pub fn set_chunk_delay(&self, d: std::time::Duration) {
        self.chunk_delay_ms
            .store(d.as_millis() as u64, Ordering::SeqCst);
    }

    /// Bytes received by resumable uploads so far.
    pub fn received_bytes(&self) -> u64 {
        self.received.load(Ordering::SeqCst)
    }

    pub fn fail_next(&self, n: u64) {
        self.fail_next.store(n, Ordering::SeqCst);
    }

    pub fn set_offline(&self, offline: bool) {
        self.offline.store(offline, Ordering::SeqCst);
    }

    /// Names of the mutating calls made so far, e.g. `upload a.txt`.
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn check(&self, what: String) -> Result<(), RemoteError> {
        if self.offline.load(Ordering::SeqCst) {
            return Err(RemoteError::Network("offline".into()));
        }
        if self.fail_next.load(Ordering::SeqCst) > 0 {
            self.fail_next.fetch_sub(1, Ordering::SeqCst);
            return Err(RemoteError::Server("injected failure".into()));
        }
        if !what.is_empty() {
            self.calls.lock().unwrap().push(what);
        }
        Ok(())
    }

    fn new_id(&self) -> String {
        format!("m{}", self.next.fetch_add(1, Ordering::SeqCst) + 1)
    }

    fn entry(id: &str, it: &Item) -> RemoteEntry {
        RemoteEntry {
            id: id.to_string(),
            parent_id: it.parent.clone(),
            kind: it.kind,
            name: it.name.clone(),
            size: it.data.len() as u64,
            mtime_ms: it.mtime_ms,
        }
    }

    fn exists_folder(items: &HashMap<String, Item>, id: &str) -> bool {
        id == ROOT
            || items
                .get(id)
                .is_some_and(|i| i.kind == Kind::Folder && !i.trashed)
    }

    // ---- direct manipulation, simulating another device ------------------------------

    pub fn put_file(&self, parent: &str, name: &str, data: &[u8], mtime_ms: i64) -> String {
        let id = self.new_id();
        self.items.lock().unwrap().insert(
            id.clone(),
            Item {
                parent: parent.into(),
                kind: Kind::File,
                name: name.into(),
                data: Bytes::copy_from_slice(data),
                mtime_ms,
                trashed: false,
            },
        );
        self.persist(Some((&id, data)));
        id
    }

    pub fn put_folder(&self, parent: &str, name: &str) -> String {
        let id = self.new_id();
        self.items.lock().unwrap().insert(
            id.clone(),
            Item {
                parent: parent.into(),
                kind: Kind::Folder,
                name: name.into(),
                data: Bytes::new(),
                mtime_ms: 0,
                trashed: false,
            },
        );
        self.persist(None);
        id
    }

    pub fn remove(&self, id: &str) {
        let mut items = self.items.lock().unwrap();
        let mut stack = vec![id.to_string()];
        while let Some(cur) = stack.pop() {
            if let Some(it) = items.get_mut(&cur) {
                it.trashed = true;
            }
            stack.extend(
                items
                    .iter()
                    .filter(|(_, i)| i.parent == cur && !i.trashed)
                    .map(|(k, _)| k.clone()),
            );
        }
        drop(items);
        self.persist(None);
    }

    pub fn move_item(&self, id: &str, parent: &str, name: &str) {
        let mut items = self.items.lock().unwrap();
        let it = items.get_mut(id).unwrap();
        it.parent = parent.into();
        it.name = name.into();
        drop(items);
        self.persist(None);
    }

    /// Live (non-trashed) entries as `path -> data` for assertions; folders end with `/`.
    pub fn snapshot(&self) -> std::collections::BTreeMap<String, Vec<u8>> {
        let items = self.items.lock().unwrap();
        let path_of = |id: &str| -> Option<String> {
            let mut parts = vec![];
            let mut cur = id.to_string();
            while cur != ROOT {
                let it = items.get(&cur)?;
                if it.trashed {
                    return None;
                }
                parts.push(it.name.clone());
                cur = it.parent.clone();
            }
            parts.reverse();
            Some(parts.join("/"))
        };
        let mut out = std::collections::BTreeMap::new();
        for (id, it) in items.iter() {
            if it.trashed {
                continue;
            }
            if let Some(p) = path_of(id) {
                if it.kind == Kind::Folder {
                    out.insert(format!("{p}/"), vec![]);
                } else {
                    out.insert(p, it.data.to_vec());
                }
            }
        }
        out
    }

    /// Live items directly under the root, as `(id, name)`.
    pub fn snapshot_ids(&self) -> Vec<(String, String)> {
        self.items
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, i)| i.parent == ROOT && !i.trashed)
            .map(|(id, i)| (id.clone(), i.name.clone()))
            .collect()
    }

    pub fn trashed_names(&self) -> Vec<String> {
        self.items
            .lock()
            .unwrap()
            .values()
            .filter(|i| i.trashed)
            .map(|i| i.name.clone())
            .collect()
    }
}

#[async_trait]
impl Remote for MemRemote {
    async fn scan(&self) -> Result<RemoteTree, RemoteError> {
        self.check(String::new())?;
        let items = self.items.lock().unwrap();
        // Only entries whose whole ancestor chain is alive.
        let alive = |id: &str| -> bool {
            let mut cur = id.to_string();
            while cur != ROOT {
                match items.get(&cur) {
                    Some(it) if !it.trashed => cur = it.parent.clone(),
                    _ => return false,
                }
            }
            true
        };
        let entries = items
            .iter()
            .filter(|(id, _)| alive(id))
            .map(|(id, it)| Self::entry(id, it))
            .collect();
        Ok(RemoteTree {
            root_id: ROOT.into(),
            entries,
            skipped: vec![],
        })
    }

    async fn create_folder(&self, parent_id: &str, name: &str) -> Result<RemoteEntry, RemoteError> {
        self.check(format!("mkdir {name}"))?;
        if !Self::exists_folder(&self.items.lock().unwrap(), parent_id) {
            return Err(RemoteError::NotFound(parent_id.into()));
        }
        let id = self.put_folder(parent_id, name);
        let items = self.items.lock().unwrap();
        Ok(Self::entry(&id, &items[&id]))
    }

    async fn upload(
        &self,
        parent_id: &str,
        name: &str,
        mut body: UploadBody,
    ) -> Result<RemoteEntry, RemoteError> {
        self.check(format!("upload {name}"))?;
        let mut data = Vec::new();
        while let Some(chunk) = body.stream.next().await {
            data.extend_from_slice(&chunk.map_err(|e| RemoteError::Network(e.to_string()))?);
        }
        if data.len() as u64 != body.size {
            return Err(RemoteError::Integrity(format!(
                "size {} != declared {}",
                data.len(),
                body.size
            )));
        }
        if !Self::exists_folder(&self.items.lock().unwrap(), parent_id) {
            return Err(RemoteError::NotFound(parent_id.into()));
        }
        let id = self.put_file(parent_id, name, &data, body.mtime_ms);
        let items = self.items.lock().unwrap();
        Ok(Self::entry(&id, &items[&id]))
    }

    async fn read(
        &self,
        file: &RemoteEntry,
        offset: u64,
        len: Option<u64>,
    ) -> Result<ByteStream, RemoteError> {
        self.check(String::new())?;
        let items = self.items.lock().unwrap();
        let it = items
            .get(&file.id)
            .filter(|i| !i.trashed)
            .ok_or_else(|| RemoteError::NotFound(file.id.clone()))?;
        let start = (offset as usize).min(it.data.len());
        let end = match len {
            Some(l) => (start + l as usize).min(it.data.len()),
            None => it.data.len(),
        };
        let data = it.data.slice(start..end);
        // Deliver in uneven chunks to exercise stream handling.
        let chunks: Vec<std::io::Result<Bytes>> = data
            .chunks(50_000)
            .map(|c| Ok(Bytes::copy_from_slice(c)))
            .collect();
        Ok(Box::pin(futures::stream::iter(chunks)))
    }

    async fn rename(
        &self,
        entry: &RemoteEntry,
        new_parent_id: &str,
        new_name: &str,
    ) -> Result<RemoteEntry, RemoteError> {
        self.check(format!("rename {} -> {new_name}", entry.name))?;
        let mut items = self.items.lock().unwrap();
        if !Self::exists_folder(&items, new_parent_id) {
            return Err(RemoteError::NotFound(new_parent_id.into()));
        }
        let it = items
            .get_mut(&entry.id)
            .filter(|i| !i.trashed)
            .ok_or_else(|| RemoteError::NotFound(entry.id.clone()))?;
        it.parent = new_parent_id.into();
        it.name = new_name.into();
        let e = Self::entry(&entry.id, it);
        drop(items);
        self.persist(None);
        Ok(e)
    }

    async fn trash(&self, entry: &RemoteEntry) -> Result<(), RemoteError> {
        self.check(format!("trash {}", entry.name))?;
        if self
            .items
            .lock()
            .unwrap()
            .get(&entry.id)
            .is_none_or(|i| i.trashed)
        {
            return Err(RemoteError::NotFound(entry.id.clone()));
        }
        self.remove(&entry.id);
        Ok(())
    }

    async fn exists(&self, entry: &RemoteEntry) -> Result<bool, RemoteError> {
        self.check(String::new())?;
        Ok(self
            .items
            .lock()
            .unwrap()
            .get(&entry.id)
            .is_some_and(|i| !i.trashed))
    }

    async fn upload_resumable(
        &self,
        parent_id: &str,
        name: &str,
        up: super::resumable::ResumableUpload,
    ) -> Result<RemoteEntry, RemoteError> {
        use super::resumable::{UploadPhase, read_range};
        let len = up.source.len();
        if len < MEM_RESUMABLE_MIN {
            return super::resumable::upload_whole(self, parent_id, name, up).await;
        }
        self.check(String::new())?;
        let mut state = up.state.clone();
        let session = match state.get::<MemResume>("mem") {
            Some(r) if r.len == len && self.sessions.lock().unwrap().contains_key(&r.session) => {
                r.session
            }
            _ => {
                let id = format!("s{}", self.next.fetch_add(1, Ordering::SeqCst) + 1);
                self.sessions.lock().unwrap().insert(id.clone(), Vec::new());
                state.set(
                    "mem",
                    &MemResume {
                        session: id.clone(),
                        len,
                    },
                );
                (up.checkpoint)(&state);
                id
            }
        };
        up.progress.set_phase(UploadPhase::Sending);
        loop {
            let offset = self
                .sessions
                .lock()
                .unwrap()
                .get(&session)
                .map(|v| v.len() as u64)
                .unwrap_or(0);
            up.progress
                .confirmed
                .store(up.outer(offset), Ordering::Relaxed);
            if offset >= len {
                break;
            }
            if up.cancel.is_cancelled() {
                return Err(RemoteError::Cancelled);
            }
            if self.offline.load(Ordering::SeqCst) {
                return Err(RemoteError::Network("offline".into()));
            }
            {
                let mut fail = self.fail_upload_at.lock().unwrap();
                if fail.is_some_and(|at| offset >= at) {
                    *fail = None;
                    return Err(RemoteError::Network("connection dropped".into()));
                }
            }
            let n = MEM_CHUNK.min(len - offset) as usize;
            up.meter.consume(n).await;
            let delay = self.chunk_delay_ms.load(Ordering::SeqCst);
            if delay > 0 {
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_millis(delay)) => {}
                    _ = up.cancel.cancelled() => return Err(RemoteError::Cancelled),
                }
            }
            let buf = read_range(&up.source, offset, n)
                .await
                .map_err(|e| RemoteError::Other(e.to_string()))?;
            self.received.fetch_add(n as u64, Ordering::SeqCst);
            if let Some(v) = self.sessions.lock().unwrap().get_mut(&session) {
                v.extend_from_slice(&buf);
            }
            (up.checkpoint)(&state);
        }
        up.progress.set_phase(UploadPhase::Verifying);
        let data = self
            .sessions
            .lock()
            .unwrap()
            .remove(&session)
            .unwrap_or_default();
        self.check(format!("upload {name}"))?;
        if !Self::exists_folder(&self.items.lock().unwrap(), parent_id) {
            return Err(RemoteError::NotFound(parent_id.into()));
        }
        let id = self.put_file(parent_id, name, &data, up.mtime_ms);
        let items = self.items.lock().unwrap();
        Ok(Self::entry(&id, &items[&id]))
    }

    async fn discard_resumable(&self, state: &super::resumable::ResumeState) {
        if let Some(r) = state.get::<MemResume>("mem") {
            self.sessions.lock().unwrap().remove(&r.session);
        }
    }

    fn resumable_min_size(&self) -> Option<u64> {
        Some(MEM_RESUMABLE_MIN)
    }

    fn resume_info(
        &self,
        state: &super::resumable::ResumeState,
    ) -> Option<super::resumable::ResumeInfo> {
        let r = state.get::<MemResume>("mem")?;
        let confirmed = self
            .sessions
            .lock()
            .unwrap()
            .get(&r.session)
            .map(|v| v.len() as u64)
            .unwrap_or(0);
        Some(super::resumable::ResumeInfo {
            confirmed,
            total: r.len,
            assembled_id: None,
        })
    }
}
