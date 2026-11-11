//! In-memory [`Remote`] that behaves like uloz.to (stable ids, immutable files, duplicate
//! names allowed, trash) with fault injection. Used by engine tests and the dev backend.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;

use super::{ByteStream, Kind, Remote, RemoteEntry, RemoteError, RemoteTree, UploadBody};

#[derive(Debug, Clone)]
struct Item {
    parent: String,
    kind: Kind,
    name: String,
    data: Bytes,
    mtime_ms: i64,
    trashed: bool,
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
}

pub const ROOT: &str = "root";

impl MemRemote {
    pub fn new() -> Self {
        Self::default()
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
        id == ROOT || items.get(id).is_some_and(|i| i.kind == Kind::Folder && !i.trashed)
    }

    // ---- direct manipulation, simulating another device ------------------------------

    pub fn put_file(&self, parent: &str, name: &str, data: &[u8], mtime_ms: i64) -> String {
        let id = self.new_id();
        self.items.lock().unwrap().insert(
            id.clone(),
            Item { parent: parent.into(), kind: Kind::File, name: name.into(), data: Bytes::copy_from_slice(data), mtime_ms, trashed: false },
        );
        id
    }

    pub fn put_folder(&self, parent: &str, name: &str) -> String {
        let id = self.new_id();
        self.items.lock().unwrap().insert(
            id.clone(),
            Item { parent: parent.into(), kind: Kind::Folder, name: name.into(), data: Bytes::new(), mtime_ms: 0, trashed: false },
        );
        id
    }

    pub fn remove(&self, id: &str) {
        let mut items = self.items.lock().unwrap();
        let mut stack = vec![id.to_string()];
        while let Some(cur) = stack.pop() {
            if let Some(it) = items.get_mut(&cur) {
                it.trashed = true;
            }
            stack.extend(items.iter().filter(|(_, i)| i.parent == cur && !i.trashed).map(|(k, _)| k.clone()));
        }
    }

    pub fn move_item(&self, id: &str, parent: &str, name: &str) {
        let mut items = self.items.lock().unwrap();
        let it = items.get_mut(id).unwrap();
        it.parent = parent.into();
        it.name = name.into();
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
        self.items.lock().unwrap().values().filter(|i| i.trashed).map(|i| i.name.clone()).collect()
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
        let entries = items.iter().filter(|(id, _)| alive(id)).map(|(id, it)| Self::entry(id, it)).collect();
        Ok(RemoteTree { root_id: ROOT.into(), entries, skipped: vec![] })
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

    async fn upload(&self, parent_id: &str, name: &str, mut body: UploadBody) -> Result<RemoteEntry, RemoteError> {
        self.check(format!("upload {name}"))?;
        let mut data = Vec::new();
        while let Some(chunk) = body.stream.next().await {
            data.extend_from_slice(&chunk.map_err(|e| RemoteError::Network(e.to_string()))?);
        }
        if data.len() as u64 != body.size {
            return Err(RemoteError::Integrity(format!("size {} != declared {}", data.len(), body.size)));
        }
        if !Self::exists_folder(&self.items.lock().unwrap(), parent_id) {
            return Err(RemoteError::NotFound(parent_id.into()));
        }
        let id = self.put_file(parent_id, name, &data, body.mtime_ms);
        let items = self.items.lock().unwrap();
        Ok(Self::entry(&id, &items[&id]))
    }

    async fn read(&self, file: &RemoteEntry, offset: u64, len: Option<u64>) -> Result<ByteStream, RemoteError> {
        self.check(String::new())?;
        let items = self.items.lock().unwrap();
        let it = items.get(&file.id).filter(|i| !i.trashed).ok_or_else(|| RemoteError::NotFound(file.id.clone()))?;
        let start = (offset as usize).min(it.data.len());
        let end = match len {
            Some(l) => (start + l as usize).min(it.data.len()),
            None => it.data.len(),
        };
        let data = it.data.slice(start..end);
        // Deliver in uneven chunks to exercise stream handling.
        let chunks: Vec<std::io::Result<Bytes>> = data.chunks(50_000).map(|c| Ok(Bytes::copy_from_slice(c))).collect();
        Ok(Box::pin(futures::stream::iter(chunks)))
    }

    async fn rename(&self, entry: &RemoteEntry, new_parent_id: &str, new_name: &str) -> Result<RemoteEntry, RemoteError> {
        self.check(format!("rename {} -> {new_name}", entry.name))?;
        let mut items = self.items.lock().unwrap();
        if !Self::exists_folder(&items, new_parent_id) {
            return Err(RemoteError::NotFound(new_parent_id.into()));
        }
        let it = items.get_mut(&entry.id).filter(|i| !i.trashed).ok_or_else(|| RemoteError::NotFound(entry.id.clone()))?;
        it.parent = new_parent_id.into();
        it.name = new_name.into();
        Ok(Self::entry(&entry.id, it))
    }

    async fn trash(&self, entry: &RemoteEntry) -> Result<(), RemoteError> {
        self.check(format!("trash {}", entry.name))?;
        if !self.items.lock().unwrap().get(&entry.id).is_some_and(|i| !i.trashed) {
            return Err(RemoteError::NotFound(entry.id.clone()));
        }
        self.remove(&entry.id);
        Ok(())
    }

    async fn exists(&self, entry: &RemoteEntry) -> Result<bool, RemoteError> {
        self.check(String::new())?;
        Ok(self.items.lock().unwrap().get(&entry.id).is_some_and(|i| !i.trashed))
    }
}
