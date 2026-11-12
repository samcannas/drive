//! Folder backups: folders outside the Drive folder (Desktop, Documents, …) mirrored one way
//! into the vault under `Backups/<device>/<name>`.
//!
//! * A separate **mirror engine** ([`drive_core::engine::mirror`]) keeps the cloud copy equal
//!   to the folders. It shares the main engine's upload throttle and automatic upload speed.
//! * [`local::BackupLocal`] reads the folders and never changes them.
//! * The [`Manager`] reads the folders ([`scan`]), turns differences into engine changes,
//!   watches them for changes, rescans them periodically as a safety net, and reports
//!   progress.
//!
//! Safety rules: the user's files are never moved, changed or deleted; a folder that can't be
//! read (unplugged drive, renamed folder) is reported, never treated as "everything deleted";
//! large deletions wait for confirmation (the engine's mass-delete guards).

#[cfg(test)]
mod e2e;
pub mod local;
pub mod scan;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use drive_core::db::{name_key, now_ms};
use drive_core::engine::{Engine, MirrorOp};
use drive_core::ipc::{BackupInfo, BackupState, FolderSuggestion, KnownFolder, SyncState};
use drive_core::remote::Kind;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use local::Sources;
use scan::{Filter, Item, Scan};

/// Top-level folder in the vault that holds every device's backups.
pub const BACKUPS_FOLDER: &str = "Backups";

/// A full rescan of every folder this often, in case change notifications were missed.
const RESCAN_EVERY: Duration = Duration::from_secs(30 * 60);
/// ...or this often when the system couldn't watch every folder (Linux watch limits).
const RESCAN_EVERY_UNWATCHED: Duration = Duration::from_secs(5 * 60);

/// One backed-up folder, as stored in the configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BackupFolder {
    pub id: String,
    /// Display name, top-level folder name in the engine and folder name in the vault.
    pub name: String,
    pub path: PathBuf,
    #[serde(default)]
    pub known: Option<KnownFolder>,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default = "scan::default_excludes")]
    pub excludes: Vec<String>,
    #[serde(default)]
    pub added_ms: i64,
    /// "Save space": the folder stays where it is, but files not used lately live only in the
    /// cloud and download again when opened (an on-demand engine runs it instead of the mirror).
    #[serde(default)]
    pub on_demand: bool,
    /// On-demand: keep this many bytes of recently used files on this device.
    #[serde(default)]
    pub keep_bytes: Option<u64>,
    /// Switching back from on-demand to a full copy is under way (downloading everything).
    #[serde(default)]
    pub leaving_on_demand: bool,
}

/// On-demand folders keep this much of their recently used files on this device by default.
pub const DEFAULT_KEEP_BYTES: u64 = 5 << 30;

impl BackupFolder {
    /// Whether the mirror engine backs this folder up (on-demand folders run on their own).
    pub fn mirrored(&self) -> bool {
        self.enabled && !self.on_demand
    }
}

fn yes() -> bool {
    true
}

/// Where this device's backups live in the vault.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BackupCloud {
    /// The device folder's name under `Backups/`.
    pub device_folder: String,
    /// Remote id of `Backups/<device_folder>` (the backup engine's root).
    pub device_slug: String,
}

impl BackupCloud {
    /// The device folder's path in the vault (and in the Drive folder on other devices).
    pub fn vault_path(&self) -> String {
        format!("{BACKUPS_FOLDER}/{}", self.device_folder)
    }
}

/// The well-known folders of this user, in display order.
pub fn known_folders() -> Vec<(KnownFolder, &'static str, PathBuf)> {
    let home = dirs::home_dir();
    let candidates = [
        (KnownFolder::Desktop, "Desktop", dirs::desktop_dir()),
        (KnownFolder::Documents, "Documents", dirs::document_dir()),
        (KnownFolder::Downloads, "Downloads", dirs::download_dir()),
        (KnownFolder::Pictures, "Pictures", dirs::picture_dir()),
        (KnownFolder::Videos, "Videos", dirs::video_dir()),
        (KnownFolder::Music, "Music", dirs::audio_dir()),
    ];
    let mut seen = HashSet::new();
    candidates
        .into_iter()
        .filter_map(|(k, n, p)| Some((k, n, p?)))
        // XDG dirs that aren't configured point at the home folder itself.
        .filter(|(_, _, p)| Some(p) != home.as_ref())
        .filter(|(_, _, p)| seen.insert(p.clone()))
        .collect()
}

/// Lower-cased, `/`-separated absolute path for overlap checks.
fn norm(p: &Path) -> String {
    let p = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let s = p.to_string_lossy().replace('\\', "/");
    let s = s
        .trim_start_matches("//?/")
        .trim_end_matches('/')
        .to_string();
    if cfg!(windows) { s.to_lowercase() } else { s }
}

fn inside(child: &str, parent: &str) -> bool {
    child == parent || child.starts_with(&format!("{parent}/"))
}

/// Why `path` can't be backed up next to the Drive folder, Drive's own data and other backups.
pub fn overlap_problem(
    path: &Path,
    drive_folder: &Path,
    own_data: &[&Path],
    others: &[&Path],
) -> Option<String> {
    let p = norm(path);
    let d = norm(drive_folder);
    if inside(&p, &d) {
        return Some("It's inside your Drive folder, which is already in the cloud.".into());
    }
    if inside(&d, &p) {
        return Some(
            "It contains your Drive folder. Back up the folders inside it instead.".into(),
        );
    }
    for own in own_data {
        let o = norm(own);
        if inside(&p, &o) || inside(&o, &p) {
            return Some("It contains Drive's own data.".into());
        }
    }
    for other in others {
        let o = norm(other);
        if inside(&p, &o) {
            return Some("It's already backed up as part of another folder.".into());
        }
        if inside(&o, &p) {
            return Some(
                "It contains a folder that's already backed up. Stop that backup first.".into(),
            );
        }
    }
    if path.parent().is_none() || p.matches('/').count() == 0 {
        return Some("Back up folders, not a whole drive.".into());
    }
    None
}

