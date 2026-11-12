//! The disk space advisor: where the space on this device goes, and what Drive can free.
//!
//! A scan ([`scan`]) measures the disk holding the user's home folder once (it can take a
//! minute on a big disk) and is saved, so the report opens instantly afterwards. The report is
//! built from that measurement each time it's asked for, with the current backups and Drive
//! folder, so it reflects actions taken since the scan without measuring again.
//!
//! Drive only acts on the user's own folders, and never moves or deletes anything: "Save
//! space" backs a folder up and makes it on-demand in place (Windows). Everything else (apps,
//! app data, the system, other cloud apps' folders) is shown so the numbers add up, with a
//! plain explanation of who manages it.

pub mod scan;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use drive_core::db::now_ms;
use drive_core::ipc::{
    DiskUsage, KnownFolder, SpaceAction, SpaceCategory, SpaceItem, SpaceReport, SpaceSaved,
    SpaceScan, SpaceScanState,
};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use self::scan::{Node, Progress, same_name, same_path};
use crate::backup;

/// Smaller folders aren't listed on their own (they're still in their parent's size).
pub const MIN_ITEM: u64 = 256 << 20;
/// Actions aren't offered when they'd free less than this.
pub const MIN_FREE: u64 = 512 << 20;
const MAX_CHILDREN: usize = 8;

/// One measured disk, as saved between runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scanned {
    pub root: PathBuf,
    pub tree: Node,
    pub total_bytes: u64,
    pub free_bytes: u64,
    /// Big files directly in the disk's root (Windows' page and hibernation files).
    #[serde(default)]
    pub root_files: Vec<(String, u64)>,
    pub started_ms: i64,
    pub finished_ms: i64,
}

#[derive(Debug, Clone)]
pub struct BackupRef {
    pub id: String,
    pub path: PathBuf,
    pub on_demand: bool,
    pub keep_bytes: u64,
    /// Files of this folder that are only in the cloud.
    pub cloud_only_bytes: u64,
}

/// Everything the report needs besides the measurement.
#[derive(Debug, Clone, Default)]
pub struct Context {
    pub home: PathBuf,
    pub drive_folder: PathBuf,
    /// Drive's own data, config and caches.
    pub own_dirs: Vec<PathBuf>,
    pub backups: Vec<BackupRef>,
    /// Other cloud apps' folders: (folder, app name).
    pub other_clouds: Vec<(PathBuf, String)>,
    pub known: Vec<(KnownFolder, String, PathBuf)>,
    /// Folders can be made on-demand here (Windows).
    pub can_save_space: bool,
    /// Downloaded, not pinned bytes in the Drive folder (what "Free up space" frees).
    pub drive_cache_bytes: u64,
    /// Bytes of the Drive folder on this device (downloaded + pinned).
    pub drive_local_bytes: u64,
    /// Files of the Drive folder that are only in the cloud.
    pub drive_cloud_only_bytes: u64,
    /// Where the Linux mount keeps downloaded files (inside `~/.cache`).
    pub drive_content_dir: Option<PathBuf>,
    pub temp_dir: Option<PathBuf>,
    /// How much an on-demand folder keeps by default.
    pub keep_bytes: u64,
}

impl Context {
    fn backup_exact(&self, p: &Path) -> Option<&BackupRef> {
        self.backups.iter().find(|b| same_path(&b.path, p))
    }
    fn backup_containing(&self, p: &Path) -> Option<&BackupRef> {
        self.backups.iter().find(|b| inside(p, &b.path))
    }
    fn known_of(&self, p: &Path) -> Option<(KnownFolder, &str)> {
        self.known
            .iter()
            .find(|(_, _, kp)| same_path(kp, p))
            .map(|(k, n, _)| (*k, n.as_str()))
    }
    fn other_cloud(&self, p: &Path) -> Option<&str> {
        self.other_clouds
            .iter()
            .find(|(c, _)| same_path(c, p) || inside(p, c) || inside(c, p))
            .map(|(_, n)| n.as_str())
    }
}

/// Whether `p` is strictly inside `parent`.
pub fn inside(p: &Path, parent: &Path) -> bool {
    let norm = |x: &Path| {
        let s = x.to_string_lossy().replace('\\', "/");
        let s = s.trim_end_matches('/').to_string();
        if cfg!(windows) { s.to_lowercase() } else { s }
    };
    let (p, parent) = (norm(p), norm(parent));
    p.len() > parent.len() && p.starts_with(&parent) && p.as_bytes()[parent.len()] == b'/'
}

