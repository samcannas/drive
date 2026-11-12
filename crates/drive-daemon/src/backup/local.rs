//! The local side of the backup engine: the backed-up folders themselves, read-only.
//!
//! Paths the engine uses are `<backup name>/<path inside the folder>`. Every method that
//! would change something on this device is a no-op: a backup never moves, changes or deletes
//! the user's files (the mirror planner never asks it to, and these no-ops make sure).

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use drive_core::db::name_key;
use drive_core::engine::local::{LocalItem, LocalSide, UploadSource};

/// Backed-up folders by the name key of their backup name.
pub type Sources = Arc<RwLock<HashMap<String, PathBuf>>>;

pub struct BackupLocal {
    sources: Sources,
}

impl BackupLocal {
    pub fn new(sources: Sources) -> Self {
        Self { sources }
    }

    /// The file on this device for an engine path, or `None` if its backup isn't active.
    pub fn resolve(sources: &Sources, path: &str) -> Option<PathBuf> {
        let path = path.trim_matches('/');
        let (top, rest) = match path.split_once('/') {
            Some((t, r)) => (t, r),
            None => (path, ""),
        };
        let base = sources.read().unwrap().get(&name_key(top))?.clone();
        if rest.is_empty() {
            return Some(base);
        }
        let mut p = base;
        for part in rest.split('/') {
            if part.is_empty() || part == "." || part == ".." {
                return None;
            }
            p.push(part);
        }
        Some(p)
    }

    fn abs(&self, path: &str) -> io::Result<PathBuf> {
        Self::resolve(&self.sources, path).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("{path} is not in a backed-up folder"),
            )
        })
    }
}

fn mtime_ms(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(windows)]
fn open_for_reading(path: &std::path::Path) -> io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    const SHARE_READ: u32 = 1;
    const SHARE_WRITE: u32 = 2;
    const SHARE_DELETE: u32 = 4;
    // Probe with writers denied: an app still writing the file (a download in progress, an
    // editor saving) makes this fail, and the engine retries later. The probe is closed at once
    // so the upload itself never stops an app from saving; a save during the upload is noticed
    // by `uploaded` and uploaded again.
    match std::fs::OpenOptions::new()
        .read(true)
        .share_mode(SHARE_READ | SHARE_DELETE)
        .open(path)
    {
        Ok(f) => drop(f),
        Err(e) if matches!(e.raw_os_error(), Some(32) | Some(33)) => {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!("{} is open in another app", path.display()),
            ));
        }
        Err(e) => return Err(e),
    }
    std::fs::OpenOptions::new()
        .read(true)
        .share_mode(SHARE_READ | SHARE_WRITE | SHARE_DELETE)
        .open(path)
}

#[cfg(not(windows))]
fn open_for_reading(path: &std::path::Path) -> io::Result<std::fs::File> {
    std::fs::File::open(path)
}

impl LocalSide for BackupLocal {
    fn valid_name(&self, _name: &str) -> bool {
        true // nothing is ever created here
    }

    fn create(&self, _item: &LocalItem) -> io::Result<()> {
        Ok(())
    }

    fn replace(&self, _item: &LocalItem) -> io::Result<()> {
        Ok(())
    }

    fn rename(&self, _from: &str, _item: &LocalItem) -> io::Result<()> {
        Ok(())
    }

    fn remove(&self, _item: &LocalItem) -> io::Result<()> {
        Ok(())
    }

    fn open_for_upload(&self, item: &LocalItem) -> io::Result<UploadSource> {
        let abs = self.abs(&item.path)?;
        let file = open_for_reading(&abs)?;
        let meta = file.metadata()?;
        if !meta.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not a file", abs.display()),
            ));
        }
        Ok(UploadSource {
            size: meta.len(),
            mtime_ms: mtime_ms(&meta),
            file,
        })
    }

    fn uploaded(&self, item: &LocalItem, size: u64, mtime_ms_uploaded: i64) -> io::Result<bool> {
        let abs = self.abs(&item.path)?;
        match std::fs::metadata(&abs) {
            Ok(meta) => Ok(meta.len() == size && mtime_ms(&meta) == mtime_ms_uploaded),
            // Gone meanwhile: the deletion is reported separately.
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(true),
            Err(e) => Err(e),
        }
    }

    fn evict(&self, _item: &LocalItem) -> io::Result<bool> {
        Ok(false) // the user's own files are never removed
    }

    fn hydrate(&self, _item: &LocalItem) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_inside_sources_only() {
        let sources: Sources = Arc::new(RwLock::new(HashMap::new()));
        sources
            .write()
            .unwrap()
            .insert(name_key("Documents"), PathBuf::from("/home/me/Documents"));
        assert_eq!(
            BackupLocal::resolve(&sources, "documents/a/b.txt"),
            Some(PathBuf::from("/home/me/Documents/a/b.txt"))
        );
        assert_eq!(
            BackupLocal::resolve(&sources, "Documents"),
            Some(PathBuf::from("/home/me/Documents"))
        );
        assert_eq!(BackupLocal::resolve(&sources, "Documents/../x"), None);
        assert_eq!(BackupLocal::resolve(&sources, "Videos/x"), None);
    }
}