/// A display name that's valid everywhere and unique among `taken`.
pub fn unique_name(base: &str, taken: &[String]) -> String {
    let clean: String = base
        .chars()
        .map(|c| {
            if "\\/:*?\"<>|".contains(c) || (c as u32) < 32 {
                '_'
            } else {
                c
            }
        })
        .collect();
    let clean = clean.trim().trim_end_matches('.').to_string();
    let clean = if clean.is_empty() {
        "Folder".to_string()
    } else {
        clean
    };
    let keys: HashSet<String> = taken.iter().map(|t| name_key(t)).collect();
    if !keys.contains(&name_key(&clean)) {
        return clean;
    }
    (2..)
        .map(|i| format!("{clean} ({i})"))
        .find(|n| !keys.contains(&name_key(n)))
        .expect("infinite range")
}

// ---- diffing ---------------------------------------------------------------------------------

/// What the database knows about a folder (paths relative to the backed-up folder).
type Known = BTreeMap<String, Item>;

fn join(name: &str, rel: &str) -> String {
    if rel.is_empty() {
        name.to_string()
    } else {
        format!("{name}/{rel}")
    }
}

fn parent_of(rel: &str) -> Option<&str> {
    if rel.is_empty() {
        None
    } else {
        Some(rel.rfind('/').map(|i| &rel[..i]).unwrap_or(""))
    }
}

fn under(rel: &str, dir: &str) -> bool {
    dir.is_empty() || rel == dir || rel.starts_with(&format!("{dir}/"))
}

/// Turns the difference between the folder (`disk`) and the database (`known`) into engine
/// changes. Both cover the same scope. With `detect_renames`, a missing item and a new one
/// that look the same (a file with the same size and time, a folder with the same name) become
/// a move, so nothing is uploaded again; their contents are compared in a second pass.
fn diff(
    name: &str,
    disk: &BTreeMap<String, Item>,
    known: &Known,
    unreadable: &[String],
    detect_renames: bool,
) -> (Vec<MirrorOp>, Vec<String>) {
    let mut ops = Vec::new();
    let new: Vec<(&String, &Item)> = disk
        .iter()
        .filter(|(p, _)| !known.contains_key(*p))
        .collect();
    // Items in folders that couldn't be read are unknown, not deleted.
    let missing: Vec<(&String, &Item)> = known
        .iter()
        .filter(|(p, _)| !disk.contains_key(*p))
        .filter(|(p, _)| {
            !unreadable
                .iter()
                .any(|u| under(p, u) && p.as_str() != u.as_str())
        })
        .collect();

    let mut renamed_from: Vec<String> = Vec::new();
    let mut renamed_to: Vec<String> = Vec::new();
    if detect_renames {
        // Folders: same name, different place (or the only folder that went and came).
        let new_dirs: Vec<&String> = new
            .iter()
            .filter(|(_, i)| i.kind == Kind::Folder)
            .map(|(p, _)| *p)
            .collect();
        let gone_dirs: Vec<&String> = missing
            .iter()
            .filter(|(_, i)| i.kind == Kind::Folder)
            .map(|(p, _)| *p)
            .collect();
        let top = |ps: &[&String]| -> Vec<String> {
            ps.iter()
                .filter(|p| !ps.iter().any(|q| q != *p && under(p, q)))
                .map(|p| p.to_string())
                .collect()
        };
        let (new_top, gone_top) = (top(&new_dirs), top(&gone_dirs));
        let leaf = |p: &str| p.rsplit('/').next().unwrap_or(p).to_string();
        for g in &gone_top {
            let candidates: Vec<&String> = if new_top.len() == 1 && gone_top.len() == 1 {
                new_top.iter().collect()
            } else {
                new_top.iter().filter(|n| leaf(n) == leaf(g)).collect()
            };
            if candidates.len() == 1 && !renamed_to.contains(candidates[0]) {
                renamed_from.push(g.clone());
                renamed_to.push(candidates[0].clone());
            }
        }
        // Files: same size and modification time, nothing else alike.
        type Candidates<'a> = (Vec<&'a String>, Vec<&'a String>);
        let mut by_key: HashMap<(u64, i64), Candidates> = HashMap::new();
        for (p, i) in &new {
            if i.kind == Kind::File && !renamed_to.iter().any(|d| under(p, d)) {
                by_key.entry((i.size, i.mtime_ms)).or_default().0.push(*p);
            }
        }
        for (p, i) in &missing {
            if i.kind == Kind::File && !renamed_from.iter().any(|d| under(p, d)) {
                by_key.entry((i.size, i.mtime_ms)).or_default().1.push(*p);
            }
        }
        for (_, (n, g)) in by_key {
            if n.len() == 1 && g.len() == 1 {
                renamed_from.push(g[0].clone());
                renamed_to.push(n[0].clone());
            }
        }
        for (from, to) in renamed_from.iter().zip(&renamed_to) {
            ops.push(MirrorOp::Rename {
                from: join(name, from),
                to: join(name, to),
            });
        }
    }
    let moved_away = |p: &str| renamed_from.iter().any(|d| under(p, d));
    let moved_here = |p: &str| renamed_to.iter().any(|d| under(p, d));

    for (p, i) in &new {
        if moved_here(p) {
            continue;
        }
        ops.push(MirrorOp::Create {
            path: join(name, p),
            kind: i.kind,
            size: i.size,
            mtime_ms: i.mtime_ms,
        });
    }
    for (p, d) in disk {
        if let Some(k) = known.get(p) {
            if k.kind != d.kind {
                ops.push(MirrorOp::Delete {
                    path: join(name, p),
                });
                ops.push(MirrorOp::Create {
                    path: join(name, p),
                    kind: d.kind,
                    size: d.size,
                    mtime_ms: d.mtime_ms,
                });
            } else if d.kind == Kind::File && (k.size != d.size || k.mtime_ms != d.mtime_ms) {
                ops.push(MirrorOp::Modify {
                    path: join(name, p),
                    size: d.size,
                    mtime_ms: d.mtime_ms,
                });
            }
        }
    }
    // Only the outermost missing item: deleting a folder takes its contents along.
    for (p, _) in &missing {
        if moved_away(p) {
            continue;
        }
        let parent_missing = parent_of(p)
            .is_some_and(|par| !par.is_empty() && missing.iter().any(|(q, _)| *q == par));
        if !parent_missing {
            ops.push(MirrorOp::Delete {
                path: join(name, p),
            });
        }
    }
    (ops, renamed_to)
}