/// The full form of a path: Windows may hand out 8.3 short names (`C:\Users\SAMSEP~1\…`)
/// that wouldn't match the names the scan reads from the disk.
pub fn long_path(p: &Path) -> PathBuf {
    match std::fs::canonicalize(p) {
        Ok(c) => {
            let s = c.to_string_lossy();
            match s.strip_prefix(r"\\?\") {
                Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
                _ => c,
            }
        }
        Err(_) => p.to_path_buf(),
    }
}

pub fn gb(bytes: u64) -> String {
    let g = bytes as f64 / (1u64 << 30) as f64;
    if g >= 10.0 {
        format!("{g:.0} GB")
    } else if g >= 1.0 {
        format!("{g:.1} GB")
    } else {
        format!("{:.0} MB", bytes as f64 / (1u64 << 20) as f64)
    }
}

/// What saving space in a folder would free: everything a backup includes, beyond what's kept.
fn savable(node: &Node, keep: u64) -> u64 {
    node.bytes.saturating_sub(node.skipped).saturating_sub(keep)
}

struct Builder<'a> {
    ctx: &'a Context,
    s: &'a Scanned,
}

fn bucket(name: &str, bytes: u64, category: SpaceCategory, note: Option<String>) -> SpaceItem {
    SpaceItem {
        path: String::new(),
        name: name.to_string(),
        bytes,
        files: 0,
        category,
        known: None,
        backup_id: None,
        on_demand: false,
        action: None,
        frees_bytes: 0,
        caution: None,
        note,
        children: vec![],
    }
}

fn display(p: &Path) -> String {
    p.display().to_string()
}

