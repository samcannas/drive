//! The interface between the engine and the platform's file system integration.
//!
//! * Windows: Cloud Files API placeholders in the sync root (`drive-daemon::windows`).
//! * Linux: a FUSE mount backed by a content cache (`drive-daemon::linux`).
//!
//! The engine calls these methods when the cloud changes; the platform layer calls the
//! engine's `local_*` methods when the user changes something.

use std::io;

use crate::db::NodeId;
use crate::remote::Kind;

/// What the platform layer needs to know about an item. `path` is relative to the root,
/// `/`-separated, and never empty except for the root itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalItem {
    pub id: NodeId,
    pub path: String,
    pub kind: Kind,
    pub size: u64,
    pub mtime_ms: i64,
    pub pinned: bool,
}

/// An open, readable snapshot of local content to upload.
pub struct UploadSource {
    pub file: std::fs::File,
    pub size: u64,
    pub mtime_ms: i64,
}

pub trait LocalSide: Send + Sync + 'static {
    /// Whether `name` can exist on this platform's file system.
    fn valid_name(&self, name: &str) -> bool {
        valid_portable_name(name)
    }

    /// A new item appeared in the cloud: show it (cloud-only) locally.
    fn create(&self, item: &LocalItem) -> io::Result<()>;

    /// A clean file's content changed in the cloud: drop the local bytes and show the new
    /// size/mtime. Never called for files with unsynced local edits.
    fn replace(&self, item: &LocalItem) -> io::Result<()>;

    /// Moves/renames an item from `from` to `item.path` (because the cloud moved it, or to make
    /// room for a conflicting item).
    fn rename(&self, from: &str, item: &LocalItem) -> io::Result<()>;

    /// The item was deleted in the cloud and has no local changes: remove it (recursively).
    fn remove(&self, item: &LocalItem) -> io::Result<()>;

    /// Opens local content for upload. Return `ErrorKind::WouldBlock` if another app is still
    /// writing the file; the engine retries later.
    fn open_for_upload(&self, item: &LocalItem) -> io::Result<UploadSource>;

    /// The upload of `size`/`mtime_ms` finished and the item now exists in the cloud. Mark it as
    /// in sync. Return `Ok(false)` if the local file changed meanwhile (it stays dirty).
    fn uploaded(&self, item: &LocalItem, size: u64, mtime_ms: i64) -> io::Result<bool>;

    /// Drops the local copy of a clean, unpinned file. `Ok(false)` if it can't be evicted now
    /// (for example because it is open).
    fn evict(&self, item: &LocalItem) -> io::Result<bool>;

    /// Downloads the complete file now (for pinned items). Blocking.
    fn hydrate(&self, item: &LocalItem) -> io::Result<()>;

    /// Reflects a pin change in the OS (e.g. the Windows "Always keep on this device" state).
    fn set_pinned(&self, _item: &LocalItem, _pinned: bool) -> io::Result<()> {
        Ok(())
    }
}

/// Names that are safe on every platform Drive supports (Windows rules are the strictest).
pub fn valid_portable_name(name: &str) -> bool {
    if name.is_empty() || name == "." || name == ".." || name.len() > 255 {
        return false;
    }
    if name.chars().any(|c| matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || (c as u32) < 32) {
        return false;
    }
    if name.ends_with(' ') || name.ends_with('.') {
        return false;
    }
    let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    !matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "COM1" | "COM2" | "COM3" | "COM4" | "COM5" | "COM6" | "COM7" | "COM8" | "COM9"
            | "LPT1" | "LPT2" | "LPT3" | "LPT4" | "LPT5" | "LPT6" | "LPT7" | "LPT8" | "LPT9"
    )
}

/// Names only invalid on Unix (`/` and NUL). Used by the Linux layer.
pub fn valid_unix_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/') && !name.contains('\0') && name.len() <= 255
}

/// Temporary/lock files that should never be uploaded.
pub fn is_ignored_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == "desktop.ini"
        || lower == "thumbs.db"
        || lower == ".ds_store"
        || lower.starts_with("~$")
        || lower.starts_with(".~lock.")
        || lower.ends_with(".tmp") && lower.starts_with('~')
        || lower.ends_with(".crdownload")
        || lower.ends_with(".part")
        || lower.ends_with(".partial")
        || lower.starts_with(".goutputstream-")
        || lower.ends_with(".swp") && lower.starts_with('.')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(valid_portable_name("report.pdf"));
        assert!(!valid_portable_name("a:b"));
        assert!(!valid_portable_name("CON.txt"));
        assert!(!valid_portable_name("trailing."));
        assert!(valid_unix_name("a:b"));
        assert!(is_ignored_name("~$Report.docx"));
        assert!(is_ignored_name("video.mp4.crdownload"));
        assert!(!is_ignored_name("notes.txt"));
    }
}
