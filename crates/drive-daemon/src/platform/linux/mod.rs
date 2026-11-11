//! Linux: the vault is a FUSE file system mounted at the Drive folder (`~/Drive`).
//!
//! * Metadata (`lookup`, `getattr`, `readdir`) is served straight from the state database;
//!   inode numbers are node ids (the root is 1).
//! * File contents live in a content cache (`<cache_dir>/<node id>`, see [`cache`]). Cloud-only
//!   files are downloaded on demand: reads wait only for the part they need while the whole
//!   file streams into the cache; a read far beyond the downloaded part is fetched directly.
//! * Writes go to the cache file; when the writer closes (or flushes) the engine is told and
//!   uploads the new version after its debounce.
//! * The engine's remote changes reach the kernel as cache invalidations ([`mount`]).
//! * Pinning: `setfattr -n user.drive.pinned -v 1 FILE`; `getfattr -n user.drive.status FILE`.
//!
//! FUSE is mounted through the `fusermount3` helper (package `fuse3`), so neither root nor the
//! libfuse headers are needed.

mod cache;
mod fs;
mod mount;
#[cfg(test)]
mod tests;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use anyhow::{Context, Result};
use drive_core::db::{NodeId, now_ms};
use drive_core::engine::Engine;
use drive_core::engine::local::{LocalItem, LocalSide, UploadSource, valid_unix_name};
use drive_core::remote::Kind;
use fuser::{Errno, INodeNo};
use tracing::{debug, info, warn};

use self::cache::Cache;
use self::mount::Invalidator;
use super::{Platform, PlatformContext};

/// Name of the list of never-uploaded files written on stop, so a sign-out can move them out
/// of the cache into the (then unmounted) folder instead of deleting them.
const UNSYNCED_MANIFEST: &str = "unsynced.json";

/// State shared by the FUSE handlers, the engine's [`LocalSide`] calls and background tasks.
pub(crate) struct Core {
    /// Weak: the engine owns the local side, which owns this.
    engine: Weak<Engine>,
    rt: tokio::runtime::Handle,
    cache: Cache,
    inval: Invalidator,
    uid: u32,
    gid: u32,
    /// Shown as the modification time of folders that don't have one.
    started_ms: i64,
}

impl Core {
    fn engine(&self) -> Result<Arc<Engine>, Errno> {
        self.engine.upgrade().ok_or(Errno::EIO)
    }
}

/// Maps engine/io errors to errno values.
pub(crate) fn errno_of(e: &io::Error) -> Errno {
    if let Some(code) = e.raw_os_error() {
        return Errno::from_i32(code);
    }
    match e.kind() {
        io::ErrorKind::NotFound => Errno::ENOENT,
        io::ErrorKind::AlreadyExists => Errno::EEXIST,
        io::ErrorKind::InvalidInput | io::ErrorKind::InvalidFilename => Errno::EINVAL,
        io::ErrorKind::WouldBlock => Errno::EAGAIN,
        io::ErrorKind::DirectoryNotEmpty => Errno::ENOTEMPTY,
        io::ErrorKind::IsADirectory => Errno::EISDIR,
        io::ErrorKind::NotADirectory => Errno::ENOTDIR,
        io::ErrorKind::StorageFull => Errno::ENOSPC,
        _ => Errno::EIO,
    }
}

pub(crate) fn engine_errno(e: drive_core::engine::EngineError) -> Errno {
    use drive_core::engine::EngineError;
    match e {
        EngineError::NotFound(_) => Errno::ENOENT,
        EngineError::Invalid(_) => Errno::EINVAL,
        EngineError::Io(e) => errno_of(&e),
        EngineError::Remote(_) | EngineError::Db(_) => Errno::EIO,
    }
}

pub fn prepare(ctx: PlatformContext) -> Result<Box<dyn Platform>> {
    let rt = tokio::runtime::Handle::try_current()
        .context("the Linux integration must be set up inside the Tokio runtime")?;
    debug!(
        "setting up the {} mount for {}",
        ctx.display_name, ctx.account
    );
    mount::check_executable()?;
    mount::prepare_mount_point(&ctx.folder)?;
    std::fs::create_dir_all(&ctx.cache_dir)
        .with_context(|| format!("creating {}", ctx.cache_dir.display()))?;
    let cache = Cache::new(ctx.cache_dir.clone());
    cache
        .reconcile(ctx.engine.db())
        .context("checking the content cache")?;
    // SAFETY: getuid/getgid cannot fail.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let core = Arc::new(Core {
        engine: Arc::downgrade(&ctx.engine),
        rt,
        cache,
        inval: Invalidator::default(),
        uid,
        gid,
        started_ms: now_ms(),
    });
    Ok(Box::new(LinuxPlatform {
        core,
        folder: ctx.folder,
        cache_dir: ctx.cache_dir,
        session: Mutex::new(None),
    }))
}