impl<'a> Builder<'a> {
    fn node_at(&self, p: &Path) -> Option<&'a Node> {
        let s: &'a Scanned = self.s;
        if same_path(p, &s.root) {
            return Some(&s.tree);
        }
        if !inside(p, &s.root) {
            return None;
        }
        let root = s.root.to_string_lossy().replace('\\', "/");
        let full = p.to_string_lossy().replace('\\', "/");
        let rel = full[root.trim_end_matches('/').len()..].trim_start_matches('/');
        s.tree.find(Path::new(rel))
    }

    /// One folder of the user's (or anything else not handled specially).
    fn folder(&self, path: &Path, node: &Node, children: bool) -> SpaceItem {
        let ctx = self.ctx;
        let known = ctx.known_of(path);
        let mut item = SpaceItem {
            path: display(path),
            name: known
                .map(|(_, n)| n.to_string())
                .unwrap_or_else(|| node_name(path, node)),
            bytes: node.bytes,
            files: node.files,
            category: SpaceCategory::UserFolder,
            known: known.map(|(k, _)| k),
            backup_id: None,
            on_demand: false,
            action: None,
            frees_bytes: 0,
            caution: None,
            note: None,
            children: vec![],
        };
        let in_backup = ctx.backup_containing(path);
        if same_path(path, &ctx.drive_folder) {
            self.drive_item(&mut item);
        } else if inside(path, &ctx.drive_folder) {
            item.category = SpaceCategory::DriveFolder;
        } else if let Some(b) = ctx.backup_exact(path) {
            item.category = SpaceCategory::Backup;
            item.backup_id = Some(b.id.clone());
            item.on_demand = b.on_demand;
            if b.on_demand {
                item.note = Some(format!(
                    "Saving space: keeps up to {} of recently used files on this PC.",
                    gb(b.keep_bytes)
                ));
            } else if ctx.can_save_space {
                let frees = savable(node, ctx.keep_bytes);
                if frees >= MIN_FREE {
                    item.action = Some(SpaceAction::TurnOnSaveSpace);
                    item.frees_bytes = frees;
                    item.caution = code_caution(node);
                }
            }
        } else if let Some(b) = in_backup {
            item.category = SpaceCategory::Backup;
            item.backup_id = Some(b.id.clone());
            item.on_demand = b.on_demand;
        } else if let Some(app) = ctx.other_cloud(path) {
            item.category = SpaceCategory::OtherCloud;
            item.note = Some(format!(
                "Managed by {app}. Use {app}'s own \u{201c}Free up space\u{201d} to save space here."
            ));
        } else if known.is_none() && node.bytes > 0 && node.skipped * 2 >= node.bytes {
            item.category = SpaceCategory::Developer;
            item.note = Some(
                "Packages and build files. Backups skip them, and tools download them again when needed."
                    .into(),
            );
        } else if known.is_none() && node.program_bytes > 0 && node.program_bytes * 2 >= node.bytes
        {
            item.category = SpaceCategory::Apps;
            item.note = Some("Programs or games. They need their files on this PC to run.".into());
        } else if known.is_none() && node.git_bytes > 0 && node.git_bytes * 2 >= node.bytes {
            item.category = SpaceCategory::Developer;
            item.note = Some(
                "Code projects. Developer tools read every file, so files saved in the cloud would download again and again."
                    .into(),
            );
        } else {
            let own: Vec<&Path> = ctx.own_dirs.iter().map(|p| p.as_path()).collect();
            let others: Vec<&Path> = ctx.backups.iter().map(|b| b.path.as_path()).collect();
            if let Some(problem) = backup::overlap_problem(path, &ctx.drive_folder, &own, &others) {
                item.note = Some(problem);
            } else if ctx.can_save_space {
                let frees = savable(node, ctx.keep_bytes);
                if frees >= MIN_FREE {
                    item.action = Some(SpaceAction::SaveSpace);
                    item.frees_bytes = frees;
                    item.caution = code_caution(node);
                }
            } else if node.bytes >= MIN_FREE {
                item.action = Some(SpaceAction::BackUp);
                item.note = Some(
                    "Back it up to keep it safe. Saving space in folders is available on Windows for now."
                        .into(),
                );
            }
        }
        if children && item.category != SpaceCategory::DriveFolder {
            item.children = node
                .children
                .iter()
                .filter(|c| c.bytes >= MIN_ITEM)
                .take(MAX_CHILDREN)
                .map(|c| self.folder(&path.join(&c.name), c, false))
                .collect();
        }
        item
    }

    fn drive_item(&self, item: &mut SpaceItem) {
        let ctx = self.ctx;
        item.category = SpaceCategory::DriveFolder;
        item.name = "Drive".into();
        if ctx.drive_cache_bytes >= MIN_FREE {
            item.action = Some(SpaceAction::FreeDriveCache);
            item.frees_bytes = ctx.drive_cache_bytes;
        }
        item.note = Some(if ctx.drive_cloud_only_bytes > 0 {
            format!(
                "Files you opened lately. {} more is only in the cloud.",
                gb(ctx.drive_cloud_only_bytes)
            )
        } else {
            "Files you opened lately.".into()
        });
    }

    /// AppData (Windows): one item with the biggest apps' folders as children; Temp separately.
    fn app_data(
        &self,
        path: &Path,
        node: &'a Node,
        temp: &mut Option<SpaceItem>,
        apps: &mut Vec<(PathBuf, &'a Node)>,
    ) -> SpaceItem {
        let mut kids: Vec<(PathBuf, &Node)> = Vec::new();
        let mut bytes = node.bytes;
        for area in &node.children {
            for c in &area.children {
                let p = path.join(&area.name).join(&c.name);
                if self
                    .ctx
                    .temp_dir
                    .as_deref()
                    .is_some_and(|t| same_path(t, &p))
                {
                    *temp = Some(self.temp_item(&p, c));
                    bytes = bytes.saturating_sub(c.bytes);
                    continue;
                }
                // Apps installed for one account (VS Code, Cursor, Discord, …).
                if same_name(&area.name, "Local") && same_name(&c.name, "Programs") {
                    apps.push((p, c));
                    bytes = bytes.saturating_sub(c.bytes);
                    continue;
                }
                kids.push((p, c));
            }
        }
        kids.sort_by(|a, b| b.1.bytes.cmp(&a.1.bytes));
        let mut item = bucket(
            "App data",
            bytes,
            SpaceCategory::AppData,
            Some("Settings and caches your apps keep. Clear caches from inside each app.".into()),
        );
        item.path = display(path);
        item.files = node.files;
        item.children = kids
            .into_iter()
            .filter(|(_, n)| n.bytes >= MIN_ITEM)
            .take(MAX_CHILDREN)
            .map(|(p, n)| {
                let own = self
                    .ctx
                    .own_dirs
                    .iter()
                    .any(|d| same_path(d, &p) || inside(d, &p));
                let mut i = bucket(
                    &if own {
                        format!("{} (Drive's own data)", node_name(&p, n))
                    } else {
                        node_name(&p, n)
                    },
                    n.bytes,
                    SpaceCategory::AppData,
                    None,
                );
                i.path = display(&p);
                i.files = n.files;
                i
            })
            .collect();
        item
    }

    fn temp_item(&self, path: &Path, node: &Node) -> SpaceItem {
        let mut i = bucket(
            "Temporary files",
            node.bytes,
            SpaceCategory::Temporary,
            Some(if cfg!(windows) {
                "Leftovers from apps and installers. Windows can clear them in Storage settings."
                    .into()
            } else {
                "Leftovers from apps. They're cleared when you restart.".into()
            }),
        );
        i.path = display(path);
        i.files = node.files;
        if cfg!(windows) {
            i.action = Some(SpaceAction::OpenSystemCleanup);
        }
        i
    }

    /// A folder of installed programs, or several merged into one bucket.
    fn apps(&self, name: &str, parts: &[(PathBuf, &Node)], note: &str) -> SpaceItem {
        let bytes = parts.iter().map(|(_, n)| n.bytes).sum();
        let mut item = bucket(name, bytes, SpaceCategory::Apps, Some(note.into()));
        item.files = parts.iter().map(|(_, n)| n.files).sum();
        if let [(p, _)] = parts {
            item.path = display(p);
        }
        let mut kids: Vec<(PathBuf, &Node)> = parts
            .iter()
            .flat_map(|(p, n)| n.children.iter().map(move |c| (p.join(&c.name), c)))
            .collect();
        kids.sort_by(|a, b| b.1.bytes.cmp(&a.1.bytes));
        item.children = kids
            .into_iter()
            .filter(|(_, n)| n.bytes >= MIN_ITEM)
            .take(MAX_CHILDREN)
            .map(|(p, n)| {
                let mut i = bucket(&node_name(&p, n), n.bytes, SpaceCategory::Apps, None);
                i.path = display(&p);
                i.files = n.files;
                i
            })
            .collect();
        item
    }

    /// The user's home folder: its folders, hidden app folders and loose files.
    fn home(&self, items: &mut Vec<SpaceItem>, apps: &mut Vec<(PathBuf, &'a Node)>) -> u64 {
        let ctx = self.ctx;
        let Some(home) = self.node_at(&ctx.home) else {
            return 0;
        };
        let mut hidden: Vec<(PathBuf, &Node)> = Vec::new();
        let mut temp = None;
        for c in &home.children {
            let p = ctx.home.join(&c.name);
            if cfg!(windows) && same_name(&c.name, "AppData") {
                items.push(self.app_data(&p, c, &mut temp, apps));
            } else if !cfg!(windows) && c.name == ".cache" {
                items.push(self.linux_cache(&p, c));
            } else if c.name.starts_with('.') {
                hidden.push((p, c));
            } else {
                items.push(self.folder(&p, c, true));
            }
        }
        items.extend(temp);
        let hidden_bytes: u64 = hidden.iter().map(|(_, n)| n.bytes).sum();
        if hidden_bytes >= MIN_ITEM {
            let mut item = bucket(
                "Hidden app folders",
                hidden_bytes,
                SpaceCategory::AppData,
                Some("Settings, tools and caches apps keep in your user folder (names starting with a dot).".into()),
            );
            hidden.sort_by(|a, b| b.1.bytes.cmp(&a.1.bytes));
            item.children = hidden
                .iter()
                .filter(|(_, n)| n.bytes >= MIN_ITEM)
                .take(MAX_CHILDREN)
                .map(|(p, n)| {
                    let mut i = bucket(&node_name(p, n), n.bytes, SpaceCategory::AppData, None);
                    i.path = display(p);
                    i.files = n.files;
                    i
                })
                .collect();
            items.push(item);
        }
        let loose = home.own_bytes();
        if loose >= MIN_ITEM {
            let mut item = bucket(
                "Files in your user folder",
                loose,
                SpaceCategory::Other,
                Some("Files kept directly in your user folder, outside its folders.".into()),
            );
            item.path = display(&ctx.home);
            items.push(item);
        }
        home.bytes
    }

    /// `~/.cache` on Linux; the Drive mount's downloaded files live there.
    fn linux_cache(&self, path: &Path, node: &Node) -> SpaceItem {
        let mut i = bucket(
            "Caches",
            node.bytes,
            SpaceCategory::Temporary,
            Some("Apps rebuild these when needed. Clear them from inside each app.".into()),
        );
        i.path = display(path);
        i.files = node.files;
        if let Some(content) = &self.ctx.drive_content_dir
            && let Some(n) = self.node_at(content)
        {
            i.bytes = i.bytes.saturating_sub(n.bytes);
        }
        i
    }

    fn windows_root(&self, items: &mut Vec<SpaceItem>, mut apps: Vec<(PathBuf, &'a Node)>) {
        let s: &'a Scanned = self.s;
        let root = &s.root;
        let users = s.tree.child("Users");
        for c in &s.tree.children {
            let p = root.join(&c.name);
            let lower = c.name.to_lowercase();
            if lower == "users" {
                continue;
            } else if lower == "program files" || lower == "program files (x86)" {
                apps.push((p, c));
            } else if lower == "programdata" {
                let mut i = bucket(
                    "App data for all accounts",
                    c.bytes,
                    SpaceCategory::AppData,
                    Some("Settings and caches apps share between all accounts on this PC.".into()),
                );
                i.path = display(&p);
                i.files = c.files;
                items.push(i);
            } else if lower == "$recycle.bin" {
                let mut i = bucket(
                    "Recycle Bin",
                    c.bytes,
                    SpaceCategory::RecycleBin,
                    Some("Deleted files. Empty the Recycle Bin to get this space back.".into()),
                );
                i.action = Some(SpaceAction::OpenSystemCleanup);
                items.push(i);
            } else if lower == "system volume information"
                || lower == "$windows.~bt"
                || lower == "$windows.~ws"
            {
                // Unreadable or Windows' own: counted in the system remainder.
            } else if c.programs || is_apps_name(&lower) {
                items.push(self.apps(
                    &node_name(&p, c),
                    &[(p.clone(), c)],
                    "Programs or games. They need their files on this PC to run.",
                ));
            } else {
                items.push(self.folder(&p, c, true));
            }
        }
        if !apps.is_empty() {
            items.push(self.apps(
                "Installed apps",
                &apps,
                "Programs installed on this PC. Uninstall the ones you don't use in Settings.",
            ));
        }
        if let Some(users) = users {
            let mine = self.node_at(&self.ctx.home).map(|n| n.bytes).unwrap_or(0);
            let others = users.bytes.saturating_sub(mine);
            if others >= MIN_ITEM {
                items.push(bucket(
                    "Other accounts and shared files",
                    others,
                    SpaceCategory::OtherUsers,
                    Some("Files of other accounts on this PC that Drive can read, and the Public folder.".into()),
                ));
            }
        }
        let root_files: u64 = self.s.root_files.iter().map(|(_, b)| b).sum();
        if root_files >= MIN_ITEM {
            let hiber = self
                .s
                .root_files
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case("hiberfil.sys"));
            let note = match hiber {
                Some((_, b)) if *b >= 1 << 30 => format!(
                    "Windows uses these as extra memory and for hibernation. If you don't hibernate, turning it off (powercfg /h off, as administrator) frees {}.",
                    gb(*b)
                ),
                _ => match self
                    .s
                    .root_files
                    .iter()
                    .find(|(n, _)| n.eq_ignore_ascii_case("pagefile.sys"))
                {
                    Some((_, b)) if *b >= 8 << 30 => format!(
                        "Windows uses these as extra memory. The page file is {}; Windows sizes it itself, and a smaller fixed size can be set in System → About → Advanced system settings → Performance.",
                        gb(*b)
                    ),
                    _ => "Windows uses these as extra memory.".into(),
                },
            };
            items.push(bucket(
                "Page and hibernation files",
                root_files,
                SpaceCategory::System,
                Some(note),
            ));
        }
    }

    /// Returns the bytes of system folders it measured (they join the system remainder).
    fn linux_root(&self, items: &mut Vec<SpaceItem>) -> u64 {
        let root = &self.s.root;
        if !same_path(root, Path::new("/")) {
            // A separate home partition: everything else in it is other accounts' files.
            for c in &self.s.tree.children {
                let p = root.join(&c.name);
                if same_path(&p, &self.ctx.home) || inside(&self.ctx.home, &p) {
                    continue;
                }
                items.push(self.folder(&p, c, true));
            }
            return 0;
        }
        let mut apps: Vec<(PathBuf, &Node)> = Vec::new();
        let mut system = 0u64;
        for c in &self.s.tree.children {
            let p = root.join(&c.name);
            match c.name.as_str() {
                "home" => {
                    let mine = self.node_at(&self.ctx.home).map(|n| n.bytes).unwrap_or(0);
                    let others = c.bytes.saturating_sub(mine);
                    if others >= MIN_ITEM {
                        items.push(bucket(
                            "Other accounts",
                            others,
                            SpaceCategory::OtherUsers,
                            Some(
                                "Files of other accounts on this computer that Drive can read."
                                    .into(),
                            ),
                        ));
                    }
                }
                "usr" | "opt" | "snap" => apps.push((p, c)),
                "var" => {
                    let mut i = bucket(
                        "System data and logs",
                        c.bytes,
                        SpaceCategory::AppData,
                        Some(
                            "Package caches, logs, containers and databases of system services."
                                .into(),
                        ),
                    );
                    i.path = display(&p);
                    i.children = c
                        .children
                        .iter()
                        .filter(|n| n.bytes >= MIN_ITEM)
                        .take(MAX_CHILDREN)
                        .map(|n| {
                            let mut k = bucket(
                                &format!("/var/{}", n.name),
                                n.bytes,
                                SpaceCategory::AppData,
                                None,
                            );
                            k.path = display(&p.join(&n.name));
                            k
                        })
                        .collect();
                    items.push(i);
                }
                "tmp" => items.push(self.temp_item(&p, c)),
                "boot" | "etc" | "root" | "srv" | "lib" | "lib64" | "lib32" | "bin" | "sbin"
                | "lost+found" => system += c.bytes,
                _ => items.push(self.folder(&p, c, true)),
            }
        }
        if !apps.is_empty() {
            items.push(self.apps("Installed software", &apps, "Programs and system packages. Remove the ones you don't use with your package manager."));
        }
        system
    }

    fn build(&self) -> Vec<SpaceItem> {
        let mut items = Vec::new();
        let mut apps = Vec::new();
        self.home(&mut items, &mut apps);
        let mut system_measured = 0;
        if cfg!(windows) {
            self.windows_root(&mut items, apps);
        } else {
            system_measured = self.linux_root(&mut items);
            // The Drive folder is a separate (FUSE) filesystem on Linux; its downloaded files are
            // in the content cache, taken out of "Caches" above.
            let mut drive = bucket(
                "Drive",
                self.ctx.drive_local_bytes,
                SpaceCategory::DriveFolder,
                None,
            );
            drive.path = display(&self.ctx.drive_folder);
            self.drive_item(&mut drive);
            items.push(drive);
        }
        items.retain(|i| i.bytes >= MIN_ITEM || i.action.is_some());

        // Everything measured but too small to list on its own.
        let listed: u64 = items.iter().map(|i| i.bytes).sum();
        let measured = self.s.tree.bytes.saturating_sub(system_measured);
        let smaller = measured.saturating_sub(listed);
        if smaller > 0 {
            items.push(bucket(
                "Smaller folders",
                smaller,
                SpaceCategory::Other,
                Some("Folders and files too small to list on their own.".into()),
            ));
        }
        // Everything not measured: the system itself, and what Drive can't read.
        let used = self.s.total_bytes.saturating_sub(self.s.free_bytes);
        let rest = used.saturating_sub(self.s.tree.bytes) + system_measured;
        if rest > 0 {
            items.push(bucket(
                if cfg!(windows) {
                    "Windows and system files"
                } else {
                    "System files"
                },
                rest,
                SpaceCategory::System,
                Some(if cfg!(windows) {
                    "Windows itself, restore points, and files Drive isn't allowed to read.".into()
                } else {
                    "The system itself and files Drive isn't allowed to read.".into()
                }),
            ));
        }
        items.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.name.cmp(&b.name)));
        items
    }
}

