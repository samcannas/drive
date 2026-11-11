//! Mount point housekeeping and kernel cache invalidation.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::mpsc;

use anyhow::{Result, bail};
use fuser::{INodeNo, Notifier};
use tracing::{debug, info, warn};

const FUSERMOUNT: &str = "fusermount3";

fn fusermount_bin() -> OsString {
    std::env::var_os("FUSERMOUNT_PATH").unwrap_or_else(|| FUSERMOUNT.into())
}

/// Fails early with a helpful message when the FUSE helper is missing.
pub fn check_executable() -> Result<()> {
    match Command::new(fusermount_bin())
        .arg("-V")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            bail!(
                "{FUSERMOUNT} was not found. Install the \"fuse3\" package (e.g. `sudo apt install fuse3`)."
            )
        }
        Err(e) => bail!("running {FUSERMOUNT} failed: {e}"),
    }
}

/// `fusermount3 -u [-z] folder`. Returns whether it succeeded.
pub fn fusermount_unmount(folder: &Path, lazy: bool) -> bool {
    let mut cmd = Command::new(fusermount_bin());
    cmd.arg("-u").arg("-q");
    if lazy {
        cmd.arg("-z");
    }
    match cmd
        .arg("--")
        .arg(folder)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
    {
        Ok(out) if out.status.success() => true,
        Ok(out) => {
            debug!(
                "{FUSERMOUNT} -u{}: {}",
                if lazy { "z" } else { "" },
                String::from_utf8_lossy(&out.stderr).trim()
            );
            false
        }
        Err(e) => {
            warn!("running {FUSERMOUNT} failed: {e}");
            false
        }
    }
}

/// A FUSE mount whose daemon died: every access fails with "Transport endpoint is not
/// connected".
pub fn is_stale_mount(folder: &Path) -> bool {
    matches!(std::fs::metadata(folder), Err(e) if e.raw_os_error() == Some(libc::ENOTCONN))
}

/// Undoes the octal escapes (`\040` for a space, ...) of `/proc/self/mountinfo`.
fn unescape_mountinfo(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))
        {
            out.push((b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0'));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The file system type mounted exactly at `folder` (e.g. `fuse.drive`), if any.
pub fn mounted_fs_type(folder: &Path) -> Option<String> {
    let target = absolute(folder);
    let info = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    let mut found = None;
    for line in info.lines() {
        // id parent major:minor root mount-point options [optional...] - fstype source super
        let Some((left, right)) = line.split_once(" - ") else {
            continue;
        };
        let Some(mp) = left.split(' ').nth(4) else {
            continue;
        };
        if Path::new(&unescape_mountinfo(mp)) == target {
            // Later lines are mounted on top of earlier ones.
            found = right.split(' ').next().map(str::to_string);
        }
    }
    found
}

fn absolute(p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|d| d.join(p))
            .unwrap_or_else(|_| p.to_path_buf())
    }
}

/// Makes sure the folder can be mounted on: cleans up a mount left behind by a crash, and
/// refuses to hide existing files.
pub fn prepare_mount_point(folder: &Path) -> Result<()> {
    if is_stale_mount(folder) || mounted_fs_type(folder).is_some_and(|t| t == "fuse.drive") {
        info!("cleaning up a previous Drive mount at {}", folder.display());
        if !fusermount_unmount(folder, false) {
            fusermount_unmount(folder, true);
        }
    }
    if is_stale_mount(folder) {
        bail!(
            "{} is still mounted by an earlier Drive session. Run `fusermount3 -uz {}` and try again.",
            folder.display(),
            folder.display()
        );
    }
    if let Some(t) = mounted_fs_type(folder) {
        bail!(
            "Something else ({t}) is mounted at {}. Choose another folder for Drive.",
            folder.display()
        );
    }
    match std::fs::metadata(folder) {
        Ok(m) if !m.is_dir() => bail!(
            "{} exists and is not a folder. Choose another location for Drive.",
            folder.display()
        ),
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => std::fs::create_dir_all(folder)?,
        Err(e) => bail!("can't use {}: {e}", folder.display()),
    }
    if std::fs::read_dir(folder)?.next().is_some() {
        bail!(
            "{} already contains files. Drive shows your cloud files in this folder and would hide them: move them \
             somewhere else (you can copy them back in once Drive is running) or choose another folder.",
            folder.display()
        );
    }
    Ok(())
}

enum Inval {
    Entry(INodeNo, String),
    Inode(INodeNo, i64, i64),
    Delete(INodeNo, INodeNo, String),
}

/// Sends kernel cache invalidations from a dedicated thread. They must never be sent from a
/// FUSE request handler: the kernel may hold locks the handler's own request needs, and the
/// notification would wait for them (deadlock).
#[derive(Default)]
pub struct Invalidator {
    tx: Mutex<Option<mpsc::Sender<Inval>>>,
}

impl Invalidator {
    pub fn start(&self, notifier: Notifier) {
        let (tx, rx) = mpsc::channel::<Inval>();
        let spawned = std::thread::Builder::new()
            .name("drive-fuse-inval".into())
            .spawn(move || {
                for msg in rx {
                    let r = match &msg {
                        Inval::Entry(parent, name) => notifier.inval_entry(*parent, name.as_ref()),
                        Inval::Inode(ino, off, len) => notifier.inval_inode(*ino, *off, *len),
                        Inval::Delete(parent, child, name) => notifier
                            .delete(*parent, *child, name.as_ref())
                            .or_else(|_| notifier.inval_entry(*parent, name.as_ref())),
                    };
                    if let Err(e) = r {
                        debug!("kernel cache invalidation failed: {e}");
                    }
                }
            });
        if spawned.is_ok() {
            *self.tx.lock().unwrap() = Some(tx);
        }
    }

    pub fn stop(&self) {
        self.tx.lock().unwrap().take();
    }

    fn send(&self, m: Inval) {
        if let Some(tx) = self.tx.lock().unwrap().as_ref() {
            let _ = tx.send(m);
        }
    }

    /// Forget a cached name (it was added, moved or removed).
    pub fn entry(&self, parent: INodeNo, name: &str) {
        self.send(Inval::Entry(parent, name.to_string()));
    }

    /// Forget a file's cached attributes and contents.
    pub fn inode(&self, ino: INodeNo) {
        self.send(Inval::Inode(ino, 0, 0));
    }

    /// Forget a file's cached attributes only.
    pub fn inode_attrs(&self, ino: INodeNo) {
        self.send(Inval::Inode(ino, -1, 0));
    }

    /// An item disappeared (also tells inotify watchers).
    pub fn delete(&self, parent: INodeNo, child: INodeNo, name: &str) {
        self.send(Inval::Delete(parent, child, name.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mountinfo_escapes() {
        assert_eq!(
            unescape_mountinfo(r"/home/me/My\040Drive"),
            "/home/me/My Drive"
        );
        assert_eq!(unescape_mountinfo(r"/a\134b"), r"/a\b");
        assert_eq!(unescape_mountinfo("/plain"), "/plain");
    }

    #[test]
    fn refuses_non_empty_folders() {
        let dir = tempfile::tempdir().unwrap();
        let mp = dir.path().join("Drive");
        prepare_mount_point(&mp).unwrap();
        assert!(mp.is_dir());
        std::fs::write(mp.join("mine.txt"), b"x").unwrap();
        let err = prepare_mount_point(&mp).unwrap_err().to_string();
        assert!(err.contains("already contains files"), "{err}");
        let file = dir.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        assert!(prepare_mount_point(&file).is_err());
    }
}
