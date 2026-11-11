//! The sync engine.
//!
//! * The **scanner** lists the remote vault every `poll_interval` (uloz.to has no change
//!   feed), plans how to fold the listing into the local state ([`plan`]) and applies it.
//! * The **worker** pushes local changes (rows with `pending` bits) to the cloud with bounded
//!   parallelism, retries and backoff.
//! * The **evictor** keeps downloaded-but-unpinned content under the cache limit (LRU) and
//!   reacts to low disk space.
//! * Platform layers report local changes through the `local_*` methods and serve file
//!   contents through [`Engine::read`].

pub mod local;
pub mod plan;
pub mod throttle;

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;

use rusqlite::params;
use tokio::sync::{Notify, Semaphore, broadcast, watch};
use tokio_util::io::ReaderStream;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::db::{self, Db, Node, NodeId, ROOT_ID, kind_int, name_key, now_ms, pending};
use crate::ipc::{Activity, ActivityKind, Event, IssueAction, IssueKind, Status, SyncState};
use crate::remote::{ByteStream, Kind, Remote, RemoteEntry, RemoteError, UploadBody};
use local::{LocalItem, LocalSide, is_ignored_name};
use plan::{Change, PlanError, PlanInput};
use throttle::{Throttle, metered};

/// More pending local deletions than this (files) need the user's confirmation.
pub const LOCAL_DELETE_GUARD: u32 = 500;

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub device_name: String,
    pub account: String,
    /// Local folder or mount point (for status and free-space checks).
    pub folder: PathBuf,
    pub cache_limit_bytes: u64,
    pub poll_interval: Duration,
    pub max_parallel_transfers: usize,
    pub upload_limit_kbps: u64,
    pub download_limit_kbps: u64,
    /// Wait this long after the last local change before uploading.
    pub debounce: Duration,
    /// Keep at least this much disk space free by evicting cached files.
    pub min_free_bytes: u64,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            device_name: "This device".into(),
            account: String::new(),
            folder: PathBuf::new(),
            cache_limit_bytes: 10 * 1024 * 1024 * 1024,
            poll_interval: Duration::from_secs(120),
            max_parallel_transfers: 3,
            upload_limit_kbps: 0,
            download_limit_kbps: 0,
            debounce: Duration::from_secs(3),
            min_free_bytes: 2 * 1024 * 1024 * 1024,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Remote(#[from] RemoteError),
    #[error("database: {0}")]
    Db(#[from] rusqlite::Error),
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl From<EngineError> for io::Error {
    fn from(e: EngineError) -> Self {
        match e {
            EngineError::NotFound(p) => io::Error::new(io::ErrorKind::NotFound, p),
            EngineError::Invalid(m) => io::Error::new(io::ErrorKind::InvalidInput, m),
            EngineError::Io(e) => e,
            other => io::Error::other(other.to_string()),
        }
    }
}

pub type Result<T, E = EngineError> = std::result::Result<T, E>;

struct Transfer {
    upload: bool,
    total: u64,
    done: Arc<AtomicU64>,
}

pub struct Engine {
    db: Arc<Db>,
    remote: Arc<dyn Remote>,
    local: OnceLock<Arc<dyn LocalSide>>,
    /// Handle to ourselves for spawning work from `&self` methods; set by `start()`.
    me: OnceLock<std::sync::Weak<Engine>>,
    cfg: RwLock<EngineConfig>,
    status_tx: watch::Sender<Status>,
    events: broadcast::Sender<Event>,
    wake_worker: Notify,
    wake_scanner: Notify,
    wake_evictor: Notify,
    status_changed: Notify,
    cancel: CancellationToken,
    op_seq: AtomicI64,
    paused: AtomicBool,
    offline: AtomicBool,
    signed_out: AtomicBool,
    wrong_password: AtomicBool,
    scanning: AtomicBool,
    in_flight: Mutex<HashSet<NodeId>>,
    orphans_in_flight: Mutex<HashSet<String>>,
    /// Remote ids we trashed, with the op sequence at that time (scans that started earlier
    /// still list them).
    recently_gone: Mutex<HashMap<String, i64>>,
    transfers: Mutex<HashMap<u64, Transfer>>,
    next_transfer: AtomicU64,
    access: Mutex<HashMap<NodeId, i64>>,
    hydrating: Mutex<HashSet<NodeId>>,
    hydrate_permits: Semaphore,
    allow_remote_removal_until: AtomicI64,
    allow_local_deletes_until: AtomicI64,
    held_remote_removals: AtomicU32,
    last_sync_ms: AtomicI64,
    up: Arc<Throttle>,
    down: Arc<Throttle>,
}

impl Engine {
    pub fn new(db: Arc<Db>, remote: Arc<dyn Remote>, cfg: EngineConfig) -> Result<Arc<Self>> {
        let max_seq: i64 = db.read(|c| {
            c.query_row("SELECT COALESCE(MAX(op_seq), 0) FROM node", [], |r| {
                r.get(0)
            })
        })?;
        let last_sync = db
            .kv_get("last_sync_ms")?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let paused = db.kv_get("paused")?.as_deref() == Some("1");
        let (status_tx, _) = watch::channel(Status::default());
        let (events, _) = broadcast::channel(512);
        let engine = Arc::new(Self {
            up: Arc::new(Throttle::new(cfg.upload_limit_kbps)),
            down: Arc::new(Throttle::new(cfg.download_limit_kbps)),
            hydrate_permits: Semaphore::new(cfg.max_parallel_transfers.max(1)),
            db,
            remote,
            local: OnceLock::new(),
            me: OnceLock::new(),
            cfg: RwLock::new(cfg),
            status_tx,
            events,
            wake_worker: Notify::new(),
            wake_scanner: Notify::new(),
            wake_evictor: Notify::new(),
            status_changed: Notify::new(),
            cancel: CancellationToken::new(),
            op_seq: AtomicI64::new(max_seq),
            paused: AtomicBool::new(paused),
            offline: AtomicBool::new(false),
            signed_out: AtomicBool::new(false),
            wrong_password: AtomicBool::new(false),
            scanning: AtomicBool::new(false),
            in_flight: Mutex::new(HashSet::new()),
            orphans_in_flight: Mutex::new(HashSet::new()),
            recently_gone: Mutex::new(HashMap::new()),
            transfers: Mutex::new(HashMap::new()),
            next_transfer: AtomicU64::new(1),
            access: Mutex::new(HashMap::new()),
            hydrating: Mutex::new(HashSet::new()),
            allow_remote_removal_until: AtomicI64::new(0),
            allow_local_deletes_until: AtomicI64::new(0),
            held_remote_removals: AtomicU32::new(0),
            last_sync_ms: AtomicI64::new(last_sync),
        });
        engine.refresh_status();
        Ok(engine)
    }

    /// Connects the platform layer. Must be called once, before [`Engine::start`].
    pub fn set_local(&self, local: Arc<dyn LocalSide>) {
        let _ = self.local.set(local);
    }

    fn local(&self) -> &Arc<dyn LocalSide> {
        self.local
            .get()
            .expect("Engine::set_local must be called before use")
    }

    pub fn db(&self) -> &Arc<Db> {
        &self.db
    }

    pub fn remote(&self) -> &Arc<dyn Remote> {
        &self.remote
    }

    pub fn config(&self) -> EngineConfig {
        self.cfg.read().unwrap().clone()
    }

    pub fn update_config(&self, f: impl FnOnce(&mut EngineConfig)) {
        let mut cfg = self.cfg.write().unwrap();
        f(&mut cfg);
        self.up.set_kbps(cfg.upload_limit_kbps);
        self.down.set_kbps(cfg.download_limit_kbps);
        drop(cfg);
        self.wake_scanner.notify_one();
        self.wake_evictor.notify_one();
        self.status_changed.notify_one();
    }

    /// Spawns the background loops on the current Tokio runtime.
    pub fn start(self: &Arc<Self>) {
        let _ = self.me.set(Arc::downgrade(self));
        assert!(
            self.local.get().is_some(),
            "Engine::set_local must be called before start"
        );
        tokio::spawn(self.clone().scanner_loop());
        tokio::spawn(self.clone().worker_loop());
        tokio::spawn(self.clone().evictor_loop());
        tokio::spawn(self.clone().housekeeping_loop());
        tokio::spawn(self.clone().status_loop());
        self.resume_pins();
    }

    pub fn shutdown(&self) {
        self.cancel.cancel();
        self.flush_access_times();
        self.db.optimize();
    }

    pub fn is_shut_down(&self) -> bool {
        self.cancel.is_cancelled()
    }

    // ---- status & events ---------------------------------------------------------------

    pub fn status(&self) -> Status {
        self.status_tx.borrow().clone()
    }

    pub fn watch_status(&self) -> watch::Receiver<Status> {
        self.status_tx.subscribe()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    fn emit_activity(&self, a: Activity) {
        let _ = self.events.send(Event::Activity(a));
    }

    fn activity(&self, kind: ActivityKind, path: &str, bytes: u64, detail: Option<&str>) {
        match self.db.add_activity(kind, path, bytes, detail) {
            Ok(a) => self.emit_activity(a),
            Err(e) => warn!("recording activity failed: {e}"),
        }
    }

    fn issue(
        &self,
        kind: IssueKind,
        key: &str,
        node: Option<NodeId>,
        path: Option<&str>,
        message: &str,
    ) {
        match self.db.raise_issue(kind, key, node, path, message) {
            Ok(true) => {
                info!("issue: {message}");
                self.issues_changed();
            }
            Ok(false) => {}
            Err(e) => warn!("recording issue failed: {e}"),
        }
    }

    fn clear_issue(&self, key: &str) {
        if self.db.clear_issue(key).unwrap_or(false) {
            self.issues_changed();
        }
    }

    fn issues_changed(&self) {
        let count = self.db.issue_count().unwrap_or(0);
        let _ = self.events.send(Event::IssuesChanged { count });
        self.status_changed.notify_one();
    }

    pub fn refresh_status(&self) {
        let st = self.compute_status();
        let changed = {
            let cur = self.status_tx.borrow();
            !status_eq(&cur, &st)
        };
        if changed {
            self.status_tx.send_replace(st.clone());
            let _ = self.events.send(Event::Status(st));
        }
    }

    fn compute_status(&self) -> Status {
        let cfg = self.cfg.read().unwrap().clone();
        let totals = self.db.storage_totals().unwrap_or_default();
        let pending = self.db.pending_count().unwrap_or(0);
        let issues = self.db.issue_count().unwrap_or(0);
        let (mut up, mut down, mut total, mut done) = (0u32, 0u32, 0u64, 0u64);
        for t in self.transfers.lock().unwrap().values() {
            if t.upload {
                up += 1;
            } else {
                down += 1;
            }
            total += t.total;
            done += t.done.load(Ordering::Relaxed).min(t.total);
        }
        let held = self.held_remote_removals.load(Ordering::Relaxed);
        let local_deletes = self.db.pending_trash_files().unwrap_or(0);
        let delete_confirm = if held > 0 {
            Some(held)
        } else if local_deletes > LOCAL_DELETE_GUARD
            && now_ms() > self.allow_local_deletes_until.load(Ordering::Relaxed)
        {
            Some(local_deletes)
        } else {
            None
        };
        let state = if self.signed_out.load(Ordering::Relaxed) {
            SyncState::SignedOut
        } else if self.wrong_password.load(Ordering::Relaxed) || delete_confirm.is_some() {
            SyncState::Error
        } else if self.offline.load(Ordering::Relaxed) {
            SyncState::Offline
        } else if self.paused.load(Ordering::Relaxed) {
            SyncState::Paused
        } else if up + down > 0 || pending > 0 {
            SyncState::Syncing
        } else if issues > 0 {
            SyncState::Error
        } else {
            SyncState::UpToDate
        };
        let headline = match state {
            SyncState::SignedOut => "Sign in to uloz.to to keep syncing".to_string(),
            SyncState::Error if self.wrong_password.load(Ordering::Relaxed) => {
                "Your encryption password doesn't open this vault".to_string()
            }
            SyncState::Error if delete_confirm.is_some() => {
                format!("Confirm deleting {} files", delete_confirm.unwrap_or(0))
            }
            SyncState::Error => format!(
                "{issues} {} need{} attention",
                plural(issues, "item", "items"),
                if issues == 1 { "s" } else { "" }
            ),
            SyncState::Offline => "Offline. Changes will sync when you reconnect".to_string(),
            SyncState::Paused => "Syncing is paused".to_string(),
            SyncState::Syncing if up > 0 && down == 0 => {
                format!("Uploading {up} {}", plural(up, "file", "files"))
            }
            SyncState::Syncing if down > 0 && up == 0 => {
                format!("Downloading {down} {}", plural(down, "file", "files"))
            }
            SyncState::Syncing if up + down > 0 => format!("Syncing {} files", up + down),
            SyncState::Syncing => {
                format!("Syncing {pending} {}", plural(pending, "change", "changes"))
            }
            SyncState::UpToDate | SyncState::NotSetUp => "Up to date".to_string(),
        };
        let last = self.last_sync_ms.load(Ordering::Relaxed);
        Status {
            state,
            headline,
            account: Some(cfg.account.clone()).filter(|a| !a.is_empty()),
            folder: Some(cfg.folder.display().to_string()),
            uploading: up,
            downloading: down,
            pending,
            transfer_bytes_total: total,
            transfer_bytes_done: done,
            issues,
            cache_bytes: totals.cache_bytes,
            cache_limit_bytes: cfg.cache_limit_bytes,
            pinned_bytes: totals.pinned_bytes,
            cloud_bytes: totals.cloud_bytes,
            cloud_files: totals.cloud_files,
            last_sync_ms: (last > 0).then_some(last),
            pending_delete_confirmation: delete_confirm,
        }
    }

    async fn status_loop(self: Arc<Self>) {
        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => return,
                _ = self.status_changed.notified() => {}
                _ = tokio::time::sleep(Duration::from_millis(1000)) => {}
            }
            self.refresh_status();
            // Coalesce bursts of changes.
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    }

    // ---- control -----------------------------------------------------------------------

    pub fn pause(&self) {
        self.paused.store(true, Ordering::Relaxed);
        let _ = self.db.kv_set("paused", "1");
        self.status_changed.notify_one();
    }

    pub fn resume(&self) {
        self.paused.store(false, Ordering::Relaxed);
        let _ = self.db.kv_set("paused", "0");
        self.wake_scanner.notify_one();
        self.wake_worker.notify_one();
        self.status_changed.notify_one();
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    pub fn sync_now(&self) {
        self.wake_scanner.notify_one();
        self.wake_worker.notify_one();
    }

    /// Clears the signed-out state after the daemon refreshed credentials.
    pub fn signed_in(&self) {
        self.signed_out.store(false, Ordering::Relaxed);
        self.clear_issue("auth");
        self.sync_now();
        self.status_changed.notify_one();
    }

    fn note_remote_error(&self, e: &RemoteError) {
        match e {
            RemoteError::Auth(msg) => {
                if !self.signed_out.swap(true, Ordering::Relaxed) {
                    warn!("signed out: {msg}");
                    self.issue(
                        IssueKind::Auth,
                        "auth",
                        None,
                        None,
                        "uloz.to didn't accept your sign-in. Sign in again to keep syncing.",
                    );
                }
            }
            RemoteError::Network(_) => {
                if !self.offline.swap(true, Ordering::Relaxed) {
                    info!("offline: {e}");
                    self.status_changed.notify_one();
                }
            }
            _ => {}
        }
    }

    fn note_remote_ok(&self) {
        if self.offline.swap(false, Ordering::Relaxed) {
            info!("back online");
            self.wake_worker.notify_one();
            self.status_changed.notify_one();
        }
    }

    fn next_seq(&self) -> i64 {
        self.op_seq.fetch_add(1, Ordering::SeqCst) + 1
    }

    // ---- helpers -----------------------------------------------------------------------

    pub fn item(&self, n: &Node) -> Result<LocalItem> {
        let path = self
            .db
            .path_of(n.id)?
            .ok_or_else(|| EngineError::NotFound(format!("node {}", n.id)))?;
        Ok(LocalItem {
            id: n.id,
            path,
            kind: n.kind,
            size: n.size,
            mtime_ms: n.mtime_ms,
            pinned: n.pinned,
        })
    }

    fn node(&self, id: NodeId) -> Result<Node> {
        self.db
            .node(id)?
            .ok_or_else(|| EngineError::NotFound(format!("node {id}")))
    }

    /// The node's entry as the remote last knew it.
    fn remote_entry(&self, n: &Node) -> Option<RemoteEntry> {
        Some(RemoteEntry {
            id: n.remote_id.clone()?,
            parent_id: n.remote_parent_id.clone().unwrap_or_default(),
            kind: n.kind,
            name: n.remote_name.clone().unwrap_or_else(|| n.name.clone()),
            size: n.size,
            mtime_ms: n.mtime_ms,
        })
    }

    fn parent_remote_id(&self, n: &Node) -> Result<Option<String>> {
        match n.parent_id {
            Some(p) => Ok(self.db.node(p)?.and_then(|p| p.remote_id)),
            None => Ok(None),
        }
    }

    fn split_path(path: &str) -> (&str, &str) {
        let path = path.trim_matches('/');
        match path.rfind('/') {
            Some(i) => (&path[..i], &path[i + 1..]),
            None => ("", path),
        }
    }

    // ---- local changes (called by the platform layer) ------------------------------------

    pub fn node_by_path(&self, path: &str) -> Result<Option<Node>> {
        Ok(self.db.node_by_path(path)?)
    }

    /// Ensures a folder node exists for `path` (creating missing ancestors as new folders).
    fn ensure_folder(&self, path: &str) -> Result<NodeId> {
        if let Some(n) = self.db.node_by_path(path)? {
            if n.is_folder() {
                return Ok(n.id);
            }
            return Err(EngineError::Invalid(format!("{path} is a file")));
        }
        let (parent, name) = Self::split_path(path);
        let parent_id = self.ensure_folder(parent)?;
        self.create_in(parent_id, name, Kind::Folder, 0, now_ms())
    }

    /// A new file or folder appeared locally at `path`. If it already exists, this is treated
    /// as a modification.
    pub fn local_create(&self, path: &str, kind: Kind, size: u64, mtime_ms: i64) -> Result<NodeId> {
        if let Some(n) = self.db.node_by_path(path)? {
            if n.kind == Kind::File && kind == Kind::File {
                self.modified(n.id, size, mtime_ms)?;
            }
            return Ok(n.id);
        }
        let (parent, name) = Self::split_path(path);
        let parent_id = self.ensure_folder(parent)?;
        self.create_in(parent_id, name, kind, size, mtime_ms)
    }

    /// Creates a new local node under `parent` (FUSE create/mkdir).
    pub fn create_in(
        &self,
        parent: NodeId,
        name: &str,
        kind: Kind,
        size: u64,
        mtime_ms: i64,
    ) -> Result<NodeId> {
        if name.is_empty() || name.contains('/') {
            return Err(EngineError::Invalid(format!("invalid name {name:?}")));
        }
        let syncs = !is_ignored_name(name);
        let now = now_ms();
        let id = self.db.write(|tx| {
            let p = db::get_node(tx, parent)?.filter(|p| !p.deleted && p.is_folder());
            let Some(p) = p else { return Ok(None) };
            tx.execute(
                "INSERT INTO node (parent_id, name, name_key, kind, size, mtime_ms, dirty, pending, changed_ms,
                                   hydrated, pinned, accessed_ms, created_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?9, ?9)",
                params![
                    parent,
                    name,
                    name_key(name),
                    kind_int(kind),
                    size as i64,
                    mtime_ms,
                    kind == Kind::File,
                    if syncs { pending::PUSH } else { 0 },
                    now,
                    kind == Kind::File,
                    p.pinned,
                ],
            )?;
            Ok(Some(tx.last_insert_rowid()))
        });
        let id = match id {
            Ok(Some(id)) => id,
            Ok(None) => return Err(EngineError::NotFound(format!("parent folder {parent}"))),
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                return Err(EngineError::Io(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{name} already exists"),
                )));
            }
            Err(e) => return Err(e.into()),
        };
        debug!("local create {name} ({kind:?}) -> {id}");
        self.wake_worker.notify_one();
        self.status_changed.notify_one();
        Ok(id)
    }

    /// Local content of the file at `path` changed.
    pub fn local_modified(&self, path: &str, size: u64, mtime_ms: i64) -> Result<()> {
        match self.db.node_by_path(path)? {
            Some(n) => self.modified(n.id, size, mtime_ms),
            None => self
                .local_create(path, Kind::File, size, mtime_ms)
                .map(|_| ()),
        }
    }

    /// Local content of a file changed (FUSE write/release).
    pub fn modified(&self, id: NodeId, size: u64, mtime_ms: i64) -> Result<()> {
        let now = now_ms();
        self.db.write(|tx| {
            tx.execute(
                "UPDATE node SET dirty = 1, size = ?2, mtime_ms = ?3, generation = generation + 1, changed_ms = ?4,
                     hydrated = 1, accessed_ms = ?4, attempts = 0, retry_at_ms = 0, last_error = NULL,
                     pending = CASE WHEN ?5 THEN pending | 1 ELSE pending END
                 WHERE id = ?1 AND kind = 0 AND deleted = 0",
                params![id, size as i64, mtime_ms, now, true],
            )?;
            // Ignored (temporary) names never upload.
            tx.execute(
                "UPDATE node SET pending = pending & ~1 WHERE id = ?1 AND remote_id IS NULL AND ?2",
                params![id, is_ignored_name(&db::get_node(tx, id)?.map(|n| n.name).unwrap_or_default())],
            )?;
            Ok(())
        })?;
        self.clear_issue(&format!("transfer:{id}"));
        self.wake_worker.notify_one();
        self.status_changed.notify_one();
        Ok(())
    }

    /// `from` was moved/renamed to `to` locally. An existing item at `to` is replaced.
    pub fn local_rename(&self, from: &str, to: &str) -> Result<()> {
        let n = self
            .db
            .node_by_path(from)?
            .ok_or_else(|| EngineError::NotFound(from.to_string()))?;
        let (parent, name) = Self::split_path(to);
        let parent_id = self.ensure_folder(parent)?;
        self.rename_id(n.id, parent_id, name)
    }

    /// Moves/renames a node locally (FUSE rename). Replaces any other item at the target.
    pub fn rename_id(&self, id: NodeId, new_parent: NodeId, new_name: &str) -> Result<()> {
        if id == ROOT_ID {
            return Err(EngineError::Invalid("cannot move the root".into()));
        }
        // Refuse to move a folder into itself.
        let mut cur = Some(new_parent);
        while let Some(c) = cur {
            if c == id {
                return Err(EngineError::Invalid(
                    "cannot move a folder into itself".into(),
                ));
            }
            cur = self.db.node(c)?.and_then(|n| n.parent_id);
        }
        if let Some(existing) = self.db.child(new_parent, new_name)?
            && existing.id != id
        {
            self.delete_id(existing.id)?;
        }
        let now = now_ms();
        let syncs = !is_ignored_name(new_name);
        self.db.write(|tx| {
            tx.execute(
                "UPDATE node SET parent_id = ?2, name = ?3, name_key = ?4, generation = generation + 1, changed_ms = ?5,
                     retry_at_ms = 0, attempts = 0
                 WHERE id = ?1 AND deleted = 0",
                params![id, new_parent, new_name, name_key(new_name), now],
            )?;
            // A temp file renamed to a real name starts syncing (and vice versa).
            tx.execute(
                "UPDATE node SET pending = CASE WHEN ?2 THEN pending | 1 ELSE pending & ~1 END
                 WHERE id = ?1 AND remote_id IS NULL",
                params![id, syncs],
            )?;
            recompute_move(tx, id)
        })?;
        self.clear_issue(&format!("invalid_name:{id}"));
        self.wake_worker.notify_one();
        self.status_changed.notify_one();
        Ok(())
    }

    /// The item at `path` was deleted locally.
    pub fn local_delete(&self, path: &str) -> Result<()> {
        match self.db.node_by_path(path)? {
            Some(n) => self.delete_id(n.id),
            None => Ok(()),
        }
    }

    /// Deletes a node locally (FUSE unlink/rmdir). Synced items are trashed remotely.
    pub fn delete_id(&self, id: NodeId) -> Result<()> {
        if id == ROOT_ID {
            return Err(EngineError::Invalid("cannot delete the root".into()));
        }
        let now = now_ms();
        self.db.write(|tx| {
            let Some(n) = db::get_node(tx, id)? else { return Ok(()) };
            // Children go away with the parent: remotely, trashing a folder trashes its files.
            tx.execute(
                "WITH RECURSIVE sub(id) AS (SELECT id FROM node WHERE parent_id = ?1
                     UNION ALL SELECT n.id FROM node n JOIN sub ON n.parent_id = sub.id)
                 DELETE FROM node WHERE id IN (SELECT id FROM sub)",
                [id],
            )?;
            if n.remote_id.is_some() {
                tx.execute(
                    "UPDATE node SET deleted = 1, pending = ?2, changed_ms = ?3, retry_at_ms = 0, attempts = 0 WHERE id = ?1",
                    params![id, pending::TRASH, now],
                )?;
            } else {
                tx.execute("DELETE FROM node WHERE id = ?1", [id])?;
            }
            Ok(())
        })?;
        self.clear_issue(&format!("transfer:{id}"));
        self.wake_worker.notify_one();
        self.status_changed.notify_one();
        Ok(())
    }

    /// Records that a file was read (for LRU eviction). Cheap; flushed periodically.
    pub fn touch(&self, id: NodeId) {
        self.access.lock().unwrap().insert(id, now_ms());
    }

    fn flush_access_times(&self) {
        let items: Vec<(NodeId, i64)> = self.access.lock().unwrap().drain().collect();
        if let Err(e) = self.db.set_access_times(&items) {
            warn!("saving access times failed: {e}");
        }
    }

    /// The platform layer finished downloading (or dropped) a file's full content.
    pub fn mark_hydrated(&self, id: NodeId, hydrated: bool) {
        let was = self.db.node(id).ok().flatten();
        if let Err(e) = self.db.set_hydrated(id, hydrated) {
            warn!("marking hydration failed: {e}");
            return;
        }
        if hydrated
            && let Some(n) = was
            && !n.hydrated
            && let Ok(Some(path)) = self.db.path_of(id)
        {
            self.activity(ActivityKind::Downloaded, &path, n.size, None);
            self.wake_evictor.notify_one();
        }
        self.status_changed.notify_one();
    }

    // ---- reading -------------------------------------------------------------------------

    /// Streams plaintext bytes of a file from the cloud.
    pub async fn read(
        self: &Arc<Self>,
        id: NodeId,
        offset: u64,
        len: Option<u64>,
    ) -> Result<ByteStream> {
        let n = self.node(id)?;
        if n.kind != Kind::File {
            return Err(EngineError::Invalid("not a file".into()));
        }
        let entry = self
            .remote_entry(&n)
            .ok_or_else(|| EngineError::NotFound("file is not in the cloud yet".into()))?;
        self.touch(id);
        let total = len
            .unwrap_or(n.size.saturating_sub(offset))
            .min(n.size.saturating_sub(offset));
        let stream = match self.remote.read(&entry, offset, len).await {
            Ok(s) => {
                self.note_remote_ok();
                s
            }
            Err(e) => {
                self.note_remote_error(&e);
                return Err(e.into());
            }
        };
        let done = Arc::new(AtomicU64::new(0));
        let tid = self.begin_transfer(false, total, done.clone());
        let guard = TransferGuard {
            engine: self.clone(),
            id: tid,
        };
        let s = metered(stream, done, self.down.clone());
        // Keep the transfer registered until the stream is dropped.
        Ok(Box::pin(GuardedStream {
            inner: s,
            _guard: guard,
        }))
    }

    fn begin_transfer(&self, upload: bool, total: u64, done: Arc<AtomicU64>) -> u64 {
        let id = self.next_transfer.fetch_add(1, Ordering::Relaxed);
        self.transfers.lock().unwrap().insert(
            id,
            Transfer {
                upload,
                total,
                done,
            },
        );
        self.status_changed.notify_one();
        id
    }

    fn end_transfer(&self, id: u64) {
        self.transfers.lock().unwrap().remove(&id);
        self.status_changed.notify_one();
    }

    // ---- pinning -------------------------------------------------------------------------

    /// Keeps a file or folder (recursively) downloaded on this device, or releases it.
    pub fn set_pinned(self: &Arc<Self>, path: &str, pinned: bool) -> Result<()> {
        let n = self
            .db
            .node_by_path(path)?
            .ok_or_else(|| EngineError::NotFound(path.to_string()))?;
        self.db.write(|tx| {
            tx.execute(
                "WITH RECURSIVE sub(id) AS (SELECT ?1 UNION ALL SELECT n.id FROM node n JOIN sub ON n.parent_id = sub.id)
                 UPDATE node SET pinned = ?2 WHERE id IN (SELECT id FROM sub)",
                params![n.id, pinned],
            )
            .map(|_| ())
        })?;
        let item = self.item(&n)?;
        if let Err(e) = self.local().set_pinned(&item, pinned) {
            warn!("setting pin state on {path} failed: {e}");
        }
        if pinned {
            self.hydrate_subtree(n.id);
        } else {
            self.wake_evictor.notify_one();
        }
        self.status_changed.notify_one();
        Ok(())
    }

    /// Unpins and evicts everything below `path` that is safely in the cloud.
    pub async fn free_up_space(self: &Arc<Self>, path: &str) -> Result<u64> {
        self.set_pinned(path, false)?;
        let n = self
            .db
            .node_by_path(path)?
            .ok_or_else(|| EngineError::NotFound(path.to_string()))?;
        let ids: Vec<NodeId> = self.db.read(|c| {
            let mut st = c.prepare(
                "WITH RECURSIVE sub(id) AS (SELECT ?1 UNION ALL SELECT n.id FROM node n JOIN sub ON n.parent_id = sub.id)
                 SELECT id FROM node WHERE id IN (SELECT id FROM sub)
                   AND kind = 0 AND hydrated = 1 AND dirty = 0 AND pending = 0 AND deleted = 0",
            )?;
            st.query_map([n.id], |r| r.get(0))?.collect()
        })?;
        let mut freed = 0;
        for id in ids {
            freed += self.evict_one(id).await;
        }
        self.status_changed.notify_one();
        Ok(freed)
    }

    fn hydrate_subtree(self: &Arc<Self>, id: NodeId) {
        let ids: Vec<NodeId> = self
            .db
            .read(|c| {
                let mut st = c.prepare(
                    "WITH RECURSIVE sub(id) AS (SELECT ?1 UNION ALL SELECT n.id FROM node n JOIN sub ON n.parent_id = sub.id)
                     SELECT id FROM node WHERE id IN (SELECT id FROM sub) AND kind = 0 AND hydrated = 0
                       AND remote_id IS NOT NULL AND deleted = 0",
                )?;
                st.query_map([id], |r| r.get(0))?.collect()
            })
            .unwrap_or_default();
        for id in ids {
            self.spawn_hydrate(id);
        }
    }

    fn spawn_hydrate(self: &Arc<Self>, id: NodeId) {
        if !self.hydrating.lock().unwrap().insert(id) {
            return;
        }
        let me = self.clone();
        tokio::spawn(async move {
            let _permit = me.hydrate_permits.acquire().await;
            let result = async {
                let n = me.node(id)?;
                if n.hydrated || n.deleted || !n.pinned {
                    return Ok(());
                }
                let item = me.item(&n)?;
                let local = me.local().clone();
                tokio::task::spawn_blocking(move || local.hydrate(&item))
                    .await
                    .map_err(io::Error::other)??;
                me.mark_hydrated(id, true);
                Ok::<_, EngineError>(())
            }
            .await;
            if let Err(e) = result {
                warn!("keeping node {id} downloaded failed: {e}");
            }
            me.hydrating.lock().unwrap().remove(&id);
        });
    }

    /// After a restart, finish downloading pinned files.
    fn resume_pins(self: &Arc<Self>) {
        let ids: Vec<NodeId> = self
            .db
            .read(|c| {
                let mut st = c.prepare(
                    "SELECT id FROM node WHERE pinned = 1 AND hydrated = 0 AND kind = 0 AND remote_id IS NOT NULL AND deleted = 0 LIMIT 10000",
                )?;
                st.query_map([], |r| r.get(0))?.collect()
            })
            .unwrap_or_default();
        for id in ids {
            self.spawn_hydrate(id);
        }
    }

    // ---- eviction ------------------------------------------------------------------------

    async fn evict_one(&self, id: NodeId) -> u64 {
        let Ok(n) = self.node(id) else { return 0 };
        if !n.hydrated || n.dirty || n.pending != 0 || n.pinned {
            return 0;
        }
        let Ok(item) = self.item(&n) else { return 0 };
        let local = self.local().clone();
        match tokio::task::spawn_blocking(move || local.evict(&item)).await {
            Ok(Ok(true)) => {
                let _ = self.db.set_hydrated(id, false);
                n.size
            }
            Ok(Ok(false)) => 0,
            Ok(Err(e)) => {
                debug!("evicting node {id} failed: {e}");
                0
            }
            Err(_) => 0,
        }
    }

    async fn evict_pass(&self) {
        let cfg = self.config();
        let Ok(totals) = self.db.storage_totals() else {
            return;
        };
        let mut need = totals
            .cache_bytes
            .saturating_sub(cfg.cache_limit_bytes.saturating_mul(9) / 10);
        if totals.cache_bytes <= cfg.cache_limit_bytes {
            need = 0;
        }
        let free = fs4::available_space(&cfg.folder).ok();
        let low_disk = free.is_some_and(|f| f < cfg.min_free_bytes);
        if let Some(f) = free
            && low_disk
        {
            need = need.max(cfg.min_free_bytes - f);
        }
        if need == 0 {
            self.clear_issue("disk_full");
            return;
        }
        let mut freed = 0u64;
        let mut evicted = 0u32;
        while freed < need {
            let Ok(batch) = self.db.eviction_candidates(64) else {
                break;
            };
            if batch.is_empty() {
                break;
            }
            let mut progress = false;
            for n in batch {
                let f = self.evict_one(n.id).await;
                if f > 0 || n.size == 0 {
                    progress = true;
                    freed += f;
                    evicted += 1;
                }
                if freed >= need {
                    break;
                }
            }
            if !progress {
                break;
            }
        }
        if evicted > 0 {
            info!("evicted {evicted} files ({freed} bytes)");
            self.activity(
                ActivityKind::Evicted,
                &format!("{evicted} files"),
                freed,
                Some("Freed up space on this device"),
            );
        }
        if low_disk && freed < need {
            self.issue(
                IssueKind::DiskFull,
                "disk_full",
                None,
                None,
                "Your disk is almost full. Free up space or unpin files you don't need on this device.",
            );
        }
        self.status_changed.notify_one();
    }

    async fn evictor_loop(self: Arc<Self>) {
        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => return,
                _ = self.wake_evictor.notified() => {}
                _ = tokio::time::sleep(Duration::from_secs(30)) => {}
            }
            self.evict_pass().await;
        }
    }

    async fn housekeeping_loop(self: Arc<Self>) {
        let mut ticks = 0u64;
        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_secs(30)) => {}
            }
            self.flush_access_times();
            ticks += 1;
            if ticks.is_multiple_of(120) {
                self.db.optimize();
            }
            // Forget trashed ids once no scan can still be listing them.
            let seq = self.op_seq.load(Ordering::SeqCst);
            self.recently_gone
                .lock()
                .unwrap()
                .retain(|_, s| *s + 10_000 > seq);
        }
    }

    // ---- scanning ------------------------------------------------------------------------

    async fn scanner_loop(self: Arc<Self>) {
        let mut backoff = Duration::from_secs(5);
        loop {
            let ok =
                if self.paused.load(Ordering::Relaxed) || self.signed_out.load(Ordering::Relaxed) {
                    true
                } else {
                    self.scan_once().await.is_ok()
                };
            let wait = if ok {
                backoff = Duration::from_secs(5);
                self.config().poll_interval
            } else {
                backoff = (backoff * 2).min(Duration::from_secs(300));
                backoff
            };
            tokio::select! {
                _ = self.cancel.cancelled() => return,
                _ = self.wake_scanner.notified() => {}
                _ = tokio::time::sleep(wait) => {}
            }
        }
    }

    /// Lists the remote and applies the differences. Public for tests and "Sync now".
    pub async fn scan_once(&self) -> Result<()> {
        self.scanning.store(true, Ordering::Relaxed);
        let start_seq = self.op_seq.load(Ordering::SeqCst);
        let result = self.remote.scan().await;
        let out = match result {
            Ok(tree) => {
                self.note_remote_ok();
                self.apply_scan(tree, start_seq)
            }
            Err(e) => {
                warn!("scan failed: {e}");
                self.note_remote_error(&e);
                Err(e.into())
            }
        };
        self.scanning.store(false, Ordering::Relaxed);
        self.refresh_status();
        out
    }

    fn apply_scan(&self, tree: crate::remote::RemoteTree, scan_seq: i64) -> Result<()> {
        // Snapshot in-flight work *before* reading the rows: anything that finishes in
        // between is then either still marked busy or already recorded in its row.
        let busy: HashSet<NodeId> = self.in_flight.lock().unwrap().clone();
        let nodes = self.db.all_nodes()?;
        let mut orphans = self.db.orphan_ids()?;
        orphans.extend(
            self.recently_gone
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, s)| **s > scan_seq)
                .map(|(k, _)| k.clone()),
        );
        let cfg = self.config();
        let stamp = chrono::Local::now().format("%Y-%m-%d %H%M").to_string();
        let local = self.local().clone();
        let valid = move |n: &str| local.valid_name(n);
        let plan = plan::plan(&PlanInput {
            nodes: &nodes,
            tree: &tree,
            scan_seq,
            orphans: &orphans,
            valid_name: &valid,
            device_name: &cfg.device_name,
            stamp: &stamp,
            allow_mass_removal: now_ms() < self.allow_remote_removal_until.load(Ordering::Relaxed),
            busy: &busy,
        });
        let plan = match plan {
            Ok(p) => {
                if self.wrong_password.swap(false, Ordering::Relaxed) {
                    self.clear_issue("wrong_password");
                }
                p
            }
            Err(PlanError::WrongPassword(n)) => {
                self.wrong_password.store(true, Ordering::Relaxed);
                self.issue(
                    IssueKind::WrongPassword,
                    "wrong_password",
                    None,
                    None,
                    &format!(
                        "None of the {n} items in your uloz.to vault can be decrypted with the saved encryption password. Nothing was changed on this device. Sign in again with the right password."
                    ),
                );
                return Ok(());
            }
        };
        if !tree.skipped.is_empty() {
            debug!(
                "{} remote items could not be decrypted and were ignored",
                tree.skipped.len()
            );
        }

        let mut ids: HashMap<i64, NodeId> = HashMap::new();
        let total = plan.changes.len();
        for change in plan.changes {
            if let Err(e) = self.apply_change(change, &mut ids) {
                warn!("applying a remote change failed: {e}");
            }
        }
        if total > 0 {
            info!("applied {total} remote changes");
        }

        for name in &plan.conflicts {
            // MoveAside already reported its own conflicts; report same-name collisions here.
            if name.contains("(conflicted copy)") {
                self.activity(
                    ActivityKind::Conflict,
                    name,
                    0,
                    Some("Two versions were uploaded at the same time"),
                );
                self.issue(
                    IssueKind::Conflict,
                    &format!("conflict:{name}"),
                    None,
                    Some(name),
                    &format!("Two devices saved a file with the same name at the same time. Both are kept; one is now \"{name}\"."),
                );
            }
        }
        self.held_remote_removals
            .store(plan.held_removal_files, Ordering::Relaxed);
        if plan.held_removal_files > 0 {
            self.issue(
                IssueKind::MassDelete,
                "mass_delete:remote",
                None,
                None,
                &format!(
                    "{} files were deleted from your uloz.to vault. Remove them from this device too? They stay in the uloz.to trash for 30 days.",
                    plan.held_removal_files
                ),
            );
        } else {
            self.clear_issue("mass_delete:remote");
        }
        for e in &plan.invalid_names {
            self.issue(
                IssueKind::InvalidName,
                &format!("invalid_remote_name:{}", e.id),
                None,
                Some(&e.name),
                &format!("\"{}\" can't be shown on this device because its name isn't allowed here. Rename it on another device.", e.name),
            );
        }
        // New or replaced files inside pinned folders: download them now.
        if let Some(me) = self.me.get().and_then(|w| w.upgrade()) {
            me.resume_pins();
        }
        let now = now_ms();
        self.last_sync_ms.store(now, Ordering::Relaxed);
        let _ = self.db.kv_set("last_sync_ms", &now.to_string());
        self.status_changed.notify_one();
        self.wake_worker.notify_one();
        Ok(())
    }

    fn apply_change(&self, change: Change, ids: &mut HashMap<i64, NodeId>) -> Result<()> {
        let real = |id: NodeId, ids: &HashMap<i64, NodeId>| {
            if id < 0 {
                ids.get(&id).copied()
            } else {
                Some(id)
            }
        };
        match change {
            Change::SetRootRemote(r) => {
                self.db.write(|tx| {
                    tx.execute("UPDATE node SET remote_id = ?1 WHERE id = 1", [&r])
                        .map(|_| ())
                })?;
            }
            Change::Create {
                temp_id,
                parent,
                name,
                entry,
                needs_move,
                pinned,
            } => {
                let Some(parent) = real(parent, ids) else {
                    return Ok(());
                };
                let now = now_ms();
                let inserted = self.db.write(|tx| {
                    let Some(p) = db::get_node(tx, parent)?.filter(|p| !p.deleted) else { return Ok(None) };
                    if db::get_child(tx, parent, &name)?.is_some() {
                        return Ok(None);
                    }
                    tx.execute(
                        "INSERT INTO node (parent_id, name, name_key, kind, size, mtime_ms, remote_id, remote_parent_id,
                                           remote_name, pending, pinned, created_ms)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                        params![
                            parent,
                            name,
                            name_key(&name),
                            kind_int(entry.kind),
                            entry.size as i64,
                            entry.mtime_ms,
                            entry.id,
                            entry.parent_id,
                            entry.name,
                            if needs_move { pending::MOVE } else { 0 },
                            pinned || p.pinned,
                            now
                        ],
                    )?;
                    Ok(Some(tx.last_insert_rowid()))
                })?;
                let Some(id) = inserted else { return Ok(()) };
                ids.insert(temp_id, id);
                let n = self.node(id)?;
                let item = self.item(&n)?;
                if let Err(e) = self.local().create(&item) {
                    warn!("showing {} locally failed: {e}", item.path);
                    self.db.write(|tx| {
                        tx.execute("DELETE FROM node WHERE id = ?1", [id])
                            .map(|_| ())
                    })?;
                    return Ok(());
                }
                if n.pinned && n.kind == Kind::File {
                    // Pinned folder: download new arrivals (needs an Arc; the scanner re-runs pins).
                    self.wake_evictor.notify_one();
                }
            }
            Change::Move {
                node,
                generation,
                parent,
                name,
                entry,
                needs_move,
            } => {
                let Some(parent) = real(parent, ids) else {
                    return Ok(());
                };
                let n = self.node(node)?;
                if n.generation != generation || n.deleted {
                    return Ok(());
                }
                if self.db.child(parent, &name)?.is_some_and(|c| c.id != node) {
                    return Ok(());
                }
                let from = self.item(&n)?.path;
                let parent_path = self
                    .db
                    .path_of(parent)?
                    .ok_or_else(|| EngineError::NotFound("parent".into()))?;
                let to = if parent_path.is_empty() {
                    name.clone()
                } else {
                    format!("{parent_path}/{name}")
                };
                let item = LocalItem {
                    id: node,
                    path: to.clone(),
                    kind: n.kind,
                    size: n.size,
                    mtime_ms: n.mtime_ms,
                    pinned: n.pinned,
                };
                self.local().rename(&from, &item)?;
                self.db.write(|tx| {
                    tx.execute(
                        "UPDATE node SET parent_id = ?2, name = ?3, name_key = ?4, remote_parent_id = ?5, remote_name = ?6 WHERE id = ?1",
                        params![node, parent, name, name_key(&name), entry.parent_id, entry.name],
                    )?;
                    if needs_move {
                        tx.execute("UPDATE node SET pending = pending | 2 WHERE id = ?1", [node])?;
                    }
                    Ok(())
                })?;
                if name.contains("(conflicted copy)") && !from.contains("(conflicted copy)") {
                    // Another device resolved a same-name collision by renaming this file.
                    self.activity(
                        ActivityKind::Conflict,
                        &to,
                        n.size,
                        Some("Two versions were uploaded at the same time"),
                    );
                    self.issue(
                        IssueKind::Conflict,
                        &format!("conflict:{to}"),
                        Some(node),
                        Some(&to),
                        &format!("Two devices saved \"{from}\" at the same time. Both versions are kept; this one is now \"{name}\"."),
                    );
                } else {
                    self.activity(ActivityKind::Moved, &to, 0, Some(&format!("from {from}")));
                }
            }
            Change::Rebase { node, entry } => {
                self.db.write(|tx| {
                    tx.execute(
                        "UPDATE node SET remote_parent_id = ?2, remote_name = ?3 WHERE id = ?1",
                        params![node, entry.parent_id, entry.name],
                    )?;
                    recompute_move(tx, node)
                })?;
            }
            Change::Replace {
                node,
                generation,
                entry,
            } => {
                let n = self.node(node)?;
                if n.generation != generation || n.dirty || n.deleted {
                    return Ok(());
                }
                let mut item = self.item(&n)?;
                item.size = entry.size;
                item.mtime_ms = entry.mtime_ms;
                self.local().replace(&item)?;
                self.db.write(|tx| {
                    tx.execute(
                        "UPDATE node SET remote_id = ?2, remote_parent_id = ?3, remote_name = ?4, size = ?5, mtime_ms = ?6,
                             hydrated = 0
                         WHERE id = ?1",
                        params![node, entry.id, entry.parent_id, entry.name, entry.size as i64, entry.mtime_ms],
                    )
                    .map(|_| ())
                })?;
                if n.pinned {
                    self.wake_evictor.notify_one();
                }
            }
            Change::Adopt {
                node,
                generation: _,
                entry,
            } => {
                self.db.write(|tx| {
                    tx.execute(
                        "UPDATE node SET remote_id = ?2, remote_parent_id = ?3, remote_name = ?4, pending = pending & ~1
                         WHERE id = ?1 AND deleted = 0",
                        params![node, entry.id, entry.parent_id, entry.name],
                    )?;
                    recompute_move(tx, node)
                })?;
            }
            Change::MoveAside {
                node,
                generation,
                name,
                detach,
            } => {
                let n = self.node(node)?;
                if n.generation != generation || n.deleted {
                    return Ok(());
                }
                let Some(parent) = n.parent_id else {
                    return Ok(());
                };
                if self.db.child(parent, &name)?.is_some() {
                    return Ok(());
                }
                let from = self.item(&n)?.path;
                let to = match from.rfind('/') {
                    Some(i) => format!("{}/{name}", &from[..i]),
                    None => name.clone(),
                };
                let item = LocalItem {
                    id: node,
                    path: to.clone(),
                    kind: n.kind,
                    size: n.size,
                    mtime_ms: n.mtime_ms,
                    pinned: n.pinned,
                };
                self.local().rename(&from, &item)?;
                self.db.write(|tx| {
                    tx.execute(
                        "UPDATE node SET name = ?2, name_key = ?3, generation = generation + 1 WHERE id = ?1",
                        params![node, name, name_key(&name)],
                    )?;
                    if detach {
                        tx.execute(
                            "UPDATE node SET remote_id = NULL, remote_parent_id = NULL, remote_name = NULL,
                                 pending = (pending | 1) & ~2 WHERE id = ?1",
                            [node],
                        )?;
                    }
                    recompute_move(tx, node)
                })?;
                self.activity(
                    ActivityKind::Conflict,
                    &to,
                    n.size,
                    Some(&format!("Kept your version of {from}")),
                );
                self.issue(
                    IssueKind::Conflict,
                    &format!("conflict:{node}"),
                    Some(node),
                    Some(&to),
                    &format!(
                        "\"{from}\" was changed both here and in the cloud. Both versions are kept: yours is now \"{}\".",
                        name
                    ),
                );
            }
            Change::Remove { node, generation } => {
                let n = self.node(node)?;
                if n.generation != generation
                    || n.dirty
                    || n.pending & pending::PUSH != 0
                    || n.deleted
                {
                    return Ok(());
                }
                if n.is_folder() && self.subtree_has_changes(node)? {
                    return Ok(());
                }
                let item = self.item(&n)?;
                self.local().remove(&item)?;
                self.db.write(|tx| {
                    tx.execute("DELETE FROM node WHERE id = ?1", [node])
                        .map(|_| ())
                })?;
                self.activity(
                    ActivityKind::Deleted,
                    &item.path,
                    n.size,
                    Some("Deleted in the cloud"),
                );
            }
            Change::Resurrect {
                node,
                generation: _,
            } => {
                self.db.write(|tx| {
                    tx.execute(
                        "UPDATE node SET remote_id = NULL, remote_parent_id = NULL, remote_name = NULL,
                             pending = (pending | 1) & ~2, retry_at_ms = 0
                         WHERE id = ?1 AND deleted = 0",
                        [node],
                    )
                    .map(|_| ())
                })?;
            }
            Change::DropTombstone { node } => {
                self.db.write(|tx| {
                    tx.execute("DELETE FROM node WHERE id = ?1 AND deleted = 1", [node])
                        .map(|_| ())
                })?;
            }
        }
        Ok(())
    }

    fn subtree_has_changes(&self, id: NodeId) -> Result<bool> {
        Ok(self.db.read(|c| {
            c.query_row(
                "WITH RECURSIVE sub(id) AS (SELECT id FROM node WHERE parent_id = ?1
                     UNION ALL SELECT n.id FROM node n JOIN sub ON n.parent_id = sub.id)
                 SELECT EXISTS (SELECT 1 FROM node WHERE id IN (SELECT id FROM sub)
                     AND (dirty = 1 OR pending & 1 != 0 OR remote_id IS NULL))",
                [id],
                |r| r.get(0),
            )
        })?)
    }

    // ---- pushing local changes -----------------------------------------------------------

    async fn worker_loop(self: Arc<Self>) {
        loop {
            if self.cancel.is_cancelled() {
                return;
            }
            let blocked = self.paused.load(Ordering::Relaxed)
                || self.signed_out.load(Ordering::Relaxed)
                || self.wrong_password.load(Ordering::Relaxed);
            let mut started = 0;
            if !blocked {
                started = self.dispatch();
            }
            let wait = if blocked {
                Duration::from_secs(30)
            } else if started > 0 {
                Duration::from_millis(500)
            } else {
                let now = now_ms();
                let debounce = self.config().debounce.as_millis() as i64;
                match self.db.next_work_at().ok().flatten() {
                    Some(at) => {
                        Duration::from_millis((at + debounce - now).clamp(200, 30_000) as u64)
                    }
                    None => Duration::from_secs(30),
                }
            };
            tokio::select! {
                _ = self.cancel.cancelled() => return,
                _ = self.wake_worker.notified() => {}
                _ = tokio::time::sleep(wait) => {}
            }
        }
    }

    /// Starts as many due work items as the parallelism limit allows. Returns how many.
    fn dispatch(self: &Arc<Self>) -> usize {
        let cfg = self.config();
        let now = now_ms();
        let max = cfg.max_parallel_transfers.max(1);
        let debounce_before = now - cfg.debounce.as_millis() as i64;
        let mut started = 0;

        let trash_blocked = {
            let n = self.db.pending_trash_files().unwrap_or(0);
            let blocked = n > LOCAL_DELETE_GUARD
                && now > self.allow_local_deletes_until.load(Ordering::Relaxed);
            if blocked {
                self.issue(
                    IssueKind::MassDelete,
                    "mass_delete:local",
                    None,
                    None,
                    &format!("You deleted {n} files on this device. Delete them from uloz.to too? They'll stay in the uloz.to trash for 30 days."),
                );
            } else {
                self.clear_issue("mass_delete:local");
            }
            blocked
        };

        let Ok(due) = self.db.due_work(now, debounce_before, 200) else {
            return 0;
        };
        for n in due {
            if self.in_flight.lock().unwrap().len() >= max {
                break;
            }
            if self.in_flight.lock().unwrap().contains(&n.id) {
                continue;
            }
            if n.deleted && trash_blocked {
                continue;
            }
            match self.ready(&n) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(e) => {
                    debug!("checking work for node {} failed: {e}", n.id);
                    continue;
                }
            }
            self.in_flight.lock().unwrap().insert(n.id);
            started += 1;
            let me = self.clone();
            tokio::spawn(async move {
                let id = n.id;
                let result = me.run_item(n).await;
                if let Err((node, e)) = result {
                    me.handle_failure(&node, e);
                }
                me.in_flight.lock().unwrap().remove(&id);
                me.status_changed.notify_one();
                me.wake_worker.notify_one();
            });
        }

        // Superseded versions left behind by re-uploads.
        if let Ok(orphans) = self.db.due_orphans(now, 8) {
            for e in orphans {
                if !self.orphans_in_flight.lock().unwrap().insert(e.id.clone()) {
                    continue;
                }
                let me = self.clone();
                tokio::spawn(async move {
                    let id = e.id.clone();
                    match me.remote.trash(&e).await {
                        Ok(()) | Err(RemoteError::NotFound(_)) => {
                            let seq = me.next_seq();
                            me.recently_gone.lock().unwrap().insert(id.clone(), seq);
                            let _ = me.db.remove_orphan(&id);
                        }
                        Err(err) => {
                            me.note_remote_error(&err);
                            let _ = me.db.orphan_failed(&id, now_ms() + 60_000);
                        }
                    }
                    me.orphans_in_flight.lock().unwrap().remove(&id);
                });
            }
        }
        started
    }

    /// Whether a node's dependencies are satisfied (parent exists remotely, etc.).
    fn ready(&self, n: &Node) -> Result<bool> {
        if n.id == ROOT_ID {
            return Ok(false);
        }
        if n.deleted {
            // Wait if an ancestor is also being trashed; it takes this one with it.
            let mut p = n.parent_id;
            while let Some(pid) = p {
                match self.db.node(pid)? {
                    Some(pn) if pn.deleted => return Ok(false),
                    Some(pn) => p = pn.parent_id,
                    None => break,
                }
            }
            return Ok(true);
        }
        Ok(self.parent_remote_id(n)?.is_some())
    }

    async fn run_item(self: &Arc<Self>, n: Node) -> std::result::Result<(), (Node, EngineError)> {
        let r = if n.deleted {
            self.push_trash(&n).await
        } else if n.pending & pending::PUSH != 0 {
            if n.is_folder() {
                self.push_mkdir(&n).await
            } else {
                self.push_upload(&n).await
            }
        } else if n.pending & pending::MOVE != 0 {
            self.push_move(&n).await
        } else {
            Ok(())
        };
        match r {
            Ok(()) => {
                self.note_remote_ok();
                self.clear_issue(&format!("transfer:{}", n.id));
                Ok(())
            }
            Err(e) => Err((n, e)),
        }
    }

    fn handle_failure(&self, n: &Node, e: EngineError) {
        let now = now_ms();
        let path = self
            .db
            .path_of(n.id)
            .ok()
            .flatten()
            .unwrap_or_else(|| n.name.clone());
        let (delay_ms, count_attempt): (i64, bool) = match &e {
            EngineError::Remote(re) => {
                self.note_remote_error(re);
                match re {
                    RemoteError::Auth(_) => (60_000, false),
                    RemoteError::Network(_) => (15_000, false),
                    RemoteError::InvalidName(msg) => {
                        self.issue(
                            IssueKind::InvalidName,
                            &format!("invalid_name:{}", n.id),
                            Some(n.id),
                            Some(&path),
                            &format!("\"{path}\" can't be uploaded: {msg}. Rename it to sync it."),
                        );
                        (86_400_000, false)
                    }
                    RemoteError::Quota => {
                        self.issue(
                            IssueKind::Other,
                            "quota",
                            None,
                            None,
                            "Your uloz.to storage is full. Free up space in your account or upgrade your plan.",
                        );
                        (30 * 60_000, false)
                    }
                    RemoteError::NotFound(_) => {
                        self.wake_scanner.notify_one();
                        (3_000, true)
                    }
                    _ => (backoff_ms(n.attempts), true),
                }
            }
            EngineError::Io(io) if io.kind() == io::ErrorKind::WouldBlock => {
                if n.attempts >= 30 {
                    self.issue(
                        IssueKind::FileLocked,
                        &format!("transfer:{}", n.id),
                        Some(n.id),
                        Some(&path),
                        &format!("\"{path}\" is open in another app. Close it so it can upload."),
                    );
                }
                (10_000, true)
            }
            EngineError::Io(io) if io.kind() == io::ErrorKind::NotFound => {
                // The local file vanished; the platform layer will report the deletion.
                (30_000, true)
            }
            _ => (backoff_ms(n.attempts), true),
        };
        warn!("syncing {path} failed: {e}");
        let attempts = n.attempts + i64::from(count_attempt);
        let _ = self.db.write(|tx| {
            tx.execute(
                "UPDATE node SET retry_at_ms = ?2, attempts = ?3, last_error = ?4 WHERE id = ?1",
                params![n.id, now + delay_ms, attempts, e.to_string()],
            )
            .map(|_| ())
        });
        if count_attempt
            && attempts >= 5
            && !matches!(&e, EngineError::Io(io) if io.kind() == io::ErrorKind::WouldBlock)
        {
            self.issue(
                IssueKind::TransferFailed,
                &format!("transfer:{}", n.id),
                Some(n.id),
                Some(&path),
                &format!(
                    "\"{path}\" couldn't be synced after several tries: {e}. Drive keeps retrying."
                ),
            );
        }
    }

    async fn push_trash(&self, n: &Node) -> Result<()> {
        let path = self.db.path_of(n.id)?.unwrap_or_else(|| n.name.clone());
        if let Some(entry) = self.remote_entry(n) {
            match self.remote.trash(&entry).await {
                Ok(()) | Err(RemoteError::NotFound(_)) => {}
                Err(e) => return Err(e.into()),
            }
            let seq = self.next_seq();
            self.recently_gone.lock().unwrap().insert(entry.id, seq);
        }
        self.db.write(|tx| {
            tx.execute("DELETE FROM node WHERE id = ?1 AND deleted = 1", [n.id])
                .map(|_| ())
        })?;
        self.activity(ActivityKind::Deleted, &path, n.size, None);
        Ok(())
    }

    async fn push_mkdir(&self, n: &Node) -> Result<()> {
        let parent = self
            .parent_remote_id(n)?
            .ok_or_else(|| EngineError::NotFound("parent folder".into()))?;
        let entry = self.remote.create_folder(&parent, &n.name).await?;
        let seq = self.next_seq();
        self.db.write(|tx| {
            tx.execute(
                "UPDATE node SET remote_id = ?2, remote_parent_id = ?3, remote_name = ?4, pending = pending & ~1,
                     op_seq = ?5, attempts = 0, last_error = NULL
                 WHERE id = ?1 AND deleted = 0",
                params![n.id, entry.id, parent, n.name, seq],
            )?;
            recompute_move(tx, n.id)
        })?;
        // If the folder was deleted while we created it, the trash pass cleans it up.
        if self.db.node(n.id)?.is_some_and(|x| x.deleted) {
            self.db.write(|tx| {
                tx.execute(
                    "UPDATE node SET remote_id = ?2, remote_parent_id = ?3, remote_name = ?4 WHERE id = ?1",
                    params![n.id, entry.id, parent, n.name],
                )
                .map(|_| ())
            })?;
        }
        let path = self.db.path_of(n.id)?.unwrap_or_default();
        self.activity(ActivityKind::CreatedFolder, &path, 0, None);
        Ok(())
    }

    async fn push_upload(&self, n: &Node) -> Result<()> {
        let parent = self
            .parent_remote_id(n)?
            .ok_or_else(|| EngineError::NotFound("parent folder".into()))?;
        // An edit of a synced file must be based on the version that is still current in the
        // cloud. If that version was replaced or deleted meanwhile, rescan first: the planner
        // turns our edit into a conflicted copy (replaced) or re-uploads it (deleted).
        if let Some(base) = self.remote_entry(n)
            && !self.remote.exists(&base).await?
        {
            debug!(
                "base version of {} changed remotely; rescanning before upload",
                n.name
            );
            self.db.write(|tx| {
                tx.execute(
                    "UPDATE node SET retry_at_ms = ?2 WHERE id = ?1",
                    params![n.id, now_ms() + 500],
                )
                .map(|_| ())
            })?;
            self.wake_scanner.notify_one();
            return Ok(());
        }
        let item = self.item(n)?;
        let local = self.local().clone();
        let it = item.clone();
        let src = tokio::task::spawn_blocking(move || local.open_for_upload(&it))
            .await
            .map_err(io::Error::other)??;
        let size = src.size;
        let mtime = src.mtime_ms;
        let file = tokio::fs::File::from_std(src.file);
        use tokio::io::AsyncReadExt;
        let reader = ReaderStream::with_capacity(file.take(size), 256 * 1024);
        let stream: ByteStream = Box::pin(reader);
        let done = Arc::new(AtomicU64::new(0));
        let tid = self.begin_transfer(true, size, done.clone());
        let body = UploadBody {
            stream: metered(stream, done, self.up.clone()),
            size,
            mtime_ms: mtime,
        };
        let result = self.remote.upload(&parent, &n.name, body).await;
        self.end_transfer(tid);
        let entry = result?;
        let seq = self.next_seq();

        // Fold the result into the current row (which may have changed during the upload).
        let mut superseded = None;
        let outcome = self.db.write(|tx| {
            let Some(cur) = db::get_node(tx, n.id)? else { return Ok(UploadOutcome::Gone) };
            if cur.deleted {
                db::add_orphan(tx, &entry)?;
                return Ok(UploadOutcome::Gone);
            }
            if let Some(old) = self.remote_entry(&cur) {
                db::add_orphan(tx, &old)?;
                superseded = Some(old);
            }
            let clean = cur.generation == n.generation;
            tx.execute(
                "UPDATE node SET remote_id = ?2, remote_parent_id = ?3, remote_name = ?4, op_seq = ?5, attempts = 0,
                     last_error = NULL, retry_at_ms = 0,
                     dirty = CASE WHEN ?6 THEN 0 ELSE dirty END,
                     pending = CASE WHEN ?6 THEN pending & ~1 ELSE pending END,
                     size = CASE WHEN ?6 THEN ?7 ELSE size END,
                     mtime_ms = CASE WHEN ?6 THEN ?8 ELSE mtime_ms END
                 WHERE id = ?1",
                params![n.id, entry.id, parent, n.name, seq, clean, size as i64, mtime],
            )?;
            recompute_move(tx, n.id)?;
            Ok(if clean { UploadOutcome::Clean } else { UploadOutcome::ChangedMeanwhile })
        })?;

        // Retire the previous version right away (it stays in the uloz.to trash), so other
        // devices stop treating it as current as soon as possible.
        if let Some(old) = superseded {
            match self.remote.trash(&old).await {
                Ok(()) | Err(RemoteError::NotFound(_)) => {
                    let seq = self.next_seq();
                    self.recently_gone
                        .lock()
                        .unwrap()
                        .insert(old.id.clone(), seq);
                    self.db.remove_orphan(&old.id)?;
                }
                Err(e) => debug!(
                    "retiring the previous version of {} failed (will retry): {e}",
                    item.path
                ),
            }
        }

        match outcome {
            UploadOutcome::Clean => {
                let local = self.local().clone();
                let it = item.clone();
                let still_same =
                    tokio::task::spawn_blocking(move || local.uploaded(&it, size, mtime))
                        .await
                        .map_err(io::Error::other)?
                        .unwrap_or_else(|e| {
                            warn!("marking {} as synced failed: {e}", item.path);
                            true
                        });
                if !still_same {
                    self.db.write(|tx| {
                        tx.execute(
                            "UPDATE node SET dirty = 1, pending = pending | 1, generation = generation + 1, changed_ms = ?2 WHERE id = ?1",
                            params![n.id, now_ms()],
                        )
                        .map(|_| ())
                    })?;
                }
                self.activity(ActivityKind::Uploaded, &item.path, size, None);
            }
            UploadOutcome::ChangedMeanwhile => {
                self.activity(
                    ActivityKind::Uploaded,
                    &item.path,
                    size,
                    Some("Changed again while uploading; uploading the newer version"),
                );
            }
            UploadOutcome::Gone => {}
        }
        self.wake_worker.notify_one();
        Ok(())
    }

    async fn push_move(&self, n: &Node) -> Result<()> {
        let parent = self
            .parent_remote_id(n)?
            .ok_or_else(|| EngineError::NotFound("parent folder".into()))?;
        let entry = self
            .remote_entry(n)
            .ok_or_else(|| EngineError::NotFound("remote item".into()))?;
        let moved = self.remote.rename(&entry, &parent, &n.name).await?;
        let seq = self.next_seq();
        self.db.write(|tx| {
            tx.execute(
                "UPDATE node SET remote_parent_id = ?2, remote_name = ?3, op_seq = ?4, attempts = 0, last_error = NULL
                 WHERE id = ?1",
                params![n.id, moved.parent_id, moved.name, seq],
            )?;
            recompute_move(tx, n.id)
        })?;
        let path = self.db.path_of(n.id)?.unwrap_or_default();
        self.activity(ActivityKind::Moved, &path, 0, None);
        Ok(())
    }

    // ---- issues --------------------------------------------------------------------------

    pub fn issues(&self) -> Result<Vec<crate::ipc::Issue>> {
        let mut list = self.db.issues()?;
        // The remote mass-delete guard can only be confirmed (or left waiting).
        for i in &mut list {
            if i.kind == IssueKind::MassDelete
                && i.message.contains("deleted from your uloz.to vault")
            {
                i.actions = vec![IssueAction::ConfirmDelete, IssueAction::Dismiss];
            }
        }
        Ok(list)
    }

    pub fn resolve_issue(self: &Arc<Self>, id: i64, action: IssueAction) -> Result<()> {
        let Some((_issue, key, node)) = self.db.issue(id)? else {
            return Err(EngineError::NotFound(format!("issue {id}")));
        };
        match action {
            IssueAction::Dismiss | IssueAction::SignIn => {}
            IssueAction::Retry => {
                if let Some(node) = node {
                    self.db.write(|tx| {
                        tx.execute(
                            "UPDATE node SET retry_at_ms = 0, attempts = 0 WHERE id = ?1",
                            [node],
                        )
                        .map(|_| ())
                    })?;
                }
                self.wake_evictor.notify_one();
                self.sync_now();
            }
            IssueAction::ConfirmDelete => {
                let until = now_ms() + 15 * 60_000;
                if key == "mass_delete:remote" {
                    self.allow_remote_removal_until
                        .store(until, Ordering::Relaxed);
                    self.held_remote_removals.store(0, Ordering::Relaxed);
                    self.wake_scanner.notify_one();
                } else {
                    self.allow_local_deletes_until
                        .store(until, Ordering::Relaxed);
                    self.wake_worker.notify_one();
                }
            }
            IssueAction::RestoreDeleted => {
                self.restore_local_deletes()?;
                self.wake_scanner.notify_one();
            }
        }
        self.db.clear_issue(&key)?;
        self.issues_changed();
        Ok(())
    }

    /// Undoes pending local deletions: the items reappear (cloud-only) and the next scan
    /// fills in their contents.
    fn restore_local_deletes(&self) -> Result<()> {
        let tombstones: Vec<Node> = self.db.read(|c| {
            let mut st = c.prepare(&format!(
                "SELECT {} FROM node WHERE deleted = 1 AND pending & 4 != 0",
                db::NODE_COLUMNS
            ))?;
            st.query_map([], Node::from_row)?.collect()
        })?;
        for n in tombstones {
            let Some(parent) = n.parent_id else { continue };
            let mut name = n.name.clone();
            let mut i = 2;
            while self.db.child(parent, &name)?.is_some() {
                let (stem, ext) = plan::split_ext(&n.name);
                name = format!("{stem} ({i}){ext}");
                i += 1;
            }
            self.db.write(|tx| {
                tx.execute(
                    "UPDATE node SET deleted = 0, pending = 0, name = ?2, name_key = ?3, hydrated = 0 WHERE id = ?1",
                    params![n.id, name, name_key(&name)],
                )?;
                recompute_move(tx, n.id)
            })?;
            let restored = self.node(n.id)?;
            let item = self.item(&restored)?;
            if let Err(e) = self.local().create(&item) {
                warn!("restoring {} failed: {e}", item.path);
            }
        }
        Ok(())
    }
}