fn node_name(p: &Path, n: &Node) -> String {
    p.file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| n.name.clone())
}

fn is_apps_name(lower: &str) -> bool {
    [
        "games",
        "steamlibrary",
        "xboxgames",
        "epic games",
        "riot games",
        "ea games",
        "gog games",
        "program files",
    ]
    .contains(&lower)
}

fn code_caution(node: &Node) -> Option<String> {
    (node.git_bytes >= MIN_FREE).then(|| {
        format!(
            "Contains code projects ({}). Developer tools read every file, so files only in the cloud would download again when you build. Consider saving space in its other folders instead.",
            gb(node.git_bytes)
        )
    })
}

/// Builds the report's items and "Drive can free up to" total.
pub fn build_items(s: &Scanned, ctx: &Context) -> (Vec<SpaceItem>, u64) {
    let items = Builder { ctx, s }.build();
    let mut can_free = 0;
    for i in &items {
        if counts(i.action) {
            can_free += i.frees_bytes;
        } else {
            can_free += i
                .children
                .iter()
                .filter(|c| counts(c.action))
                .map(|c| c.frees_bytes)
                .sum::<u64>();
        }
    }
    (items, can_free)
}

fn counts(a: Option<SpaceAction>) -> bool {
    matches!(
        a,
        Some(SpaceAction::SaveSpace | SpaceAction::TurnOnSaveSpace | SpaceAction::FreeDriveCache)
    )
}

