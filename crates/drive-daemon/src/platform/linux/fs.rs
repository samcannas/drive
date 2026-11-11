//! The FUSE request handlers.
//!
//! Handlers run on fuser's worker threads and only do local work there (database lookups,
//! cache file I/O). Anything that needs the network is moved to the Tokio runtime and replies
//! from there, so a slow download never stalls other requests.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use drive_core::db::{Node, NodeId, now_ms, pending};
use drive_core::engine::local::valid_unix_name;
use drive_core::remote::Kind;
use fuser::{
    BsdFileFlags, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation,
    INodeNo, InitFlags, KernelConfig, LockOwner, OpenAccMode, OpenFlags, RenameFlags, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyOpen,
    ReplyStatfs, ReplyWrite, ReplyXattr, Request, TimeOrNow, WriteFlags,
};
use tracing::{debug, warn};

use super::cache::{Entry, FAR_AHEAD};
use super::{Core, engine_errno, errno_of};

/// How long the kernel may cache names and attributes. Remote changes are pushed as explicit
/// invalidations, so this mainly bounds staleness if one is missed.
const TTL: Duration = Duration::from_secs(1);

pub const XATTR_PINNED: &str = "user.drive.pinned";
pub const XATTR_STATUS: &str = "user.drive.status";

struct OpenFile {
    entry: Arc<Entry>,
    write: bool,
}

struct DirItem {
    ino: u64,
    name: String,
    node: Node,
}

pub struct DriveFs {
    core: Arc<Core>,
    files: Arc<Mutex<HashMap<u64, OpenFile>>>,
    dirs: Mutex<HashMap<u64, Vec<DirItem>>>,
    next_fh: AtomicU64,
}

fn name_str(name: &OsStr) -> Result<&str, Errno> {
    let s = name.to_str().ok_or(Errno::EINVAL)?;
    if s.len() > 255 {
        return Err(Errno::ENAMETOOLONG);
    }
    if !valid_unix_name(s) {
        return Err(Errno::EINVAL);
    }
    Ok(s)
}

fn ms_to_time(ms: i64) -> SystemTime {
    if ms >= 0 {
        UNIX_EPOCH + Duration::from_millis(ms as u64)
    } else {
        UNIX_EPOCH
    }
}

fn time_to_ms(t: TimeOrNow) -> i64 {
    match t {
        TimeOrNow::Now => now_ms(),
        TimeOrNow::SpecificTime(t) => match t.duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_millis() as i64,
            Err(e) => -(e.duration().as_millis() as i64),
        },
    }
}

impl Core {
    fn node(&self, ino: INodeNo) -> Result<Node, Errno> {
        let engine = self.engine()?;
        match engine.db().node(ino.0 as NodeId) {
            Ok(Some(n)) if !n.deleted => Ok(n),
            Ok(_) => Err(Errno::ENOENT),
            Err(e) => {
                warn!("database lookup failed: {e}");
                Err(Errno::EIO)
            }
        }
    }

    /// The child called exactly `name` (names are unique case-insensitively, but a lookup
    /// must not alias "a.txt" to "A.txt", or case-only renames would be impossible).
    fn child(&self, parent: INodeNo, name: &str) -> Result<Option<Node>, Errno> {
        let engine = self.engine()?;
        engine
            .db()
            .child(parent.0 as NodeId, name)
            .map_err(|_| Errno::EIO)
    }

    fn exact_child(&self, parent: INodeNo, name: &str) -> Result<Node, Errno> {
        match self.child(parent, name)? {
            Some(n) if n.name == name => Ok(n),
            _ => Err(Errno::ENOENT),
        }
    }

    fn has_children(&self, id: NodeId) -> Result<bool, Errno> {
        let engine = self.engine()?;
        engine
            .db()
            .children(id)
            .map(|c| !c.is_empty())
            .map_err(|_| Errno::EIO)
    }

