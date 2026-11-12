//! Measuring where a disk's space goes: a parallel walk that adds up folder sizes.
//!
//! Built to be quick and honest on a big, nearly full disk:
//! * sizes come from the directory listing (no file is opened), folders are read in parallel;
//! * online-only files of cloud apps count as 0 (they take no space), and sparse or compressed
//!   files count what they really occupy (a 256 GB virtual disk image may use 30 GB);
//! * symbolic links and junctions are never followed (Windows has junction loops in AppData);
//! * hard-linked files count once. On Linux every file's link count is known from the listing.
//!   Windows doesn't list it, so files are opened to ask only where hard links are common:
//!   package stores (pnpm's store is linked into every project's `node_modules`);
//! * on Linux the walk stays on one filesystem;
//! * folders a backup would skip (`node_modules`, tagged caches, Python virtual environments)
//!   are measured too, but their bytes are remembered as `skipped`: saving space in a folder
//!   leaves them on the disk, so estimates must not count them.
//!
//! Only folders of at least [`KEEP_MIN`] and up to [`KEEP_DEPTH`] levels deep are kept in the
//! result; everything else is added into its parent, so the tree stays small enough to save.

use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::backup::scan::Filter;

/// Folders this many levels below the scan root keep no children.
pub const KEEP_DEPTH: usize = 6;
/// Smaller folders are only counted in their parent.
pub const KEEP_MIN: u64 = 32 << 20;

const CACHEDIR_SIGNATURE: &[u8] = b"Signature: 8a477f597d28d172789f06886806bc55";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub name: String,
    /// Space used on the disk by everything inside.
    pub bytes: u64,
    pub files: u64,
    /// Of `bytes`, what's inside folders a backup skips (would stay on the disk).
    #[serde(default)]
    pub skipped: u64,
    /// Contains a `.git` folder directly (a code project).
    #[serde(default)]
    pub git: bool,
    /// A code project is inside, at any depth.
    #[serde(default)]
    pub has_git: bool,
    /// Of `bytes`, what's inside code projects.
    #[serde(default)]
    pub git_bytes: u64,
    /// Programs are directly inside (`.dll` files, or a `bin` / `steamapps` folder).
    #[serde(default)]
    pub programs: bool,
    /// Of `bytes`, what's inside program folders.
    #[serde(default)]
    pub program_bytes: u64,
    /// The folder couldn't be read (its size is unknown and counted nowhere).
    #[serde(default)]
    pub unreadable: bool,
    /// Largest first.
    #[serde(default)]
    pub children: Vec<Node>,
}

impl Node {
    /// A child by name (case-insensitively on Windows).
    pub fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|c| same_name(&c.name, name))
    }

    /// The node at `rel` below this one (`rel` relative, any separators).
    pub fn find(&self, rel: &Path) -> Option<&Node> {
        let mut cur = self;
        for part in rel.components() {
            let name = part.as_os_str().to_string_lossy();
            cur = cur.child(&name)?;
        }
        Some(cur)
    }

    /// Bytes directly in this folder (not in kept subfolders).
    pub fn own_bytes(&self) -> u64 {
        self.bytes
            .saturating_sub(self.children.iter().map(|c| c.bytes).sum::<u64>())
    }
}

pub fn same_name(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a.eq_ignore_ascii_case(b) || a.to_lowercase() == b.to_lowercase()
    } else {
        a == b
    }
}

/// Same path, ignoring trailing separators (and case and slash direction on Windows).
pub fn same_path(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| {
        let s = p.to_string_lossy();
        if cfg!(windows) {
            s.replace('\\', "/").trim_end_matches('/').to_lowercase()
        } else {
            s.trim_end_matches('/').to_string()
        }
    };
    norm(a) == norm(b)
}

/// Shared with the progress reporter; set `cancel` to stop early.
#[derive(Default)]
pub struct Progress {
    pub bytes: AtomicU64,
    pub files: AtomicU64,
    pub current: Mutex<String>,
    pub cancel: AtomicBool,
}

impl Progress {
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

pub struct Options {
    /// Folders not to enter (e.g. `C:\Windows`, counted as the system remainder instead).
    pub skip: Vec<PathBuf>,
    /// What a backup skips (for `Node::skipped`).
    pub filter: Filter,
    /// Threads reading folders.
    pub threads: usize,
}

struct Ctx<'a> {
    opts: &'a Options,
    progress: &'a Progress,
    #[cfg(unix)]
    dev: u64,
    /// Hard-linked files already counted: (volume or device, file id).
    seen_links: Mutex<HashSet<(u64, u64)>>,
}

