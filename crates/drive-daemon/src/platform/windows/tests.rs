//! End-to-end tests against real Cloud Files sync roots.
//!
//! Each test registers a sync root under `target/cfapi-tests` (WinRT rejects roots under
//! `%TEMP%`) with an engine over an encrypted in-memory vault, and removes it again on drop,
//! even when the test fails.
//!
//! Windows doesn't send rename/delete callbacks for the provider's own process, so "user"
//! actions run in child processes (`cmd`, `attrib`), like Explorer would.

use std::collections::BTreeMap;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cloud_filter::placeholder::PinState;
use drive_core::crypt::Cipher;
use drive_core::db::Db;
use drive_core::engine::plan::VAULT_MARKER;
use drive_core::engine::{Engine, EngineConfig};
use drive_core::remote::crypt::CryptRemote;
use drive_core::remote::mem::{MemRemote, ROOT};
use drive_core::remote::{Kind, Remote, UploadBody, read_all};

use super::util::{DiskItem, inspect};
use super::{NOT_UPLOADED_FOLDER, register};
use crate::platform::{Platform, PlatformContext};

const WAIT: Duration = Duration::from_secs(30);

fn init_logging() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("DRIVE_TEST_LOG")
                .unwrap_or_else(|_| "warn".into()),
        )
        .with_test_writer()
        .try_init();
}

struct Fixture {
    base: PathBuf,
    folder: PathBuf,
    db_path: PathBuf,
    account: String,
    mem: Arc<MemRemote>,
    remote: Arc<dyn Remote>,
    running: Option<(Arc<Engine>, Box<dyn Platform>)>,
}

/// Removes test sync roots left behind by earlier test processes that were killed before
/// their `Drop` ran (they would otherwise linger in Explorer's navigation pane).
fn sweep_stale_test_roots() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let Ok(roots) = cloud_filter::root::active_roots() else {
            return;
        };
        for info in roots {
            let id = info.id();
            let id_str = id.to_os_string().to_string_lossy().into_owned();
            // Ids look like `Drive!<sid>!test-<name>-<pid>-<rand>`.
            let Some(rest) = id_str
                .split("!test-")
                .nth(1)
                .filter(|_| id_str.starts_with("Drive!"))
            else {
                continue;
            };
            let pid = rest.rsplit('-').nth(1).and_then(|p| p.parse::<u32>().ok());
            let alive = pid.is_some_and(|p| p == std::process::id() || process_running(p));
            if !alive {
                let _ = id.unregister();
            }
        }
    });
}

fn process_running(pid: u32) -> bool {
    Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
        .unwrap_or(true)
}

impl Fixture {
    fn new(name: &str) -> Self {
        init_logging();
        sweep_stale_test_roots();
        let unique = format!("{name}-{}-{}", std::process::id(), rand::random::<u32>());
        let base = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/cfapi-tests")
            .join(&unique);
        std::fs::create_dir_all(&base).unwrap();
        let base = super::util::strip_verbatim(&std::fs::canonicalize(&base).unwrap());
        let mem = Arc::new(MemRemote::new());
        let remote: Arc<dyn Remote> = Arc::new(CryptRemote::new(
            mem.clone(),
            Cipher::new("pw", "salt").unwrap(),
        ));
        Self {
            folder: base.join("Drive"),
            db_path: base.join("state.db"),
            base,
            account: format!("test-{unique}"),
            mem,
            remote,
            running: None,
        }
    }

    fn engine(&self) -> &Arc<Engine> {
        &self.running.as_ref().expect("running").0
    }

    /// Starts an engine and the Windows integration, the way the service does.
    fn start(&mut self) {
        let db = Arc::new(Db::open(&self.db_path).unwrap());
        let cfg = EngineConfig {
            device_name: "Test PC".into(),
            folder: self.folder.clone(),
            debounce: Duration::from_millis(200),
            poll_interval: Duration::from_secs(3600),
            min_free_bytes: 0,
            ..Default::default()
        };
        let engine = Engine::new(db, self.remote.clone(), cfg).unwrap();
        let platform = super::prepare(PlatformContext {
            engine: engine.clone(),
            folder: self.folder.clone(),
            cache_dir: self.base.join("cache"),
            display_name: "Drive (test)".into(),
            account: self.account.clone(),
        })
        .unwrap();
        engine.set_local(platform.local_side());
        engine.start();
        platform.start().unwrap();
        self.running = Some((engine, platform));
    }

