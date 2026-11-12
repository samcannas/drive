//! Windows: the Drive folder is a Cloud Files API sync root.
//!
//! * [`register`] makes the folder a sync root (Explorer navigation pane, status icons,
//!   "Always keep on this device" / "Free up space"). Hydration is progressive and the
//!   namespace is always full: the engine creates every placeholder itself.
//! * [`local::WinLocal`] is the engine's [`LocalSide`]: cloud changes become placeholder
//!   operations. Placeholder identities carry the node id (see [`util::encode_blob`]).
//! * [`filter::Filter`] answers Cloud Files callbacks: it streams file contents from the
//!   engine when an app opens a cloud-only file, approves deletes and renames, and forwards
//!   change notifications.
//! * [`reconcile::Worker`] turns local changes into engine calls. It is state based: for every
//!   path that changed (debounced) it compares the disk with the database, so it doesn't matter
//!   how the change came about (Explorer, an app, a command prompt, or while Drive was not
//!   running). Moves are recognised by placeholder identity. Changes the engine itself makes
//!   through [`LocalSide`] are briefly suppressed so they are never reported back.

mod filter;
mod local;
mod reconcile;
mod register;
mod util;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use cloud_filter::root::{Connection, Session};
use drive_core::db::NodeId;
use drive_core::engine::Engine;
use drive_core::engine::local::LocalSide;
use tracing::{debug, info, warn};

use super::{Platform, PlatformContext};

/// After the engine changed a path through [`LocalSide`], ignore it for this long.
const SUPPRESS_FOR: Duration = Duration::from_millis(1500);

/// Messages for the reconcile worker.
pub(crate) enum Msg {
    /// These absolute paths may have changed.
    Paths(Vec<PathBuf>),
    /// Look at everything below this folder (e.g. a folder that was adopted).
    Subtree(PathBuf),
    /// Compare the whole folder with the database.
    FullScan,
    /// The directory watcher died; fall back to periodic full scans.
    WatcherStopped,
    Stop,
}

#[derive(Default)]
struct Suppression {
    active: u32,
    until: Option<Instant>,
}

/// State shared by the local side, the callbacks and the worker.
pub(crate) struct Shared {
    engine: Weak<Engine>,
    /// The sync root (absolute, as the file system spells it, without `\\?\`).
    pub root: PathBuf,
    pub rt: tokio::runtime::Handle,
    /// Ties placeholder identities to this database.
    pub tag: u64,
    tx: Mutex<Sender<Msg>>,
    suppressed: Mutex<HashMap<String, Suppression>>,
    /// Byte ranges transferred per file since it was last dehydrated.
    coverage: Mutex<HashMap<NodeId, Vec<Range<u64>>>>,
    /// Folders converted to placeholders before they existed in the cloud: marked in sync
    /// once they do.
    pub unsynced_folders: Mutex<HashMap<NodeId, PathBuf>>,
}

impl Shared {
    pub fn engine(&self) -> Option<Arc<Engine>> {
        self.engine.upgrade().filter(|e| !e.is_shut_down())
    }

    pub fn abs(&self, rel: &str) -> PathBuf {
        util::abs_path(&self.root, rel)
    }

    pub fn rel(&self, abs: &Path) -> Option<String> {
        util::rel_path(&self.root, abs)
    }

    pub fn send(&self, msg: Msg) {
        let _ = self.tx.lock().unwrap_or_else(|e| e.into_inner()).send(msg);
    }