/// Unmounts (if still mounted), moves never-uploaded files out of the cache into `folder`,
/// and deletes the cache.
pub fn teardown(folder: &Path, cache_dir: &Path) -> Result<()> {
    if mount::mounted_fs_type(folder).is_some() || mount::is_stale_mount(folder) {
        mount::fusermount_unmount(folder, true);
    }
    rescue_unsynced(folder, cache_dir);
    match std::fs::remove_dir_all(cache_dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("removing {}", cache_dir.display())),
    }
    // The mount point itself was created for Drive; leave it only if it holds rescued files.
    let _ = std::fs::remove_dir(folder);
    Ok(())
}

struct LinuxPlatform {
    core: Arc<Core>,
    folder: PathBuf,
    cache_dir: PathBuf,
    session: Mutex<Option<fuser::BackgroundSession>>,
}

impl Platform for LinuxPlatform {
    fn local_side(&self) -> Arc<dyn LocalSide> {
        Arc::new(LinuxLocal {
            core: self.core.clone(),
        })
    }

    fn start(&self) -> Result<()> {
        let mut session = self.session.lock().unwrap();
        if session.is_some() {
            return Ok(());
        }
        let fs = fs::DriveFs::new(self.core.clone());
        let mut cfg = fuser::Config::default();
        cfg.mount_options = vec![
            fuser::MountOption::FSName("drive".into()),
            fuser::MountOption::Subtype("drive".into()),
            fuser::MountOption::DefaultPermissions,
            fuser::MountOption::NoDev,
            fuser::MountOption::NoSuid,
            fuser::MountOption::NoAtime,
            fuser::MountOption::RW,
        ];
        // `auto_unmount` needs `allow_other` (fuser refuses it otherwise); a mount left behind
        // by a crash is cleaned up by the next start instead (see `prepare_mount_point`).
        cfg.n_threads = Some(4);
        let s = fuser::Session::new(fs, &self.folder, &cfg).map_err(|e| {
            anyhow::anyhow!(
                "mounting {} failed: {e}. Drive needs FUSE: install the \"fuse3\" package and make sure /dev/fuse exists.",
                self.folder.display()
            )
        })?;
        self.core.inval.start(s.notifier());
        *session = Some(s.spawn().context("starting the file system threads")?);
        info!("mounted Drive at {}", self.folder.display());
        Ok(())
    }

    fn stop(&self) {
        let Some(session) = self.session.lock().unwrap().take() else {
            return;
        };
        if let Some(engine) = self.core.engine.upgrade() {
            write_unsynced_manifest(&engine, &self.cache_dir);
        }
        self.core.inval.stop();
        if mount::fusermount_unmount(&self.folder, false) {
            // The kernel ends the session; wait (briefly) for the threads to finish.
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(session.join());
            });
            match rx.recv_timeout(Duration::from_secs(5)) {
                Ok(Ok(())) => info!("unmounted {}", self.folder.display()),
                Ok(Err(e)) => warn!("file system session ended with an error: {e}"),
                Err(_) => warn!("file system threads did not stop in time"),
            }
        } else {
            // Something still uses the folder (an open file, a shell inside it): detach it so
            // it disappears as soon as it is no longer in use.
            warn!("{} is busy; unmounting it lazily", self.folder.display());
            mount::fusermount_unmount(&self.folder, true);
            drop(session);
        }
    }
}

/// The engine's view of the mount.
struct LinuxLocal {
    core: Arc<Core>,
}

fn split_parent(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => ("", path),
    }
}

impl LinuxLocal {
    fn parent_ino(&self, path: &str) -> Option<INodeNo> {
        let engine = self.core.engine.upgrade()?;
        let (parent, _) = split_parent(path);
        engine
            .db()
            .node_by_path(parent)
            .ok()
            .flatten()
            .map(|n| INodeNo(n.id as u64))
    }

    /// File ids at or below `id`.
    fn subtree_files(&self, id: NodeId) -> Vec<NodeId> {
        let Some(engine) = self.core.engine.upgrade() else {
            return vec![];
        };
        let db = engine.db();
        let mut out = vec![];
        let mut stack = vec![id];
        while let Some(cur) = stack.pop() {
            match db.node(cur) {
                Ok(Some(n)) if n.kind == Kind::File => out.push(n.id),
                Ok(Some(_)) => stack.extend(
                    db.children(cur)
                        .unwrap_or_default()
                        .into_iter()
                        .map(|c| c.id),
                ),
                _ => {}
            }
        }
        out
    }
}

impl LocalSide for LinuxLocal {
    fn valid_name(&self, name: &str) -> bool {
        valid_unix_name(name)
    }

    fn create(&self, item: &LocalItem) -> io::Result<()> {
        // A stale file from a reused node id must not show up as this file's content.
        if item.kind == Kind::File {
            self.core.cache.discard(item.id);
        }
        if let Some(parent) = self.parent_ino(&item.path) {
            self.core.inval.entry(parent, split_parent(&item.path).1);
        }
        Ok(())
    }

    fn replace(&self, item: &LocalItem) -> io::Result<()> {
        self.core.cache.discard(item.id);
        self.core.inval.inode(INodeNo(item.id as u64));
        Ok(())
    }

