//! Local change detection.
//!
//! Every path that may have changed (from the directory watcher, Cloud Files callbacks, or a
//! full walk) is compared with the database:
//!
//! * a placeholder whose identity belongs to a node at another path was moved there;
//! * an item the database doesn't know is new (folders are converted to placeholders right
//!   away so later moves are recognised; files once they have been uploaded);
//! * a file that is no longer in sync (or an ordinary file whose size/mtime differ) changed;
//! * a database path with nothing on disk was deleted;
//! * a pin state that differs from the database is a request from Explorer ("Always keep on
//!   this device" / "Free up space").
//!
//! Because only the end state matters, bursts (safe-save patterns, copies in progress) are
//! simply debounced, and changes the engine made itself compare equal once the engine has
//! recorded them (until then they are suppressed, see [`Shared::begin_op`]).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use cloud_filter::placeholder::PinState;
use drive_core::db::{Node, ROOT_ID, pending};
use drive_core::engine::Engine;
use drive_core::engine::local::is_ignored_name;
use drive_core::remote::Kind;
use tracing::{debug, info, warn};

use super::local::{mark_folder_in_sync, set_identity};
use super::util::{self, BlobId, DiskItem, encode_blob, inspect, key};
use super::{Msg, Shared};

/// Wait for this much quiet before looking at changed paths...
const QUIET: Duration = Duration::from_millis(400);
/// ...but never longer than this after the first change.
const MAX_DELAY: Duration = Duration::from_secs(3);
/// Safety net: compare everything periodically.
const FULL_SCAN_EVERY: Duration = Duration::from_secs(15 * 60);
/// Without a working watcher, compare everything this often.
const FULL_SCAN_WITHOUT_WATCHER: Duration = Duration::from_secs(30);
const RETRY: Duration = Duration::from_secs(2);

pub struct Worker {
    shared: Arc<Shared>,
    /// Whether pin changes are acted on (needs a started engine).
    pins: bool,
    queue: HashMap<String, PathBuf>,
    first_change: Option<Instant>,
    last_change: Instant,
    deferred: HashMap<String, (PathBuf, Instant)>,
    watcher_ok: bool,
    last_full: Instant,
}

impl Worker {
    pub fn new(shared: Arc<Shared>, pins: bool) -> Self {
        Self {
            shared,
            pins,
            queue: HashMap::new(),
            first_change: None,
            last_change: Instant::now(),
            deferred: HashMap::new(),
            watcher_ok: true,
            last_full: Instant::now(),
        }
    }

    pub fn run(mut self, rx: Receiver<Msg>) {
        // Engine calls may spawn Tokio tasks (e.g. downloading pinned files).
        let _rt = self.shared.rt.enter();
        loop {
            let now = Instant::now();
            let mut wait = if self.watcher_ok {
                FULL_SCAN_EVERY
            } else {
                FULL_SCAN_WITHOUT_WATCHER
            }
            .saturating_sub(now - self.last_full);
            if let Some(first) = self.first_change {
                let due = (self.last_change + QUIET).min(first + MAX_DELAY);
                wait = wait.min(due.saturating_duration_since(now));
            }
            if let Some(at) = self.deferred.values().map(|(_, at)| *at).min() {
                wait = wait.min(at.saturating_duration_since(now));
            }
            if !self
                .shared
                .unsynced_folders
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty()
            {
                wait = wait.min(Duration::from_secs(2));
            }
            match rx.recv_timeout(wait) {
                Ok(Msg::Paths(paths)) => {
                    for p in paths {
                        self.enqueue(p);
                    }
                }
                Ok(Msg::Subtree(dir)) => {
                    for p in walk(&dir) {
                        self.enqueue(p);
                    }
                }
                Ok(Msg::FullScan) => self.full_pass(),
                Ok(Msg::WatcherStopped) => self.watcher_ok = false,
                Ok(Msg::Stop) | Err(RecvTimeoutError::Disconnected) => return,
                Err(RecvTimeoutError::Timeout) => {}
            }
            let now = Instant::now();
            // Deferred paths whose suppression is over.
            let due: Vec<String> = self
                .deferred
                .iter()
                .filter(|(_, (_, at))| *at <= now)
                .map(|(k, _)| k.clone())
                .collect();
            for k in due {
                if let Some((p, _)) = self.deferred.remove(&k) {
                    self.enqueue(p);
                }
            }
            if let Some(first) = self.first_change
                && (now >= self.last_change + QUIET || now >= first + MAX_DELAY)
            {
                let batch: Vec<PathBuf> = self.queue.drain().map(|(_, p)| p).collect();
                self.first_change = None;
                self.process(batch);
            }
            self.sync_folders();
            let interval = if self.watcher_ok {
                FULL_SCAN_EVERY
            } else {
                FULL_SCAN_WITHOUT_WATCHER
            };
            if self.last_full.elapsed() >= interval {
                self.full_pass();
            }
        }
    }

