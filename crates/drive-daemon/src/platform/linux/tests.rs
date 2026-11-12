//! End-to-end tests of the FUSE mount: a real engine over an encrypted in-memory vault,
//! mounted in a temporary directory. Skipped (with a message) where FUSE isn't available.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use drive_core::crypt::Cipher;
use drive_core::db::Db;
use drive_core::engine::{Engine, EngineConfig};
use drive_core::remote::crypt::CryptRemote;
use drive_core::remote::mem::{MemRemote, ROOT};
use drive_core::remote::{Kind, Remote, RemoteEntry, UploadBody, read_all};

use crate::platform::{Platform, PlatformContext};

fn fuse_available() -> bool {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/fuse")
        .is_ok()
        && std::process::Command::new("fusermount3")
            .arg("-V")
            .output()
            .is_ok()
}

macro_rules! require_fuse {
    () => {
        if !fuse_available() {
            eprintln!("skipping: FUSE is not available here (/dev/fuse or fusermount3 missing)");
            return;
        }
    };
}

fn pseudo_random(len: usize, seed: u32) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2654435761).wrapping_add(1);
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x >> 8) as u8
        })
        .collect()
}

fn wait_until(what: &str, timeout: Duration, mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {what}");
}

fn getx(p: &Path, name: &str) -> Option<String> {
    let cp = CString::new(p.as_os_str().as_bytes()).unwrap();
    let cn = CString::new(name).unwrap();
    let mut buf = [0u8; 256];
    // SAFETY: valid strings and buffer.
    let n = unsafe { libc::getxattr(cp.as_ptr(), cn.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
    (n >= 0).then(|| String::from_utf8_lossy(&buf[..n as usize]).into_owned())
}

fn setx(p: &Path, name: &str, value: &str) -> io::Result<()> {
    let cp = CString::new(p.as_os_str().as_bytes()).unwrap();
    let cn = CString::new(name).unwrap();
    // SAFETY: valid strings and buffer.
    let r = unsafe {
        libc::setxattr(
            cp.as_ptr(),
            cn.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
        )
    };
    if r == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

struct Harness {
    mem: Arc<MemRemote>,
    remote: Arc<CryptRemote<MemRemote>>,
    engine: Arc<Engine>,
    platform: Option<Box<dyn Platform>>,
    mnt: PathBuf,
    cache: PathBuf,
    _dir: tempfile::TempDir,
    rt: tokio::runtime::Runtime,
}

impl Harness {
    /// Seeds the vault with `files` (path -> content; a trailing `/` makes a folder), then
    /// mounts it and runs a first scan.
    fn new(files: &[(&str, &[u8])]) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap();
        let mem = Arc::new(MemRemote::new());
        let remote = Arc::new(CryptRemote::new(
            mem.clone(),
            Cipher::new("pw", "salt").unwrap(),
        ));
        let dir = tempfile::tempdir().unwrap();
        let mnt = dir.path().join("Drive");
        let cache = dir.path().join("cache");
        let h = rt.block_on(async {
            let mut folders: BTreeMap<String, String> = BTreeMap::new();
            folders.insert(String::new(), ROOT.to_string());
            for (path, data) in files {
                let (parent, name) = match path.trim_end_matches('/').rsplit_once('/') {
                    Some((p, n)) => (p.to_string(), n.to_string()),
                    None => (String::new(), path.trim_end_matches('/').to_string()),
                };
                let pid = folders[&parent].clone();
                if path.ends_with('/') {
                    let e = remote.create_folder(&pid, &name).await.unwrap();
                    folders.insert(path.trim_end_matches('/').to_string(), e.id);
                } else {
                    remote
                        .upload(
                            &pid,
                            &name,
                            UploadBody::from_bytes(data.to_vec(), 1_700_000_000_000),
                        )
                        .await
                        .unwrap();
                }
            }
            let db = Arc::new(Db::open_in_memory().unwrap());
            let cfg = EngineConfig {
                device_name: "test".into(),
                debounce: Duration::ZERO,
                poll_interval: Duration::from_secs(3600),
                folder: mnt.clone(),
                min_free_bytes: 0,
                ..Default::default()
            };
            let engine = Engine::new(db, remote.clone() as Arc<dyn Remote>, cfg).unwrap();
            let platform = super::prepare(PlatformContext {
                show_in_explorer: true,
                engine: engine.clone(),
                folder: mnt.clone(),
                cache_dir: cache.clone(),
                display_name: "Drive".into(),
                account: "test".into(),
            })
            .unwrap();
            engine.set_local(platform.local_side());
            engine.start();
            platform.start().unwrap();
            engine.scan_once().await.unwrap();
            (engine, platform)
        });
        Harness {
            mem,
            remote,
            engine: h.0,
            platform: Some(h.1),
            mnt,
            cache,
            _dir: dir,
            rt,
        }
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.mnt.join(rel)
    }

    fn node(&self, rel: &str) -> drive_core::db::Node {
        self.engine
            .node_by_path(rel)
            .unwrap()
            .unwrap_or_else(|| panic!("no node for {rel}"))
    }

    fn cache_file(&self, rel: &str) -> PathBuf {
        self.cache.join(self.node(rel).id.to_string())
    }

    /// Waits until every local change is in the cloud.
    fn idle(&self) {
        let engine = self.engine.clone();
        // Not `sync_now()`: that also starts remote scans, and a scan that overlaps the end of
        // an upload is an engine race (it can turn the new file into a conflicted copy). The
        // worker wakes up by itself when the mount reports a change.
        wait_until("sync to finish", Duration::from_secs(40), || {
            std::thread::sleep(Duration::from_millis(20));
            let st = engine.status();
            engine.db().pending_count().unwrap() == 0
                && engine.db().orphan_ids().unwrap().is_empty()
                && st.uploading == 0
                && st.downloading == 0
        });
    }

    fn scan(&self) {
        self.rt.block_on(self.engine.scan_once()).unwrap();
    }

    /// The vault's decrypted contents: path -> bytes (folders end with `/`).
    fn cloud(&self) -> BTreeMap<String, Vec<u8>> {
        self.rt.block_on(async {
            let tree = self.remote.scan().await.unwrap();
            let by_id: std::collections::HashMap<String, RemoteEntry> = tree
                .entries
                .iter()
                .map(|e| (e.id.clone(), e.clone()))
                .collect();
            let path_of = |e: &RemoteEntry| {
                let mut parts = vec![e.name.clone()];
                let mut cur = e.parent_id.clone();
                while cur != tree.root_id {
                    let p = &by_id[&cur];
                    parts.push(p.name.clone());
                    cur = p.parent_id.clone();
                }
                parts.reverse();
                parts.join("/")
            };
            let mut out = BTreeMap::new();
            for e in &tree.entries {
                if e.kind == Kind::Folder {
                    out.insert(format!("{}/", path_of(e)), vec![]);
                } else {
                    let data = read_all(self.remote.read(e, 0, None).await.unwrap())
                        .await
                        .unwrap();
                    out.insert(path_of(e), data);
                }
            }
            out
        })
    }

    fn cloud_file(&self, path: &str) -> Vec<u8> {
        self.cloud()
            .get(path)
            .cloned()
            .unwrap_or_else(|| self.missing(path))
    }

    fn missing(&self, path: &str) -> ! {
        panic!(
            "{path} is not in the cloud: {:?}
nodes: {:#?}",
            self.cloud().keys().collect::<Vec<_>>(),
            self.engine.db().all_nodes().unwrap()
        )
    }

    fn remote_entry(&self, path: &str) -> RemoteEntry {
        let n = self.node(path);
        self.rt
            .block_on(self.remote.scan())
            .unwrap()
            .entries
            .into_iter()
            .find(|e| Some(&e.id) == n.remote_id.as_ref())
            .unwrap()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(p) = self.platform.take() {
            p.stop();
        }
        self.engine.shutdown();
    }
}

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

#[test]
fn lists_and_reads_cloud_files() {
    require_fuse!();
    let big = pseudo_random(20 * 1024 * 1024 + 123, 7);
    let h = Harness::new(&[
        ("hello.txt", b"hello world"),
        ("Docs/", b""),
        ("Docs/big.bin", &big),
        ("Docs/empty.txt", b""),
    ]);

    assert_eq!(names(&h.mnt), ["Docs", "hello.txt"]);
    assert_eq!(names(&h.p("Docs")), ["big.bin", "empty.txt"]);
    let m = std::fs::metadata(h.p("Docs/big.bin")).unwrap();
    assert!(m.is_file());
    assert_eq!(m.len(), big.len() as u64);
    assert_eq!(m.blocks(), 0, "cloud-only files take no space");
    assert_eq!(m.mtime(), 1_700_000_000);
    assert!(std::fs::metadata(h.p("Docs")).unwrap().is_dir());
    assert_eq!(
        getx(&h.p("hello.txt"), "user.drive.status").as_deref(),
        Some("cloud-only")
    );
    assert!(std::fs::metadata(h.p("missing")).is_err());
    assert_eq!(
        std::fs::metadata(h.p("HELLO.txt")).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );

    // cat downloads the file and keeps it.
    assert_eq!(std::fs::read(h.p("hello.txt")).unwrap(), b"hello world");
    wait_until("hydration", Duration::from_secs(5), || {
        h.node("hello.txt").hydrated
    });
    assert_eq!(
        std::fs::read(h.cache_file("hello.txt")).unwrap(),
        b"hello world"
    );
    wait_until("st_blocks", Duration::from_secs(5), || {
        std::fs::metadata(h.p("hello.txt")).unwrap().blocks() > 0
    });
    assert_eq!(
        getx(&h.p("hello.txt"), "user.drive.status").as_deref(),
        Some("downloaded")
    );
    assert_eq!(std::fs::read(h.p("Docs/empty.txt")).unwrap(), b"");

    // A read far into a cloud-only file is served directly; nearby reads wait for the download.
    let mut f = std::fs::File::open(h.p("Docs/big.bin")).unwrap();
    let mut buf = vec![0u8; 10_000];
    f.seek(SeekFrom::Start(15 * 1024 * 1024)).unwrap();
    f.read_exact(&mut buf).unwrap();
    assert_eq!(buf, big[15 * 1024 * 1024..15 * 1024 * 1024 + 10_000]);
    f.seek(SeekFrom::Start(4096)).unwrap();
    f.read_exact(&mut buf).unwrap();
    assert_eq!(buf, big[4096..4096 + 10_000]);
    // The tail, past the end.
    f.seek(SeekFrom::Start(big.len() as u64 - 100)).unwrap();
    let mut tail = vec![];
    f.read_to_end(&mut tail).unwrap();
    assert_eq!(tail, big[big.len() - 100..]);
    drop(f);

    // dd with skip/count.
    let out = std::process::Command::new("dd")
        .arg(format!("if={}", h.p("Docs/big.bin").display()))
        .args(["bs=4096", "skip=1000", "count=3", "status=none"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(out.stdout, big[1000 * 4096..1003 * 4096]);

    assert!(std::fs::read(h.p("Docs/big.bin")).unwrap() == big);
    wait_until("big file hydration", Duration::from_secs(10), || {
        h.node("Docs/big.bin").hydrated
    });
    assert_eq!(
        std::fs::metadata(h.cache_file("Docs/big.bin"))
            .unwrap()
            .len(),
        big.len() as u64
    );
    assert!(h.cloud().contains_key("Docs/big.bin"));
}

#[test]
fn local_changes_reach_the_cloud() {
    require_fuse!();
    let h = Harness::new(&[
        ("cloud.txt", b"from the cloud"),
        ("append.txt", b"abc"),
        ("a.txt", b"case"),
    ]);

    // New file.
    std::fs::write(h.p("new.txt"), b"brand new").unwrap();
    assert_eq!(std::fs::read(h.p("new.txt")).unwrap(), b"brand new");
    h.idle();
    assert_eq!(h.cloud_file("new.txt"), b"brand new");
    assert_eq!(
        getx(&h.p("new.txt"), "user.drive.status").as_deref(),
        Some("downloaded")
    );

    // Append to a cloud-only file (downloads it first).
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(h.p("append.txt"))
        .unwrap();
    f.write_all(b"def").unwrap();
    drop(f);
    assert_eq!(std::fs::read(h.p("append.txt")).unwrap(), b"abcdef");
    h.idle();
    assert_eq!(h.cloud_file("append.txt"), b"abcdef");

    // Overwrite a cloud-only file without reading it (O_TRUNC).
    std::fs::write(h.p("cloud.txt"), b"replaced").unwrap();
    h.idle();
    assert_eq!(h.cloud_file("cloud.txt"), b"replaced");

    // Truncate.
    std::fs::OpenOptions::new()
        .write(true)
        .open(h.p("new.txt"))
        .unwrap()
        .set_len(5)
        .unwrap();
    assert_eq!(std::fs::metadata(h.p("new.txt")).unwrap().len(), 5);
    h.idle();
    assert_eq!(h.cloud_file("new.txt"), b"brand");

    // Folders, nested files, renames.
    std::fs::create_dir(h.p("Work")).unwrap();
    assert_eq!(
        std::fs::create_dir(h.p("Work")).unwrap_err().kind(),
        io::ErrorKind::AlreadyExists
    );
    std::fs::write(h.p("Work/notes.md"), b"# notes").unwrap();
    std::fs::rename(h.p("new.txt"), h.p("Work/renamed.txt")).unwrap();
    assert!(!h.p("new.txt").exists());
    h.idle();
    let cloud = h.cloud();
    assert_eq!(
        cloud
            .get("Work/notes.md")
            .cloned()
            .unwrap_or_else(|| h.missing("Work/notes.md")),
        b"# notes"
    );
    assert_eq!(
        cloud
            .get("Work/renamed.txt")
            .cloned()
            .unwrap_or_else(|| h.missing("Work/renamed.txt")),
        b"brand"
    );
    assert!(!cloud.contains_key("new.txt"));

    // Case-only rename.
    std::fs::rename(h.p("a.txt"), h.p("A.txt")).unwrap();
    assert_eq!(
        names(&h.mnt)
            .iter()
            .filter(|n| n.eq_ignore_ascii_case("a.txt"))
            .collect::<Vec<_>>(),
        ["A.txt"]
    );
    h.idle();
    assert_eq!(h.cloud_file("A.txt"), b"case");

    // rename over an existing file replaces it; RENAME_NOREPLACE / EXCHANGE are refused.
    std::fs::write(h.p("Work/other.txt"), b"other").unwrap();
    let c = |p: PathBuf| CString::new(p.as_os_str().as_bytes()).unwrap();
    let (a, b) = (c(h.p("Work/other.txt")), c(h.p("Work/notes.md")));
    // SAFETY: valid paths.
    let r = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            a.as_ptr(),
            libc::AT_FDCWD,
            b.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    assert_eq!(
        (r, io::Error::last_os_error().raw_os_error()),
        (-1, Some(libc::EEXIST))
    );
    let r = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            a.as_ptr(),
            libc::AT_FDCWD,
            b.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    assert_eq!(
        (r, io::Error::last_os_error().raw_os_error()),
        (-1, Some(libc::EINVAL))
    );
    std::fs::rename(h.p("Work/other.txt"), h.p("Work/notes.md")).unwrap();
    assert_eq!(std::fs::read(h.p("Work/notes.md")).unwrap(), b"other");

    // Deleting: rmdir needs an empty folder.
    let e = std::fs::remove_dir(h.p("Work")).unwrap_err();
    assert_eq!(e.raw_os_error(), Some(libc::ENOTEMPTY));
    assert_eq!(
        std::fs::remove_file(h.p("Work"))
            .unwrap_err()
            .raw_os_error(),
        Some(libc::EISDIR)
    );
    std::fs::remove_file(h.p("Work/notes.md")).unwrap();
    std::fs::remove_file(h.p("Work/renamed.txt")).unwrap();
    std::fs::remove_dir(h.p("Work")).unwrap();
    std::fs::remove_file(h.p("cloud.txt")).unwrap();
    h.idle();
    let cloud = h.cloud();
    assert_eq!(
        cloud.keys().cloned().collect::<Vec<_>>(),
        ["A.txt", "append.txt"]
    );
    assert_eq!(names(&h.mnt), ["A.txt", "append.txt"]);

    // Larger writes in pieces, then read back through the mount.
    let data = pseudo_random(3 * 1024 * 1024 + 7, 3);
    let mut f = std::fs::File::create(h.p("big.bin")).unwrap();
    for chunk in data.chunks(100_000) {
        f.write_all(chunk).unwrap();
    }
    drop(f);
    assert!(std::fs::read(h.p("big.bin")).unwrap() == data);
    h.idle();
    assert!(h.cloud_file("big.bin") == data);
}

#[test]
fn eviction_and_pinning() {
    require_fuse!();
    let data = pseudo_random(300_000, 11);
    let h = Harness::new(&[
        ("photo.jpg", &data),
        ("Album/", b""),
        ("Album/one.jpg", b"one"),
        ("Album/two.jpg", b"two"),
    ]);

    assert!(std::fs::read(h.p("photo.jpg")).unwrap() == data);
    wait_until("hydration", Duration::from_secs(5), || {
        h.node("photo.jpg").hydrated
    });
    assert!(h.cache_file("photo.jpg").exists());

    // Free up space: the cached copy goes, the file stays and downloads again on demand.
    // (The kernel sends `release` asynchronously after close(), so allow it a moment.)
    let mut freed = 0;
    for _ in 0..50 {
        freed = h.rt.block_on(h.engine.free_up_space("photo.jpg")).unwrap();
        if freed > 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(freed, data.len() as u64);
    assert!(!h.cache_file("photo.jpg").exists());
    assert!(!h.node("photo.jpg").hydrated);
    wait_until("st_blocks = 0", Duration::from_secs(5), || {
        std::fs::metadata(h.p("photo.jpg")).unwrap().blocks() == 0
    });
    assert_eq!(
        getx(&h.p("photo.jpg"), "user.drive.status").as_deref(),
        Some("cloud-only")
    );
    assert!(std::fs::read(h.p("photo.jpg")).unwrap() == data);

    // An open file isn't evicted.
    let f = std::fs::File::open(h.p("photo.jpg")).unwrap();
    wait_until("hydration", Duration::from_secs(5), || {
        h.node("photo.jpg").hydrated
    });
    assert_eq!(
        h.rt.block_on(h.engine.free_up_space("photo.jpg")).unwrap(),
        0
    );
    drop(f);

    // Pin a folder through the xattr: everything in it downloads.
    assert_eq!(
        getx(&h.p("Album"), "user.drive.pinned").as_deref(),
        Some("0")
    );
    setx(&h.p("Album"), "user.drive.pinned", "1").unwrap();
    wait_until("pinned files to download", Duration::from_secs(10), || {
        h.node("Album/one.jpg").hydrated && h.node("Album/two.jpg").hydrated
    });
    assert_eq!(
        std::fs::read(h.cache_file("Album/two.jpg")).unwrap(),
        b"two"
    );
    assert_eq!(
        getx(&h.p("Album/one.jpg"), "user.drive.pinned").as_deref(),
        Some("1")
    );
    assert_eq!(
        getx(&h.p("Album/one.jpg"), "user.drive.status").as_deref(),
        Some("pinned")
    );
    assert_eq!(
        getx(&h.p("Album"), "user.drive.status").as_deref(),
        Some("pinned")
    );
    assert_eq!(
        setx(&h.p("Album"), "user.drive.status", "x")
            .unwrap_err()
            .raw_os_error(),
        Some(libc::EPERM)
    );
    assert_eq!(
        setx(&h.p("Album"), "user.other", "x")
            .unwrap_err()
            .raw_os_error(),
        Some(libc::ENOTSUP)
    );
    assert_eq!(
        h.rt.block_on(h.engine.free_up_space("Album/one.jpg"))
            .unwrap(),
        3,
        "free up space unpins"
    );
    setx(&h.p("Album"), "user.drive.pinned", "0").unwrap();
    assert!(!h.node("Album/two.jpg").pinned);
}

#[test]
fn network_failures_return_eio() {
    require_fuse!();
    let h = Harness::new(&[("doc.txt", b"remote content"), ("edit.txt", b"edit me")]);
    h.mem.set_offline(true);
    let start = Instant::now();
    let e = std::fs::read(h.p("doc.txt")).unwrap_err();
    assert_eq!(e.raw_os_error(), Some(libc::EIO));
    let e = std::fs::OpenOptions::new()
        .append(true)
        .open(h.p("edit.txt"))
        .unwrap_err();
    assert_eq!(e.raw_os_error(), Some(libc::EIO));
    assert!(start.elapsed() < Duration::from_secs(10), "failed fast");
    // Metadata still works offline, and local writes are kept for later.
    assert_eq!(std::fs::metadata(h.p("doc.txt")).unwrap().len(), 14);
    std::fs::write(h.p("offline.txt"), b"written offline").unwrap();
    assert_eq!(
        std::fs::read(h.p("offline.txt")).unwrap(),
        b"written offline"
    );

    h.mem.set_offline(false);
    assert_eq!(std::fs::read(h.p("doc.txt")).unwrap(), b"remote content");
    h.idle();
    assert_eq!(h.cloud_file("offline.txt"), b"written offline");
}

#[test]
fn remote_changes_show_up() {
    require_fuse!();
    let h = Harness::new(&[
        ("keep.txt", b"v1"),
        ("gone.txt", b"bye"),
        ("move.txt", b"moving"),
    ]);
    assert_eq!(std::fs::read(h.p("keep.txt")).unwrap(), b"v1");
    assert!(std::fs::metadata(h.p("gone.txt")).is_ok());
    assert!(std::fs::metadata(h.p("arrived.txt")).is_err());

    // Another device adds, replaces, moves and deletes.
    let old = h.remote_entry("keep.txt");
    let gone = h.remote_entry("gone.txt");
    let moved = h.remote_entry("move.txt");
    h.rt.block_on(async {
        h.remote
            .upload(
                ROOT,
                "arrived.txt",
                UploadBody::from_bytes(b"hello".to_vec(), 1),
            )
            .await
            .unwrap();
        h.remote
            .upload(
                ROOT,
                "keep.txt",
                UploadBody::from_bytes(b"version 2".to_vec(), 2),
            )
            .await
            .unwrap();
        h.remote.trash(&old).await.unwrap();
        h.remote.trash(&gone).await.unwrap();
        h.remote.rename(&moved, ROOT, "moved.txt").await.unwrap();
    });
    h.scan();
    wait_until("remote changes to show", Duration::from_secs(5), || {
        names(&h.mnt) == ["arrived.txt", "keep.txt", "moved.txt"]
    });
    assert_eq!(std::fs::read(h.p("arrived.txt")).unwrap(), b"hello");
    assert_eq!(std::fs::read(h.p("keep.txt")).unwrap(), b"version 2");
    assert_eq!(std::fs::read(h.p("moved.txt")).unwrap(), b"moving");
    assert!(std::fs::metadata(h.p("gone.txt")).is_err());
    assert!(std::fs::metadata(h.p("move.txt")).is_err());
}

#[test]
fn sign_out_keeps_unsynced_files() {
    require_fuse!();
    let mut h = Harness::new(&[("synced.txt", b"in the cloud")]);
    h.mem.set_offline(true);
    std::fs::create_dir(h.p("Drafts")).unwrap();
    std::fs::write(h.p("Drafts/draft.txt"), b"not uploaded yet").unwrap();
    h.platform.take().unwrap().stop();
    assert!(super::mount::mounted_fs_type(&h.mnt).is_none());
    super::teardown(&h.mnt, &h.cache).unwrap();
    assert!(!h.cache.exists());
    assert_eq!(
        std::fs::read(h.mnt.join("Drafts/draft.txt")).unwrap(),
        b"not uploaded yet"
    );
    assert!(!h.mnt.join("synced.txt").exists());
}

#[test]
fn leftover_mount_is_taken_over() {
    require_fuse!();
    let h = Harness::new(&[("a.txt", b"a")]);
    let folder = h.mnt.clone();
    assert_eq!(
        super::mount::mounted_fs_type(&folder).as_deref(),
        Some("fuse.drive")
    );
    // A new session finds an earlier Drive mount on the folder: it unmounts it and goes on.
    super::mount::prepare_mount_point(&folder).unwrap();
    assert!(super::mount::mounted_fs_type(&folder).is_none());
    assert!(names(&folder).is_empty());
    drop(h);
    assert!(super::mount::mounted_fs_type(&folder).is_none());
}

#[test]
fn unmounts_on_stop() {
    require_fuse!();
    let h = Harness::new(&[("a.txt", b"a")]);
    let folder = h.mnt.clone();
    assert_eq!(names(&folder), ["a.txt"]);
    drop(h);
    assert!(super::mount::mounted_fs_type(&folder).is_none());
}