    /// Marks `rels` as being changed by the engine until the guard drops (plus a grace period).
    pub fn begin_op<'a>(&'a self, rels: &[&str]) -> OpGuard<'a> {
        let keys: Vec<String> = rels.iter().map(|r| util::key(r)).collect();
        let mut map = self.suppressed.lock().unwrap_or_else(|e| e.into_inner());
        for k in &keys {
            map.entry(k.clone()).or_default().active += 1;
        }
        OpGuard { shared: self, keys }
    }

    /// If `rel` (or a folder above it) is being changed by the engine, when to look again.
    pub fn suppressed_until(&self, rel: &str) -> Option<Instant> {
        let now = Instant::now();
        let mut map = self.suppressed.lock().unwrap_or_else(|e| e.into_inner());
        map.retain(|_, s| s.active > 0 || s.until.is_some_and(|u| u > now));
        let k = util::key(rel);
        let mut cur: &str = &k;
        let mut result: Option<Instant> = None;
        loop {
            if let Some(s) = map.get(cur) {
                let at = if s.active > 0 {
                    now + Duration::from_millis(300)
                } else {
                    s.until.unwrap_or(now)
                };
                result = Some(result.map_or(at, |r: Instant| r.max(at)));
            }
            match cur.rfind('/') {
                Some(i) => cur = &cur[..i],
                None if !cur.is_empty() => cur = "",
                None => break,
            }
        }
        result
    }

    /// Records that `range` of `id` is now on disk; returns whether the whole file is.
    pub fn add_coverage(&self, id: NodeId, range: Range<u64>, size: u64) -> bool {
        let mut map = self.coverage.lock().unwrap_or_else(|e| e.into_inner());
        let ranges = map.entry(id).or_default();
        ranges.push(range);
        ranges.sort_by_key(|r| r.start);
        let mut merged: Vec<Range<u64>> = Vec::new();
        for r in ranges.drain(..) {
            match merged.last_mut() {
                Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
                _ => merged.push(r),
            }
        }
        let full = merged
            .first()
            .is_some_and(|r| r.start == 0 && r.end >= size);
        if full {
            map.remove(&id);
        } else {
            *ranges = merged;
        }
        full
    }

    pub fn clear_coverage(&self, id: NodeId) {
        self.coverage
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
    }
}

pub(crate) struct OpGuard<'a> {
    shared: &'a Shared,
    keys: Vec<String>,
}

impl Drop for OpGuard<'_> {
    fn drop(&mut self) {
        let until = Instant::now() + SUPPRESS_FOR;
        let mut map = self
            .shared
            .suppressed
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for k in &self.keys {
            let s = map.entry(k.clone()).or_default();
            s.active = s.active.saturating_sub(1);
            s.until = Some(until);
        }
    }
}

struct WindowsPlatform {
    shared: Arc<Shared>,
    local: Arc<local::WinLocal>,
    rx: Mutex<Option<Receiver<Msg>>>,
    conn: Mutex<Option<Connection<filter::Filter>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl Platform for WindowsPlatform {
    fn local_side(&self) -> Arc<dyn LocalSide> {
        self.local.clone()
    }

    fn start(&self) -> Result<()> {
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        if conn.is_some() {
            return Ok(());
        }
        let filter = filter::Filter::new(self.shared.clone());
        let c = Session::new()
            .connect(&self.shared.root, filter)
            .with_context(|| {
                format!("connecting to the sync root {}", self.shared.root.display())
            })?;
        *conn = Some(c);
        drop(conn);
        local::mark_folder_in_sync(&self.shared.root);

        if let Some(rx) = self.rx.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let shared = self.shared.clone();
            let handle = std::thread::Builder::new()
                .name("drive-cfapi-changes".into())
                .spawn(move || reconcile::Worker::new(shared, true).run(rx))
                .context("starting the change watcher")?;
            *self.worker.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
        }
        // Catch up with everything that happened while we weren't connected (pin changes
        // included; structural changes were already reported in prepare).
        self.shared.send(Msg::FullScan);
        info!("Drive folder {} is live", self.shared.root.display());
        Ok(())
    }

    fn stop(&self) {
        if let Some(c) = self.conn.lock().unwrap_or_else(|e| e.into_inner()).take() {
            drop(c);
            debug!("disconnected {}", self.shared.root.display());
        }
        self.shared.send(Msg::Stop);
        if let Some(h) = self.worker.lock().unwrap_or_else(|e| e.into_inner()).take()
            && h.thread().id() != std::thread::current().id()
        {
            let _ = h.join();
        }
    }
}