// ---- reading the database --------------------------------------------------------------------

/// Everything the database knows under `name/rel` (deep) or directly in it (shallow), with
/// paths relative to the backed-up folder.
fn known_items(engine: &Engine, name: &str, rel: &str, deep: bool) -> Result<Known> {
    let Some(start) = engine.node_by_path(&join(name, rel))? else {
        return Ok(Known::new());
    };
    if start.kind != Kind::Folder {
        return Ok(Known::new());
    }
    let prefix = rel.to_string();
    let rows: Vec<(String, i64, i64, i64)> = engine.db().read(|c| {
        let sql = if deep {
            "WITH RECURSIVE t(id, path) AS (
                 SELECT id, name FROM node WHERE parent_id = ?1 AND deleted = 0
                 UNION ALL SELECT n.id, t.path || '/' || n.name FROM node n JOIN t ON n.parent_id = t.id
                 WHERE n.deleted = 0)
             SELECT t.path, n.kind, n.size, n.mtime_ms FROM t JOIN node n ON n.id = t.id"
        } else {
            "SELECT name, kind, size, mtime_ms FROM node WHERE parent_id = ?1 AND deleted = 0"
        };
        let mut st = c.prepare(sql)?;
        let rows = st.query_map([start.id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        rows.collect()
    })?;
    Ok(rows
        .into_iter()
        .map(|(p, kind, size, mtime)| {
            let item = Item {
                kind: if kind == 1 { Kind::Folder } else { Kind::File },
                size: size.max(0) as u64,
                mtime_ms: mtime,
            };
            (join(&prefix, &p).trim_start_matches('/').to_string(), item)
        })
        .collect())
}

// ---- the manager -----------------------------------------------------------------------------

/// Result of reading a folder: changes, moved folders, skipped online-only files, problem.
type ScanOutcome = (Vec<MirrorOp>, Vec<String>, u64, Option<String>);

#[derive(Debug, Default, Clone)]
struct Runtime {
    scanning: bool,
    /// The folder can't be read right now (missing, unplugged, no permission).
    problem: Option<String>,
    online_only: u64,
    last_complete_ms: Option<i64>,
    complete_now: bool,
}

struct Watch {
    watcher: notify::RecommendedWatcher,
    /// Folders watched individually (Linux); empty when the root is watched recursively.
    dirs: HashSet<PathBuf>,
}

#[derive(Debug)]
struct Changed {
    id: String,
    paths: Vec<PathBuf>,
    rescan: bool,
}

pub struct Manager {
    engine: Arc<Engine>,
    sources: Sources,
    folders: RwLock<Vec<BackupFolder>>,
    runtime: Mutex<HashMap<String, Runtime>>,
    watches: Mutex<HashMap<String, Watch>>,
    changes: tokio::sync::mpsc::UnboundedSender<Changed>,
    changes_rx: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<Changed>>>,
    /// Serialises reads of the folders and their application to the database.
    work: tokio::sync::Mutex<()>,
    on_change: Arc<dyn Fn() + Send + Sync>,
    cancel: CancellationToken,
    unwatched: std::sync::atomic::AtomicBool,
}

impl Manager {
    pub fn new(
        engine: Arc<Engine>,
        sources: Sources,
        folders: Vec<BackupFolder>,
        on_change: Arc<dyn Fn() + Send + Sync>,
    ) -> Arc<Self> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let runtime = folders
            .iter()
            .map(|f| {
                let done = engine
                    .db()
                    .kv_get(&format!("backup_complete:{}", f.id))
                    .ok()
                    .flatten()
                    .and_then(|v| v.parse().ok());
                (
                    f.id.clone(),
                    Runtime {
                        last_complete_ms: done,
                        ..Default::default()
                    },
                )
            })
            .collect();
        Arc::new(Self {
            engine,
            sources,
            folders: RwLock::new(folders),
            runtime: Mutex::new(runtime),
            watches: Mutex::new(HashMap::new()),
            changes: tx,
            changes_rx: Mutex::new(Some(rx)),
            work: tokio::sync::Mutex::new(()),
            on_change,
            cancel: CancellationToken::new(),
            unwatched: std::sync::atomic::AtomicBool::new(false),
        })
    }

    pub fn engine(&self) -> &Arc<Engine> {
        &self.engine
    }

    pub fn folders(&self) -> Vec<BackupFolder> {
        self.folders.read().unwrap().clone()
    }

    fn folder(&self, id: &str) -> Option<BackupFolder> {
        self.folders
            .read()
            .unwrap()
            .iter()
            .find(|f| f.id == id)
            .cloned()
    }

    /// The backed-up folder an engine path (`<name>/…`) belongs to, and the rest of the path.
    pub fn split_engine_path(&self, path: &str) -> Option<(BackupFolder, String)> {
        let (top, rest) = path.split_once('/').unwrap_or((path, ""));
        let f = self
            .folders
            .read()
            .unwrap()
            .iter()
            .find(|f| name_key(&f.name) == name_key(top))
            .cloned()?;
        Some((f, rest.to_string()))
    }

    fn sync_sources(&self) {
        let folders = self.folders.read().unwrap();
        let enabled: Vec<&BackupFolder> = folders.iter().filter(|f| f.mirrored()).collect();
        *self.sources.write().unwrap() = enabled
            .iter()
            .map(|f| (name_key(&f.name), f.path.clone()))
            .collect();
        self.engine
            .set_mirror_roots(enabled.iter().map(|f| f.name.clone()));
    }

    fn set_runtime(&self, id: &str, f: impl FnOnce(&mut Runtime)) {
        let mut rt = self.runtime.lock().unwrap();
        f(rt.entry(id.to_string()).or_default());
    }

    /// Reads every enabled folder, then lets the engine match the cloud copy and upload.
    pub fn start(self: &Arc<Self>) {
        self.sync_sources();
        let me = self.clone();
        tokio::spawn(async move {
            for f in me.folders().into_iter().filter(|f| f.mirrored()) {
                me.full_scan(&f).await;
                me.watch(&f);
            }
            me.engine.set_local_ready(true);
            (me.on_change)();
        });
        if let Some(rx) = self.changes_rx.lock().unwrap().take() {
            tokio::spawn(self.clone().change_loop(rx));
        }
        tokio::spawn(self.clone().rescan_loop());
        tokio::spawn(self.clone().progress_loop());
    }

    pub fn stop(&self) {
        self.cancel.cancel();
        self.watches.lock().unwrap().clear();
        self.engine.shutdown();
    }

    // ---- folder changes -------------------------------------------------------------------

    /// Starts backing up a new folder (already validated by the caller).
    pub async fn add(self: &Arc<Self>, folder: BackupFolder) {
        self.folders.write().unwrap().push(folder.clone());
        self.enable_now(folder).await;
    }

    async fn enable_now(self: &Arc<Self>, folder: BackupFolder) {
        // Nothing uploads until the cloud copy has been matched with the folder again.
        self.engine.set_local_ready(false);
        self.sync_sources();
        (self.on_change)();
        let me = self.clone();
        tokio::spawn(async move {
            me.full_scan(&folder).await;
            me.watch(&folder);
            me.engine.set_local_ready(true);
            (me.on_change)();
        });
    }

    /// The per-folder off switch. Off keeps the cloud copy as it is; on matches the folder with
    /// it again (files that didn't change aren't uploaded again).
    pub async fn set_enabled(self: &Arc<Self>, id: &str, enabled: bool) -> Result<()> {
        let Some(folder) = self.folder(id) else {
            bail!("No such backup");
        };
        if folder.enabled == enabled {
            return Ok(());
        }
        if folder.on_demand {
            bail!(
                "Switch off Save space for {} first, so all its files are on this PC again.",
                folder.name
            );
        }
        for f in self
            .folders
            .write()
            .unwrap()
            .iter_mut()
            .filter(|f| f.id == id)
        {
            f.enabled = enabled;
        }
        if enabled {
            let folder = self.folder(id).expect("just updated");
            self.enable_now(folder).await;
        } else {
            let _guard = self.work.lock().await;
            self.watches.lock().unwrap().remove(id);
            self.sync_sources();
            self.engine.forget_subtree(&folder.name)?;
            self.set_runtime(id, |r| *r = Runtime::default());
            (self.on_change)();
        }
        Ok(())
    }

    /// Stops backing up a folder for good. The folder on this device is never touched. With
    /// `delete_from_cloud`, the cloud copy goes to the uloz.to trash.
    pub async fn remove(self: &Arc<Self>, id: &str, delete_from_cloud: bool) -> Result<()> {
        let Some(folder) = self.folder(id) else {
            bail!("No such backup");
        };
        if folder.on_demand {
            bail!(
                "Switch off Save space for {} first, so all its files are on this PC again.",
                folder.name
            );
        }
        if folder.enabled {
            self.set_enabled(id, false).await?;
        }
        if delete_from_cloud {
            let remote = self.engine.remote().clone();
            let root = self
                .engine
                .db()
                .node(drive_core::db::ROOT_ID)?
                .and_then(|n| n.remote_id);
            if let Some(root) = root {
                for e in remote.list(&root).await? {
                    if e.kind == Kind::Folder && name_key(&e.name) == name_key(&folder.name) {
                        remote.trash(&e).await?;
                        info!("moved the cloud copy of {} to the trash", folder.name);
                    }
                }
            }
        }
        self.folders.write().unwrap().retain(|f| f.id != id);
        self.runtime.lock().unwrap().remove(id);
        let _ = self
            .engine
            .db()
            .kv_set(&format!("backup_complete:{id}"), "");
        (self.on_change)();
        Ok(())
    }

    /// Replaces a folder's skip list and rereads it: newly skipped items leave the cloud copy
    /// (to its trash), newly included ones upload.
    pub async fn set_excludes(self: &Arc<Self>, id: &str, excludes: Vec<String>) -> Result<()> {
        if let Some(f) = self.folder(id).filter(|f| f.on_demand) {
            bail!(
                "Switch off Save space for {} to change what's skipped.",
                f.name
            );
        }
        for f in self
            .folders
            .write()
            .unwrap()
            .iter_mut()
            .filter(|f| f.id == id)
        {
            f.excludes = excludes.clone();
        }
        let Some(folder) = self.folder(id) else {
            bail!("No such backup");
        };
        if folder.mirrored() {
            let me = self.clone();
            tokio::spawn(async move {
                me.full_scan(&folder).await;
                me.watch(&folder); // the watcher filters by the skip list too
            });
        }
        (self.on_change)();
        Ok(())
    }

    /// Changes a folder's settings in place (no other effect).
    pub fn update_folder(&self, id: &str, f: impl FnOnce(&mut BackupFolder)) {
        if let Some(folder) = self
            .folders
            .write()
            .unwrap()
            .iter_mut()
            .find(|x| x.id == id)
        {
            f(folder);
        }
        (self.on_change)();
    }

    /// Hands a folder over to its on-demand engine (`on`), or takes it back. While on-demand,
    /// this manager doesn't read or mirror it, and its cloud copy is left as it is (the
    /// on-demand engine works on it). Taking it back matches the folder with the cloud copy
    /// again: files that didn't change aren't uploaded again.
    pub async fn set_on_demand(self: &Arc<Self>, id: &str, on: bool) -> Result<()> {
        let Some(folder) = self.folder(id) else {
            bail!("No such backup");
        };
        if folder.on_demand == on {
            return Ok(());
        }
        if on {
            let _guard = self.work.lock().await;
            for f in self
                .folders
                .write()
                .unwrap()
                .iter_mut()
                .filter(|f| f.id == id)
            {
                f.on_demand = true;
            }
            self.watches.lock().unwrap().remove(id);
            self.sync_sources();
            self.engine.forget_subtree(&folder.name)?;
            self.set_runtime(id, |r| *r = Runtime::default());
            (self.on_change)();
        } else {
            for f in self
                .folders
                .write()
                .unwrap()
                .iter_mut()
                .filter(|f| f.id == id)
            {
                f.on_demand = false;
                f.leaving_on_demand = false;
            }
            let folder = self.folder(id).expect("just updated");
            if folder.enabled {
                self.enable_now(folder).await;
            }
        }
        Ok(())
    }

    /// Rereads every enabled folder now.
    pub fn rescan_all(self: &Arc<Self>) {
        let me = self.clone();
        tokio::spawn(async move {
            for f in me.folders().into_iter().filter(|f| f.mirrored()) {
                me.full_scan(&f).await;
            }
        });
    }

    // ---- reading folders --------------------------------------------------------------------

    /// Reads a whole folder and applies the differences.
    pub async fn full_scan(self: &Arc<Self>, folder: &BackupFolder) {
        let _guard = self.work.lock().await;
        if self.cancel.is_cancelled() || !self.folder(&folder.id).is_some_and(|f| f.mirrored()) {
            return;
        }
        self.set_runtime(&folder.id, |r| r.scanning = true);
        (self.on_change)();
        let engine = self.engine.clone();
        let f = folder.clone();
        let started = Instant::now();
        let result = tokio::task::spawn_blocking(move || -> Result<ScanOutcome> {
            if !f.path.is_dir() {
                return Ok((
                    Vec::new(),
                    Vec::new(),
                    0,
                    Some(
                        "The folder can't be found. Reconnect its drive, or stop backing it up."
                            .into(),
                    ),
                ));
            }
            if std::fs::read_dir(&f.path).is_err() {
                return Ok((
                    Vec::new(),
                    Vec::new(),
                    0,
                    Some("Drive isn't allowed to read this folder.".into()),
                ));
            }
            let filter = Filter::new(&f.excludes);
            let mut disk = Scan::default();
            scan::walk(&f.path, "", false, &filter, None, &mut disk);
            let known = known_items(&engine, &f.name, "", true)?;
            let mut ops = Vec::new();
            if engine.node_by_path(&f.name)?.is_none() {
                ops.push(MirrorOp::Create {
                    path: f.name.clone(),
                    kind: Kind::Folder,
                    size: 0,
                    mtime_ms: now_ms(),
                });
            }
            if disk.unreadable_dirs.iter().any(|d| d.is_empty()) {
                return Ok((
                    Vec::new(),
                    Vec::new(),
                    0,
                    Some("Drive isn't allowed to read this folder.".into()),
                ));
            }
            let (more, renamed) = diff(&f.name, &disk.items, &known, &disk.unreadable_dirs, true);
            ops.extend(more);
            Ok((ops, renamed, disk.online_only, None))
        })
        .await;
        match result {
            Ok(Ok((ops, renamed, online_only, problem))) => {
                let count = ops.len();
                if let Err(e) = self.engine.mirror_apply(&ops) {
                    warn!("applying changes in {} failed: {e}", folder.name);
                }
                if !renamed.is_empty() {
                    // Moved folders: compare their contents now that they're in place.
                    self.diff_dirs(folder, renamed.into_iter().map(|r| (r, true)).collect());
                }
                if count > 0 {
                    info!(
                        "{}: {count} changes after reading the folder ({:.1}s)",
                        folder.name,
                        started.elapsed().as_secs_f32()
                    );
                }
                self.set_runtime(&folder.id, |r| {
                    r.problem = problem;
                    r.online_only = online_only;
                });
            }
            Ok(Err(e)) => {
                warn!("reading {} failed: {e:#}", folder.path.display());
                self.set_runtime(&folder.id, |r| r.problem = Some(format!("{e:#}")));
            }
            Err(e) => warn!("reading {} panicked: {e}", folder.path.display()),
        }
        self.set_runtime(&folder.id, |r| r.scanning = false);
        (self.on_change)();
    }

    /// Compares some folders (relative paths; `true` = with everything below) with the
    /// database and applies the differences. Called with the work lock held or from a scan.
    fn diff_dirs(&self, folder: &BackupFolder, dirs: Vec<(String, bool)>) {
        if !folder.path.is_dir() {
            // Gone or unplugged: report it; never mirror "everything deleted".
            self.set_runtime(&folder.id, |r| {
                r.problem = Some(
                    "The folder can't be found. Reconnect its drive, or stop backing it up.".into(),
                )
            });
            return;
        }
        self.set_runtime(&folder.id, |r| r.problem = None);
        let filter = Filter::new(&folder.excludes);
        let mut unreadable: Vec<String> = Vec::new();
        let mut disk: BTreeMap<String, Item> = BTreeMap::new();
        let mut known = Known::new();
        let mut scopes: Vec<(String, bool)> = Vec::new();
        for (rel, deep) in dirs {
            if scopes.iter().any(|(s, d)| *d && under(&rel, s)) {
                continue;
            }
            scopes.retain(|(s, _)| !(deep && under(s, &rel) && *s != rel));
            scopes.push((rel, deep));
        }
        for (rel, deep) in &scopes {
            let abs = rel
                .split('/')
                .filter(|p| !p.is_empty())
                .fold(folder.path.clone(), |a, p| a.join(p));
            if !abs.is_dir() {
                continue; // deleted itself: its parent's comparison covers it
            }
            let mut s = Scan::default();
            scan::walk(&folder.path, rel, !deep, &filter, None, &mut s);
            // New folders found by a shallow read: their contents are new too.
            if !deep {
                let new_dirs: Vec<String> = s
                    .items
                    .iter()
                    .filter(|(p, i)| {
                        i.kind == Kind::Folder
                            && self
                                .engine
                                .node_by_path(&join(&folder.name, p))
                                .ok()
                                .flatten()
                                .is_none()
                    })
                    .map(|(p, _)| p.clone())
                    .collect();
                for d in new_dirs {
                    scan::walk(&folder.path, &d, false, &filter, None, &mut s);
                }
            }
            disk.extend(s.items);
            unreadable.extend(s.unreadable_dirs);
            match known_items(&self.engine, &folder.name, rel, *deep) {
                Ok(k) => known.extend(k),
                Err(e) => warn!("reading the backup database failed: {e}"),
            }
        }
        // Shallow scopes only compare direct children; deeper database rows of new folders
        // don't exist, so nothing else needs trimming.
        let (ops, renamed) = diff(&folder.name, &disk, &known, &unreadable, true);
        if !ops.is_empty() {
            debug!("{}: {} changes", folder.name, ops.len());
            if let Err(e) = self.engine.mirror_apply(&ops) {
                warn!("applying changes in {} failed: {e}", folder.name);
            }
        }
        if !renamed.is_empty() {
            // Moved folders: compare their contents now that they're in place.
            self.diff_dirs(folder, renamed.into_iter().map(|r| (r, true)).collect());
        }
    }

    // ---- watching --------------------------------------------------------------------------

    fn watch(self: &Arc<Self>, folder: &BackupFolder) {
        use notify::Watcher;
        let id = folder.id.clone();
        let root = folder.path.clone();
        let tx = self.changes.clone();
        let filter = Filter::new(&folder.excludes);
        let handler = move |res: notify::Result<notify::Event>| match res {
            Ok(ev) => {
                if matches!(ev.kind, notify::EventKind::Access(_)) {
                    return; // reading files (our own uploads) must not loop back
                }
                let paths: Vec<PathBuf> = ev
                    .paths
                    .into_iter()
                    .filter(|p| {
                        p.strip_prefix(&root)
                            .map(|rel| {
                                !rel.components()
                                    .any(|c| filter.skips_name(&c.as_os_str().to_string_lossy()))
                            })
                            .unwrap_or(false)
                    })
                    .collect();
                let rescan = ev.attrs.flag() == Some(notify::event::Flag::Rescan);
                if !paths.is_empty() || rescan {
                    let _ = tx.send(Changed {
                        id: id.clone(),
                        paths,
                        rescan,
                    });
                }
            }
            Err(e) => {
                debug!("watch error: {e}");
                let _ = tx.send(Changed {
                    id: id.clone(),
                    paths: Vec::new(),
                    rescan: true,
                });
            }
        };
        let mut watcher = match notify::recommended_watcher(handler) {
            Ok(w) => w,
            Err(e) => {
                warn!("can't watch {} for changes: {e}", folder.path.display());
                self.unwatched
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                return;
            }
        };
        let mut dirs = HashSet::new();
        if cfg!(target_os = "linux") {
            // inotify watches each folder separately; a recursive watch would also cover the
            // skipped ones (node_modules can be tens of thousands of folders).
            for d in self.included_dirs(folder) {
                match watcher.watch(&d, notify::RecursiveMode::NonRecursive) {
                    Ok(()) => {
                        dirs.insert(d);
                    }
                    Err(e) => {
                        warn!(
                            "can't watch every folder in {} ({e}); checking it every few minutes instead",
                            folder.path.display()
                        );
                        self.unwatched
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                        break;
                    }
                }
            }
        } else if let Err(e) = watcher.watch(&folder.path, notify::RecursiveMode::Recursive) {
            warn!("can't watch {} for changes: {e}", folder.path.display());
            self.unwatched
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.watches
            .lock()
            .unwrap()
            .insert(folder.id.clone(), Watch { watcher, dirs });
    }

    /// Absolute paths of the folder and every included subfolder, from the database.
    fn included_dirs(&self, folder: &BackupFolder) -> Vec<PathBuf> {
        let mut out = vec![folder.path.clone()];
        if let Ok(known) = known_items(&self.engine, &folder.name, "", true) {
            out.extend(
                known
                    .iter()
                    .filter(|(_, i)| i.kind == Kind::Folder)
                    .map(|(p, _)| {
                        p.split('/')
                            .fold(folder.path.clone(), |acc, part| acc.join(part))
                    }),
            );
        }
        out
    }

    fn watch_new_dirs(&self, folder: &BackupFolder) {
        if !cfg!(target_os = "linux") {
            return;
        }
        use notify::Watcher;
        let dirs = self.included_dirs(folder);
        let mut watches = self.watches.lock().unwrap();
        let Some(w) = watches.get_mut(&folder.id) else {
            return;
        };
        for d in dirs {
            if !w.dirs.contains(&d)
                && w.watcher
                    .watch(&d, notify::RecursiveMode::NonRecursive)
                    .is_ok()
            {
                w.dirs.insert(d);
            }
        }
        w.dirs.retain(|d| d.is_dir());
    }

    async fn change_loop(self: Arc<Self>, mut rx: tokio::sync::mpsc::UnboundedReceiver<Changed>) {
        loop {
            let first = tokio::select! {
                _ = self.cancel.cancelled() => return,
                m = rx.recv() => match m { Some(m) => m, None => return },
            };
            // Gather the burst: until quiet for a second, at most five seconds.
            let mut batch = vec![first];
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let wait =
                    Duration::from_secs(1).min(deadline.saturating_duration_since(Instant::now()));
                match tokio::time::timeout(wait, rx.recv()).await {
                    Ok(Some(m)) => batch.push(m),
                    _ => break,
                }
            }
            let mut by_folder: HashMap<String, (Vec<PathBuf>, bool)> = HashMap::new();
            for m in batch {
                let e = by_folder.entry(m.id).or_default();
                e.0.extend(m.paths);
                e.1 |= m.rescan;
            }
            for (id, (paths, rescan)) in by_folder {
                let Some(folder) = self.folder(&id).filter(|f| f.mirrored()) else {
                    continue;
                };
                if rescan {
                    self.full_scan(&folder).await;
                    continue;
                }
                let _guard = self.work.lock().await;
                let me = self.clone();
                let f = folder.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    let dirs = dirty_dirs(&f, &paths);
                    me.diff_dirs(&f, dirs);
                    me.watch_new_dirs(&f);
                })
                .await;
                (self.on_change)();
            }
        }
    }

    async fn rescan_loop(self: Arc<Self>) {
        loop {
            let every = if self.unwatched.load(std::sync::atomic::Ordering::Relaxed) {
                RESCAN_EVERY_UNWATCHED
            } else {
                RESCAN_EVERY
            };
            tokio::select! {
                _ = self.cancel.cancelled() => return,
                _ = tokio::time::sleep(every) => {}
            }
            for f in self.folders().into_iter().filter(|f| f.mirrored()) {
                self.full_scan(&f).await;
                self.watch_new_dirs(&f);
            }
        }
    }

    /// Tells the service about progress while anything is happening.
    async fn progress_loop(self: Arc<Self>) {
        let mut last: Option<Vec<(BackupState, u64, u64, u32)>> = None;
        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_secs(2)) => {}
            }
            let infos = self.infos();
            let now: Vec<_> = infos
                .iter()
                .map(|i| (i.state, i.done_files, i.files, i.pending))
                .collect();
            if last.as_ref() != Some(&now) {
                last = Some(now);
                (self.on_change)();
            }
        }
    }

    // ---- reporting -------------------------------------------------------------------------

    pub fn infos(&self) -> Vec<BackupInfo> {
        let stats = self.engine.top_level_stats().unwrap_or_default();
        let stats: HashMap<String, drive_core::engine::SubtreeStats> =
            stats.into_iter().map(|(k, v)| (name_key(&k), v)).collect();
        let status = self.engine.status();
        let linked = self.engine.mirror_linked();
        let waiting = matches!(
            status.state,
            SyncState::Paused | SyncState::Offline | SyncState::SignedOut
        );
        let folders = self.folders();
        let mut out = Vec::with_capacity(folders.len());
        let mut rt = self.runtime.lock().unwrap();
        for f in folders {
            let s = stats.get(&name_key(&f.name)).copied().unwrap_or_default();
            let r = rt.entry(f.id.clone()).or_default();
            let (state, detail) = if !f.enabled {
                (BackupState::Off, None)
            } else if f.on_demand {
                // Filled in by the service from the folder's on-demand engine.
                (
                    BackupState::Converting,
                    Some("Getting the folder ready…".to_string()),
                )
            } else if let Some(p) = &r.problem {
                (BackupState::Problem, Some(p.clone()))
            } else if r.scanning || !linked {
                let d = if s.files > 0 {
                    format!("Checking {} files…", fmt_count(s.files))
                } else {
                    "Looking through the folder…".to_string()
                };
                (BackupState::Scanning, Some(d))
            } else if waiting && s.pending > 0 {
                (BackupState::Waiting, Some(status.headline.clone()))
            } else if s.pending > 0 || s.done_files < s.files {
                let left = s.files.saturating_sub(s.done_files);
                (
                    BackupState::BackingUp,
                    Some(format!("{} files left", fmt_count(left))),
                )
            } else {
                (BackupState::UpToDate, None)
            };
            if f.on_demand {
                // Tracked by the on-demand engine.
            } else if state == BackupState::UpToDate {
                if !r.complete_now {
                    r.complete_now = true;
                    let t = now_ms();
                    r.last_complete_ms = Some(t);
                    let _ = self
                        .engine
                        .db()
                        .kv_set(&format!("backup_complete:{}", f.id), &t.to_string());
                }
            } else {
                r.complete_now = false;
            }
            out.push(BackupInfo {
                id: f.id.clone(),
                name: f.name.clone(),
                path: f.path.display().to_string(),
                known: f.known,
                enabled: f.enabled,
                state,
                detail,
                excludes: f.excludes.clone(),
                files: s.files,
                bytes: s.bytes,
                done_files: s.done_files,
                done_bytes: s.done_bytes,
                pending: s.pending,
                skipped_online_only: r.online_only,
                added_ms: f.added_ms,
                last_complete_ms: r.last_complete_ms,
                on_demand: f.on_demand,
                on_demand_available: crate::platform::ON_DEMAND_FOLDERS,
                local_bytes: s.bytes,
                keep_bytes: f.keep_bytes.unwrap_or(DEFAULT_KEEP_BYTES),
            });
        }
        out
    }
}

