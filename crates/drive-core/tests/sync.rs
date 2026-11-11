//! End-to-end sync tests: two simulated devices share one encrypted in-memory vault.
//!
//! Each device has a real engine and database and a fake local file system that behaves like
//! the Windows placeholder model (files are either cloud-only or hydrated).

use std::collections::BTreeMap;
use std::io::{Seek, Write};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use drive_core::crypt::Cipher;
use drive_core::db::{Db, now_ms};
use drive_core::engine::local::{LocalItem, LocalSide, UploadSource};
use drive_core::engine::{Engine, EngineConfig};
use drive_core::ipc::{IssueAction, IssueKind, SyncState};
use drive_core::remote::crypt::CryptRemote;
use drive_core::remote::mem::MemRemote;
use drive_core::remote::{Kind, Remote, read_all};

#[derive(Debug, Clone, PartialEq)]
enum Entry {
    Folder,
    /// `None` = cloud-only placeholder.
    File {
        data: Option<Vec<u8>>,
        size: u64,
    },
}

#[derive(Default)]
struct FakeLocal {
    fs: Mutex<BTreeMap<String, Entry>>,
    engine: OnceLock<Weak<Engine>>,
    open: Mutex<std::collections::HashSet<String>>,
}

impl FakeLocal {
    fn move_tree(&self, from: &str, to: &str) {
        let mut fs = self.fs.lock().unwrap();
        let keys: Vec<String> = fs
            .keys()
            .filter(|k| *k == from || k.starts_with(&format!("{from}/")))
            .cloned()
            .collect();
        for k in keys {
            let v = fs.remove(&k).unwrap();
            fs.insert(format!("{to}{}", &k[from.len()..]), v);
        }
    }
}

impl LocalSide for FakeLocal {
    fn create(&self, item: &LocalItem) -> std::io::Result<()> {
        let mut fs = self.fs.lock().unwrap();
        if fs.contains_key(&item.path) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                item.path.clone(),
            ));
        }
        let e = match item.kind {
            Kind::Folder => Entry::Folder,
            Kind::File => Entry::File {
                data: None,
                size: item.size,
            },
        };
        fs.insert(item.path.clone(), e);
        Ok(())
    }
    fn replace(&self, item: &LocalItem) -> std::io::Result<()> {
        self.fs.lock().unwrap().insert(
            item.path.clone(),
            Entry::File {
                data: None,
                size: item.size,
            },
        );
        Ok(())
    }
    fn rename(&self, from: &str, item: &LocalItem) -> std::io::Result<()> {
        self.move_tree(from, &item.path);
        Ok(())
    }
    fn remove(&self, item: &LocalItem) -> std::io::Result<()> {
        let mut fs = self.fs.lock().unwrap();
        fs.retain(|k, _| k != &item.path && !k.starts_with(&format!("{}/", item.path)));
        Ok(())
    }
    fn open_for_upload(&self, item: &LocalItem) -> std::io::Result<UploadSource> {
        let fs = self.fs.lock().unwrap();
        let Some(Entry::File { data: Some(d), .. }) = fs.get(&item.path) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                item.path.clone(),
            ));
        };
        let mut f = tempfile::tempfile()?;
        f.write_all(d)?;
        f.rewind()?;
        Ok(UploadSource {
            file: f,
            size: d.len() as u64,
            mtime_ms: 1,
        })
    }
    fn uploaded(&self, _item: &LocalItem, _size: u64, _mtime_ms: i64) -> std::io::Result<bool> {
        Ok(true)
    }
    fn evict(&self, item: &LocalItem) -> std::io::Result<bool> {
        if self.open.lock().unwrap().contains(&item.path) {
            return Ok(false);
        }
        let mut fs = self.fs.lock().unwrap();
        if let Some(Entry::File { data, .. }) = fs.get_mut(&item.path) {
            *data = None;
        }
        Ok(true)
    }
    fn hydrate(&self, item: &LocalItem) -> std::io::Result<()> {
        let engine = self.engine.get().and_then(|w| w.upgrade()).expect("engine");
        let id = item.id;
        let data = tokio::runtime::Handle::current()
            .block_on(async move { read_all(engine.read(id, 0, None).await?).await })?;
        self.fs.lock().unwrap().insert(
            item.path.clone(),
            Entry::File {
                size: data.len() as u64,
                data: Some(data),
            },
        );
        Ok(())
    }
}