impl Drop for WindowsPlatform {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The folder as the file system spells it (absolute, no `\\?\`).
fn normalize_folder(folder: &Path) -> Result<PathBuf> {
    let abs =
        std::path::absolute(folder).with_context(|| format!("resolving {}", folder.display()))?;
    Ok(match std::fs::canonicalize(&abs) {
        Ok(c) => util::strip_verbatim(&c),
        Err(_) => abs,
    })
}

fn placeholder_tag(engine: &Engine) -> Result<u64> {
    const KEY: &str = "windows_placeholder_tag";
    if let Some(v) = engine.db().kv_get(KEY)?.and_then(|v| v.parse().ok()) {
        return Ok(v);
    }
    let tag: u64 = rand::random::<u64>() | 1;
    engine.db().kv_set(KEY, &tag.to_string())?;
    Ok(tag)
}

pub fn prepare(ctx: PlatformContext) -> Result<Box<dyn Platform>> {
    let rt = tokio::runtime::Handle::try_current()
        .context("the Windows integration needs a Tokio runtime")?;
    std::fs::create_dir_all(&ctx.folder)
        .with_context(|| format!("creating {}", ctx.folder.display()))?;
    let root = normalize_folder(&ctx.folder)?;
    let reg = register::register(&root, &ctx.account, &ctx.display_name)?;
    if reg == register::Registration::Native {
        warn!(
            "{} works, but won't appear in Explorer's navigation pane",
            root.display()
        );
    }
    debug!(
        "not using {} (no content cache on Windows)",
        ctx.cache_dir.display()
    );
    let tag = placeholder_tag(&ctx.engine)?;
    let (tx, rx) = mpsc::channel();
    let shared = Arc::new(Shared {
        engine: Arc::downgrade(&ctx.engine),
        root,
        rt,
        tag,
        tx: Mutex::new(tx),
        suppressed: Mutex::new(HashMap::new()),
        coverage: Mutex::new(HashMap::new()),
        unsynced_folders: Mutex::new(HashMap::new()),
    });
    // Before the engine's first scan: adopt files already in the folder and catch up with
    // changes made while Drive wasn't running, so they aren't mistaken for missing items.
    // (Pin changes need the engine running; they are handled once the folder is live.)
    let started = Instant::now();
    reconcile::Worker::new(shared.clone(), false).full_pass();
    debug!("initial reconciliation took {:?}", started.elapsed());
    let local = Arc::new(local::WinLocal::new(shared.clone()));
    Ok(Box::new(WindowsPlatform {
        shared,
        local,
        rx: Mutex::new(Some(rx)),
        conn: Mutex::new(None),
        worker: Mutex::new(None),
    }))
}

/// Bytes and number of files under `folder` whose content is (partly) only in the cloud.
pub fn cloud_only(folder: &Path) -> Result<(u64, u64)> {
    let root = normalize_folder(folder)?;
    let (mut bytes, mut files) = (0u64, 0u64);
    for p in walk_items(&root) {
        if let Ok(Some(item)) = util::inspect(&p, 0)
            && item.ph.as_ref().is_some_and(|ph| ph.partial_on_disk)
        {
            bytes += item.size;
            files += 1;
        }
    }
    Ok((bytes, files))
}

/// Turns an on-demand folder back into an ordinary one: every placeholder becomes an ordinary
/// file again and the sync root is unregistered. Refuses (changing nothing) while any file is
/// still only in the cloud, so no file can disappear. Call after disconnecting.
pub fn release(folder: &Path, account: &str) -> Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::CloudFilters::{CF_REVERT_FLAG_NONE, CfRevertPlaceholder};
    let (bytes, files) = cloud_only(folder)?;
    if files > 0 {
        anyhow::bail!("{files} files ({bytes} bytes) are still only in the cloud");
    }
    let root = normalize_folder(folder)?;
    let mut items = walk_items(&root);
    // Deepest first, so folders are reverted after their contents. The root goes last, after
    // unregistering (Windows finds the registration through it).
    items.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    let mut failed = 0u32;
    let mut revert = |p: &Path| {
        // Skipped files are ordinary ones, but pinning the folder marked them too.
        clear_pin_attributes(p);
        let Ok(Some(item)) = util::inspect(p, 0) else {
            return;
        };
        if item.ph.is_none() {
            return;
        }
        let result = util::open_attrs(p).and_then(|f| {
            unsafe { CfRevertPlaceholder(HANDLE(f.as_raw_handle()), CF_REVERT_FLAG_NONE, None) }
                .map_err(util::win_err)
        });
        if let Err(e) = result {
            failed += 1;
            warn!("making {} an ordinary file failed: {e}", p.display());
        }
        clear_pin_attributes(p);
    };
    for p in &items {
        revert(p);
    }
    register::unregister(&root);
    register::unregister_account(account);
    revert(&root);
    if failed > 0 {
        warn!(
            "{failed} items in {} stayed placeholders (their content is on disk)",
            root.display()
        );
    }
    info!("{} is an ordinary folder again", root.display());
    Ok(())
}

/// Pin attributes mean nothing on ordinary files. Clear them, so the files don't come back as
/// "always keep on this device" if the folder is made on-demand again.
fn clear_pin_attributes(p: &Path) {
    use std::os::windows::fs::MetadataExt;
    use windows::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_PINNED, FILE_ATTRIBUTE_UNPINNED, FILE_FLAGS_AND_ATTRIBUTES,
        SetFileAttributesW,
    };
    let Ok(meta) = std::fs::symlink_metadata(p) else {
        return;
    };
    let pin = FILE_ATTRIBUTE_PINNED.0 | FILE_ATTRIBUTE_UNPINNED.0;
    let attrs = meta.file_attributes();
    if attrs & pin == 0 {
        return;
    }
    let wide = windows::core::HSTRING::from(p.as_os_str());
    if let Err(e) = unsafe { SetFileAttributesW(&wide, FILE_FLAGS_AND_ATTRIBUTES(attrs & !pin)) } {
        debug!("clearing the pin state of {} failed: {e}", p.display());
    }
}

pub fn forget_registration(account: &str) {
    register::unregister_account(account);
}

/// Every file and folder under `root` (not following links).
fn walk_items(root: &Path) -> Vec<PathBuf> {
    let mut items = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                stack.push(entry.path());
            }
            items.push(entry.path());
        }
    }
    items
}

/// Name of the folder (next to the Drive folder) that receives files that were never uploaded
/// when the account is disconnected.
pub const NOT_UPLOADED_FOLDER: &str = "Drive (not uploaded)";

/// Sign-out: unregisters the sync root, deletes everything that is safely in the cloud and
/// moves files that aren't (new or modified, not yet uploaded) to "Drive (not uploaded)".
pub fn teardown(folder: &Path, _cache_dir: &Path) -> Result<()> {
    let root = normalize_folder(folder)?;
    if !root.exists() {
        register::unregister(&root);
        return Ok(());
    }
    // Classify while placeholders still know whether they are in sync.
    let mut synced: Vec<PathBuf> = Vec::new();
    let mut keep: Vec<(String, PathBuf)> = Vec::new();
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_symlink() {
                keep.push((util::rel_path(&root, &path).unwrap_or_default(), path));
                continue;
            }
            if ft.is_dir() {
                dirs.push(path.clone());
                stack.push(path);
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_lowercase();
            let state = util::inspect(&path, 0).ok().flatten();
            let in_cloud = state
                .as_ref()
                .and_then(|s| s.ph.as_ref())
                .is_some_and(|p| p.in_sync);
            if in_cloud || name == "desktop.ini" || name == "thumbs.db" {
                synced.push(path);
            } else {
                keep.push((util::rel_path(&root, &path).unwrap_or_default(), path));
            }
        }
    }
    // Cloud-only placeholders disappear with the registration; hydrated ones become ordinary
    // files, which we delete below.
    register::unregister(&root);
    for p in &synced {
        if let Err(e) = remove_file_force(p)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            warn!("removing {} failed: {e}", p.display());
        }
    }
    if !keep.is_empty() {
        let dest_root = root
            .parent()
            .map(|p| p.join(NOT_UPLOADED_FOLDER))
            .unwrap_or_else(|| root.join(NOT_UPLOADED_FOLDER));
        for (rel, src) in &keep {
            let mut dest = util::abs_path(&dest_root, rel);
            if let Some(parent) = dest.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            dest = unique_path(dest);
            match std::fs::rename(src, &dest) {
                Ok(()) => info!("kept {} (not uploaded) as {}", rel, dest.display()),
                Err(e) => warn!(
                    "moving {} out of the Drive folder failed: {e}",
                    src.display()
                ),
            }
        }
    }
    dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
    for d in dirs {
        let _ = std::fs::remove_dir(&d);
    }
    match std::fs::remove_dir(&root) {
        Ok(()) => info!("removed {}", root.display()),
        Err(e) => warn!("{} was left in place: {e}", root.display()),
    }
    Ok(())
}

fn remove_file_force(p: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(p) {
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            if let Ok(meta) = std::fs::metadata(p) {
                let mut perms = meta.permissions();
                #[allow(clippy::permissions_set_readonly_false)]
                perms.set_readonly(false);
                let _ = std::fs::set_permissions(p, perms);
            }
            std::fs::remove_file(p)
        }
        other => other,
    }
}

fn unique_path(p: PathBuf) -> PathBuf {
    if !p.exists() {
        return p;
    }
    let stem = p
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = p
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    for i in 2.. {
        let cand = p.with_file_name(format!("{stem} ({i}){ext}"));
        if !cand.exists() {
            return cand;
        }
    }
    p
}
