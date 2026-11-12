//! End-to-end tests of folder backups: a real mirror engine and manager over an encrypted
//! in-memory vault, backing up a temporary folder.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use drive_core::crypt::Cipher;
use drive_core::db::Db;
use drive_core::engine::{Engine, EngineConfig};
use drive_core::ipc::BackupState;
use drive_core::remote::crypt::CryptRemote;
use drive_core::remote::mem::{MemRemote, ROOT};
use drive_core::remote::subtree::Subtree;
use drive_core::remote::{Kind, Remote, read_all};

use super::local::BackupLocal;
use super::{BackupFolder, Manager, scan};

const CACHEDIR_TAG: &[u8] = b"Signature: 8a477f597d28d172789f06886806bc55\n";

struct Rig {
    mem: Arc<MemRemote>,
    device: String,
    remote: Arc<dyn Remote>,
    manager: Arc<Manager>,
    _db_dir: tempfile::TempDir,
}

fn folder(path: &Path) -> BackupFolder {
    BackupFolder {
        id: "f1".into(),
        name: "Documents".into(),
        path: path.to_path_buf(),
        known: None,
        enabled: true,
        excludes: scan::default_excludes(),
        added_ms: 0,
    }
}

impl Rig {
    async fn new(source: &Path) -> Self {
        Self::with_mem(source, Arc::new(MemRemote::new()), None).await
    }

    async fn with_mem(source: &Path, mem: Arc<MemRemote>, device: Option<String>) -> Self {
        let device = device.unwrap_or_else(|| mem.put_folder(ROOT, "device"));
        let remote: Arc<dyn Remote> = Arc::new(CryptRemote::new(
            Arc::new(Subtree::new(mem.clone(), device.clone())),
            Cipher::new("pw", "salt").unwrap(),
        ));
        let db_dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Db::open(&db_dir.path().join("b.db")).unwrap());
        let cfg = EngineConfig {
            mirror: true,
            debounce: Duration::ZERO,
            poll_interval: Duration::from_secs(3600),
            min_free_bytes: 0,
            adaptive_upload: false,
            ..Default::default()
        };
        let engine = Engine::new(db, remote.clone(), cfg).unwrap();
        let sources = Arc::new(RwLock::new(HashMap::new()));
        engine.set_local(Arc::new(BackupLocal::new(sources.clone())));
        let manager = Manager::new(
            engine.clone(),
            sources,
            vec![folder(source)],
            Arc::new(|| {}),
        );
        engine.start();
        manager.start();
        Rig {
            mem,
            device,
            remote,
            manager,
            _db_dir: db_dir,
        }
    }

    /// Decrypted cloud copy: path → content (folders end with `/`).
    async fn cloud(&self) -> BTreeMap<String, Vec<u8>> {
        let tree = self.remote.scan().await.unwrap();
        let by_id: HashMap<&str, &drive_core::remote::RemoteEntry> =
            tree.entries.iter().map(|e| (e.id.as_str(), e)).collect();
        let path = |id: &str| {
            let mut parts = vec![];
            let mut cur = id.to_string();
            while let Some(e) = by_id.get(cur.as_str()) {
                parts.push(e.name.clone());
                cur = e.parent_id.clone();
            }
            parts.reverse();
            parts.join("/")
        };
        let mut out = BTreeMap::new();
        for e in &tree.entries {
            let p = path(&e.id);
            if e.kind == Kind::Folder {
                out.insert(format!("{p}/"), vec![]);
            } else {
                let data = read_all(self.remote.read(e, 0, None).await.unwrap())
                    .await
                    .unwrap();
                out.insert(p, data);
            }
        }
        out
    }

    fn uploads(&self) -> usize {
        self.mem
            .calls()
            .iter()
            .filter(|c| c.starts_with("upload"))
            .count()
    }

    /// Waits until the folder is fully backed up (or `timeout`).
    async fn settle(&self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            self.manager.engine().sync_now();
            tokio::time::sleep(Duration::from_millis(50)).await;
            let infos = self.manager.infos();
            let busy = infos
                .iter()
                .any(|i| matches!(i.state, BackupState::Scanning | BackupState::BackingUp));
            let orphans = self.manager.engine().db().orphan_ids().unwrap().len();
            if !busy && orphans == 0 && self.manager.engine().db().pending_count().unwrap() == 0 {
                tokio::time::sleep(Duration::from_millis(100)).await;
                if self.manager.engine().db().pending_count().unwrap() == 0 {
                    return;
                }
            }
            assert!(Instant::now() < deadline, "backup didn't settle: {infos:?}");
        }
    }

    async fn rescan(&self) {
        let f = self.manager.folders().into_iter().next().unwrap();
        self.manager.full_scan(&f).await;
        self.settle().await;
    }
}

