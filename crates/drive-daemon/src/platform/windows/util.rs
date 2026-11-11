//! Small helpers: paths, file times, placeholder identities and handle flavours.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf, Prefix};
use std::time::{SystemTime, UNIX_EPOCH};

use cloud_filter::placeholder::{PinState, Placeholder};
use drive_core::db::NodeId;
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_PINNED, FILE_ATTRIBUTE_UNPINNED, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FILE_WRITE_ATTRIBUTES,
};

/// Placeholder identities are `DRV1` + the database's tag + the node id. The tag ties them to
/// one state database: placeholders left behind by an older database are recognisably foreign.
const BLOB_MAGIC: &[u8; 4] = b"DRV1";

pub fn encode_blob(tag: u64, id: NodeId) -> Vec<u8> {
    let mut b = Vec::with_capacity(20);
    b.extend_from_slice(BLOB_MAGIC);
    b.extend_from_slice(&tag.to_le_bytes());
    b.extend_from_slice(&id.to_le_bytes());
    b
}

/// What a placeholder identity says about itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobId {
    /// Ours, from this database.
    Node(NodeId),
    /// Some other identity (an older database, another provider, or none at all).
    Foreign,
}

pub fn decode_blob(tag: u64, blob: &[u8]) -> BlobId {
    if blob.len() == 20 && &blob[..4] == BLOB_MAGIC && blob[4..12] == tag.to_le_bytes() {
        BlobId::Node(i64::from_le_bytes(blob[12..20].try_into().unwrap()))
    } else {
        BlobId::Foreign
    }
}

/// Vault path (`a/b.txt`) → absolute path under `root`.
pub fn abs_path(root: &Path, rel: &str) -> PathBuf {
    let mut p = root.to_path_buf();
    for part in rel.split('/').filter(|s| !s.is_empty()) {
        p.push(part);
    }
    p
}

/// Strips the `\\?\` prefix std::fs::canonicalize adds, so paths compare with plain ones.
pub fn strip_verbatim(p: &Path) -> PathBuf {
    let mut comps = p.components();
    match comps.next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::VerbatimDisk(d) => {
                let mut out = PathBuf::from(format!("{}:\\", d as char));
                out.extend(comps.filter(|c| !matches!(c, Component::RootDir)));
                out
            }
            Prefix::VerbatimUNC(server, share) => {
                let mut out = PathBuf::from(r"\\");
                out.push(server);
                out.push(share);
                out.extend(comps.filter(|c| !matches!(c, Component::RootDir)));
                out
            }
            _ => p.to_path_buf(),
        },
        _ => p.to_path_buf(),
    }
}

/// Absolute path → vault path, or `None` if it is outside `root`. Case-insensitive, like NTFS.
pub fn rel_path(root: &Path, abs: &Path) -> Option<String> {
    let abs = strip_verbatim(abs);
    let mut r = root.components();
    let mut a = abs.components();
    loop {
        match (r.next(), a.next()) {
            (None, rest) => {
                let mut parts: Vec<String> = Vec::new();
                if let Some(c) = rest {
                    parts.push(c.as_os_str().to_string_lossy().into_owned());
                }
                parts.extend(a.map(|c| c.as_os_str().to_string_lossy().into_owned()));
                if parts.iter().any(|p| p == ".." || p == ".") {
                    return None;
                }
                return Some(parts.join("/"));
            }
            (Some(x), Some(y)) if eq_ci(x.as_os_str(), y.as_os_str()) => {}
            _ => return None,
        }
    }
}

fn eq_ci(a: &std::ffi::OsStr, b: &std::ffi::OsStr) -> bool {
    a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
}

pub fn parent_and_name(rel: &str) -> (&str, &str) {
    match rel.rfind('/') {
        Some(i) => (&rel[..i], &rel[i + 1..]),
        None => ("", rel),
    }
}

/// Lower-cased key for path comparisons (NTFS is case-insensitive).
pub fn key(rel: &str) -> String {
    rel.to_lowercase()
}

pub fn depth(rel: &str) -> usize {
    if rel.is_empty() {
        0
    } else {
        rel.matches('/').count() + 1
    }
}

// ---- time ------------------------------------------------------------------------------

const FILETIME_UNIX_EPOCH: i64 = 116_444_736_000_000_000;

pub fn filetime_from_ms(ms: i64) -> i64 {
    let ms = if ms <= 0 { now_ms() } else { ms };
    ms.saturating_mul(10_000)
        .saturating_add(FILETIME_UNIX_EPOCH)
}

pub fn ms_from_filetime(ft: u64) -> i64 {
    (ft as i64 - FILETIME_UNIX_EPOCH) / 10_000
}

pub fn mtime_ms(meta: &std::fs::Metadata) -> i64 {
    ms_from_filetime(meta.last_write_time())
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---- handles -----------------------------------------------------------------------------

const SHARE_ALL: u32 = FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0;

/// A metadata-only handle (never hydrates, never blocks other apps). Works for folders too.
pub fn open_meta(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES.0)
        .share_mode(SHARE_ALL)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
        .open(path)
}

/// A handle that may change attributes, pin and in-sync state, without blocking other apps.
pub fn open_attrs(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES.0 | FILE_WRITE_ATTRIBUTES.0)
        .share_mode(SHARE_ALL)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
        .open(path)
}