    fn enqueue(&mut self, p: PathBuf) {
        let k = p.to_string_lossy().to_lowercase();
        self.queue.insert(k, p);
        let now = Instant::now();
        self.first_change.get_or_insert(now);
        self.last_change = now;
    }

    /// Compares the whole folder with the database.
    pub fn full_pass(&mut self) {
        self.last_full = Instant::now();
        let Some(engine) = self.shared.engine() else {
            return;
        };
        if !self.shared.root.is_dir() {
            warn!(
                "the Drive folder {} is missing; not syncing local changes",
                self.shared.root.display()
            );
            return;
        }
        let mut paths = walk(&self.shared.root);
        match engine.db().all_nodes() {
            Ok(nodes) => {
                for n in nodes.iter().filter(|n| !n.deleted && n.id != ROOT_ID) {
                    if let Ok(Some(p)) = engine.db().path_of(n.id) {
                        paths.push(self.shared.abs(&p));
                    }
                }
            }
            Err(e) => warn!("reading the database failed: {e}"),
        }
        if self.pins {
            paths.push(self.shared.root.clone());
        }
        let n = paths.len();
        self.process(paths);
        debug!("compared {n} paths with the database");
    }

    fn process(&mut self, paths: Vec<PathBuf>) {
        let Some(engine) = self.shared.engine() else {
            return;
        };
        if !self.shared.root.is_dir() {
            warn!(
                "the Drive folder {} is missing; not syncing local changes",
                self.shared.root.display()
            );
            return;
        }
        let mut seen = std::collections::HashSet::new();
        let mut existing: Vec<(String, PathBuf, DiskItem)> = Vec::new();
        let mut missing: Vec<String> = Vec::new();
        for abs in paths {
            let Some(rel) = self.shared.rel(&abs) else {
                continue;
            };
            if rel.split('/').any(is_ignored_name) {
                continue;
            }
            if let Some(until) = self.shared.suppressed_until(&rel) {
                self.deferred.insert(key(&rel), (abs, until));
                continue;
            }
            match inspect(&abs, self.shared.tag) {
                Ok(Some(d)) => {
                    // The spelling on disk (the path we were told about may differ in case).
                    let rel = std::fs::canonicalize(&abs)
                        .ok()
                        .and_then(|c| self.shared.rel(&c))
                        .unwrap_or(rel);
                    if rel.split('/').any(is_ignored_name) || !seen.insert(rel.clone()) {
                        continue;
                    }
                    existing.push((rel, abs, d));
                }
                Ok(None) => {
                    if seen.insert(rel.clone()) {
                        missing.push(rel);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Unsupported => {}
                Err(e) => {
                    debug!("can't look at {}: {e}", abs.display());
                    self.deferred
                        .insert(key(&rel), (abs, Instant::now() + RETRY));
                }
            }
        }
        // Parents before children; moves and creations before deletions (a move reports
        // both the old and the new path).
        existing.sort_by_key(|(rel, _, _)| util::depth(rel));
        missing.sort_by_key(|rel| util::depth(rel));
        for (rel, abs, d) in existing {
            if let Err(e) = self.existing(&engine, &rel, &abs, &d) {
                warn!("handling a local change to {rel} failed: {e}");
            }
        }
        for rel in missing {
            if let Err(e) = self.missing(&engine, &rel) {
                warn!("handling the local deletion of {rel} failed: {e}");
            }
        }
    }

    fn existing(
        &mut self,
        engine: &Arc<Engine>,
        rel: &str,
        abs: &Path,
        d: &DiskItem,
    ) -> anyhow::Result<()> {
        let db = engine.db();
        if rel.is_empty() {
            if let Some(root) = db.node(ROOT_ID)? {
                self.pins(engine, rel, &root, d);
            }
            return Ok(());
        }
        let by_path = engine.node_by_path(rel)?;
        let by_blob = match d.ph.as_ref().map(|p| p.id) {
            Some(BlobId::Node(id)) => db.node(id)?.filter(|n| !n.deleted),
            _ => None,
        };
        let mut node = by_path.clone();
        if let Some(bn) = by_blob {
            let old = db.path_of(bn.id)?.unwrap_or_default();
            if old == rel {
                node = Some(bn);
            } else if bn.id != ROOT_ID {
                // Is the item still at its old place too (a copy that kept the identity)?
                let same_item = key(&old) == key(rel);
                let duplicate = !same_item
                    && inspect(&self.shared.abs(&old), self.shared.tag)
                        .ok()
                        .flatten()
                        .and_then(|o| o.ph)
                        .is_some_and(|p| p.id == BlobId::Node(bn.id));
                if !duplicate {
                    info!("local move {old} -> {rel}");
                    engine.local_rename(&old, rel)?;
                    node = db.node(bn.id)?;
                }
            }
        }
        match node {
            None => self.new_item(engine, rel, abs, d),
            Some(n) if (n.kind == Kind::Folder) != d.is_dir => {
                info!("{rel} changed between file and folder");
                engine.local_delete(rel)?;
                self.new_item(engine, rel, abs, d)
            }
            Some(n) => {
                self.known_item(engine, rel, abs, d, &n)?;
                self.pins(engine, rel, &n, d);
                Ok(())
            }
        }
    }

    fn new_item(
        &mut self,
        engine: &Arc<Engine>,
        rel: &str,
        abs: &Path,
        d: &DiskItem,
    ) -> anyhow::Result<()> {
        if d.is_dir {
            let id = engine.local_create(rel, Kind::Folder, 0, d.mtime_ms)?;
            info!("new local folder {rel}");
            match set_identity(abs, &encode_blob(self.shared.tag, id), false) {
                Ok(()) => {
                    self.shared
                        .unsynced_folders
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(id, abs.to_path_buf());
                }
                Err(e) => debug!("converting {rel} to a placeholder failed: {e}"),
            }
            // Its contents (a folder moved or copied in reports only itself).
            let children = walk_shallow(abs);
            self.process(children);
            return Ok(());
        }
        if let Some(ph) = &d.ph
            && ph.partial_on_disk
        {
            // A cloud-only placeholder we don't know (left over from an older state database).
            // Its content isn't here, so it can't be uploaded; it is replaced if the cloud has
            // the item.
            debug!("ignoring unknown cloud-only placeholder {rel}");
            return Ok(());
        }
        engine.local_create(rel, Kind::File, d.size, d.mtime_ms)?;
        info!("new local file {rel} ({} bytes)", d.size);
        Ok(())
    }

    fn known_item(
        &mut self,
        engine: &Arc<Engine>,
        rel: &str,
        abs: &Path,
        d: &DiskItem,
        n: &Node,
    ) -> anyhow::Result<()> {
        let blob = encode_blob(self.shared.tag, n.id);
        if d.is_dir {
            if d.ph.as_ref().is_none_or(|p| p.id != BlobId::Node(n.id)) {
                let synced = n.remote_id.is_some();
                if set_identity(abs, &blob, synced).is_ok() && !synced {
                    self.shared
                        .unsynced_folders
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(n.id, abs.to_path_buf());
                }
            }
            return Ok(());
        }
        let same_meta = d.size == n.size && d.mtime_ms == n.mtime_ms;
        let clean = !n.dirty && n.pending & pending::PUSH == 0 && n.remote_id.is_some();
        match &d.ph {
            Some(ph) if ph.in_sync && ph.id == BlobId::Node(n.id) => {
                // Unchanged; keep the download state in step.
                let on_disk = !ph.partial_on_disk;
                if on_disk != n.hydrated {
                    engine.mark_hydrated(n.id, on_disk);
                }
            }
            Some(ph) => {
                if same_meta && clean && !ph.partial_on_disk {
                    // Attribute-only change (or an identity from an older database).
                    set_identity(abs, &blob, true)?;
                } else if !same_meta || !n.dirty {
                    info!("local change to {rel}");
                    engine.local_modified(rel, d.size, d.mtime_ms)?;
                }
            }
            None => {
                if !same_meta {
                    info!("local change to {rel}");
                    engine.local_modified(rel, d.size, d.mtime_ms)?;
                } else if clean {
                    // Uploaded, but converting it to a placeholder didn't happen (it was open).
                    if let Err(e) = set_identity(abs, &blob, true) {
                        debug!("converting {rel} to a placeholder failed: {e}");
                        self.deferred.insert(
                            key(rel),
                            (abs.to_path_buf(), Instant::now() + Duration::from_secs(10)),
                        );
                    } else if n.pinned {
                        let _ = super::local::set_pin(abs, PinState::Pinned, false);
                    }
                }
            }
        }
        Ok(())
    }

    /// Explorer's "Always keep on this device" and "Free up space".
    fn pins(&self, engine: &Arc<Engine>, rel: &str, n: &Node, d: &DiskItem) {
        if !self.pins {
            return;
        }
        let Some(ph) = &d.ph else { return };
        match ph.pin {
            PinState::Pinned if !n.pinned => {
                info!(
                    "keeping {} on this device",
                    if rel.is_empty() { "everything" } else { rel }
                );
                if let Err(e) = engine.set_pinned(rel, true) {
                    warn!("pinning {rel} failed: {e}");
                }
            }
            PinState::Unpinned => {
                info!(
                    "freeing up space used by {}",
                    if rel.is_empty() {
                        "the Drive folder"
                    } else {
                        rel
                    }
                );
                let engine = engine.clone();
                let rel = rel.to_string();
                self.shared.rt.spawn(async move {
                    if let Err(e) = engine.free_up_space(&rel).await {
                        warn!("freeing up {rel} failed: {e}");
                    }
                });
            }
            PinState::Unspecified | PinState::Inherit if n.pinned => {
                // "Always keep on this device" was turned off. (An item inside a folder that is
                // still pinned only lost its own flag; Windows keeps it with the folder.)
                let parent_pinned = n
                    .parent_id
                    .and_then(|p| engine.db().node(p).ok().flatten())
                    .is_some_and(|p| p.pinned);
                if !parent_pinned {
                    info!("no longer keeping {rel} on this device");
                    if let Err(e) = engine.set_pinned(rel, false) {
                        warn!("unpinning {rel} failed: {e}");
                    }
                }
            }
            _ => {}
        }
    }

    fn missing(&mut self, engine: &Arc<Engine>, rel: &str) -> anyhow::Result<()> {
        let Some(n) = engine.node_by_path(rel)? else {
            return Ok(());
        };
        if n.id == ROOT_ID {
            return Ok(());
        }
        // Re-check: it may have reappeared (safe-save patterns) since we looked.
        if inspect(&self.shared.abs(rel), self.shared.tag)
            .ok()
            .flatten()
            .is_some()
        {
            return Ok(());
        }
        info!("local delete {rel}");
        engine.local_delete(rel)?;
        Ok(())
    }

    /// Marks folders in sync once the cloud has them.
    fn sync_folders(&self) {
        let Some(engine) = self.shared.engine() else {
            return;
        };
        let mut map = self
            .shared
            .unsynced_folders
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if map.is_empty() {
            return;
        }
        map.retain(|id, path| match engine.db().node(*id) {
            Ok(Some(n)) if n.deleted => false,
            Ok(Some(n)) if n.remote_id.is_some() => {
                mark_folder_in_sync(path);
                false
            }
            Ok(Some(_)) => true,
            _ => false,
        });
    }
}

/// Everything below `dir` (not following links), parents first.
fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_symlink() || is_ignored_name(&e.file_name().to_string_lossy()) {
                continue;
            }
            let p = e.path();
            if ft.is_dir() {
                stack.push(p.clone());
            }
            out.push(p);
        }
    }
    out
}

fn walk_shallow(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|rd| rd.flatten().map(|e| e.path()).collect())
        .unwrap_or_default()
}