    pub fn attr(&self, n: &Node) -> FileAttr {
        let (mut size, mut mtime) = (n.size, n.mtime_ms);
        if n.kind == Kind::File
            && let Some(e) = self.cache.get(n.id)
        {
            let st = e.st.lock().unwrap();
            if st.local {
                size = st.size;
                mtime = st.mtime_ms;
            }
        }
        if mtime <= 0 {
            mtime = self.started_ms;
        }
        let t = ms_to_time(mtime);
        let folder = n.kind == Kind::Folder;
        // Cloud-only files take no space here: `du` and file managers show that.
        let local = folder || n.hydrated || n.remote_id.is_none() || n.dirty;
        FileAttr {
            ino: INodeNo(n.id as u64),
            size: if folder { 0 } else { size },
            blocks: if local && !folder {
                size.div_ceil(512)
            } else {
                0
            },
            atime: t,
            mtime: t,
            ctime: t,
            crtime: t,
            kind: if folder {
                FileType::Directory
            } else {
                FileType::RegularFile
            },
            perm: if folder { 0o755 } else { 0o644 },
            nlink: if folder { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    fn status_of(&self, n: &Node) -> &'static str {
        let syncing = n.dirty || n.pending & (pending::PUSH | pending::MOVE) != 0;
        if n.kind == Kind::Folder {
            return if syncing {
                "syncing"
            } else if n.pinned {
                "pinned"
            } else {
                "synced"
            };
        }
        if syncing || n.remote_id.is_none() {
            "syncing"
        } else if n.pinned {
            "pinned"
        } else if n.hydrated {
            "downloaded"
        } else if self
            .cache
            .get(n.id)
            .is_some_and(|e| e.st.lock().unwrap().downloading)
        {
            "downloading"
        } else {
            "cloud-only"
        }
    }
}

impl DriveFs {
    pub fn new(core: Arc<Core>) -> Self {
        Self {
            core,
            files: Arc::new(Mutex::new(HashMap::new())),
            dirs: Mutex::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
        }
    }

    fn add_file(&self, entry: Arc<Entry>, write: bool) -> FileHandle {
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        self.files
            .lock()
            .unwrap()
            .insert(fh, OpenFile { entry, write });
        FileHandle(fh)
    }

    fn file(&self, fh: FileHandle) -> Option<(Arc<Entry>, bool)> {
        self.files
            .lock()
            .unwrap()
            .get(&fh.0)
            .map(|f| (f.entry.clone(), f.write))
    }

    fn reply_node(&self, n: &Node, reply: ReplyEntry) {
        reply.entry(&TTL, &self.core.attr(n), Generation(0));
    }

    /// Creates a file node with empty content in the cache.
    fn new_file(&self, parent: INodeNo, name: &OsStr) -> Result<(Node, Arc<Entry>), Errno> {
        let name = name_str(name)?;
        if self.core.child(parent, name)?.is_some() {
            return Err(Errno::EEXIST);
        }
        let engine = self.core.engine()?;
        let id = engine
            .create_in(parent.0 as NodeId, name, Kind::File, 0, now_ms())
            .map_err(engine_errno)?;
        let node = self.core.node(INodeNo(id as u64))?;
        let entry = self
            .core
            .cache
            .acquire(&node, true, true)
            .map_err(|e| errno_of(&e))?;
        Ok((node, entry))
    }

    fn snapshot_dir(&self, ino: INodeNo) -> Result<Vec<DirItem>, Errno> {
        let dir = self.core.node(ino)?;
        if dir.kind != Kind::Folder {
            return Err(Errno::ENOTDIR);
        }
        let parent = match dir.parent_id {
            Some(p) => self
                .core
                .node(INodeNo(p as u64))
                .unwrap_or_else(|_| dir.clone()),
            None => dir.clone(),
        };
        let engine = self.core.engine()?;
        let children = engine.db().children(dir.id).map_err(|_| Errno::EIO)?;
        let mut items = Vec::with_capacity(children.len() + 2);
        items.push(DirItem {
            ino: dir.id as u64,
            name: ".".into(),
            node: dir.clone(),
        });
        items.push(DirItem {
            ino: parent.id as u64,
            name: "..".into(),
            node: parent,
        });
        items.extend(children.into_iter().map(|n| DirItem {
            ino: n.id as u64,
            name: n.name.clone(),
            node: n,
        }));
        Ok(items)
    }

    fn set_pinned(&self, ino: INodeNo, pinned: bool) -> Result<(), Errno> {
        let engine = self.core.engine()?;
        let path = engine
            .db()
            .path_of(ino.0 as NodeId)
            .map_err(|_| Errno::EIO)?
            .ok_or(Errno::ENOENT)?;
        // Pinning spawns downloads on the runtime.
        let _guard = self.core.rt.enter();
        engine.set_pinned(&path, pinned).map_err(engine_errno)
    }
}

fn xattr_reply(reply: ReplyXattr, size: u32, value: &[u8]) {
    if size == 0 {
        reply.size(value.len() as u32);
    } else if (size as usize) < value.len() {
        reply.error(Errno::ERANGE);
    } else {
        reply.data(value);
    }
}

fn parse_bool(v: &[u8]) -> Option<bool> {
    match String::from_utf8_lossy(v)
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" | "" => Some(false),
        _ => None,
    }
}

impl Filesystem for DriveFs {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> std::io::Result<()> {
        // O_TRUNC arrives with open() instead of as a separate truncate (no download first).
        let _ = config.add_capabilities(InitFlags::FUSE_ATOMIC_O_TRUNC);
        // ls -l / file managers: names and attributes in one round trip.
        let _ = config
            .add_capabilities(InitFlags::FUSE_DO_READDIRPLUS | InitFlags::FUSE_READDIRPLUS_AUTO);
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(name) = name.to_str() else {
            return reply.error(Errno::ENOENT);
        };
        match self.core.exact_child(parent, name) {
            Ok(n) => self.reply_node(&n, reply),
            Err(e) => reply.error(e),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.core.node(ino) {
            Ok(n) => reply.attr(&TTL, &self.core.attr(&n)),
            Err(e) => reply.error(e),
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let node = match self.core.node(ino) {
            Ok(n) => n,
            Err(e) => return reply.error(e),
        };
        if uid.is_some_and(|u| u != self.core.uid) || gid.is_some_and(|g| g != self.core.gid) {
            return reply.error(Errno::EPERM);
        }
        // Permission bits aren't stored (files are 0644, folders 0755); accept chmod quietly so
        // `cp -p` and friends work.
        if node.kind == Kind::Folder {
            if size.is_some() {
                return reply.error(Errno::EISDIR);
            }
            return reply.attr(&TTL, &self.core.attr(&node));
        }
        // An open handle carries the change to the engine on close; otherwise report now.
        let handle = fh
            .and_then(|fh| self.file(fh))
            .filter(|(_, w)| *w)
            .map(|(e, _)| e);
        let (entry, temp) = match &handle {
            Some(e) => (e.clone(), false),
            None if size.is_some() || mtime.is_some() => {
                match self.core.cache.acquire(&node, true, false) {
                    Ok(e) => (e, true),
                    Err(e) => return reply.error(errno_of(&e)),
                }
            }
            None => return reply.attr(&TTL, &self.core.attr(&node)),
        };
        let core = self.core.clone();
        let finish = move |core: &Arc<Core>,
                           entry: &Arc<Entry>,
                           result: Result<(), Errno>,
                           reply: ReplyAttr| {
            if result.is_ok() && temp {
                core.report(entry);
            }
            if temp {
                core.cache.release(entry, true);
            }
            match result.and_then(|_| core.node(ino)) {
                Ok(n) => reply.attr(&TTL, &core.attr(&n)),
                Err(e) => reply.error(e),
            }
        };
        let apply_mtime = move |entry: &Arc<Entry>| {
            if let Some(t) = mtime {
                let ms = time_to_ms(t);
                let mut st = entry.st.lock().unwrap();
                // Changing the mtime of a cloud-only file would need a download and an upload;
                // it is ignored instead.
                if st.complete && ms != st.mtime_ms {
                    st.mtime_ms = ms;
                    st.local = true;
                    st.dirty = true;
                }
            }
        };
        let complete = entry.st.lock().unwrap().complete;
        match size {
            Some(size) if size > 0 && !complete => {
                // Extending/shrinking a cloud-only file: download it first.
                let rt = self.core.rt.clone();
                rt.spawn(async move {
                    let r = core
                        .wait_complete(&entry)
                        .await
                        .and_then(|_| core.truncate(&entry, size));
                    if r.is_ok() {
                        apply_mtime(&entry);
                    }
                    finish(&core, &entry, r, reply);
                });
            }
            Some(size) => {
                let r = core.truncate(&entry, size);
                if r.is_ok() {
                    apply_mtime(&entry);
                }
                finish(&core, &entry, r, reply);
            }
            None => {
                apply_mtime(&entry);
                finish(&core, &entry, Ok(()), reply);
            }
        }
    }

    fn mknod(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        if mode & libc::S_IFMT != libc::S_IFREG && mode & libc::S_IFMT != 0 {
            return reply.error(Errno::EPERM);
        }
        match self.new_file(parent, name) {
            Ok((node, entry)) => {
                self.core.cache.release(&entry, true);
                self.reply_node(&node, reply);
            }
            Err(e) => reply.error(e),
        }
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let r = (|| {
            let name = name_str(name)?;
            if self.core.child(parent, name)?.is_some() {
                return Err(Errno::EEXIST);
            }
            let engine = self.core.engine()?;
            let id = engine
                .create_in(parent.0 as NodeId, name, Kind::Folder, 0, now_ms())
                .map_err(engine_errno)?;
            self.core.node(INodeNo(id as u64))
        })();
        match r {
            Ok(n) => self.reply_node(&n, reply),
            Err(e) => reply.error(e),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let r = (|| {
            let n = self
                .core
                .exact_child(parent, name.to_str().ok_or(Errno::ENOENT)?)?;
            if n.kind == Kind::Folder {
                return Err(Errno::EISDIR);
            }
            self.core.engine()?.delete_id(n.id).map_err(engine_errno)?;
            self.core.cache.discard(n.id);
            Ok(())
        })();
        match r {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let r = (|| {
            let n = self
                .core
                .exact_child(parent, name.to_str().ok_or(Errno::ENOENT)?)?;
            if n.kind != Kind::Folder {
                return Err(Errno::ENOTDIR);
            }
            if self.core.has_children(n.id)? {
                return Err(Errno::ENOTEMPTY);
            }
            self.core.engine()?.delete_id(n.id).map_err(engine_errno)
        })();
        match r {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let r = (|| {
            if flags.intersects(RenameFlags::RENAME_EXCHANGE | RenameFlags::RENAME_WHITEOUT) {
                return Err(Errno::EINVAL);
            }
            let src = self
                .core
                .exact_child(parent, name.to_str().ok_or(Errno::ENOENT)?)?;
            let newname = name_str(newname)?;
            let dest_dir = self.core.node(newparent)?;
            if dest_dir.kind != Kind::Folder {
                return Err(Errno::ENOTDIR);
            }
            // Names are unique case-insensitively, so "b.txt" -> "A.txt" replaces "a.txt".
            let target = self
                .core
                .child(newparent, newname)?
                .filter(|t| t.id != src.id);
            if let Some(t) = &target {
                if flags.contains(RenameFlags::RENAME_NOREPLACE) {
                    return Err(Errno::EEXIST);
                }
                match (src.kind, t.kind) {
                    (Kind::File, Kind::Folder) => return Err(Errno::EISDIR),
                    (Kind::Folder, Kind::File) => return Err(Errno::ENOTDIR),
                    (Kind::Folder, Kind::Folder) if self.core.has_children(t.id)? => {
                        return Err(Errno::ENOTEMPTY);
                    }
                    _ => {}
                }
            }
            self.core
                .engine()?
                .rename_id(src.id, newparent.0 as NodeId, newname)
                .map_err(engine_errno)?;
            if let Some(t) = target {
                self.core.cache.discard(t.id);
                if t.name != newname {
                    // The kernel only knows it replaced `newname`.
                    self.core
                        .inval
                        .delete(newparent, INodeNo(t.id as u64), &t.name);
                }
            }
            Ok(())
        })();
        match r {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let node = match self.core.node(ino) {
            Ok(n) if n.kind == Kind::Folder => return reply.error(Errno::EISDIR),
            Ok(n) => n,
            Err(e) => return reply.error(e),
        };
        let write = flags.acc_mode() != OpenAccMode::O_RDONLY;
        let entry = match self.core.cache.acquire(&node, write, false) {
            Ok(e) => e,
            Err(e) => return reply.error(errno_of(&e)),
        };
        if let Ok(engine) = self.core.engine() {
            engine.touch(node.id);
        }
        let fh = self.add_file(entry.clone(), write);
        if !write {
            return reply.opened(fh, FopenFlags::empty());
        }
        if flags.0 & libc::O_TRUNC != 0 {
            return match self.core.truncate(&entry, 0) {
                Ok(()) => reply.opened(fh, FopenFlags::empty()),
                Err(e) => {
                    self.release_handle(fh);
                    reply.error(e)
                }
            };
        }
        if entry.st.lock().unwrap().complete {
            return reply.opened(fh, FopenFlags::empty());
        }
        // Writing needs the whole file here first.
        let core = self.core.clone();
        let files = self.files.clone();
        self.core.rt.spawn(async move {
            match core.wait_complete(&entry).await {
                Ok(()) => reply.opened(fh, FopenFlags::empty()),
                Err(e) => {
                    if let Some(f) = files.lock().unwrap().remove(&fh.0) {
                        core.cache.release(&f.entry, f.write);
                    }
                    reply.error(e)
                }
            }
        });
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let Some((entry, _)) = self.file(fh) else {
            return reply.error(Errno::EBADF);
        };
        let (complete, have, total) = {
            let st = entry.st.lock().unwrap();
            (st.complete, st.have, st.remote_size)
        };
        let serve = |entry: &Entry, offset: u64, len: usize, reply: ReplyData| match entry
            .read_at(offset, len)
        {
            Ok(data) => reply.data(&data),
            Err(e) => reply.error(errno_of(&e)),
        };
        if complete {
            return serve(&entry, offset, size as usize, reply);
        }
        if offset >= total {
            return reply.data(&[]);
        }
        let end = (offset + u64::from(size)).min(total);
        if have >= end {
            return serve(&entry, offset, (end - offset) as usize, reply);
        }
        let core = self.core.clone();
        if offset > have + FAR_AHEAD {
            // Far ahead of the download (a seek): fetch just this range, keep downloading.
            core.start_download(&entry);
            let id = entry.id;
            self.core.rt.spawn(async move {
                match core.read_range(id, offset, end - offset).await {
                    Ok(data) => reply.data(&data),
                    Err(e) => reply.error(e),
                }
            });
        } else {
            self.core.rt.spawn(async move {
                match core.wait_for(&entry, end).await {
                    Ok(()) => serve(&entry, offset, (end - offset) as usize, reply),
                    Err(e) => reply.error(e),
                }
            });
        }
    }

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let Some((entry, write)) = self.file(fh) else {
            return reply.error(Errno::EBADF);
        };
        if !write {
            return reply.error(Errno::EBADF);
        }
        let mut st = entry.st.lock().unwrap();
        if !st.complete {
            return reply.error(Errno::EIO);
        }
        if let Err(e) = entry.file.write_all_at(data, offset) {
            return reply.error(errno_of(&e));
        }
        st.size = st.size.max(offset + data.len() as u64);
        st.have = st.size;
        st.dirty = true;
        st.local = true;
        st.mtime_ms = now_ms();
        drop(st);
        self.core.cache.note_write(entry.id);
        reply.written(data.len() as u32);
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        if let Some((entry, true)) = self.file(fh) {
            self.core.report(&entry);
        }
        reply.ok();
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.release_handle(fh);
        reply.ok();
    }

    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        if let Some((entry, true)) = self.file(fh) {
            if let Err(e) = entry.file.sync_data() {
                return reply.error(errno_of(&e));
            }
            self.core.report(&entry);
        }
        reply.ok();
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        match self.snapshot_dir(ino) {
            Ok(items) => {
                let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
                self.dirs.lock().unwrap().insert(fh, items);
                reply.opened(FileHandle(fh), FopenFlags::empty());
            }
            Err(e) => reply.error(e),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let dirs = self.dirs.lock().unwrap();
        let Some(items) = dirs.get(&fh.0) else {
            return reply.error(Errno::EBADF);
        };
        for (i, it) in items.iter().enumerate().skip(offset as usize) {
            let kind = if it.node.kind == Kind::Folder {
                FileType::Directory
            } else {
                FileType::RegularFile
            };
            if reply.add(INodeNo(it.ino), (i + 1) as u64, kind, &it.name) {
                break;
            }
        }
        reply.ok();
    }

    fn readdirplus(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectoryPlus,
    ) {
        let dirs = self.dirs.lock().unwrap();
        let Some(items) = dirs.get(&fh.0) else {
            return reply.error(Errno::EBADF);
        };
        for (i, it) in items.iter().enumerate().skip(offset as usize) {
            let attr = self.core.attr(&it.node);
            if reply.add(
                INodeNo(it.ino),
                (i + 1) as u64,
                &it.name,
                &TTL,
                &attr,
                Generation(0),
            ) {
                break;
            }
        }
        reply.ok();
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        self.dirs.lock().unwrap().remove(&fh.0);
        reply.ok();
    }

    fn fsyncdir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        // Report the disk that holds the cache: that is where new files go.
        let path = std::ffi::CString::new(self.core.cache.dir().as_os_str().as_encoded_bytes())
            .unwrap_or_default();
        let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: valid C string and out-pointer.
        if unsafe { libc::statvfs(path.as_ptr(), &mut s) } != 0 {
            return reply.statfs(0, 0, 0, 0, 0, 4096, 255, 4096);
        }
        reply.statfs(
            s.f_blocks,
            s.f_bfree,
            s.f_bavail,
            s.f_files,
            s.f_ffree,
            s.f_bsize as u32,
            255,
            s.f_frsize as u32,
        );
    }

    fn setxattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        name: &OsStr,
        value: &[u8],
        _flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        match name.to_str() {
            Some(XATTR_PINNED) => match parse_bool(value) {
                Some(pinned) => match self.set_pinned(ino, pinned) {
                    Ok(()) => reply.ok(),
                    Err(e) => reply.error(e),
                },
                None => reply.error(Errno::EINVAL),
            },
            Some(XATTR_STATUS) => reply.error(Errno::EPERM),
            _ => reply.error(Errno::ENOTSUP),
        }
    }

    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        let node = match self.core.node(ino) {
            Ok(n) => n,
            Err(e) => return reply.error(e),
        };
        match name.to_str() {
            Some(XATTR_PINNED) => xattr_reply(reply, size, if node.pinned { b"1" } else { b"0" }),
            Some(XATTR_STATUS) => xattr_reply(reply, size, self.core.status_of(&node).as_bytes()),
            _ => reply.error(Errno::NO_XATTR),
        }
    }

    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        if let Err(e) = self.core.node(ino) {
            return reply.error(e);
        }
        let names = format!("{XATTR_PINNED}\0{XATTR_STATUS}\0");
        xattr_reply(reply, size, names.as_bytes());
    }