/// Walks `root`. Returns `None` if cancelled.
pub fn scan(root: &Path, opts: &Options, progress: &Progress) -> anyhow::Result<Option<Node>> {
    let meta = fs::metadata(root)?;
    anyhow::ensure!(meta.is_dir(), "{} isn't a folder", root.display());
    let ctx = Ctx {
        opts,
        progress,
        #[cfg(unix)]
        dev: std::os::unix::fs::MetadataExt::dev(&meta),
        seen_links: Mutex::new(HashSet::new()),
    };
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(opts.threads.max(1))
        .stack_size(8 << 20)
        .thread_name(|i| format!("space-scan-{i}"))
        .build()?;
    let name = root.display().to_string();
    let node = pool.install(|| scan_dir(root, name, 0, Flags::default(), &ctx));
    Ok((!progress.cancelled()).then_some(node))
}

/// What a folder inherits from the folders above it.
#[derive(Clone, Copy, Default)]
struct Flags {
    /// Inside a folder a backup skips.
    skipped: bool,
    /// Inside a package store or `node_modules`, where files are often hard links.
    links: bool,
}

struct Sub {
    path: PathBuf,
    name: String,
    flags: Flags,
}

fn link_prone(name: &str, parent: &Path) -> bool {
    name.eq_ignore_ascii_case("node_modules")
        || name.eq_ignore_ascii_case(".pnpm-store")
        || (name.eq_ignore_ascii_case("store")
            && parent
                .file_name()
                .is_some_and(|p| p.to_string_lossy().eq_ignore_ascii_case("pnpm")))
}

fn scan_dir(path: &Path, name: String, depth: usize, inherited: Flags, ctx: &Ctx<'_>) -> Node {
    let mut node = Node {
        name,
        ..Default::default()
    };
    if ctx.progress.cancelled() {
        return node;
    }
    if depth <= 5
        && let Ok(mut cur) = ctx.progress.current.try_lock()
    {
        *cur = path.display().to_string();
    }
    let entries = match fs::read_dir(path) {
        Ok(e) => e,
        Err(_) => {
            node.unreadable = true;
            return node;
        }
    };
    let mut subdirs: Vec<Sub> = Vec::new();
    let mut file_bytes = 0u64;
    let mut files = 0u64;
    let mut tool_folder = false;
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let ft = meta.file_type();
        // Symbolic links and junctions: never followed (cloud placeholders aren't links).
        if ft.is_symlink() {
            continue;
        }
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        if ft.is_dir() {
            let child = entry.path();
            if ctx.opts.skip.iter().any(|s| same_path(s, &child)) {
                continue;
            }
            #[cfg(unix)]
            if std::os::unix::fs::MetadataExt::dev(&meta) != ctx.dev {
                continue; // another filesystem mounted here
            }
            if name == ".git" {
                node.git = true;
            }
            if depth > 0 && (name.eq_ignore_ascii_case("bin") || name == "steamapps") {
                node.programs = true;
            }
            subdirs.push(Sub {
                flags: Flags {
                    skipped: inherited.skipped || ctx.opts.filter.skips_name(&name),
                    links: inherited.links || link_prone(&name, path),
                },
                name: name.into_owned(),
                path: child,
            });
        } else if ft.is_file() {
            if name == "pyvenv.cfg" {
                tool_folder = true;
            } else if name == "CACHEDIR.TAG" {
                tool_folder |= is_cachedir_tag(&entry.path());
            }
            if depth > 0 && !node.programs && has_ext(&name, ".dll") {
                node.programs = true;
            }
            files += 1;
            file_bytes += on_disk(&entry, &meta, inherited.links, ctx);
        }
    }
    let skipped = inherited.skipped || tool_folder;
    ctx.progress.bytes.fetch_add(file_bytes, Ordering::Relaxed);
    ctx.progress.files.fetch_add(files, Ordering::Relaxed);

    let mut children: Vec<Node> = subdirs
        .into_par_iter()
        .map(|s| {
            let flags = Flags {
                skipped: skipped || s.flags.skipped,
                links: s.flags.links,
            };
            scan_dir(&s.path, s.name, depth + 1, flags, ctx)
        })
        .collect();

    node.bytes = file_bytes + children.iter().map(|c| c.bytes).sum::<u64>();
    node.files = files + children.iter().map(|c| c.files).sum::<u64>();
    node.skipped = if skipped {
        node.bytes
    } else {
        children.iter().map(|c| c.skipped).sum()
    };
    node.has_git = node.git || children.iter().any(|c| c.has_git);
    node.git_bytes = if node.git {
        node.bytes
    } else {
        children.iter().map(|c| c.git_bytes).sum()
    };
    node.program_bytes = if node.programs {
        node.bytes
    } else {
        children.iter().map(|c| c.program_bytes).sum()
    };
    if depth >= KEEP_DEPTH {
        children.clear();
    } else {
        children.retain(|c| c.bytes >= KEEP_MIN || (c.unreadable && depth < 2));
        children.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.name.cmp(&b.name)));
    }
    node.children = children;
    node
}

