//! Reading backed-up folders: what is backed up and what is skipped.
//!
//! Skipped anywhere in a backed-up folder:
//! * names matching the folder's skip list (`node_modules`, `*.tmp`, …; see [`default_excludes`]),
//! * temporary and lock files ([`is_ignored_name`]),
//! * folders tagged as caches with a `CACHEDIR.TAG` file (Rust `target`, many tool caches;
//!   <https://bford.info/cachedir/>) and Python virtual environments (`pyvenv.cfg`),
//! * symbolic links and junctions (Windows' legacy `My Music` links in Documents, …),
//! * online-only files of other cloud apps (reading them would download them), counted so the
//!   user can be told.
//!
//! Skipped folders are never entered, which matters: a developer's Documents folder can hold
//! millions of files in `node_modules` and build output.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use drive_core::engine::local::is_ignored_name;
use drive_core::remote::Kind;

/// The skip list new backups start with.
pub fn default_excludes() -> Vec<String> {
    [
        "node_modules",
        "bower_components",
        "__pycache__",
        ".venv",
        ".tox",
        ".gradle",
        ".pnpm-store",
        ".npm",
        ".cache",
        ".next",
        ".nuxt",
        ".svelte-kit",
        ".turbo",
        ".parcel-cache",
        ".pytest_cache",
        ".mypy_cache",
        ".ruff_cache",
        ".terraform",
        ".Trash-*",
        "$RECYCLE.BIN",
        "System Volume Information",
        "*.tmp",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

const CACHEDIR_SIGNATURE: &[u8] = b"Signature: 8a477f597d28d172789f06886806bc55";

/// A compiled skip list.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    patterns: Vec<String>,
}

impl Filter {
    pub fn new(excludes: &[String]) -> Self {
        Self {
            patterns: excludes
                .iter()
                .map(|p| p.trim().to_lowercase())
                .filter(|p| !p.is_empty())
                .collect(),
        }
    }

    /// Whether an item with this name is skipped (case-insensitively, like Windows).
    pub fn skips_name(&self, name: &str) -> bool {
        if is_ignored_name(name) {
            return true;
        }
        let lower = name.to_lowercase();
        self.patterns.iter().any(|p| glob_match(p, &lower))
    }
}

/// `*` matches any run of characters, `?` one character; everything else literally.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// One item of a backed-up folder. `rel` is `/`-separated and relative to the folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub kind: Kind,
    pub size: u64,
    pub mtime_ms: i64,
}

#[derive(Debug, Default)]
pub struct Scan {
    /// By relative path. Folders included.
    pub items: BTreeMap<String, Item>,
    pub online_only: u64,
    pub unreadable: u64,
    /// Folders (relative) that couldn't be read: their contents are unknown, not deleted.
    pub unreadable_dirs: Vec<String>,
    /// Whether the walk finished (false when it ran out of time).
    pub complete: bool,
}

/// How a directory entry is treated.
enum Kindish {
    File(Item),
    Dir(i64),
    Skip,
    OnlineOnly,
}

fn mtime_ms(meta: &fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(windows)]
fn classify(meta: &fs::Metadata) -> Kindish {
    use std::os::windows::fs::MetadataExt;
    const REPARSE_POINT: u32 = 0x400;
    const OFFLINE: u32 = 0x1000;
    const RECALL_ON_OPEN: u32 = 0x4_0000;
    const RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;
    let attrs = meta.file_attributes();
    if attrs & (OFFLINE | RECALL_ON_OPEN | RECALL_ON_DATA_ACCESS) != 0 {
        return Kindish::OnlineOnly;
    }
    if attrs & REPARSE_POINT != 0 || meta.file_type().is_symlink() {
        return Kindish::Skip;
    }
    plain(meta)
}

#[cfg(not(windows))]
fn classify(meta: &fs::Metadata) -> Kindish {
    if meta.file_type().is_symlink() {
        return Kindish::Skip;
    }
    plain(meta)
}

fn plain(meta: &fs::Metadata) -> Kindish {
    if meta.is_dir() {
        Kindish::Dir(mtime_ms(meta))
    } else if meta.is_file() {
        Kindish::File(Item {
            kind: Kind::File,
            size: meta.len(),
            mtime_ms: mtime_ms(meta),
        })
    } else {
        Kindish::Skip // sockets, devices, pipes
    }
}

