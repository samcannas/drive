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
        for _ in 0..1500 {
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
    // The device that resolves the collision reports it; the other just receives the copy.
    let reported = |d: &Device| {
        d.engine
            .issues()
            .unwrap()
            .iter()
            .any(|i| i.kind == IssueKind::Conflict)
    };
    assert!(
        reported(&a) || reported(&b),
        "no device reported the conflict"
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

// ---- Track 1: transfers ----------------------------------------------------------------

use chrono::Timelike;
use drive_core::engine::schedule::NightWindow;
use drive_core::ipc::{TransferMode, TransferState};

fn device_with(remote: Arc<dyn Remote>, cfg: EngineConfig, db: Arc<Db>) -> Device {
    let engine = Engine::new(db, remote, cfg).unwrap();
    let local = Arc::new(FakeLocal::default());
    let _ = local.engine.set(Arc::downgrade(&engine));
    engine.set_local(local.clone());
    engine.start();
    Device { engine, local }
}

fn retry_now(d: &Device) {
    d.engine
        .db()
        .write(|tx| {
            tx.execute("UPDATE node SET retry_at_ms = 0", [])
                .map(|_| ())
        })
        .unwrap();
    d.engine.sync_now();
}

async fn until(what: &str, mut f: impl FnMut() -> bool) {
    for _ in 0..1000 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for: {what}");
}

fn item(d: &Device, path: &str) -> Option<drive_core::ipc::TransferItem> {
    d.engine
        .transfers()
        .items
        .into_iter()
        .find(|i| i.path == path)
}

fn big(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 7 % 253) as u8).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn big_upload_continues_after_a_dropped_connection() {
    let (mem, remote) = vault();
    let a = device(remote.clone(), "A");
    let data = big(1_000_000);
    let enc = drive_core::crypt::encrypted_size(data.len() as u64);
    mem.fail_upload_at(400_000);
    a.write("movie.mp4", &data);
    until("the upload to break off", || {
        mem.received_bytes() >= 400_000
    })
    .await;
    until("the failure to be recorded", || {
        item(&a, "movie.mp4").is_some_and(|i| i.state == TransferState::Waiting)
    })
    .await;
    let waiting = item(&a, "movie.mp4").unwrap();
    assert!(
        waiting.resumable && waiting.bytes_done >= 300_000,
        "{waiting:?}"
    );
    retry_now(&a);
    a.sync().await;
    assert!(
        mem.received_bytes() <= enc + 65_536,
        "only the rest was sent: {} of {enc}",
        mem.received_bytes()
    );
    let b = device(remote, "B");
    b.sync().await;
    assert_eq!(b.read("movie.mp4").await, data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn upload_picks_up_where_it_left_off_after_a_restart() {
    let (mem, remote) = vault();
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("state.db");
    let data = big(1_500_000);
    let enc = drive_core::crypt::encrypted_size(data.len() as u64);
    mem.set_chunk_delay(Duration::from_millis(15));
    {
        let a = device_with(
            remote.clone(),
            config("A"),
            Arc::new(Db::open(&db_path).unwrap()),
        );
        a.write("big.iso", &data);
        until("some progress", || {
            item(&a, "big.iso").is_some_and(|i| i.bytes_done > 400_000)
        })
        .await;
        // The app quits (or crashes) mid-upload.
        a.engine.shutdown();
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(mem.received_bytes() < enc);
    mem.set_chunk_delay(Duration::ZERO);
    // A new process: same database, same cloud.
    let a = device_with(
        remote.clone(),
        config("A"),
        Arc::new(Db::open(&db_path).unwrap()),
    );
    // The local file is still there (the fake file system doesn't survive the "restart").
    a.local.fs.lock().unwrap().insert(
        "big.iso".into(),
        Entry::File {
            data: Some(data.clone()),
            size: data.len() as u64,
        },
    );
    let queued = item(&a, "big.iso").expect("listed after restart");
    assert!(
        queued.resumable && queued.bytes_done >= 400_000,
        "{queued:?}"
    );
    a.sync().await;
    assert!(
        mem.received_bytes() <= enc + 65_536,
        "{} of {enc} bytes",
        mem.received_bytes()
    );
    let b = device(remote, "B");
    b.sync().await;
    assert_eq!(b.read("big.iso").await, data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pausing_keeps_progress_and_resuming_finishes() {
    let (mem, remote) = vault();
    mem.set_chunk_delay(Duration::from_millis(15));
    let a = device(remote.clone(), "A");
    let data = big(1_200_000);
    let enc = drive_core::crypt::encrypted_size(data.len() as u64);
    a.write("talk.mkv", &data);
    until("uploading", || {
        item(&a, "talk.mkv")
            .is_some_and(|i| i.state == TransferState::Active && i.bytes_done > 200_000)
    })
    .await;
    let id = item(&a, "talk.mkv").unwrap().id;
    a.engine
        .set_transfer_mode(id, TransferMode::Paused)
        .unwrap();
    until("paused", || {
        item(&a, "talk.mkv").is_some_and(|i| i.state == TransferState::Paused)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    let paused = item(&a, "talk.mkv").unwrap();
    assert_eq!(paused.mode, TransferMode::Paused);
    assert!(
        paused.resumable && paused.bytes_done > 200_000,
        "{paused:?}"
    );
    let sent = mem.received_bytes();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(mem.received_bytes(), sent, "nothing sent while paused");
    mem.set_chunk_delay(Duration::ZERO);
    a.engine.set_transfer_mode(id, TransferMode::Auto).unwrap();
    a.sync().await;
    assert!(mem.received_bytes() <= enc + 65_536);
    assert!(item(&a, "talk.mkv").is_none(), "done");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn large_files_wait_for_the_night_window_unless_forced() {
    let (mem, remote) = vault();
    let now = chrono::Local::now();
    let start = ((now.time().hour() + 2) % 24) as u16 * 60;
    let mut cfg = config("A");
    cfg.night = Some(NightWindow {
        start,
        end: (start + 60) % (24 * 60),
    });
    cfg.night_min_bytes = 500_000;
    let a = device_with(remote.clone(), cfg, Arc::new(Db::open_in_memory().unwrap()));
    a.write("small.txt", b"hello");
    a.write("huge.mov", &big(800_000));
    until("the small file to upload", || {
        mem.snapshot().contains_key("small.txt") || item(&a, "small.txt").is_none()
    })
    .await;
    let list = a.engine.transfers();
    let huge = list
        .items
        .iter()
        .find(|i| i.path == "huge.mov")
        .expect("listed");
    assert_eq!(huge.state, TransferState::Scheduled);
    assert!(
        huge.reason.as_deref().unwrap_or("").contains("tonight"),
        "{huge:?}"
    );
    assert!(list.night_window_opens_ms.is_some());
    assert_eq!(
        list.upload_remaining_bytes, 0,
        "held uploads don't count toward the ETA"
    );
    a.engine
        .set_transfer_mode(huge.id, TransferMode::Now)
        .unwrap();
    a.sync().await;
    assert!(item(&a, "huge.mov").is_none());
    // The mode resets after the upload.
    assert_eq!(
        a.engine
            .node_by_path("huge.mov")
            .unwrap()
            .unwrap()
            .upload_mode,
        0
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_list_explains_waits_and_estimates() {
    let (mem, remote) = vault();
    let a = device(remote.clone(), "A");
    mem.set_offline(true);
    a.write("notes.txt", b"x");
    until("an offline reason", || {
        item(&a, "notes.txt")
            .and_then(|i| i.reason)
            .is_some_and(|r| r.contains("Offline") || r.contains("Connection"))
    })
    .await;
    mem.set_offline(false);
    retry_now(&a);
    a.sync().await;
    // A slow upload (about 3 s) gets a rate and an ETA.
    mem.set_chunk_delay(Duration::from_millis(200));
    a.write("slow.bin", &big(1_000_000));
    until("a rate and an ETA", || {
        let l = a.engine.transfers();
        l.upload_rate_bps > 0 && l.upload_eta_secs.is_some()
    })
    .await;
    mem.set_chunk_delay(Duration::ZERO);
    a.sync().await;
}

// ---- Automatic upload speed: wiring ----------------------------------------------------

#[tokio::test]
async fn automatic_upload_speed_measures_and_can_be_switched_off() {
    // Something to time handshakes against.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let _ = listener.accept().await;
        }
    });
    let mut cfg = config("A");
    cfg.probe_target = Some(addr.to_string());
    let d = device_with(
        Arc::new(MemRemote::new()),
        cfg,
        Arc::new(Db::open_in_memory().unwrap()),
    );

    // Measuring starts with the engine; nothing is limited on an idle, healthy connection.
    let measured = wait_for(Duration::from_secs(5), || {
        d.engine.upload_speed().base_ms.is_some()
    })
    .await;
    assert!(measured, "no measurements: {:?}", d.engine.upload_speed());
    let s = d.engine.upload_speed();
    assert_eq!(s.mode, drive_core::engine::adaptive::Mode::Unlimited);
    assert!(s.base_ms.unwrap() < 100.0, "{s:?}");
    let list = d.engine.transfers();
    assert_eq!(list.auto_upload_limit_bps, None);
    assert!(!list.auto_upload_checking);

    // Switched off: measurements stop.
    d.engine.update_config(|c| c.adaptive_upload = false);
    let off = wait_for(Duration::from_secs(8), || {
        d.engine.upload_speed().base_ms.is_none()
    })
    .await;
    assert!(off, "still measuring: {:?}", d.engine.upload_speed());
}

async fn wait_for(limit: Duration, mut f: impl FnMut() -> bool) -> bool {
    let end = tokio::time::Instant::now() + limit;
    while tokio::time::Instant::now() < end {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    f()
}