enum UploadOutcome {
    Clean,
    ChangedMeanwhile,
    Gone,
}

/// Sets or clears the MOVE bit depending on whether the local location differs from the
/// remote one.
fn recompute_move(tx: &rusqlite::Connection, id: NodeId) -> rusqlite::Result<()> {
    tx.execute(
        "UPDATE node SET pending = CASE
             WHEN deleted = 0 AND remote_id IS NOT NULL
                  AND (remote_parent_id IS NOT (SELECT p.remote_id FROM node p WHERE p.id = node.parent_id)
                       OR remote_name IS NOT name)
             THEN pending | 2 ELSE pending & ~2 END
         WHERE id = ?1",
        [id],
    )
    .map(|_| ())
}

fn backoff_ms(attempts: i64) -> i64 {
    let base = 2_000i64.saturating_mul(1 << attempts.clamp(0, 8));
    base.min(15 * 60_000)
}

fn plural<'a>(n: u32, one: &'a str, many: &'a str) -> &'a str {
    if n == 1 { one } else { many }
}

fn status_eq(a: &Status, b: &Status) -> bool {
    serde_json::to_value(a).ok() == serde_json::to_value(b).ok()
}

/// Unregisters a download from the transfer list when its stream is dropped.
struct TransferGuard {
    engine: Arc<Engine>,
    id: u64,
}

impl Drop for TransferGuard {
    fn drop(&mut self) {
        self.engine.end_transfer(self.id);
    }
}

struct GuardedStream {
    inner: ByteStream,
    _guard: TransferGuard,
}

impl futures::Stream for GuardedStream {
    type Item = io::Result<bytes::Bytes>;
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}