    fn stop(&mut self) {
        if let Some((engine, platform)) = self.running.take() {
            platform.stop();
            engine.shutdown();
        }
    }

    fn path(&self, rel: &str) -> PathBuf {
        super::util::abs_path(&self.folder, rel)
    }

    fn disk(&self, rel: &str) -> Option<DiskItem> {
        inspect(&self.path(rel), 0).unwrap()
    }

    /// Runs a command line in `cmd.exe` inside the Drive folder (a separate process, like a user).
    fn cmd(&self, line: &str) {
        let out = Command::new("cmd")
            .arg("/D")
            .arg("/C")
            .raw_arg(line)
            .current_dir(&self.folder)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "`{line}` failed: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Decrypted cloud contents: `path -> bytes` (folders end with `/`).
    async fn cloud(&self) -> BTreeMap<String, Vec<u8>> {
        // A version can be retired between listing and reading it: take a fresh snapshot then.
        for _ in 0..20 {
            if let Some(c) = self.try_cloud().await {
                return c;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the cloud kept changing while taking a snapshot");
    }

    async fn try_cloud(&self) -> Option<BTreeMap<String, Vec<u8>>> {
        let tree = self.remote.scan().await.unwrap();
        let by_id: std::collections::HashMap<_, _> =
            tree.entries.iter().map(|e| (e.id.clone(), e)).collect();
        let path_of = |id: &str| {
            let mut parts = vec![];
            let mut cur = id.to_string();
            while cur != tree.root_id {
                let e = by_id[&cur];
                parts.push(e.name.clone());
                cur = e.parent_id.clone();
            }
            parts.reverse();
            parts.join("/")
        };
        let mut out = BTreeMap::new();
        for e in &tree.entries {
            let p = path_of(&e.id);
            if p == VAULT_MARKER {
                continue;
            }
            if e.kind == Kind::Folder {
                out.insert(format!("{p}/"), vec![]);
            } else {
                out.insert(
                    p,
                    read_all(self.remote.read(e, 0, None).await.ok()?)
                        .await
                        .ok()?,
                );
            }
        }
        Some(out)
    }

    /// Puts a file into the cloud (as another device would) and lets the engine see it.
    async fn put_cloud(&self, rel: &str, data: &[u8]) {
        let (parent, name) = super::util::parent_and_name(rel);
        let mut parent_id = ROOT.to_string();
        let tree = self.remote.scan().await.unwrap();
        for part in parent.split('/').filter(|p| !p.is_empty()) {
            parent_id = match tree
                .entries
                .iter()
                .find(|e| e.parent_id == parent_id && e.name == part)
            {
                Some(e) => e.id.clone(),
                None => {
                    self.remote
                        .create_folder(&parent_id, part)
                        .await
                        .unwrap()
                        .id
                }
            };
        }
        // Replacing: upload the new version, then retire the old one (what the engine does).
        let old = tree
            .entries
            .iter()
            .find(|e| e.parent_id == parent_id && e.name == name)
            .cloned();
        self.remote
            .upload(
                &parent_id,
                name,
                UploadBody::from_bytes(data.to_vec(), drive_core::db::now_ms()),
            )
            .await
            .unwrap();
        if let Some(old) = old {
            self.remote.trash(&old).await.unwrap();
        }
        self.engine().scan_once().await.unwrap();
    }

    fn node(&self, rel: &str) -> Option<drive_core::db::Node> {
        self.engine().node_by_path(rel).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop();
        register::unregister(&self.folder);
        // The engine's tasks let go of the database shortly after shutdown.
        for _ in 0..50 {
            if std::fs::remove_dir_all(&self.base).is_ok() || !self.base.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

async fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while start.elapsed() < WAIT {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {what}");
}

async fn wait_cloud(fx: &Fixture, what: &str, f: impl Fn(&BTreeMap<String, Vec<u8>>) -> bool) {
    let start = Instant::now();
    loop {
        let c = fx.cloud().await;
        if f(&c) {
            return;
        }
        if start.elapsed() > WAIT {
            let shown: Vec<String> = c
                .iter()
                .map(|(k, v)| {
                    if v.len() < 100 {
                        format!("{k}={:?}", String::from_utf8_lossy(v))
                    } else {
                        format!("{k}=<{} bytes>", v.len())
                    }
                })
                .collect();
            panic!("timed out waiting for {what}; cloud has {shown:?}");
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

fn cloud_only(d: &Option<DiskItem>) -> bool {
    d.as_ref()
        .and_then(|d| d.ph.as_ref())
        .is_some_and(|p| p.in_sync && p.partial_on_disk)
}

fn hydrated_in_sync(d: &Option<DiskItem>) -> bool {
    d.as_ref()
        .and_then(|d| d.ph.as_ref())
        .is_some_and(|p| p.in_sync && !p.partial_on_disk)
}

fn pin(d: &Option<DiskItem>) -> Option<PinState> {
    d.as_ref().and_then(|d| d.ph.as_ref()).map(|p| p.pin)
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| ((i * 31 + seed as usize) % 251) as u8)
        .collect()
}

async fn read_blocking(path: PathBuf) -> std::io::Result<Vec<u8>> {
    tokio::task::spawn_blocking(move || std::fs::read(path))
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cloud_files_show_up_and_download_on_demand() {
    let mut fx = Fixture::new("cloud");
    fx.start();
    assert!(register::is_sync_root(&fx.folder));
    let big = pattern(3 * 1024 * 1024 + 17, 1);
    fx.put_cloud("hello.txt", b"hello world").await;
    fx.put_cloud("big.bin", &big).await;
    fx.put_cloud("Docs/inner.txt", b"inside").await;

    // Cloud-only placeholders with the right size.
    let d = fx.disk("big.bin");
    assert!(cloud_only(&d), "{d:?}");
    assert_eq!(d.unwrap().size, big.len() as u64);
    assert!(fx.disk("Docs").is_some_and(|d| d.is_dir));
    assert!(cloud_only(&fx.disk("Docs/inner.txt")));
    assert!(!fx.node("big.bin").unwrap().hydrated);

    // Another app copies the file out: it is downloaded on demand.
    let out = fx.base.join("big.copy");
    fx.cmd(&format!("copy /b big.bin \"{}\"", out.display()));
    assert_eq!(std::fs::read(&out).unwrap(), big);
    wait_for("big.bin to count as downloaded", || {
        fx.node("big.bin").unwrap().hydrated
    })
    .await;
    assert!(hydrated_in_sync(&fx.disk("big.bin")));
    // And `type` in a prompt.
    let typed = Command::new("cmd")
        .args(["/D", "/C", "type", "hello.txt"])
        .current_dir(&fx.folder)
        .output()
        .unwrap();
    assert_eq!(typed.stdout, b"hello world");

    // Explorer's "Always keep on this device" (attrib +P) downloads and pins.
    fx.cmd(r"attrib +P Docs\inner.txt");
    wait_for("inner.txt pinned and downloaded", || {
        fx.node("Docs/inner.txt")
            .is_some_and(|n| n.pinned && n.hydrated)
            && hydrated_in_sync(&fx.disk("Docs/inner.txt"))
    })
    .await;
    assert_eq!(std::fs::read(fx.path("Docs/inner.txt")).unwrap(), b"inside");

    // Explorer's "Free up space" (attrib +U) drops the local copy.
    fx.cmd("attrib +U big.bin");
    wait_for("big.bin freed up", || {
        cloud_only(&fx.disk("big.bin")) && !fx.node("big.bin").unwrap().hydrated
    })
    .await;
    assert_eq!(pin(&fx.disk("big.bin")), Some(PinState::Unspecified));

    // The same through the engine (`drive pin` / `drive free`).
    fx.engine().set_pinned("hello.txt", true).unwrap();
    wait_for("hello.txt pinned", || {
        hydrated_in_sync(&fx.disk("hello.txt")) && fx.node("hello.txt").unwrap().hydrated
    })
    .await;
    assert_eq!(pin(&fx.disk("hello.txt")), Some(PinState::Pinned));
    let freed = fx.engine().free_up_space("hello.txt").await.unwrap();
    assert_eq!(freed, 11);
    assert!(cloud_only(&fx.disk("hello.txt")));
    assert_eq!(pin(&fx.disk("hello.txt")), Some(PinState::Unspecified));

    // A newer version from another device replaces the pinned file and is downloaded again.
    fx.put_cloud("Docs/inner.txt", b"inside, version 2").await;
    wait_for("new version of inner.txt", || {
        std::fs::read(fx.path("Docs/inner.txt")).ok().as_deref() == Some(b"inside, version 2")
    })
    .await;
    assert_eq!(pin(&fx.disk("Docs/inner.txt")), Some(PinState::Pinned));
    // ... and an unpinned one just shows the new size until opened.
    fx.put_cloud("hello.txt", b"hello again, world").await;
    let d = fx.disk("hello.txt");
    assert!(cloud_only(&d) && d.unwrap().size == 18);
    assert_eq!(
        read_blocking(fx.path("hello.txt")).await.unwrap(),
        b"hello again, world"
    );

    // Offline: opening a cloud-only file fails quickly with an error instead of hanging.
    fx.engine().free_up_space("hello.txt").await.unwrap();
    assert!(cloud_only(&fx.disk("hello.txt")));
    fx.mem.set_offline(true);
    let started = Instant::now();
    let r = tokio::time::timeout(Duration::from_secs(20), read_blocking(fx.path("hello.txt")))
        .await
        .expect("read hung");
    let err = r.expect_err("reading while offline should fail");
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "took {:?}",
        started.elapsed()
    );
    eprintln!("offline read error: {err} ({:?})", err.raw_os_error());
    let ext = Command::new("cmd")
        .args(["/D", "/C", "type", "hello.txt"])
        .current_dir(&fx.folder)
        .output()
        .unwrap();
    // (`type` reports the error but still exits with 0.)
    assert!(ext.stdout.is_empty() && !ext.stderr.is_empty(), "{ext:?}");
    eprintln!(
        "cmd type while offline: {}",
        String::from_utf8_lossy(&ext.stderr).trim()
    );
    fx.mem.set_offline(false);
    assert_eq!(
        read_blocking(fx.path("hello.txt")).await.unwrap(),
        b"hello again, world"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_changes_reach_the_cloud() {
    let mut fx = Fixture::new("local");
    fx.start();

    // New file.
    fx.cmd("echo hello> note.txt");
    wait_cloud(&fx, "note.txt uploaded", |c| {
        c.get("note.txt").map(|v| v.as_slice()) == Some(b"hello\r\n")
    })
    .await;
    wait_for("note.txt in sync", || {
        hydrated_in_sync(&fx.disk("note.txt"))
    })
    .await;

    // New folder; move the file into it.
    fx.cmd("mkdir Folder");
    wait_cloud(&fx, "Folder created", |c| c.contains_key("Folder/")).await;
    fx.cmd(r"move note.txt Folder\note.txt");
    wait_cloud(&fx, "note.txt moved", |c| {
        c.contains_key("Folder/note.txt") && !c.contains_key("note.txt")
    })
    .await;

    // Edit it.
    fx.cmd(r"echo more>> Folder\note.txt");
    wait_cloud(&fx, "edit uploaded", |c| {
        c.get("Folder/note.txt").map(|v| v.as_slice()) == Some(b"hello\r\nmore\r\n")
    })
    .await;
    wait_for("edited note.txt in sync", || {
        hydrated_in_sync(&fx.disk("Folder/note.txt"))
    })
    .await;

    // Rename the folder, and a file inside a case-only rename.
    fx.cmd("ren Folder Renamed");
    wait_cloud(&fx, "folder renamed", |c| {
        c.contains_key("Renamed/note.txt") && !c.contains_key("Folder/")
    })
    .await;
    fx.cmd(r"ren Renamed\note.txt NOTE.txt");
    wait_cloud(&fx, "case-only rename", |c| {
        c.contains_key("Renamed/NOTE.txt")
    })
    .await;

    // Copy a bigger file in from outside the folder.
    let data = pattern(5 * 1024 * 1024 + 3, 9);
    let outside = fx.base.join("photo.raw");
    std::fs::write(&outside, &data).unwrap();
    fx.cmd(&format!("copy /b \"{}\" photo.raw", outside.display()));
    wait_cloud(&fx, "photo.raw uploaded", |c| {
        c.get("photo.raw") == Some(&data)
    })
    .await;

    // Temporary and lock files stay local.
    fx.cmd("echo lock> ~$report.docx");

    // Safe-save: write a temp file, swap it in.
    fx.cmd(r"(echo version two)> Renamed\~tmp1.tmp && move /y Renamed\~tmp1.tmp Renamed\NOTE.txt");
    wait_cloud(&fx, "safe-saved file", |c| {
        c.get("Renamed/NOTE.txt").map(|v| v.as_slice()) == Some(b"version two\r\n")
    })
    .await;

    // Delete a file, then the folder.
    fx.cmd("del photo.raw");
    wait_cloud(&fx, "photo.raw deleted", |c| !c.contains_key("photo.raw")).await;
    fx.cmd("rmdir /s /q Renamed");
    wait_cloud(&fx, "folder deleted", |c| {
        c.keys().all(|k| !k.starts_with("Renamed"))
    })
    .await;

    let cloud = fx.cloud().await;
    assert!(
        cloud.keys().all(|k| !k.contains('~')),
        "temp files were uploaded: {:?}",
        cloud.keys()
    );
    assert!(fx.path("~$report.docx").exists());
    assert!(cloud.is_empty(), "{:?}", cloud.keys());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn existing_files_are_adopted_and_offline_changes_caught_up() {
    let mut fx = Fixture::new("adopt");
    std::fs::create_dir_all(fx.folder.join("sub")).unwrap();
    std::fs::write(fx.path("a.txt"), b"a").unwrap();
    std::fs::write(fx.path("sub/b.txt"), b"b").unwrap();
    std::fs::write(fx.path("d.txt"), b"d").unwrap();
    fx.start();
    wait_cloud(&fx, "existing files uploaded", |c| {
        c.len() == 4 && c.get("sub/b.txt").map(|v| v.as_slice()) == Some(b"b")
    })
    .await;
    wait_for("adopted files in sync", || {
        ["a.txt", "sub/b.txt", "d.txt"]
            .iter()
            .all(|p| hydrated_in_sync(&fx.disk(p)))
    })
    .await;

    // Drive isn't running while the user works.
    fx.stop();
    std::fs::write(fx.path("a.txt"), b"a, edited offline").unwrap();
    std::fs::rename(fx.path("sub/b.txt"), fx.path("sub/c.txt")).unwrap();
    std::fs::remove_file(fx.path("d.txt")).unwrap();
    std::fs::write(fx.path("new.txt"), b"new").unwrap();
    let uploads_before = fx
        .mem
        .calls()
        .iter()
        .filter(|c| c.starts_with("upload"))
        .count();

    fx.start();
    wait_cloud(&fx, "offline changes", |c| {
        c.get("a.txt").map(|v| v.as_slice()) == Some(b"a, edited offline")
            && c.get("sub/c.txt").map(|v| v.as_slice()) == Some(b"b")
            && !c.contains_key("sub/b.txt")
            && !c.contains_key("d.txt")
            && c.get("new.txt").map(|v| v.as_slice()) == Some(b"new")
    })
    .await;
    // The rename was a rename, not a re-upload: only a.txt and new.txt were uploaded.
    let uploads = fx
        .mem
        .calls()
        .iter()
        .filter(|c| c.starts_with("upload"))
        .count()
        - uploads_before;
    assert_eq!(uploads, 2, "{:?}", fx.mem.calls());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn release_turns_the_folder_back_into_ordinary_files() {
    let mut fx = Fixture::new("release");
    std::fs::create_dir_all(fx.folder.join("sub")).unwrap();
    std::fs::write(fx.path("sub/kept.txt"), b"kept").unwrap();
    std::fs::write(fx.path("local.txt"), b"was here before").unwrap();
    fx.start();
    fx.put_cloud("cloud.bin", &pattern(300_000, 3)).await;
    wait_for("local file in sync", || {
        hydrated_in_sync(&fx.disk("local.txt"))
    })
    .await;
    wait_for("cloud file appears", || cloud_only(&fx.disk("cloud.bin"))).await;
    assert_eq!(super::cloud_only(&fx.folder).unwrap(), (300_000, 1));

    // Refuses while something is only in the cloud.
    fx.stop();
    assert!(super::release(&fx.folder, &fx.account).is_err());
    assert!(register::is_sync_root(&fx.folder));

    // Download everything (as switching off does: keep it all on this device), then release.
    fx.start();
    fx.running.as_ref().unwrap().0.set_pinned("", true).unwrap();
    wait_for("cloud.bin downloaded", || {
        super::cloud_only(&fx.folder).is_ok_and(|c| c.1 == 0)
    })
    .await;
    fx.stop();
    assert!(register::is_registered_account(&fx.account));
    super::release(&fx.folder, &fx.account).unwrap();
    assert!(!register::is_sync_root(&fx.folder));
    assert!(
        !register::is_registered_account(&fx.account),
        "still registered"
    );
    for (rel, data) in [
        ("local.txt", b"was here before".to_vec()),
        ("cloud.bin", pattern(300_000, 3)),
    ] {
        let d = fx.disk(rel).expect("still there");
        assert!(d.ph.is_none(), "{rel} is still a placeholder");
        let attrs = std::os::windows::fs::MetadataExt::file_attributes(
            &std::fs::metadata(fx.path(rel)).unwrap(),
        );
        assert_eq!(attrs & 0x0018_0000, 0, "{rel} still has pin attributes");
        assert_eq!(std::fs::read(fx.path(rel)).unwrap(), data);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sign_out_removes_the_folder_but_keeps_unsynced_files() {
    let mut fx = Fixture::new("teardown");
    fx.start();
    fx.put_cloud("cloud.txt", b"only in the cloud").await;
    fx.put_cloud("Docs/kept.txt", b"downloaded").await;
    assert_eq!(
        read_blocking(fx.path("Docs/kept.txt")).await.unwrap(),
        b"downloaded"
    );
    fx.mem.set_offline(true);
    std::fs::write(fx.path("Docs/unsynced.txt"), b"not uploaded yet").unwrap();
    wait_for("unsynced.txt noticed", || {
        fx.node("Docs/unsynced.txt").is_some()
    })
    .await;

    fx.stop();
    super::teardown(&fx.folder, &fx.base.join("cache")).unwrap();
    assert!(!fx.folder.exists(), "Drive folder left behind");
    assert!(!register::is_sync_root(&fx.folder));
    assert!(cloud_filter::root::SyncRootId::from_path(&fx.folder).is_err());
    let kept = fx
        .base
        .join(NOT_UPLOADED_FOLDER)
        .join("Docs")
        .join("unsynced.txt");
    assert_eq!(std::fs::read(&kept).unwrap(), b"not uploaded yet");
    assert!(
        !fx.base
            .join(NOT_UPLOADED_FOLDER)
            .join("Docs")
            .join("kept.txt")
            .exists()
    );
}