/// A handle no other app can share: fails with a sharing violation while the file is open
/// anywhere else. Needed to dehydrate.
pub fn open_exclusive(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES.0 | FILE_WRITE_ATTRIBUTES.0)
        .share_mode(0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
        .open(path)
}

/// Read access that keeps other apps from writing while we hold it (and fails if one is).
pub fn open_read_deny_write(path: &Path, write_attrs: bool) -> io::Result<File> {
    let mut access = windows::Win32::Foundation::GENERIC_READ.0;
    if write_attrs {
        access |= FILE_WRITE_ATTRIBUTES.0;
    }
    OpenOptions::new()
        .access_mode(access)
        .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_DELETE.0)
        .open(path)
}

/// Opens for reading while letting other apps keep writing, renaming and deleting.
pub fn open_read_shared(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .access_mode(windows::Win32::Foundation::GENERIC_READ.0)
        .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0)
        .open(path)
}

pub fn is_sharing_violation(e: &io::Error) -> bool {
    // ERROR_SHARING_VIOLATION, ERROR_LOCK_VIOLATION
    matches!(e.raw_os_error(), Some(32) | Some(33))
}

pub fn win_err(e: windows::core::Error) -> io::Error {
    io::Error::from(e)
}

// ---- placeholder state ---------------------------------------------------------------------

/// What we need to know about an item on disk.
#[derive(Debug, Clone)]
pub struct DiskItem {
    pub is_dir: bool,
    pub size: u64,
    pub mtime_ms: i64,
    /// `None` for ordinary files and folders.
    pub ph: Option<PhState>,
}

#[derive(Debug, Clone)]
pub struct PhState {
    pub id: BlobId,
    pub in_sync: bool,
    pub pin: PinState,
    /// Some or all of the content is only in the cloud.
    pub partial_on_disk: bool,
}

/// Inspects `path` without hydrating it. `Ok(None)` if it doesn't exist; symlinks and
/// junctions (which we never follow) are reported as `ErrorKind::Unsupported`.
pub fn inspect(path: &Path, tag: u64) -> io::Result<Option<DiskItem>> {
    let lmeta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) if e.raw_os_error() == Some(3) => return Ok(None), // ERROR_PATH_NOT_FOUND
        Err(e) => return Err(e),
    };
    if lmeta.file_type().is_symlink() {
        return Err(io::Error::new(io::ErrorKind::Unsupported, "symbolic link"));
    }
    let file = match open_meta(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound || e.raw_os_error() == Some(3) => {
            return Ok(None);
        }
        Err(e) => return Err(e),
    };
    let meta = file.metadata()?;
    let attrs = meta.file_attributes();
    let mut item = DiskItem {
        is_dir: meta.is_dir(),
        size: meta.len(),
        mtime_ms: mtime_ms(&meta),
        ph: None,
    };
    // Don't trust FILE_ATTRIBUTE_REPARSE_POINT: placeholder compatibility modes can hide it.
    // CfGetPlaceholderInfo answers "not a cloud file" for ordinary items.
    let ph = Placeholder::from(file);
    let info = match ph.info() {
        Ok(Some(i)) => i,
        Ok(None) => return Ok(Some(item)),
        Err(e) => return Err(win_err(e)),
    };
    let state = ph.state().ok().flatten();
    let pin = match info.pin_state() {
        // The attribute is what Explorer sets; trust it if they disagree.
        _ if attrs & FILE_ATTRIBUTE_PINNED.0 != 0 => PinState::Pinned,
        _ if attrs & FILE_ATTRIBUTE_UNPINNED.0 != 0 => PinState::Unpinned,
        p => p,
    };
    item.ph = Some(PhState {
        id: decode_blob(tag, info.blob()),
        in_sync: info.is_in_sync(),
        pin,
        partial_on_disk: !item.is_dir && state.is_some_and(|s| s.partial_on_disk()),
    });
    Ok(Some(item))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blobs_round_trip() {
        let b = encode_blob(7, 42);
        assert_eq!(decode_blob(7, &b), BlobId::Node(42));
        assert_eq!(decode_blob(8, &b), BlobId::Foreign);
        assert_eq!(decode_blob(7, b"other"), BlobId::Foreign);
    }

    #[test]
    fn relative_paths() {
        let root = Path::new(r"C:\Users\me\Drive");
        assert_eq!(
            rel_path(root, Path::new(r"C:\Users\me\Drive\a\b.txt")).as_deref(),
            Some("a/b.txt")
        );
        assert_eq!(
            rel_path(root, Path::new(r"c:\users\ME\drive\a")).as_deref(),
            Some("a")
        );
        assert_eq!(
            rel_path(root, Path::new(r"\\?\C:\Users\me\Drive\x")).as_deref(),
            Some("x")
        );
        assert_eq!(
            rel_path(root, Path::new(r"C:\Users\me\Drive")).as_deref(),
            Some("")
        );
        assert_eq!(rel_path(root, Path::new(r"C:\Users\me\Drive2\x")), None);
        assert_eq!(rel_path(root, Path::new(r"C:\Users\me")), None);
        assert_eq!(
            abs_path(root, "a/b.txt"),
            PathBuf::from(r"C:\Users\me\Drive\a\b.txt")
        );
    }

    #[test]
    fn file_times() {
        assert_eq!(
            ms_from_filetime(filetime_from_ms(1_700_000_000_123) as u64),
            1_700_000_000_123
        );
    }
}