// ---- running scans ----------------------------------------------------------------------------

#[derive(Default)]
struct State {
    saved: Option<Scanned>,
    scan: SpaceScan,
    progress: Option<Arc<Progress>>,
}

/// Holds the last measurement and runs new ones (one at a time).
pub struct Advisor {
    file: PathBuf,
    state: Mutex<State>,
}

impl Advisor {
    pub fn new(data_dir: &Path) -> Self {
        let file = data_dir.join("space.json");
        let saved: Option<Scanned> = std::fs::read(&file)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok());
        let scan = match &saved {
            Some(s) => SpaceScan {
                state: SpaceScanState::Done,
                scanned_bytes: s.tree.bytes,
                scanned_files: s.tree.files,
                current: None,
                started_ms: Some(s.started_ms),
                finished_ms: Some(s.finished_ms),
                error: None,
            },
            None => SpaceScan::default(),
        };
        Self {
            file,
            state: Mutex::new(State {
                saved,
                scan,
                progress: None,
            }),
        }
    }

    /// No report yet and none being made (a failed first scan isn't retried by itself).
    pub fn needs_first_scan(&self) -> bool {
        let st = self.state.lock().unwrap();
        st.saved.is_none() && st.scan.state == SpaceScanState::Idle
    }

    #[cfg(test)]
    pub fn has_report(&self) -> bool {
        self.state.lock().unwrap().saved.is_some()
    }

    pub fn is_scanning(&self) -> bool {
        self.state.lock().unwrap().scan.state == SpaceScanState::Scanning
    }

    /// The report as of now.
    pub fn report(&self, ctx: &Context) -> SpaceReport {
        let st = self.state.lock().unwrap();
        let mut scan = st.scan.clone();
        if let Some(p) = &st.progress {
            scan.scanned_bytes = p.bytes.load(std::sync::atomic::Ordering::Relaxed);
            scan.scanned_files = p.files.load(std::sync::atomic::Ordering::Relaxed);
            scan.current = Some(p.current.lock().unwrap().clone()).filter(|c| !c.is_empty());
        }
        let (items, can_free_bytes) = match &st.saved {
            Some(s) => build_items(s, ctx),
            None => (vec![], 0),
        };
        let scanned_root = st
            .saved
            .as_ref()
            .map(|s| s.root.clone())
            .or_else(|| scan_root(&ctx.home));
        let saved = SpaceSaved {
            drive_folder_bytes: ctx.drive_cloud_only_bytes,
            backups_bytes: ctx.backups.iter().map(|b| b.cloud_only_bytes).sum(),
        };
        SpaceReport {
            scan,
            disks: disks(scanned_root.as_deref()),
            saved,
            items,
            can_free_bytes,
        }
    }

    /// Marks a scan as started; `None` if one is already running.
    pub fn begin(&self) -> Option<Arc<Progress>> {
        let mut st = self.state.lock().unwrap();
        if st.scan.state == SpaceScanState::Scanning {
            return None;
        }
        let p = Arc::new(Progress::default());
        st.progress = Some(p.clone());
        st.scan = SpaceScan {
            state: SpaceScanState::Scanning,
            started_ms: Some(now_ms()),
            finished_ms: st.scan.finished_ms,
            ..Default::default()
        };
        Some(p)
    }

    pub fn cancel(&self) {
        if let Some(p) = &self.state.lock().unwrap().progress {
            p.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Measures the disk holding `home` (blocking; call from a blocking thread).
    pub fn run(&self, home: &Path, progress: &Progress) {
        let started = now_ms();
        // A bug in the walk must never leave the advisor "scanning" forever.
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| measure(home, progress)))
                .unwrap_or_else(|_| Err(anyhow::anyhow!("an internal error stopped the scan")));
        let mut st = self.state.lock().unwrap();
        st.progress = None;
        match result {
            Ok(Some(s)) => {
                info!(
                    "measured {} in {:.1} s: {} files, {}",
                    s.root.display(),
                    (now_ms() - started) as f64 / 1000.0,
                    s.tree.files,
                    gb(s.tree.bytes)
                );
                if let Ok(bytes) = serde_json::to_vec(&s)
                    && let Err(e) = std::fs::write(&self.file, bytes)
                {
                    warn!("saving the disk report failed: {e}");
                }
                st.scan = SpaceScan {
                    state: SpaceScanState::Done,
                    scanned_bytes: s.tree.bytes,
                    scanned_files: s.tree.files,
                    current: None,
                    started_ms: Some(s.started_ms),
                    finished_ms: Some(s.finished_ms),
                    error: None,
                };
                st.saved = Some(s);
            }
            Ok(None) => {
                // Cancelled: back to the previous report, if any.
                st.scan = match &st.saved {
                    Some(s) => SpaceScan {
                        state: SpaceScanState::Done,
                        scanned_bytes: s.tree.bytes,
                        scanned_files: s.tree.files,
                        current: None,
                        started_ms: Some(s.started_ms),
                        finished_ms: Some(s.finished_ms),
                        error: None,
                    },
                    None => SpaceScan::default(),
                };
            }
            Err(e) => {
                warn!("measuring the disk failed: {e:#}");
                st.scan.state = SpaceScanState::Failed;
                st.scan.current = None;
                st.scan.error = Some(format!("Couldn't read the disk: {e}"));
            }
        }
    }
}