struct Device {
    engine: Arc<Engine>,
    local: Arc<FakeLocal>,
}

fn config(name: &str) -> EngineConfig {
    EngineConfig {
        device_name: name.into(),
        debounce: Duration::ZERO,
        poll_interval: Duration::from_secs(3600),
        folder: std::env::temp_dir(),
        min_free_bytes: 0,
        ..Default::default()
    }
}

fn device(remote: Arc<dyn Remote>, name: &str) -> Device {
    let db = Arc::new(Db::open_in_memory().unwrap());
    let engine = Engine::new(db, remote, config(name)).unwrap();
    let local = Arc::new(FakeLocal::default());
    let _ = local.engine.set(Arc::downgrade(&engine));
    engine.set_local(local.clone());
    engine.start();
    Device { engine, local }
}

impl Device {
    fn write(&self, path: &str, data: &[u8]) {
        self.local.fs.lock().unwrap().insert(
            path.into(),
            Entry::File {
                data: Some(data.to_vec()),
                size: data.len() as u64,
            },
        );
        self.engine
            .local_modified(path, data.len() as u64, now_ms())
            .unwrap();
    }
    fn mkdir(&self, path: &str) {
        self.local
            .fs
            .lock()
            .unwrap()
            .insert(path.into(), Entry::Folder);
        self.engine
            .local_create(path, Kind::Folder, 0, now_ms())
            .unwrap();
    }
    fn rename(&self, from: &str, to: &str) {
        self.local.move_tree(from, to);
        self.engine.local_rename(from, to).unwrap();
    }
    fn delete(&self, path: &str) {
        self.local
            .fs
            .lock()
            .unwrap()
            .retain(|k, _| k != path && !k.starts_with(&format!("{path}/")));
        self.engine.local_delete(path).unwrap();
    }
    /// Opens a file like an app would: hydrates placeholders on demand.
    async fn read(&self, path: &str) -> Vec<u8> {
        let existing = self.local.fs.lock().unwrap().get(path).cloned();
        match existing {
            Some(Entry::File { data: Some(d), .. }) => d,
            Some(Entry::File { data: None, .. }) => {
                let n = self.engine.node_by_path(path).unwrap().expect("node");
                let data = read_all(self.engine.read(n.id, 0, None).await.unwrap())
                    .await
                    .unwrap();
                self.local.fs.lock().unwrap().insert(
                    path.into(),
                    Entry::File {
                        size: data.len() as u64,
                        data: Some(data.clone()),
                    },
                );
                self.engine.mark_hydrated(n.id, true);
                data
            }
            other => panic!("{path}: {other:?} (have {:?})", self.paths()),
        }
    }
    fn paths(&self) -> Vec<String> {
        self.local.fs.lock().unwrap().keys().cloned().collect()
    }
    async fn idle(&self) {
        for _ in 0..400 {
            self.engine.sync_now();
            tokio::time::sleep(Duration::from_millis(10)).await;
            let st = self.engine.status();
            let orphans = self.engine.db().orphan_ids().unwrap().len();
            if self.engine.db().pending_count().unwrap() == 0
                && orphans == 0
                && st.uploading == 0
                && st.downloading == 0
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
                if self.engine.db().pending_count().unwrap() == 0 {
                    return;
                }
            }
        }
        panic!(
            "device did not become idle: {:?}",
            self.engine.db().all_nodes().unwrap()
        );
    }
    async fn sync(&self) {
        self.idle().await;
        self.engine.scan_once().await.unwrap();
        self.idle().await;
    }
}