    fn removexattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        match name.to_str() {
            Some(XATTR_PINNED) => match self.set_pinned(ino, false) {
                Ok(()) => reply.ok(),
                Err(e) => reply.error(e),
            },
            Some(XATTR_STATUS) => reply.error(Errno::EPERM),
            _ => reply.error(Errno::NO_XATTR),
        }
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        match self.new_file(parent, name) {
            Ok((node, entry)) => {
                let write = OpenFlags(flags).acc_mode() != OpenAccMode::O_RDONLY;
                if !write {
                    // `acquire` registered a writer; this handle only reads.
                    let mut st = entry.st.lock().unwrap();
                    st.writers = st.writers.saturating_sub(1);
                }
                let fh = self.add_file(entry, write);
                debug!("created {} ({})", node.name, node.id);
                reply.created(
                    &TTL,
                    &self.core.attr(&node),
                    Generation(0),
                    fh,
                    FopenFlags::empty(),
                );
            }
            Err(e) => reply.error(e),
        }
    }

    fn fallocate(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _offset: u64,
        _length: u64,
        _mode: i32,
        reply: ReplyEmpty,
    ) {
        reply.error(Errno::EOPNOTSUPP);
    }
}

impl DriveFs {
    fn release_handle(&self, fh: FileHandle) {
        let f = self.files.lock().unwrap().remove(&fh.0);
        if let Some(f) = f {
            if f.write {
                self.core.report(&f.entry);
            }
            self.core.cache.release(&f.entry, f.write);
        }
    }
}