/// The root of the disk (Windows) or filesystem (Linux) holding `home`.
pub fn scan_root(home: &Path) -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let mut comps = home.components();
        let prefix = comps.next()?;
        Some(PathBuf::from(format!(
            "{}\\",
            prefix.as_os_str().to_string_lossy()
        )))
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let dev = std::fs::metadata(home).ok()?.dev();
        let mut cur = home.to_path_buf();
        while let Some(parent) = cur.parent() {
            match std::fs::metadata(parent) {
                Ok(m) if m.dev() == dev => cur = parent.to_path_buf(),
                _ => break,
            }
        }
        Some(cur)
    }
}

fn measure(home: &Path, progress: &Progress) -> anyhow::Result<Option<Scanned>> {
    let started_ms = now_ms();
    let root = scan_root(home).ok_or_else(|| anyhow::anyhow!("no disk for {}", home.display()))?;
    let mut skip = Vec::new();
    if cfg!(windows) {
        skip.push(root.join("Windows"));
    }
    let threads = std::thread::available_parallelism()
        .map(|n| n.get() * 2)
        .unwrap_or(8)
        .clamp(4, 16);
    let opts = scan::Options {
        skip,
        filter: backup::scan::Filter::new(&backup::scan::default_excludes()),
        threads,
    };
    let Some(tree) = scan::scan(&root, &opts, progress)? else {
        return Ok(None);
    };
    let mut root_files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&root) {
        for e in entries.flatten() {
            if let Ok(m) = e.metadata()
                && m.is_file()
                && m.len() >= MIN_ITEM
            {
                root_files.push((e.file_name().to_string_lossy().into_owned(), m.len()));
            }
        }
    }
    let total_bytes = fs4::total_space(&root)?;
    let free_bytes = fs4::available_space(&root)?;
    Ok(Some(Scanned {
        root,
        tree,
        total_bytes,
        free_bytes,
        root_files,
        started_ms,
        finished_ms: now_ms(),
    }))
}