fn fmt_count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Which folders to compare after change notifications: a changed or new folder with
/// everything below it, and the parent of every changed path (directly).
fn dirty_dirs(folder: &BackupFolder, paths: &[PathBuf]) -> Vec<(String, bool)> {
    let filter = Filter::new(&folder.excludes);
    let mut out: Vec<(String, bool)> = Vec::new();
    let mut add = |rel: String, deep: bool| {
        if let Some(e) = out.iter_mut().find(|(r, _)| *r == rel) {
            e.1 |= deep;
        } else {
            out.push((rel, deep));
        }
    };
    for p in paths {
        let Ok(rel) = p.strip_prefix(&folder.path) else {
            continue;
        };
        let rel: Vec<String> = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        if rel.iter().any(|c| filter.skips_name(c)) {
            continue;
        }
        // Inside a tagged cache or virtual environment: skip (checked on the folders above).
        let mut abs = folder.path.clone();
        let mut skipped = false;
        for c in rel.iter().take(rel.len().saturating_sub(1)) {
            abs.push(c);
            if scan::is_tool_folder(&abs) {
                skipped = true;
                break;
            }
        }
        if skipped {
            continue;
        }
        let rel = rel.join("/");
        if p.is_dir() {
            add(rel.clone(), true);
        }
        if let Some(parent) = parent_of(&rel) {
            add(parent.to_string(), false);
        }
    }
    out
}