    fn rename(&self, from: &str, item: &LocalItem) -> io::Result<()> {
        // Called before the database moves the node, so `from` still resolves.
        if let Some(old_parent) = self.parent_ino(from) {
            self.core.inval.entry(old_parent, split_parent(from).1);
        }
        if let Some(new_parent) = self.parent_ino(&item.path) {
            self.core
                .inval
                .entry(new_parent, split_parent(&item.path).1);
        }
        Ok(())
    }

    fn remove(&self, item: &LocalItem) -> io::Result<()> {
        for id in self.subtree_files(item.id) {
            self.core.cache.discard(id);
        }
        if let Some(parent) = self.parent_ino(&item.path) {
            self.core
                .inval
                .delete(parent, INodeNo(item.id as u64), split_parent(&item.path).1);
        }
        Ok(())
    }

    fn open_for_upload(&self, item: &LocalItem) -> io::Result<UploadSource> {
        self.core.cache.open_for_upload(item)
    }

    fn uploaded(&self, item: &LocalItem, _size: u64, _mtime_ms: i64) -> io::Result<bool> {
        Ok(self.core.cache.uploaded(item.id))
    }

    fn evict(&self, item: &LocalItem) -> io::Result<bool> {
        let evicted = self.core.cache.evict(item.id)?;
        if evicted {
            // st_blocks changes.
            self.core.inval.inode(INodeNo(item.id as u64));
        }
        Ok(evicted)
    }

    fn hydrate(&self, item: &LocalItem) -> io::Result<()> {
        let engine = self
            .core
            .engine
            .upgrade()
            .ok_or_else(|| io::Error::other("shutting down"))?;
        let node = engine
            .db()
            .node(item.id)
            .map_err(io::Error::other)?
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        if node.kind != Kind::File {
            return Ok(());
        }
        let entry = self.core.cache.acquire(&node, false, false)?;
        // Runs on a blocking thread (never a runtime worker): wait for the download there.
        let (tx, rx) = std::sync::mpsc::channel();
        let core = self.core.clone();
        let e = entry.clone();
        self.core.rt.spawn(async move {
            let _ = tx.send(core.wait_complete(&e).await);
        });
        let result = rx.recv().unwrap_or(Err(Errno::EIO));
        self.core.cache.release(&entry, false);
        result
            .map_err(|e| io::Error::other(format!("downloading {} failed ({e:?})", item.path)))?;
        debug!("hydrated {}", item.path);
        Ok(())
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct UnsyncedFile {
    id: NodeId,
    path: String,
}

/// Records files whose only copy is in the cache (never uploaded, or changed since).
fn write_unsynced_manifest(engine: &Engine, cache_dir: &Path) {
    let list: Vec<UnsyncedFile> = match engine.db().all_nodes() {
        Ok(nodes) => nodes
            .into_iter()
            .filter(|n| n.kind == Kind::File && !n.deleted && (n.dirty || n.remote_id.is_none()))
            .filter_map(|n| {
                Some(UnsyncedFile {
                    id: n.id,
                    path: engine.db().path_of(n.id).ok().flatten()?,
                })
            })
            .collect(),
        Err(e) => {
            warn!("listing unsynced files failed: {e}");
            return;
        }
    };
    let path = cache_dir.join(UNSYNCED_MANIFEST);
    if list.is_empty() {
        let _ = std::fs::remove_file(path);
        return;
    }
    if let Err(e) = serde_json::to_vec(&list)
        .map_err(io::Error::other)
        .and_then(|b| std::fs::write(&path, b))
    {
        warn!("saving the list of unsynced files failed: {e}");
    }
}

/// Moves files listed in the manifest from the cache into `folder` (sign-out keeps them).
fn rescue_unsynced(folder: &Path, cache_dir: &Path) {
    let Ok(bytes) = std::fs::read(cache_dir.join(UNSYNCED_MANIFEST)) else {
        return;
    };
    let Ok(list) = serde_json::from_slice::<Vec<UnsyncedFile>>(&bytes) else {
        return;
    };
    for f in list {
        let src = cache_dir.join(f.id.to_string());
        if !src.is_file() {
            continue;
        }
        let rel: PathBuf = f
            .path
            .split('/')
            .filter(|p| !p.is_empty() && *p != "." && *p != "..")
            .collect();
        let mut dest = folder.join(&rel);
        if let Some(parent) = dest.parent()
            && std::fs::create_dir_all(parent).is_err()
        {
            continue;
        }
        let mut n = 2;
        while dest.exists() {
            let stem = rel
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let ext = rel
                .extension()
                .map(|e| format!(".{}", e.to_string_lossy()))
                .unwrap_or_default();
            dest = folder
                .join(&rel)
                .with_file_name(format!("{stem} ({n}){ext}"));
            n += 1;
        }
        match std::fs::rename(&src, &dest).or_else(|_| std::fs::copy(&src, &dest).map(|_| ())) {
            Ok(()) => info!("kept unsynced file {}", dest.display()),
            Err(e) => warn!("keeping unsynced file {} failed: {e}", dest.display()),
        }
    }
}