// ---- disks ------------------------------------------------------------------------------------

/// Fixed disks on this device; `scanned` first.
pub fn disks(scanned: Option<&Path>) -> Vec<DiskUsage> {
    let mut out: Vec<DiskUsage> = list_disks()
        .into_iter()
        .filter_map(|(path, name)| {
            let total = fs4::total_space(&path).ok()?;
            let free = fs4::available_space(&path).ok()?;
            (total > 0).then(|| DiskUsage {
                scanned: scanned.is_some_and(|s| same_path(s, &path)),
                path: display(&path),
                name,
                total_bytes: total,
                free_bytes: free,
            })
        })
        .collect();
    out.sort_by_key(|d| !d.scanned);
    out
}

#[cfg(windows)]
fn list_disks() -> Vec<(PathBuf, String)> {
    use windows::Win32::Storage::FileSystem::{
        GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
    };
    use windows::core::PCWSTR;
    const DRIVE_FIXED: u32 = 3;
    let mask = unsafe { GetLogicalDrives() };
    let mut out = Vec::new();
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        let root = format!("{letter}:\\");
        let wide: Vec<u16> = root.encode_utf16().chain(Some(0)).collect();
        if unsafe { GetDriveTypeW(PCWSTR(wide.as_ptr())) } != DRIVE_FIXED {
            continue;
        }
        let mut label = [0u16; 261];
        let ok = unsafe {
            GetVolumeInformationW(
                PCWSTR(wide.as_ptr()),
                Some(&mut label),
                None,
                None,
                None,
                None,
            )
        }
        .is_ok();
        let label = if ok {
            String::from_utf16_lossy(&label[..label.iter().position(|c| *c == 0).unwrap_or(0)])
        } else {
            String::new()
        };
        let label = if label.trim().is_empty() {
            "Local Disk".to_string()
        } else {
            label
        };
        out.push((PathBuf::from(&root), format!("{label} ({letter}:)")));
    }
    out
}

#[cfg(unix)]
fn list_disks() -> Vec<(PathBuf, String)> {
    const REAL: &[&str] = &[
        "ext4", "ext3", "btrfs", "xfs", "f2fs", "vfat", "exfat", "ntfs", "ntfs3", "zfs", "bcachefs",
    ];
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    let mounts = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
    for line in mounts.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        let (dev, mount, fstype) = match f.as_slice() {
            [d, m, t, ..] => (*d, *m, *t),
            _ => continue,
        };
        if !dev.starts_with("/dev/") || dev.starts_with("/dev/loop") || !REAL.contains(&fstype) {
            continue;
        }
        if mount.starts_with("/boot") || mount.starts_with("/snap") || !seen.insert(dev.to_string())
        {
            continue;
        }
        let mount = mount.replace("\\040", " ");
        out.push((PathBuf::from(&mount), mount));
    }
    out
}

#[cfg(test)]
mod tests;