fn write(p: &Path, data: &[u8]) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, data).unwrap();
}

fn source_tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let r = dir.path();
    write(&r.join("notes.txt"), b"notes");
    write(&r.join("Work/report.pdf"), &vec![7u8; 200_000]);
    write(&r.join("Work/Q3/plan.docx"), b"plan v1");
    write(&r.join("~$plan.docx"), b"lock file");
    write(
        &r.join("app/node_modules/left-pad/index.js"),
        b"module.exports",
    );
    write(&r.join("app/src/main.ts"), b"console.log(1)");
    write(&r.join("rust/target/debug/huge.bin"), &vec![1u8; 50_000]);
    write(&r.join("rust/target/CACHEDIR.TAG"), CACHEDIR_TAG);
    dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn backs_up_a_folder_and_follows_changes() {
    let src = source_tree();
    let rig = Rig::new(src.path()).await;
    rig.settle().await;

    let cloud = rig.cloud().await;
    let paths: Vec<&str> = cloud.keys().map(|s| s.as_str()).collect();
    assert_eq!(
        paths,
        [
            "Documents/",
            "Documents/Work/",
            "Documents/Work/Q3/",
            "Documents/Work/Q3/plan.docx",
            "Documents/Work/report.pdf",
            "Documents/app/",
            "Documents/app/src/",
            "Documents/app/src/main.ts",
            "Documents/notes.txt",
            "Documents/rust/",
        ]
    );
    assert_eq!(cloud["Documents/Work/report.pdf"], vec![7u8; 200_000]);
    let info = &rig.manager.infos()[0];
    assert_eq!(info.state, BackupState::UpToDate);
    assert_eq!((info.files, info.done_files), (4, 4));
    assert!(info.last_complete_ms.is_some());
    // The names in the cloud are encrypted.
    assert!(rig.mem.snapshot().keys().all(|k| !k.contains("report")));

    // Edit, add, delete, move.
    let r = src.path();
    write(&r.join("Work/Q3/plan.docx"), b"plan v2, longer");
    write(&r.join("new.txt"), b"new");
    fs::remove_file(r.join("notes.txt")).unwrap();
    let uploads_before = rig.uploads();
    fs::create_dir_all(r.join("Archive")).unwrap();
    fs::rename(r.join("Work/report.pdf"), r.join("Archive/report-2026.pdf")).unwrap();
    rig.rescan().await;

    let cloud = rig.cloud().await;
    assert_eq!(cloud["Documents/Work/Q3/plan.docx"], b"plan v2, longer");
    assert_eq!(cloud["Documents/new.txt"], b"new");
    assert!(!cloud.contains_key("Documents/notes.txt"));
    assert!(!cloud.contains_key("Documents/Work/report.pdf"));
    assert_eq!(
        cloud["Documents/Archive/report-2026.pdf"],
        vec![7u8; 200_000]
    );
    // The moved file wasn't uploaded again: just the edit and the new file.
    assert_eq!(rig.uploads() - uploads_before, 2, "{:?}", rig.mem.calls());
    // Removed and replaced versions went to the trash, not gone for good.
    assert!(!rig.mem.trashed_names().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watcher_picks_up_changes() {
    let src = source_tree();
    let rig = Rig::new(src.path()).await;
    rig.settle().await;
    write(&src.path().join("Work/watched.txt"), b"seen by the watcher");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        rig.settle().await;
        if rig.cloud().await.contains_key("Documents/Work/watched.txt") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the watcher didn't pick up the new file"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cloud_tampering_is_repaired() {
    let src = source_tree();
    let rig = Rig::new(src.path()).await;
    rig.settle().await;
    // Someone deletes a file from the cloud copy and drops a stranger into it.
    let tree = rig.remote.scan().await.unwrap();
    let notes = tree
        .entries
        .iter()
        .find(|e| e.name == "notes.txt")
        .unwrap()
        .clone();
    rig.mem.remove(&notes.id);
    let docs = tree
        .entries
        .iter()
        .find(|e| e.name == "Documents")
        .unwrap()
        .clone();
    rig.remote
        .upload(
            &docs.id,
            "stranger.txt",
            drive_core::remote::UploadBody::from_bytes(b"x".to_vec(), 1),
        )
        .await
        .unwrap();
    rig.manager.engine().scan_once().await.unwrap();
    rig.settle().await;
    let cloud = rig.cloud().await;
    assert_eq!(cloud["Documents/notes.txt"], b"notes");
    assert!(!cloud.contains_key("Documents/stranger.txt"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn off_switch_keeps_the_copy_and_on_again_uploads_nothing() {
    let src = source_tree();
    let rig = Rig::new(src.path()).await;
    rig.settle().await;
    let before = rig.cloud().await;
    rig.manager.set_enabled("f1", false).await.unwrap();
    assert_eq!(rig.manager.infos()[0].state, BackupState::Off);
    rig.manager.engine().scan_once().await.unwrap();
    rig.settle().await;
    assert_eq!(
        rig.cloud().await,
        before,
        "switching off must keep the cloud copy"
    );

    let uploads = rig.uploads();
    rig.manager.set_enabled("f1", true).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    rig.settle().await;
    assert_eq!(
        rig.uploads(),
        uploads,
        "unchanged files must be recognised, not uploaded again"
    );
    assert_eq!(rig.cloud().await, before);
    assert_eq!(rig.manager.infos()[0].state, BackupState::UpToDate);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reinstall_recognises_the_existing_copy() {
    let src = source_tree();
    let mem = Arc::new(MemRemote::new());
    let device = {
        let rig = Rig::with_mem(src.path(), mem.clone(), None).await;
        rig.settle().await;
        let d = rig.device.clone();
        rig.manager.stop();
        d
    };
    let uploads = mem
        .calls()
        .iter()
        .filter(|c| c.starts_with("upload"))
        .count();
    // A fresh database (new install) pointing at the same cloud folder.
    let rig = Rig::with_mem(src.path(), mem.clone(), Some(device)).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    rig.settle().await;
    assert_eq!(rig.uploads(), uploads);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_backing_up_and_delete_from_cloud() {
    let src = source_tree();
    let rig = Rig::new(src.path()).await;
    rig.settle().await;
    rig.manager.remove("f1", true).await.unwrap();
    assert!(rig.manager.folders().is_empty());
    let cloud = rig.cloud().await;
    assert!(cloud.is_empty(), "{cloud:?}");
    // The folder on this device is untouched.
    assert_eq!(fs::read(src.path().join("notes.txt")).unwrap(), b"notes");
    assert!(src.path().join("Work/report.pdf").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_folder_is_reported_not_deleted() {
    let parent = tempfile::tempdir().unwrap();
    let path = parent.path().join("USB");
    let src = source_tree();
    // Copy the tree into a folder we can make disappear.
    fn copy(from: &Path, to: &Path) {
        fs::create_dir_all(to).unwrap();
        for e in fs::read_dir(from).unwrap().flatten() {
            let t = to.join(e.file_name());
            if e.file_type().unwrap().is_dir() {
                copy(&e.path(), &t);
            } else {
                fs::copy(e.path(), &t).unwrap();
            }
        }
    }
    copy(src.path(), &path);
    let rig = Rig::new(&path).await;
    rig.settle().await;
    let before = rig.cloud().await;
    fs::rename(&path, parent.path().join("USB-unplugged")).unwrap();
    let f = rig.manager.folders().into_iter().next().unwrap();
    rig.manager.full_scan(&f).await;
    rig.manager.engine().scan_once().await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let info = &rig.manager.infos()[0];
    assert_eq!(info.state, BackupState::Problem, "{info:?}");
    assert_eq!(
        rig.cloud().await,
        before,
        "an unplugged folder must not empty the backup"
    );
}