/// Files and bytes (if the folder could be read), and when that was measured.
type Measured = (Option<(u64, u64)>, Instant);

/// Measured sizes of the well-known folders, for suggestions. Measured in the background and
/// remembered for a while (a large Documents folder takes a few seconds to read).
#[derive(Default)]
pub struct Suggestions {
    measured: Mutex<HashMap<KnownFolder, Measured>>,
    running: std::sync::atomic::AtomicBool,
}

impl Suggestions {
    const FRESH_FOR: Duration = Duration::from_secs(6 * 3600);

    /// Suggestions for well-known folders that aren't backed up (or blocked, with a reason).
    pub fn list(
        self: &Arc<Self>,
        backed_up: &[PathBuf],
        drive_folder: &Path,
        own_data: &[&Path],
    ) -> Vec<FolderSuggestion> {
        let measured = self.measured.lock().unwrap().clone();
        let mut stale = false;
        let backed: Vec<String> = backed_up.iter().map(|p| norm(p)).collect();
        let others: Vec<&Path> = backed_up.iter().map(|p| p.as_path()).collect();
        let mut out = Vec::new();
        for (known, name, path) in known_folders() {
            if !path.is_dir() || backed.contains(&norm(&path)) {
                continue;
            }
            let size = match measured.get(&known) {
                Some((s, at)) => {
                    stale |= at.elapsed() > Self::FRESH_FOR;
                    *s
                }
                None => {
                    stale = true;
                    None
                }
            };
            out.push(FolderSuggestion {
                known,
                name: name.to_string(),
                path: path.display().to_string(),
                files: size.map(|s| s.0),
                bytes: size.map(|s| s.1),
                blocked: overlap_problem(&path, drive_folder, own_data, &others),
            });
        }
        if stale {
            self.refresh();
        }
        out.sort_by(|a, b| b.bytes.unwrap_or(0).cmp(&a.bytes.unwrap_or(0)));
        out
    }