fn has_ext(name: &str, ext: &str) -> bool {
    // Compared as bytes: slicing the string could split a character (`bug-🪲.md`).
    let (n, e) = (name.as_bytes(), ext.as_bytes());
    n.len() > e.len() && n[n.len() - e.len()..].eq_ignore_ascii_case(e)
}

fn is_cachedir_tag(path: &Path) -> bool {
    let mut sig = [0u8; 43];
    fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut sig))
        .is_ok()
        && sig == CACHEDIR_SIGNATURE
}

/// Space a file takes on the disk.
#[cfg(windows)]
fn on_disk(entry: &fs::DirEntry, meta: &fs::Metadata, links: bool, ctx: &Ctx<'_>) -> u64 {
    use std::os::windows::fs::MetadataExt;
    const SPARSE: u32 = 0x200;
    const COMPRESSED: u32 = 0x800;
    const OFFLINE: u32 = 0x1000;
    const RECALL_ON_OPEN: u32 = 0x4_0000;
    const RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;
    let attrs = meta.file_attributes();
    if attrs & (OFFLINE | RECALL_ON_OPEN | RECALL_ON_DATA_ACCESS) != 0 {
        return 0; // online-only: the data isn't on this disk
    }
    if links
        && meta.len() > 0
        && let Some(id) = hard_link_id(&entry.path())
        && !ctx.seen_links.lock().unwrap().insert(id)
    {
        return 0; // another name of a file already counted
    }
    if attrs & (SPARSE | COMPRESSED) != 0
        && let Some(real) = allocated(&entry.path())
    {
        return real;
    }
    meta.len()
}

/// The file's (volume, file id) if it has more than one name.
#[cfg(windows)]
fn hard_link_id(path: &Path) -> Option<(u64, u64)> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    const FILE_READ_ATTRIBUTES: u32 = 0x80;
    let file = fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(7)
        .open(path)
        .ok()?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }.ok()?;
    (info.nNumberOfLinks > 1).then_some((
        info.dwVolumeSerialNumber as u64,
        ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
    ))
}

/// What a sparse or compressed file really occupies.
#[cfg(windows)]
fn allocated(path: &Path) -> Option<u64> {
    use windows::Win32::Foundation::{GetLastError, NO_ERROR};
    use windows::Win32::Storage::FileSystem::{GetCompressedFileSizeW, INVALID_FILE_SIZE};
    use windows::core::PCWSTR;
    let wide = widestring::U16CString::from_os_str(path.as_os_str()).ok()?;
    let mut high = 0u32;
    let low = unsafe { GetCompressedFileSizeW(PCWSTR(wide.as_ptr()), Some(&mut high)) };
    if low == INVALID_FILE_SIZE && unsafe { GetLastError() } != NO_ERROR {
        return None;
    }
    Some(((high as u64) << 32) | low as u64)
}