/// Whether a folder is a tagged cache or a Python virtual environment.
pub fn is_tool_folder(dir: &Path) -> bool {
    if dir.join("pyvenv.cfg").is_file() {
        return true;
    }
    let mut sig = [0u8; 43];
    match fs::File::open(dir.join("CACHEDIR.TAG")) {
        Ok(mut f) => f.read_exact(&mut sig).is_ok() && sig == CACHEDIR_SIGNATURE,
        Err(_) => false,
    }
}

/// What a single path is now. `None`: gone or skipped.
#[cfg(test)]
pub fn stat(abs: &Path, filter: &Filter) -> Option<Item> {
    let name = abs.file_name()?.to_string_lossy();
    if filter.skips_name(&name) {
        return None;
    }
    let meta = fs::symlink_metadata(abs).ok()?;
    match classify(&meta) {
        Kindish::File(i) => Some(i),
        Kindish::Dir(m) if !is_tool_folder(abs) => Some(Item {
            kind: Kind::Folder,
            size: 0,
            mtime_ms: m,
        }),
        _ => None,
    }
}

/// Reads `root/rel` (recursively unless `shallow`) into `out`, with paths relative to `root`.
/// Stops when `deadline` passes (then `out.complete` stays false).
pub fn walk(
    root: &Path,
    rel: &str,
    shallow: bool,
    filter: &Filter,
    deadline: Option<Instant>,
    out: &mut Scan,
) {
    out.complete = true;
    let mut stack: Vec<String> = vec![rel.trim_matches('/').to_string()];
    while let Some(dir_rel) = stack.pop() {
        if deadline.is_some_and(|d| Instant::now() > d) {
            out.complete = false;
            return;
        }
        let dir_abs: PathBuf = if dir_rel.is_empty() {
            root.to_path_buf()
        } else {
            root.join(&dir_rel)
        };
        let entries = match fs::read_dir(&dir_abs) {
            Ok(e) => e,
            Err(_) => {
                out.unreadable += 1;
                out.unreadable_dirs.push(dir_rel.clone());
                continue;
            }
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if filter.skips_name(&name) {
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                out.unreadable += 1;
                continue;
            };
            let child_rel = if dir_rel.is_empty() {
                name.clone()
            } else {
                format!("{dir_rel}/{name}")
            };
            match classify(&meta) {
                Kindish::File(item) => {
                    out.items.insert(child_rel, item);
                }
                Kindish::Dir(m) => {
                    if is_tool_folder(&entry.path()) {
                        continue;
                    }
                    out.items.insert(
                        child_rel.clone(),
                        Item {
                            kind: Kind::Folder,
                            size: 0,
                            mtime_ms: m,
                        },
                    );
                    if !shallow {
                        stack.push(child_rel);
                    }
                }
                Kindish::OnlineOnly => out.online_only += 1,
                Kindish::Skip => {}
            }
        }
    }
}