    fn refresh(self: &Arc<Self>) {
        if self.running.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let me = self.clone();
        tokio::task::spawn_blocking(move || {
            let filter = Filter::new(&scan::default_excludes());
            for (known, _, path) in known_folders() {
                let m =
                    scan::measure(&path, &filter, Duration::from_secs(60)).map(|(f, b, _)| (f, b));
                me.measured
                    .lock()
                    .unwrap()
                    .insert(known, (m, Instant::now()));
            }
            me.running.store(false, std::sync::atomic::Ordering::SeqCst);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(size: u64, mtime: i64) -> Item {
        Item {
            kind: Kind::File,
            size,
            mtime_ms: mtime,
        }
    }
    fn dir() -> Item {
        Item {
            kind: Kind::Folder,
            size: 0,
            mtime_ms: 0,
        }
    }

    #[test]
    fn diff_creates_modifies_deletes() {
        let disk: BTreeMap<String, Item> = [
            ("a".into(), dir()),
            ("a/new.txt".into(), file(1, 1)),
            ("same.txt".into(), file(5, 5)),
            ("edited.txt".into(), file(7, 9)),
        ]
        .into();
        let known: Known = [
            ("same.txt".into(), file(5, 5)),
            ("edited.txt".into(), file(7, 8)),
            ("old".into(), dir()),
            ("old/x.txt".into(), file(3, 3)),
        ]
        .into();
        let (ops, _) = diff("Docs", &disk, &known, &[], false);
        assert!(ops.contains(&MirrorOp::Create {
            path: "Docs/a".into(),
            kind: Kind::Folder,
            size: 0,
            mtime_ms: 0
        }));
        assert!(ops.contains(&MirrorOp::Create {
            path: "Docs/a/new.txt".into(),
            kind: Kind::File,
            size: 1,
            mtime_ms: 1
        }));
        assert!(ops.contains(&MirrorOp::Modify {
            path: "Docs/edited.txt".into(),
            size: 7,
            mtime_ms: 9
        }));
        assert!(ops.contains(&MirrorOp::Delete {
            path: "Docs/old".into()
        }));
        assert!(
            !ops.iter()
                .any(|o| matches!(o, MirrorOp::Delete { path } if path == "Docs/old/x.txt"))
        );
    }

    #[test]
    fn diff_detects_moves() {
        let disk: BTreeMap<String, Item> = [
            ("Archive".into(), dir()),
            ("Archive/Trip".into(), dir()),
            ("Archive/Trip/p.jpg".into(), file(9, 9)),
            ("renamed.pdf".into(), file(4, 4)),
        ]
        .into();
        let known: Known = [
            ("Archive".into(), dir()),
            ("Trip".into(), dir()),
            ("Trip/p.jpg".into(), file(9, 9)),
            ("report.pdf".into(), file(4, 4)),
        ]
        .into();
        let (ops, renamed) = diff("D", &disk, &known, &[], true);
        assert!(ops.contains(&MirrorOp::Rename {
            from: "D/Trip".into(),
            to: "D/Archive/Trip".into()
        }));
        assert!(ops.contains(&MirrorOp::Rename {
            from: "D/report.pdf".into(),
            to: "D/renamed.pdf".into()
        }));
        assert!(
            !ops.iter()
                .any(|o| matches!(o, MirrorOp::Create { .. } | MirrorOp::Delete { .. })),
            "{ops:?}"
        );
        assert!(renamed.contains(&"Archive/Trip".to_string()));
    }

    #[test]
    fn overlap_rules() {
        let base = tempfile::tempdir().unwrap();
        let home = base.path();
        let drive = home.join("Drive");
        let docs = home.join("Documents");
        std::fs::create_dir_all(drive.join("x")).unwrap();
        std::fs::create_dir_all(docs.join("sub")).unwrap();
        assert!(overlap_problem(&docs, &drive, &[], &[]).is_none());
        assert!(overlap_problem(&drive.join("x"), &drive, &[], &[]).is_some());
        assert!(overlap_problem(home, &drive, &[], &[]).is_some());
        assert!(overlap_problem(&docs.join("sub"), &drive, &[], &[docs.as_path()]).is_some());
        assert!(overlap_problem(&docs, &drive, &[], &[docs.join("sub").as_path()]).is_some());
    }

    #[test]
    fn names() {
        assert_eq!(unique_name("Documents", &[]), "Documents");
        assert_eq!(
            unique_name("Documents", &["documents".into()]),
            "Documents (2)"
        );
        assert_eq!(unique_name("a:b", &[]), "a_b");
        assert_eq!(fmt_count(1234567), "1,234,567");
    }
}