fn vault() -> (Arc<MemRemote>, Arc<dyn Remote>) {
    let mem = Arc::new(MemRemote::new());
    let crypt: Arc<dyn Remote> = Arc::new(CryptRemote::new(
        mem.clone(),
        Cipher::new("pw", "salt").unwrap(),
    ));
    (mem, crypt)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn files_flow_between_devices() {
    let (_mem, remote) = vault();
    let a = device(remote.clone(), "A");
    let b = device(remote.clone(), "B");

    a.mkdir("Docs");
    a.write("Docs/plan.txt", b"version 1");
    a.write("photo.jpg", &vec![7u8; 300_000]);
    a.sync().await;
    b.sync().await;
    assert_eq!(b.paths(), vec!["Docs", "Docs/plan.txt", "photo.jpg"]);
    // B sees placeholders until something opens them.
    assert!(matches!(
        b.local.fs.lock().unwrap()["photo.jpg"],
        Entry::File {
            data: None,
            size: 300_000
        }
    ));
    assert_eq!(b.read("Docs/plan.txt").await, b"version 1");
    assert_eq!(b.read("photo.jpg").await, vec![7u8; 300_000]);

    // Edit on B, see it on A.
    b.write("Docs/plan.txt", b"version 2 from B");
    b.sync().await;
    a.sync().await;
    assert_eq!(a.read("Docs/plan.txt").await, b"version 2 from B");

    // Rename and move on A.
    a.mkdir("Archive");
    a.rename("Docs/plan.txt", "Archive/plan-final.txt");
    a.sync().await;
    b.sync().await;
    assert_eq!(
        b.paths(),
        vec!["Archive", "Archive/plan-final.txt", "Docs", "photo.jpg"]
    );
    assert_eq!(b.read("Archive/plan-final.txt").await, b"version 2 from B");

    // Delete on B.
    b.delete("Docs");
    b.delete("photo.jpg");
    b.sync().await;
    a.sync().await;
    assert_eq!(a.paths(), vec!["Archive", "Archive/plan-final.txt"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_edits_keep_both_versions() {
    let (_mem, remote) = vault();
    let a = device(remote.clone(), "A");
    let b = device(remote.clone(), "B");
    a.write("notes.txt", b"base");
    a.sync().await;
    b.sync().await;
    assert_eq!(b.read("notes.txt").await, b"base");

    // Both edit before syncing.
    a.write("notes.txt", b"edit from A");
    b.write("notes.txt", b"edit from B");
    a.sync().await;
    b.sync().await; // B's scan sees A's version replacing the base it edited
    a.sync().await;

    // Both versions survive on both devices: one keeps the name, the other is a clearly
    // labelled copy. (Which one depends on timing when both uploads race.)
    let names = b.paths();
    assert_eq!(names.len(), 2, "{names:?}");
    assert!(names.contains(&"notes.txt".to_string()));
    let copy = names
        .iter()
        .find(|n| n.contains("conflict"))
        .unwrap_or_else(|| panic!("no conflicted copy in {names:?}"))
        .clone();
    assert_eq!(a.paths(), names);
    let mut contents = vec![b.read("notes.txt").await, b.read(&copy).await];
    contents.sort();
    assert_eq!(
        contents,
        vec![b"edit from A".to_vec(), b"edit from B".to_vec()]
    );
    assert_eq!(a.read("notes.txt").await, b.read("notes.txt").await);
    assert_eq!(a.read(&copy).await, b.read(&copy).await);
    assert!(
        b.engine
            .issues()
            .unwrap()
            .iter()
            .any(|i| i.kind == IssueKind::Conflict)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn edits_survive_remote_deletion() {
    let (_mem, remote) = vault();
    let a = device(remote.clone(), "A");
    let b = device(remote.clone(), "B");
    a.mkdir("Shared");
    a.write("Shared/doc.txt", b"v1");
    a.sync().await;
    b.sync().await;
    // A deletes the folder; before B hears about it, B edits a file inside.
    a.delete("Shared");
    a.sync().await;
    b.write("Shared/doc.txt", b"edited offline on B");
    b.sync().await;
    a.sync().await;
    assert_eq!(a.read("Shared/doc.txt").await, b"edited offline on B");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retries_after_failures_and_offline() {
    let (mem, remote) = vault();
    let a = device(remote.clone(), "A");
    mem.set_offline(true);
    a.write("x.bin", b"data");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(a.engine.status().state, SyncState::Offline);
    mem.set_offline(false);
    // Retry timers are long; make the work due again as "Retry" would.
    a.engine
        .db()
        .write(|tx| {
            tx.execute("UPDATE node SET retry_at_ms = 0", [])
                .map(|_| ())
        })
        .unwrap();
    mem.fail_next(2);
    for _ in 0..3 {
        a.engine
            .db()
            .write(|tx| {
                tx.execute("UPDATE node SET retry_at_ms = 0", [])
                    .map(|_| ())
            })
            .unwrap();
        a.engine.sync_now();
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    a.sync().await;
    assert_eq!(a.engine.status().state, SyncState::UpToDate);
    let b = device(remote, "B");
    b.sync().await;
    assert_eq!(b.read("x.bin").await, b"data");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_mass_deletion_needs_confirmation() {
    let (mem, remote) = vault();
    let a = device(remote.clone(), "A");
    for i in 0..120 {
        a.write(&format!("f{i}.txt"), b"x");
    }
    a.sync().await;
    assert_eq!(a.paths().len(), 120);
    // Someone empties the vault behind our back.
    for (id, _) in mem.snapshot_ids() {
        mem.remove(&id);
    }
    a.sync().await;
    assert_eq!(a.paths().len(), 120, "nothing removed without confirmation");
    let issue = a
        .engine
        .issues()
        .unwrap()
        .into_iter()
        .find(|i| i.kind == IssueKind::MassDelete)
        .expect("guard issue");
    assert_eq!(a.engine.status().pending_delete_confirmation, Some(120));
    a.engine
        .resolve_issue(issue.id, IssueAction::ConfirmDelete)
        .unwrap();
    a.sync().await;
    assert!(a.paths().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wrong_password_never_deletes() {
    let (mem, remote) = vault();
    let a = device(remote, "A");
    a.write("secret.txt", b"x");
    a.sync().await;
    let wrong: Arc<dyn Remote> = Arc::new(CryptRemote::new(
        mem.clone(),
        Cipher::new("other", "salt").unwrap(),
    ));
    let b = device(wrong, "B");
    b.engine.scan_once().await.unwrap();
    assert!(b.paths().is_empty());
    assert_eq!(b.engine.status().state, SyncState::Error);
    assert!(
        b.engine
            .issues()
            .unwrap()
            .iter()
            .any(|i| i.kind == IssueKind::WrongPassword)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn eviction_respects_limit_and_pins() {
    let (_mem, remote) = vault();
    let a = device(remote.clone(), "A");
    let b = device(remote, "B");
    for i in 0..5 {
        a.write(&format!("f{i}.bin"), &vec![i as u8; 1000]);
    }
    a.sync().await;
    b.sync().await;
    b.engine.set_pinned("f0.bin", true).unwrap();
    for i in 0..5 {
        b.read(&format!("f{i}.bin")).await;
    }
    b.engine.update_config(|c| c.cache_limit_bytes = 2000);
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if b.engine.db().storage_totals().unwrap().cache_bytes <= 2000 {
            break;
        }
    }
    let totals = b.engine.db().storage_totals().unwrap();
    assert!(totals.cache_bytes <= 2000, "{totals:?}");
    assert_eq!(totals.pinned_bytes, 1000);
    assert!(
        matches!(
            b.local.fs.lock().unwrap()["f0.bin"],
            Entry::File { data: Some(_), .. }
        ),
        "pinned file kept"
    );
    // Evicted files still open on demand.
    assert_eq!(b.read("f1.bin").await, vec![1u8; 1000]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn safe_save_pattern() {
    // Many editors save by writing a temp file and renaming it over the original.
    let (_mem, remote) = vault();
    let a = device(remote.clone(), "A");
    let b = device(remote.clone(), "B");
    a.write("report.docx", b"v1");
    a.sync().await;
    a.write("~WRL0001.tmp", b"v2");
    a.rename("~WRL0001.tmp", "report.docx");
    a.sync().await;
    b.sync().await;
    assert_eq!(b.paths(), vec!["report.docx"]);
    assert_eq!(b.read("report.docx").await, b"v2");
}