/// Size of a folder after the skips, within `budget`. `None` if it can't be read.
pub fn measure(root: &Path, filter: &Filter, budget: Duration) -> Option<(u64, u64, bool)> {
    if !root.is_dir() {
        return None;
    }
    let mut scan = Scan::default();
    walk(
        root,
        "",
        false,
        filter,
        Some(Instant::now() + budget),
        &mut scan,
    );
    let (files, bytes) = scan
        .items
        .values()
        .filter(|i| i.kind == Kind::File)
        .fold((0u64, 0u64), |(f, b), i| (f + 1, b + i.size));
    Some((files, bytes, scan.complete))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        assert!(glob_match("*.tmp", "a.tmp"));
        assert!(glob_match(".trash-*", ".trash-1000"));
        assert!(glob_match("node_modules", "node_modules"));
        assert!(!glob_match("*.tmp", "a.tmpx"));
        assert!(glob_match("a?c", "abc"));
        assert!(glob_match("*", ""));
    }

    #[test]
    fn walks_with_skips() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src/deep")).unwrap();
        fs::write(root.join("src/deep/a.txt"), b"hello").unwrap();
        fs::write(root.join("notes.txt"), b"x").unwrap();
        fs::write(root.join("junk.TMP"), b"x").unwrap();
        fs::write(root.join("~$lock.docx"), b"x").unwrap();
        fs::create_dir_all(root.join("app/node_modules/pkg")).unwrap();
        fs::write(root.join("app/node_modules/pkg/index.js"), b"x").unwrap();
        fs::create_dir_all(root.join("rust/target/debug")).unwrap();
        fs::write(root.join("rust/target/CACHEDIR.TAG"), CACHEDIR_SIGNATURE).unwrap();
        fs::write(root.join("rust/target/debug/big.bin"), b"x").unwrap();
        fs::create_dir_all(root.join("py/env/lib")).unwrap();
        fs::write(root.join("py/env/pyvenv.cfg"), b"home = x").unwrap();
        let filter = Filter::new(&default_excludes());
        let mut scan = Scan::default();
        walk(root, "", false, &filter, None, &mut scan);
        let paths: Vec<&str> = scan.items.keys().map(|s| s.as_str()).collect();
        assert_eq!(
            paths,
            [
                "app",
                "notes.txt",
                "py",
                "rust",
                "src",
                "src/deep",
                "src/deep/a.txt"
            ]
        );
        assert!(scan.complete);
        assert_eq!(scan.items["src/deep/a.txt"].size, 5);

        let mut shallow = Scan::default();
        walk(root, "src", true, &filter, None, &mut shallow);
        let paths: Vec<&str> = shallow.items.keys().map(|s| s.as_str()).collect();
        assert_eq!(paths, ["src/deep"]);

        assert_eq!(
            measure(root, &filter, Duration::from_secs(5)),
            Some((2, 6, true))
        );
        assert!(stat(&root.join("rust/target"), &filter).is_none());
        assert!(stat(&root.join("notes.txt"), &filter).is_some());
    }
}

#[cfg(test)]
mod measure_real {
    /// Read-only measurement of this machine's well-known folders (run by hand:
    /// `cargo test -p drive-daemon measure_real -- --ignored --nocapture`).
    #[test]
    #[ignore]
    fn measure_known_folders() {
        let filter = super::Filter::new(&super::default_excludes());
        for (_, name, path) in crate::backup::known_folders() {
            let t = std::time::Instant::now();
            let r = super::measure(&path, &filter, std::time::Duration::from_secs(300));
            eprintln!(
                "{name:<10} {:?} in {:.1}s",
                r.map(|(f, b, c)| (f, format!("{:.1} GB", b as f64 / 1e9), c)),
                t.elapsed().as_secs_f32()
            );
            if std::env::var("DRIVE_MEASURE_TOP").is_ok() {
                let mut scan = super::Scan::default();
                super::walk(&path, "", false, &filter, None, &mut scan);
                let mut by_dir: std::collections::HashMap<String, (u64, u64)> = Default::default();
                let mut by_name: std::collections::HashMap<String, (u64, u64)> = Default::default();
                for (rel, item) in &scan.items {
                    if item.kind != drive_core::remote::Kind::File {
                        continue;
                    }
                    let parts: Vec<&str> = rel.split('/').collect();
                    let key = parts[..parts.len().saturating_sub(1).min(3)].join("/");
                    let e = by_dir.entry(key).or_default();
                    e.0 += 1;
                    e.1 += item.size;
                    for p in &parts[..parts.len() - 1] {
                        let e = by_name.entry(p.to_lowercase()).or_default();
                        e.0 += 1;
                        e.1 += item.size;
                    }
                }
                let mut v: Vec<_> = by_dir.into_iter().collect();
                v.sort_by(|a, b| b.1.1.cmp(&a.1.1));
                for (d, (f, b)) in v.iter().take(15) {
                    eprintln!("    {:>7.2} GB {:>7} files  {d}", *b as f64 / 1e9, f);
                }
                let mut n: Vec<_> = by_name
                    .into_iter()
                    .filter(|(k, _)| {
                        [
                            "target",
                            "build",
                            "dist",
                            ".git",
                            ".next",
                            "out",
                            "bin",
                            "obj",
                            "library",
                            "node_modules",
                            ".gradle",
                            "vendor",
                            "venv",
                            "env",
                        ]
                        .contains(&k.as_str())
                    })
                    .collect();
                n.sort_by(|a, b| b.1.1.cmp(&a.1.1));
                for (d, (f, b)) in n.iter().take(10) {
                    eprintln!(
                        "    folder name {d:<12} {:>7.2} GB {:>7} files",
                        *b as f64 / 1e9,
                        f
                    );
                }
            }
        }
    }
}