#[cfg(unix)]
fn on_disk(_entry: &fs::DirEntry, meta: &fs::Metadata, _links: bool, ctx: &Ctx<'_>) -> u64 {
    use std::os::unix::fs::MetadataExt;
    if meta.nlink() > 1
        && !ctx
            .seen_links
            .lock()
            .unwrap()
            .insert((meta.dev(), meta.ino()))
    {
        return 0; // another name of a file already counted
    }
    meta.blocks() * 512
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> Options {
        Options {
            skip: vec![],
            filter: Filter::new(&crate::backup::scan::default_excludes()),
            threads: 4,
        }
    }

    fn write(path: &Path, bytes: usize) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, vec![7u8; bytes]).unwrap();
    }

    #[test]
    fn sums_sizes_skips_and_flags() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(&r.join("Videos/a.mp4"), 300_000);
        write(&r.join("Videos/Trip/b.mp4"), 200_000);
        write(&r.join("Code/proj/.git/HEAD"), 10);
        write(&r.join("Code/proj/node_modules/x/index.js"), 50_000);
        write(&r.join("Code/proj/src/main.rs"), 1_000);
        write(&r.join("Code/venv/pyvenv.cfg"), 10);
        write(&r.join("Code/venv/lib.py"), 40_000);
        write(&r.join("Tools/app/tool.dll"), 5_000);
        write(&r.join("Skipme/huge.bin"), 900_000);
        let mut o = opts();
        o.skip.push(r.join("Skipme"));
        let progress = Progress::default();
        let root = scan(r, &o, &progress).unwrap().unwrap();

        let total: u64 = 300_000 + 200_000 + 10 + 50_000 + 1_000 + 10 + 40_000 + 5_000;
        // On Linux sizes are whole blocks; on Windows they're the file lengths.
        if cfg!(windows) {
            assert_eq!(root.bytes, total);
            assert_eq!(progress.bytes.load(Ordering::Relaxed), total);
            assert_eq!(root.skipped, 50_000 + 10 + 40_000);
            assert_eq!(root.git_bytes, 10 + 50_000 + 1_000);
            assert_eq!(root.program_bytes, 5_000);
        } else {
            assert!(root.bytes >= total);
        }
        assert_eq!(root.files, 8);
        assert!(root.has_git);
        // Small folders are folded into their parents (KEEP_MIN is 32 MB).
        assert!(root.children.is_empty());
    }

    #[test]
    fn keeps_big_folders_with_structure() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        let big = (KEEP_MIN + 1) as usize;
        write(&r.join("Videos/2024/movie.mkv"), big);
        write(&r.join("Videos/small.txt"), 10);
        write(&r.join("Projects/app/.git/config"), 10);
        write(&r.join("Projects/app/build.bin"), big);
        let progress = Progress::default();
        let root = scan(r, &opts(), &progress).unwrap().unwrap();
        let videos = root.child("Videos").unwrap();
        assert_eq!(videos.files, 2);
        assert!(videos.find(Path::new("2024")).is_some());
        assert!(!videos.has_git);
        let projects = root.find(Path::new("Projects")).unwrap();
        assert!(projects.has_git);
        assert_eq!(projects.git_bytes, projects.bytes);
        assert!(projects.find(Path::new("app")).unwrap().git);
        assert!(root.children.windows(2).all(|w| w[0].bytes >= w[1].bytes));
    }

    #[test]
    fn hard_links_count_once() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        // Like pnpm: a store, and a project whose node_modules links to it.
        write(&r.join("pnpm/store/v10/files/ab/data"), 100_000);
        let linked = r.join("proj/node_modules/.pnpm/pkg/index.js");
        fs::create_dir_all(linked.parent().unwrap()).unwrap();
        fs::hard_link(r.join("pnpm/store/v10/files/ab/data"), &linked).unwrap();
        let root = scan(r, &opts(), &Progress::default()).unwrap().unwrap();
        assert_eq!(root.files, 2);
        assert!(root.bytes < 200_000, "{}", root.bytes);
        if cfg!(windows) {
            assert_eq!(root.bytes, 100_000);
        }
    }

    #[test]
    fn cancel_stops() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("a/b.txt"), 10);
        let progress = Progress::default();
        progress.cancel.store(true, Ordering::Relaxed);
        assert!(scan(dir.path(), &opts(), &progress).unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn links_arent_followed() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(&r.join("a/data.bin"), 100_000);
        std::os::unix::fs::symlink(r.join("a"), r.join("loop")).unwrap();
        let root = scan(r, &opts(), &Progress::default()).unwrap().unwrap();
        assert!(root.bytes < 200_000, "{}", root.bytes);
    }

    #[test]
    fn extensions_with_any_characters() {
        assert!(has_ext("lib.DLL", ".dll"));
        assert!(!has_ext("bug-report-🪲.md", ".dll"));
        assert!(!has_ext("🪲", ".dll"));
        assert!(has_ext("🪲.dll", ".dll"));
    }

    #[test]
    fn paths_compare_loosely() {
        assert!(same_path(Path::new("/a/b/"), Path::new("/a/b")));
        if cfg!(windows) {
            assert!(same_path(
                Path::new(r"C:\Users\Me\"),
                Path::new("c:/users/me")
            ));
        }
    }
}
