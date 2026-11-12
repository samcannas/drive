//! A fake Drive service for developing and screenshotting the UI without the real one.
//!
//! Enabled by `DRIVE_UI_MOCK=1`, or automatically in debug builds when the service can't be
//! reached. Release builds never use it: [`forced`] is always false there and the automatic
//! fallback in `backend` is debug-only.
//!
//! Optional knobs (debug builds only):
//! * `DRIVE_UI_MOCK=onboarding` starts in the not-set-up state.
//! * `DRIVE_UI_MOCK_MASS_DELETE=1` starts with a pending mass-delete confirmation.
//! * `DRIVE_UI_MOCK_TRANSFERS=1` starts with a long transfer list: one item in every state,
//!   a paused upload, large files held for the overnight window and a partly done one that
//!   resumes. Progress moves, and items finish and come back.
//! * `DRIVE_UI_MOCK_BACKUPS=1` starts with backed-up folders in every state: one backing up,
//!   one up to date, one switched off, one whose folder is missing and Downloads saving space
//!   on this device (on-demand; Windows only). Making a folder on-demand or a full copy again
//!   takes about 5 s; making Downloads a full copy again fails for lack of disk space. Without it
//!   there are no backups and the suggestions show; adding one scans, backs up and finishes in
//!   about 20 s.
//! * `DRIVE_UI_MOCK_UPDATE=1` offers a made-up update (see `updater`); installing it only
//!   pretends. Works with or without the mock service.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use drive_core::ipc::{
    AccountCheck, Activity, ActivityKind, BackupInfo, BackupOverview, BackupState, Call, ErrorCode,
    Event, FolderSuggestion, Hello, Issue, IssueAction, IssueKind, KnownFolder, RecoveryKit,
    RpcError, Settings, SetupResult, Status, SyncState, TransferDirection, TransferItem,
    TransferList, TransferMode, TransferState,
};
use rand::Rng;
use serde_json::{Value, json};
use tokio::sync::broadcast;

const GB: u64 = 1024 * 1024 * 1024;
const MB: u64 = 1024 * 1024;

/// Whether the mock was requested explicitly. Always false in release builds.
pub fn forced() -> bool {
    cfg!(debug_assertions)
        && std::env::var("DRIVE_UI_MOCK").is_ok_and(|v| !v.is_empty() && v != "0")
}

/// Whether to start with the long transfer list. Always false in release builds.
pub fn transfers_forced() -> bool {
    cfg!(debug_assertions) && std::env::var("DRIVE_UI_MOCK_TRANSFERS").is_ok_and(|v| v == "1")
}

/// Whether the update UI should be fed a made-up update. Always false in release builds.
pub fn update_forced() -> bool {
    cfg!(debug_assertions) && std::env::var("DRIVE_UI_MOCK_UPDATE").is_ok_and(|v| v == "1")
}

/// Whether to start with backed-up folders in every state. Always false in release builds.
fn backups_forced() -> bool {
    cfg!(debug_assertions) && std::env::var("DRIVE_UI_MOCK_BACKUPS").is_ok_and(|v| v == "1")
}

struct State {
    set_up: bool,
    paused: bool,
    tick: u64,
    next_id: i64,
    account: Option<String>,
    settings: Settings,
    activity: Vec<Activity>,
    issues: Vec<Issue>,
    pending_delete: Option<u32>,
    /// Size of the batch currently "transferring".
    batch: Vec<(&'static str, u64, bool)>,
    /// The long transfer list (`DRIVE_UI_MOCK_TRANSFERS=1`); replaces `batch` when set.
    transfers: Option<Vec<MockTransfer>>,
    backups: Vec<MockBackup>,
    suggestions_dismissed: bool,
    /// A backup was added at some point, so the vault has a `Backups/<device>` folder.
    backups_ever: bool,
    next_backup: u32,
}

/// A backed-up folder and how the mock moves it along.
struct MockBackup {
    info: BackupInfo,
    /// Ticks left while scanning.
    scan: u32,
    /// Ticks a full backup takes once scanning is done.
    ticks: u64,
    /// What the folder holds after skips (`info.files` grows towards it while scanning).
    total_files: u64,
    total_bytes: u64,
    /// Which sample file uploads next.
    cursor: usize,
    /// Switching between a full copy and on-demand.
    convert: Option<Convert>,
}

/// A folder switching modes: ticks left, and the bytes on this device it goes from and to.
struct Convert {
    ticks: u32,
    from: u64,
    to: u64,
}

/// Ticks a switch between a full copy and on-demand takes (about 5 s).
const CONVERT_TICKS: u32 = 7;
/// Free space the mock pretends this device has: making the on-demand Downloads a full copy
/// again doesn't fit.
const FREE_BYTES: u64 = 21 * GB;
const KEEP_DEFAULT: u64 = 5 * GB;

#[derive(Clone)]
struct MockTransfer {
    id: i64,
    path: &'static str,
    direction: TransferDirection,
    state: TransferState,
    mode: TransferMode,
    hold: Hold,
    done: u64,
    total: u64,
    /// Bytes per second while active.
    speed: u64,
    resumable: bool,
    /// Ticks left while preparing or verifying.
    countdown: u32,
    retry_at_ms: Option<i64>,
}

/// Why a waiting transfer can't start.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Hold {
    None,
    Locked,
    Offline,
}

pub struct Mock {
    state: Mutex<State>,
    events: broadcast::Sender<Event>,
}

const FILES: &[(&str, u64, bool)] = &[
    ("Documents/Budget 2026.xlsx", 84 * 1024, true),
    (
        "Photos/2026/Lisbon/IMG_4821.HEIC",
        3 * MB + 512 * 1024,
        true,
    ),
    (
        "Photos/2026/Lisbon/IMG_4822.HEIC",
        2 * MB + 900 * 1024,
        true,
    ),
    ("Projects/drive/Design review.pdf", 14 * MB, false),
    (
        "Music/Field recordings/Rain on tin roof.flac",
        38 * MB,
        false,
    ),
    ("Documents/Taxes/2025 return.pdf", 640 * 1024, true),
    ("Videos/Birthday.mp4", 212 * MB, true),
];

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn default_folder() -> String {
    crate::platform::default_folder()
}

impl Mock {
    pub fn new() -> Arc<Self> {
        let mode = std::env::var("DRIVE_UI_MOCK").unwrap_or_default();
        let set_up = mode != "onboarding";
        let mass = std::env::var("DRIVE_UI_MOCK_MASS_DELETE").is_ok_and(|v| v == "1");
        let now = now_ms();
        let activity = vec![
            act(
                5,
                now - 2 * 60_000,
                ActivityKind::Uploaded,
                "Documents/Meeting notes.docx",
                48_000,
            ),
            act(
                4,
                now - 26 * 60_000,
                ActivityKind::Downloaded,
                "Photos/2026/Lisbon/IMG_4790.HEIC",
                3_100_000,
            ),
            act(
                3,
                now - 3 * 3_600_000,
                ActivityKind::CreatedFolder,
                "Projects/Garden",
                0,
            ),
            act(
                2,
                now - 5 * 3_600_000,
                ActivityKind::Moved,
                "Documents/Lease.pdf",
                220_000,
            ),
            act(
                1,
                now - 26 * 3_600_000,
                ActivityKind::Conflict,
                "Notes/Shopping list.txt",
                900,
            ),
        ];
        let mut issues = vec![Issue {
            id: 1,
            kind: IssueKind::FileLocked,
            path: Some("Documents/Budget 2026.xlsx".into()),
            backup: None,
            message:
                "Budget 2026.xlsx is open in another app. Drive will upload it after you close it."
                    .into(),
            created_ms: now - 4 * 60_000,
            actions: vec![IssueAction::Retry, IssueAction::Dismiss],
        }];
        if mass {
            issues.push(mass_issue(2, 1_240, now));
        }
        let backups = if backups_forced() {
            rich_backups(now)
        } else {
            Vec::new()
        };
        let mut activity = activity;
        if backups_forced() {
            for (id, mins, kind, path) in [
                (
                    7,
                    1,
                    ActivityKind::Uploaded,
                    "Taxes/2025/Receipts/Pharmacy.pdf",
                ),
                (6, 9, ActivityKind::Deleted, "Drafts/Old cover letter.docx"),
            ] {
                let mut a = act(id, now - mins * 60_000, kind, path, 120_000);
                a.backup = Some("b1".into());
                activity.insert(0, a);
            }
        }
        let state = State {
            set_up,
            paused: false,
            tick: 0,
            next_id: 100,
            account: set_up.then(|| "alex.novak".into()),
            settings: Settings {
                folder: default_folder(),
                cache_limit_bytes: 20 * GB,
                poll_interval_secs: 60,
                upload_limit_kbps: 0,
                download_limit_kbps: 0,
                adaptive_upload: true,
                max_parallel_transfers: 4,
                launch_at_login: true,
                notifications: true,
                device_name: std::env::var("COMPUTERNAME").unwrap_or_else(|_| "This PC".into()),
                app_token_custom: false,
                close_action: Default::default(),
                auto_update: true,
                night_uploads: transfers_forced(),
                night_start: drive_core::ipc::default_night_start(),
                night_end: drive_core::ipc::default_night_end(),
                night_min_bytes: drive_core::ipc::default_night_min_bytes(),
            },
            activity,
            issues,
            pending_delete: mass.then_some(1_240),
            batch: Vec::new(),
            transfers: transfers_forced().then(|| rich_transfers(now)),
            backups_ever: !backups.is_empty(),
            backups,
            suggestions_dismissed: false,
            next_backup: 10,
        };
        let (events, _) = broadcast::channel(64);
        let mock = Arc::new(Self {
            state: Mutex::new(state),
            events,
        });
        let ticker = mock.clone();
        tauri::async_runtime::spawn(async move {
            let mut iv = tokio::time::interval(Duration::from_millis(700));
            loop {
                iv.tick().await;
                ticker.tick();
            }
        });
        mock
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    fn emit_status(&self, st: &State) {
        let _ = self.events.send(Event::Status(status_of(st)));
    }

    fn emit_transfers(&self, st: &State) {
        let _ = self.events.send(Event::Transfers(transfers_of(st)));
    }

    fn emit_backups(&self, st: &State) {
        let _ = self.events.send(Event::Backups(overview_of(st)));
    }

    /// Moves scanning and backing-up folders along by one tick (0.7 s).
    fn tick_backups(&self, st: &mut State) {
        let now = now_ms();
        let mut changed = false;
        let mut done = Vec::new();
        for b in st.backups.iter_mut() {
            match b.info.state {
                BackupState::Scanning => {
                    changed = true;
                    b.scan = b.scan.saturating_sub(1);
                    if b.scan == 0 {
                        b.info.files = b.total_files;
                        b.info.bytes = b.total_bytes;
                        b.info.state = BackupState::BackingUp;
                        b.info.detail = None;
                    } else {
                        let seen = b.total_files * u64::from(8 - b.scan.min(7)) / 8;
                        b.info.files = seen;
                        b.info.bytes = b.total_bytes * u64::from(8 - b.scan.min(7)) / 8;
                        b.info.detail = Some(format!("Looking through {} files…", thousands(seen)));
                    }
                    b.info.pending = (b.info.files - b.info.done_files.min(b.info.files)) as u32;
                }
                BackupState::Converting => {
                    changed = true;
                    let Some(c) = b.convert.as_mut() else {
                        continue;
                    };
                    c.ticks = c.ticks.saturating_sub(1);
                    let left = u64::from(c.ticks);
                    let total = u64::from(CONVERT_TICKS);
                    b.info.local_bytes = if c.to > c.from {
                        c.to - (c.to - c.from) * left / total
                    } else {
                        c.to + (c.from - c.to) * left / total
                    };
                    if !b.info.on_demand {
                        b.info.detail = Some(format!(
                            "Downloading {} back to this PC…",
                            size(b.info.bytes.saturating_sub(b.info.local_bytes))
                        ));
                    }
                    if c.ticks == 0 {
                        b.info.local_bytes = c.to;
                        b.convert = None;
                        b.info.detail = None;
                        b.info.state = if !b.info.enabled {
                            BackupState::Off
                        } else if b.info.done_files == b.info.files {
                            BackupState::UpToDate
                        } else {
                            BackupState::BackingUp
                        };
                    }
                }
                BackupState::BackingUp => {
                    changed = true;
                    let step = (b.total_files / b.ticks).max(1);
                    b.info.done_files = (b.info.done_files + step).min(b.info.files);
                    b.info.done_bytes = if b.info.done_files == b.info.files {
                        b.info.bytes
                    } else {
                        b.info.bytes / b.info.files.max(1) * b.info.done_files
                    };
                    b.info.pending = (b.info.files - b.info.done_files) as u32;
                    if b.info.done_files == b.info.files {
                        b.info.state = BackupState::UpToDate;
                        b.info.last_complete_ms = Some(now);
                    }
                    if b.cursor % 3 == 0 || b.info.state == BackupState::UpToDate {
                        done.push((b.info.id.clone(), sample_file(b.info.known, b.cursor / 3)));
                    }
                    b.cursor += 1;
                }
                _ => {}
            }
        }
        for (id, (path, bytes)) in done {
            st.next_id += 1;
            let mut a = act(st.next_id, now, ActivityKind::Uploaded, path, bytes);
            a.backup = Some(id);
            st.activity.insert(0, a.clone());
            st.activity.truncate(50);
            let _ = self.events.send(Event::Activity(a));
        }
        if changed {
            self.emit_backups(st);
            self.emit_status(st);
        }
    }

    /// Cycles between "syncing" (about 10 s) and "up to date" (about 12 s).
    fn tick(&self) {
        let mut st = self.state.lock().unwrap();
        if !st.set_up || st.paused {
            return;
        }
        st.tick += 1;
        self.tick_backups(&mut st);
        if st.transfers.is_some() {
            self.tick_transfers(&mut st);
            self.emit_status(&st);
            self.emit_transfers(&st);
            return;
        }
        let phase = st.tick % 32;
        if phase == 1 {
            let mut rng = rand::thread_rng();
            let start = rng.gen_range(0..FILES.len());
            let n = rng.gen_range(2..=4);
            st.batch = (0..n).map(|i| FILES[(start + i) % FILES.len()]).collect();
        }
        if phase == 15 {
            let now = now_ms();
            let batch = std::mem::take(&mut st.batch);
            for (path, bytes, up) in batch {
                st.next_id += 1;
                let a = act(
                    st.next_id,
                    now,
                    if up {
                        ActivityKind::Uploaded
                    } else {
                        ActivityKind::Downloaded
                    },
                    path,
                    bytes,
                );
                st.activity.insert(0, a.clone());
                let _ = self.events.send(Event::Activity(a));
            }
            st.activity.truncate(50);
        }
        if phase <= 15 {
            self.emit_status(&st);
            self.emit_transfers(&st);
        }
    }

    /// Moves the long transfer list along by one tick (0.7 s).
    fn tick_transfers(&self, st: &mut State) {
        let now = now_ms();
        let mut rng = rand::thread_rng();
        let mut finished = Vec::new();
        for t in st.transfers.iter_mut().flatten() {
            match t.state {
                TransferState::Preparing | TransferState::Verifying => {
                    t.countdown = t.countdown.saturating_sub(1);
                    if t.countdown == 0 {
                        if t.state == TransferState::Verifying {
                            finished.push(t.id);
                        } else {
                            t.state = TransferState::Queued;
                        }
                    }
                }
                TransferState::Active => {
                    let step = t.speed * 7 / 10 * rng.gen_range(80..=120) / 100;
                    t.done = (t.done + step).min(t.total);
                    if t.done == t.total {
                        if t.direction == TransferDirection::Upload {
                            t.state = TransferState::Verifying;
                            t.countdown = 3;
                        } else {
                            finished.push(t.id);
                        }
                    }
                }
                TransferState::Waiting if t.hold == Hold::Offline => {
                    if t.retry_at_ms.is_none_or(|at| at <= now) {
                        t.retry_at_ms = Some(now + 30_000);
                    }
                }
                _ => {}
            }
        }
        for id in finished {
            let list = st.transfers.get_or_insert_default();
            let Some(pos) = list.iter().position(|t| t.id == id) else {
                continue;
            };
            let mut t = list.remove(pos);
            st.next_id += 1;
            let kind = match t.direction {
                TransferDirection::Upload => ActivityKind::Uploaded,
                TransferDirection::Download => ActivityKind::Downloaded,
            };
            let a = act(st.next_id, now, kind, t.path, t.total);
            st.activity.insert(0, a.clone());
            st.activity.truncate(50);
            let _ = self.events.send(Event::Activity(a));
            // The same file comes back as a new change, so the demo never runs dry.
            t.id = st.next_id + 1000;
            t.done = 0;
            t.state = TransferState::Preparing;
            t.countdown = 4;
            st.transfers.get_or_insert_default().push(t);
        }
        // Two uploads and one download at a time, like a modest connection.
        let list = st.transfers.get_or_insert_default();
        for (dir, slots) in [
            (TransferDirection::Upload, 2),
            (TransferDirection::Download, 1),
        ] {
            let mut busy = list
                .iter()
                .filter(|t| t.direction == dir && moving(t.state))
                .count();
            for t in list.iter_mut() {
                if busy >= slots {
                    break;
                }
                if t.direction == dir && t.state == TransferState::Queued {
                    t.state = TransferState::Active;
                    busy += 1;
                }
            }
        }
    }

    pub async fn call(&self, call: Call) -> Result<Value, RpcError> {
        // A little latency makes loading states visible.
        let delay = match call {
            Call::CheckAccount(_) | Call::Setup(_) => 900,
            Call::ExportRecovery | Call::SignOut => 400,
            _ => 30,
        };
        tokio::time::sleep(Duration::from_millis(delay)).await;
        let mut st = self.state.lock().unwrap();
        let v = match call {
            Call::Hello => json!(Hello { protocol: drive_core::ipc::PROTOCOL_VERSION, version: "0.1.0-mock".into(), platform: std::env::consts::OS.into() }),
            Call::Status => json!(status_of(&st)),
            Call::Subscribe => Value::Null,
            Call::CheckAccount(c) => {
                if c.password == "wrong" || c.login.is_empty() {
                    return Err(err(ErrorCode::AuthFailed, "That login or password isn't right."));
                }
                let exists = !c.login.contains("new");
                json!(AccountCheck { login: c.login, vault_exists: exists, vault_has_files: exists })
            }
            Call::Setup(req) => {
                if st.set_up {
                    return Err(err(ErrorCode::AlreadySetUp, "Drive is already set up."));
                }
                let exists = !req.login.contains("new");
                if exists && req.encryption_password.as_deref() == Some("wrong") {
                    return Err(err(
                        ErrorCode::WrongEncryptionPassword,
                        "That encryption password doesn't open this vault.",
                    ));
                }
                let generated = (!exists && req.encryption_password.is_none()).then(generate_password);
                st.set_up = true;
                st.account = Some(req.login);
                if let Some(f) = req.folder {
                    st.settings.folder = f;
                }
                st.settings.app_token_custom = req.app_token.is_some_and(|t| !t.is_empty());
                st.tick = 0;
                self.emit_status(&st);
                json!(SetupResult { folder: st.settings.folder.clone(), generated_encryption_password: generated, created_vault: !exists })
            }
            Call::Pause => {
                st.paused = true;
                self.emit_status(&st);
                self.emit_backups(&st);
                Value::Null
            }
            Call::Resume => {
                st.paused = false;
                self.emit_status(&st);
                self.emit_backups(&st);
                Value::Null
            }
            Call::SyncNow => {
                st.tick = 0;
                Value::Null
            }
            Call::GetSettings => json!(st.settings),
            Call::UpdateSettings(p) => {
                let s = &mut st.settings;
                if let Some(v) = p.cache_limit_bytes { s.cache_limit_bytes = v }
                if let Some(v) = p.poll_interval_secs { s.poll_interval_secs = v }
                if let Some(v) = p.upload_limit_kbps { s.upload_limit_kbps = v }
                if let Some(v) = p.download_limit_kbps { s.download_limit_kbps = v }
                if let Some(v) = p.adaptive_upload { s.adaptive_upload = v }
                if let Some(v) = p.max_parallel_transfers { s.max_parallel_transfers = v }
                if let Some(v) = p.launch_at_login { s.launch_at_login = v }
                if let Some(v) = p.notifications { s.notifications = v }
                if let Some(v) = p.close_action { s.close_action = v }
                if let Some(v) = p.auto_update { s.auto_update = v }
                if let Some(v) = p.device_name { s.device_name = v }
                if let Some(v) = p.app_token { s.app_token_custom = !v.is_empty() }
                let start = p.night_start.as_deref().unwrap_or(&s.night_start);
                let end = p.night_end.as_deref().unwrap_or(&s.night_end);
                if parse_hhmm(start).is_none() || parse_hhmm(end).is_none() {
                    return Err(err(ErrorCode::BadRequest, "Enter a time like 01:00."));
                }
                if start == end {
                    return Err(err(ErrorCode::BadRequest, "Start and end can't be the same time. Change one of them."));
                }
                if let Some(v) = p.night_start { s.night_start = v }
                if let Some(v) = p.night_end { s.night_end = v }
                if let Some(v) = p.night_uploads { s.night_uploads = v }
                if let Some(v) = p.night_min_bytes { s.night_min_bytes = v.max(MB) }
                reschedule(&mut st);
                json!(st.settings)
            }
            Call::RecentActivity { limit } => json!(st.activity.iter().take(limit as usize).collect::<Vec<_>>()),
            Call::ListIssues => json!(st.issues),
            Call::ResolveIssue { id, action } => {
                let Some(pos) = st.issues.iter().position(|i| i.id == id) else {
                    return Err(err(ErrorCode::NotFound, "That issue was already resolved."));
                };
                if !st.issues[pos].actions.contains(&action) {
                    return Err(err(ErrorCode::BadRequest, "That action isn't available for this issue."));
                }
                let issue = st.issues.remove(pos);
                if issue.kind == IssueKind::MassDelete {
                    st.pending_delete = None;
                }
                let _ = self.events.send(Event::IssuesChanged { count: st.issues.len() as u32 });
                self.emit_status(&st);
                Value::Null
            }
            Call::SetPinned { .. } | Call::FreeUpSpace { .. } => Value::Null,
            Call::ExportRecovery => json!(RecoveryKit {
                rclone_config: "[drive-ulozto]\ntype = webdav\nurl = https://example.invalid\n\n[drive]\ntype = crypt\nremote = drive-ulozto:Drive\npassword = MOCK-OBSCURED\npassword2 = MOCK-OBSCURED\n".into(),
                encryption_password: "mock-password-not-real".into(),
                encryption_salt: "mock-salt-not-real".into(),
                instructions: "This is a mock recovery kit for UI development. It does not open any real vault.".into(),
            }),
            Call::SignOut => {
                st.set_up = false;
                st.account = None;
                st.issues.clear();
                st.pending_delete = None;
                self.emit_status(&st);
                Value::Null
            }
            Call::Shutdown => Value::Null,
            Call::Transfers => json!(transfers_of(&st)),
            Call::SetTransferMode { id, mode } => {
                set_transfer_mode(&mut st, id, mode)?;
                self.emit_status(&st);
                self.emit_transfers(&st);
                Value::Null
            }
            Call::Backups => json!(overview_of(&st)),
            Call::AddBackup { path } => {
                let info = add_backup(&mut st, &path)?;
                self.emit_backups(&st);
                self.emit_status(&st);
                json!(info)
            }
            Call::SetBackupEnabled { id, enabled } => {
                let b = backup_mut(&mut st, &id)?;
                b.info.enabled = enabled;
                b.info.state = match (enabled, b.info.state) {
                    (false, _) => BackupState::Off,
                    (true, BackupState::Problem) => BackupState::Problem,
                    (true, _) => {
                        // Looks through the folder again for what changed while it was off.
                        b.scan = 3;
                        BackupState::Scanning
                    }
                };
                if !enabled {
                    b.info.detail = None;
                }
                self.emit_backups(&st);
                self.emit_status(&st);
                Value::Null
            }
            Call::RemoveBackup { id, delete_from_cloud } => {
                let Some(pos) = st.backups.iter().position(|b| b.info.id == id) else {
                    return Err(err(ErrorCode::NotFound, "That folder isn't backed up anymore."));
                };
                let b = st.backups.remove(pos);
                tracing::info!(
                    "[mock] stopped backing up {} ({})",
                    b.info.name,
                    if delete_from_cloud { "cloud copy moved to the trash" } else { "cloud copy kept" }
                );
                self.emit_backups(&st);
                self.emit_status(&st);
                Value::Null
            }
            Call::SetBackupExcludes { id, excludes } => {
                let excludes = clean_excludes(excludes)?;
                let b = backup_mut(&mut st, &id)?;
                b.info.excludes = excludes;
                if b.info.enabled && b.info.state != BackupState::Problem {
                    b.info.state = BackupState::Scanning;
                    b.scan = 3;
                }
                let info = b.info.clone();
                self.emit_backups(&st);
                json!(info)
            }
            Call::SetBackupOnDemand {
                id,
                on_demand,
                keep_bytes,
            } => {
                let b = backup_mut(&mut st, &id)?;
                set_on_demand(b, on_demand, keep_bytes)?;
                let info = info_of(b);
                self.emit_backups(&st);
                json!(info)
            }
            Call::DismissBackupSuggestions => {
                st.suggestions_dismissed = true;
                self.emit_backups(&st);
                Value::Null
            }
        };
        Ok(v)
    }
}

fn err(code: ErrorCode, message: &str) -> RpcError {
    RpcError {
        code,
        message: message.into(),
    }
}

fn act(id: i64, at_ms: i64, kind: ActivityKind, path: &str, bytes: u64) -> Activity {
    let detail = (kind == ActivityKind::Conflict).then(|| "Both copies were kept.".to_string());
    Activity {
        id,
        at_ms,
        kind,
        path: path.into(),
        backup: None,
        bytes,
        detail,
    }
}

fn mass_issue(id: i64, count: u32, now: i64) -> Issue {
    Issue {
        id,
        kind: IssueKind::MassDelete,
        path: Some("Photos".into()),
        backup: None,
        message: format!(
            "{count} items were deleted from your Drive folder. Drive is waiting for you before deleting them from the cloud."
        ),
        created_ms: now - 60_000,
        actions: vec![IssueAction::ConfirmDelete, IssueAction::RestoreDeleted],
    }
}

fn generate_password() -> String {
    const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
    let mut rng = rand::thread_rng();
    (0..6)
        .map(|_| {
            (0..5)
                .map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char)
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("-")
}

fn status_of(st: &State) -> Status {
    if !st.set_up {
        return Status {
            state: SyncState::NotSetUp,
            headline: "Not set up".into(),
            ..Default::default()
        };
    }
    let phase = st.tick % 32;
    let (syncing, total, done, up, down) = if let Some(list) = &st.transfers {
        let moving: Vec<_> = list.iter().filter(|t| moving(t.state)).collect();
        let up = moving
            .iter()
            .filter(|t| t.direction == TransferDirection::Upload)
            .count() as u32;
        (
            !st.paused && !moving.is_empty(),
            moving.iter().map(|t| t.total).sum::<u64>(),
            moving.iter().map(|t| t.done).sum::<u64>(),
            up,
            moving.len() as u32 - up,
        )
    } else {
        let syncing = !st.paused && (1..15).contains(&phase) && !st.batch.is_empty();
        let total: u64 = st.batch.iter().map(|b| b.1).sum();
        let up = st.batch.iter().filter(|b| b.2).count() as u32;
        let done = if syncing { total * phase / 15 } else { 0 };
        (syncing, total, done, up, st.batch.len() as u32 - up)
    };
    let backing_up: Vec<&MockBackup> = st
        .backups
        .iter()
        .filter(|b| b.info.state == BackupState::BackingUp)
        .collect();
    let (state, headline) = if st.paused {
        (SyncState::Paused, "Syncing paused".to_string())
    } else if !syncing && !backing_up.is_empty() {
        (
            SyncState::Syncing,
            format!("Backing up {}…", backing_up[0].info.name),
        )
    } else if syncing {
        let n = (up + down) as usize;
        (
            SyncState::Syncing,
            format!("Syncing {n} {}…", if n == 1 { "file" } else { "files" }),
        )
    } else {
        (SyncState::UpToDate, "Up to date".to_string())
    };
    Status {
        state,
        headline,
        account: st.account.clone(),
        folder: Some(st.settings.folder.clone()),
        uploading: if syncing {
            up
        } else if st.paused {
            0
        } else {
            backing_up.len() as u32
        },
        downloading: if syncing { down } else { 0 },
        pending: if syncing { up + down } else { 0 },
        transfer_bytes_total: if syncing { total } else { 0 },
        transfer_bytes_done: done,
        issues: st.issues.len() as u32,
        cache_bytes: 6 * GB + 300 * MB,
        cache_limit_bytes: st.settings.cache_limit_bytes,
        pinned_bytes: 2 * GB + 150 * MB,
        cloud_bytes: 148 * GB + 700 * MB,
        cloud_files: 23_418,
        last_sync_ms: Some(now_ms() - 40_000),
        pending_delete_confirmation: st.pending_delete,
    }
}

// ---- transfers ---------------------------------------------------------------------------

/// Preparing, sending or verifying: holds a transfer slot.
fn moving(s: TransferState) -> bool {
    matches!(
        s,
        TransferState::Preparing | TransferState::Active | TransferState::Verifying
    )
}

/// Sort order of the list: moving, then waiting, then held.
fn rank(s: TransferState) -> u8 {
    match s {
        TransferState::Active | TransferState::Verifying | TransferState::Preparing => 0,
        TransferState::Queued | TransferState::Waiting => 1,
        TransferState::Paused | TransferState::Scheduled => 2,
    }
}

fn parse_hhmm(s: &str) -> Option<(i64, i64)> {
    let (h, m) = s.split_once(':')?;
    let (h, m): (i64, i64) = (h.parse().ok()?, m.parse().ok()?);
    ((0..24).contains(&h) && (0..60).contains(&m) && s.len() == 5).then_some((h, m))
}

/// The next time the overnight window opens. The mock has no time zone database, so this is
/// UTC; the rows' reasons quote the configured time, as the real service does.
fn next_window_ms(settings: &Settings, now: i64) -> Option<i64> {
    const DAY: i64 = 86_400_000;
    let (h, m) = parse_hhmm(&settings.night_start)?;
    let mut t = now - now.rem_euclid(DAY) + (h * 60 + m) * 60_000;
    if t <= now {
        t += DAY;
    }
    Some(t)
}

fn upload(
    id: i64,
    path: &'static str,
    total: u64,
    done: u64,
    state: TransferState,
) -> MockTransfer {
    MockTransfer {
        id,
        path,
        direction: TransferDirection::Upload,
        state,
        mode: TransferMode::Auto,
        hold: Hold::None,
        done,
        total,
        speed: 2 * MB + 100 * 1024,
        resumable: total > 8 * MB,
        countdown: 4,
        retry_at_ms: None,
    }
}

fn rich_transfers(now: i64) -> Vec<MockTransfer> {
    use TransferState::*;
    vec![
        upload(
            201,
            "Videos/2026/Birthday party at the lake house (full length, 4K).mp4",
            1_640 * MB,
            45 * MB,
            Active,
        ),
        MockTransfer {
            speed: 900 * 1024,
            ..upload(
                202,
                "Photos/2026/Lisbon/IMG_4822.HEIC",
                3 * MB + 200 * 1024,
                0,
                Active,
            )
        },
        MockTransfer {
            direction: TransferDirection::Download,
            speed: 6 * MB,
            ..upload(
                203,
                "Projects/drive/Design review.pdf",
                84 * MB,
                30 * MB,
                Active,
            )
        },
        MockTransfer {
            countdown: 6,
            ..upload(
                204,
                "Documents/Taxes/2025 return.pdf",
                640 * 1024,
                640 * 1024,
                Verifying,
            )
        },
        MockTransfer {
            countdown: 8,
            ..upload(
                205,
                "Documents/Scans/Passport and ID card.pdf",
                2 * MB,
                0,
                Preparing,
            )
        },
        upload(
            206,
            "Music/Field recordings/Rain on tin roof.flac",
            38 * MB,
            0,
            Queued,
        ),
        MockTransfer {
            hold: Hold::Locked,
            ..upload(207, "Documents/Budget 2026.xlsx", 84 * 1024, 0, Waiting)
        },
        MockTransfer {
            hold: Hold::Offline,
            retry_at_ms: Some(now + 25_000),
            ..upload(
                208,
                "Projects/Garden/Site plan.dwg",
                12 * MB,
                5 * MB,
                Waiting,
            )
        },
        MockTransfer {
            mode: TransferMode::Paused,
            ..upload(
                209,
                "Backups/Laptop image 2026-11.vhdx",
                48 * GB,
                12 * GB + 400 * MB,
                Paused,
            )
        },
        upload(
            210,
            "Videos/Drone/Coast flight.mov",
            3 * GB + 200 * MB,
            0,
            Scheduled,
        ),
        upload(
            211,
            "Videos/Drone/Harbor at dusk.mov",
            2 * GB + 400 * MB,
            600 * MB,
            Scheduled,
        ),
    ]
}

fn waits_for_night(settings: &Settings, t: &MockTransfer) -> bool {
    t.direction == TransferDirection::Upload
        && match t.mode {
            TransferMode::Tonight => true,
            TransferMode::Auto => settings.night_uploads && t.total >= settings.night_min_bytes,
            TransferMode::Paused | TransferMode::Now => false,
        }
}

/// After a settings change: hold or release the uploads that follow the overnight setting.
fn reschedule(st: &mut State) {
    let settings = st.settings.clone();
    for t in st.transfers.iter_mut().flatten() {
        if t.mode != TransferMode::Auto {
            continue;
        }
        let hold = waits_for_night(&settings, t);
        if hold && matches!(t.state, TransferState::Queued | TransferState::Active) {
            t.state = TransferState::Scheduled;
        } else if !hold && t.state == TransferState::Scheduled {
            t.state = TransferState::Queued;
        }
    }
}

fn set_transfer_mode(st: &mut State, id: i64, mode: TransferMode) -> Result<(), RpcError> {
    let settings = st.settings.clone();
    let Some(t) = st.transfers.iter_mut().flatten().find(|t| t.id == id) else {
        return Err(err(
            ErrorCode::NotFound,
            "That transfer has already finished.",
        ));
    };
    if t.direction == TransferDirection::Download {
        return Err(err(
            ErrorCode::BadRequest,
            "Downloads can't be paused or scheduled one at a time.",
        ));
    }
    t.mode = mode;
    t.hold = Hold::None;
    t.retry_at_ms = None;
    t.state = match mode {
        TransferMode::Paused => TransferState::Paused,
        TransferMode::Now => TransferState::Active,
        _ if waits_for_night(&settings, t) => TransferState::Scheduled,
        _ => TransferState::Queued,
    };
    Ok(())
}

fn item_of(t: &MockTransfer, settings: &Settings, now: i64) -> TransferItem {
    let rate = if t.state == TransferState::Active {
        t.speed
    } else {
        0
    };
    let reason = match (t.state, t.hold) {
        (TransferState::Waiting, Hold::Locked) => {
            Some("Open in another app — Drive will upload it once it's closed".to_string())
        }
        (TransferState::Waiting, Hold::Offline) => {
            let secs = ((t.retry_at_ms.unwrap_or(now) - now) / 1000).max(1);
            Some(format!("Offline — retrying in {secs} s"))
        }
        (TransferState::Scheduled, _) => {
            Some(format!("Waiting for tonight ({})", settings.night_start))
        }
        _ => None,
    };
    TransferItem {
        id: t.id,
        path: t.path.into(),
        direction: t.direction,
        state: t.state,
        reason,
        bytes_done: t.done,
        bytes_total: t.total,
        rate_bps: rate,
        eta_secs: (rate > 0).then(|| (t.total - t.done) / rate),
        retry_at_ms: t.retry_at_ms,
        resumable: t.resumable,
        mode: t.mode,
    }
}

/// The short batch as a transfer list: everything moves at once, in step with the status.
fn batch_items(st: &State) -> Vec<TransferItem> {
    let phase = st.tick % 32;
    if st.paused || !(1..15).contains(&phase) {
        return Vec::new();
    }
    st.batch
        .iter()
        .enumerate()
        .map(|(i, &(path, total, up))| {
            let rate = total / 10 + 1;
            let done = total * phase / 15;
            TransferItem {
                id: 100 + i as i64,
                path: path.into(),
                direction: if up {
                    TransferDirection::Upload
                } else {
                    TransferDirection::Download
                },
                state: TransferState::Active,
                reason: None,
                bytes_done: done,
                bytes_total: total,
                rate_bps: rate,
                eta_secs: Some((total - done) / rate),
                retry_at_ms: None,
                resumable: false,
                mode: TransferMode::Auto,
            }
        })
        .collect()
}

fn transfers_of(st: &State) -> TransferList {
    let now = now_ms();
    let mut items = match &st.transfers {
        Some(list) => list.iter().map(|t| item_of(t, &st.settings, now)).collect(),
        None => batch_items(st),
    };
    items.sort_by_key(|t| rank(t.state));
    let uploads: Vec<&TransferItem> = items
        .iter()
        .filter(|t| t.direction == TransferDirection::Upload)
        .collect();
    let upload_rate: u64 = uploads.iter().map(|t| t.rate_bps).sum();
    let remaining: u64 = uploads
        .iter()
        .filter(|t| rank(t.state) < 2)
        .map(|t| t.bytes_total - t.bytes_done)
        .sum();
    let held = st.settings.night_uploads || uploads.iter().any(|t| t.mode == TransferMode::Tonight);
    TransferList {
        total_count: items.len() as u32,
        upload_rate_bps: upload_rate,
        download_rate_bps: items
            .iter()
            .filter(|t| t.direction == TransferDirection::Download)
            .map(|t| t.rate_bps)
            .sum(),
        upload_remaining_bytes: remaining,
        upload_eta_secs: (upload_rate > 0).then(|| remaining / upload_rate),
        night_window_opens_ms: if held {
            next_window_ms(&st.settings, now)
        } else {
            None
        },
        night_window_open: false,
        // The transfers scenario shows uploads slowed to keep the connection responsive.
        auto_upload_limit_bps: (st.settings.adaptive_upload
            && upload_rate > 0
            && transfers_forced())
        .then_some(1_450_000),
        auto_upload_checking: false,
        items,
    }
}

// ---- backups -----------------------------------------------------------------------------

const DEFAULT_EXCLUDES: &[&str] = &[
    "node_modules",
    "__pycache__",
    ".cache",
    ".gradle",
    ".tox",
    "bower_components",
    "*.tmp",
    "~$*",
    "Thumbs.db",
    "desktop.ini",
    ".DS_Store",
];

/// 12400 → "12,400".
fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The well-known folders of this user with made-up sizes (after the default skips).
/// A well-known folder: which one, its name, where it is, and (files, bytes) once measured.
type Known = (KnownFolder, &'static str, PathBuf, Option<(u64, u64)>);

fn known_folders() -> Vec<Known> {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("C:\\Users\\alex"));
    let dir = |d: Option<PathBuf>, name: &str| d.unwrap_or_else(|| home.join(name));
    vec![
        (
            KnownFolder::Desktop,
            "Desktop",
            dir(dirs::desktop_dir(), "Desktop"),
            Some((449, 410 * MB)),
        ),
        (
            KnownFolder::Documents,
            "Documents",
            dir(dirs::document_dir(), "Documents"),
            Some((98_000, 14 * GB)),
        ),
        (
            KnownFolder::Downloads,
            "Downloads",
            dir(dirs::download_dir(), "Downloads"),
            Some((400, 22 * GB)),
        ),
        (
            KnownFolder::Pictures,
            "Pictures",
            dir(dirs::picture_dir(), "Pictures"),
            Some((3_120, 6 * GB + 300 * MB)),
        ),
        // Still being measured.
        (
            KnownFolder::Videos,
            "Videos",
            dir(dirs::video_dir(), "Videos"),
            None,
        ),
    ]
}

/// A file the mock "backs up" next, relative to the backed-up folder.
fn sample_file(known: Option<KnownFolder>, i: usize) -> (&'static str, u64) {
    const DOCS: &[(&str, u64)] = &[
        ("Taxes/2025/Receipts/Pharmacy.pdf", 220_000),
        ("Thesis/Chapter 3 - Methods.docx", 1_400_000),
        ("Recipes/Grandma's apple cake.txt", 3_000),
        ("Insurance/Home policy 2026.pdf", 640_000),
    ];
    const PICS: &[(&str, u64)] = &[
        ("2026/Lisbon/IMG_4901.HEIC", 3_400_000),
        ("2026/Lisbon/IMG_4902.HEIC", 3_100_000),
        ("Screenshots/Screenshot 2026-11-10 093112.png", 420_000),
    ];
    const MISC: &[(&str, u64)] = &[
        ("Notes.txt", 2_000),
        ("Project plan.xlsx", 88_000),
        ("Setup files/installer.msi", 48 * MB),
    ];
    let list = match known {
        Some(KnownFolder::Pictures | KnownFolder::Videos) => PICS,
        Some(KnownFolder::Documents) => DOCS,
        _ => MISC,
    };
    list[i % list.len()]
}

fn backup_info(
    id: &str,
    name: &str,
    path: &Path,
    known: Option<KnownFolder>,
    now: i64,
) -> BackupInfo {
    BackupInfo {
        id: id.into(),
        name: name.into(),
        path: path.to_string_lossy().into_owned(),
        known,
        enabled: true,
        state: BackupState::Scanning,
        detail: Some("Looking through the folder…".into()),
        excludes: DEFAULT_EXCLUDES.iter().map(|s| s.to_string()).collect(),
        files: 0,
        bytes: 0,
        done_files: 0,
        done_bytes: 0,
        pending: 0,
        skipped_online_only: 0,
        added_ms: now,
        last_complete_ms: None,
        on_demand: false,
        on_demand_available: cfg!(windows),
        local_bytes: 0,
        keep_bytes: KEEP_DEFAULT,
    }
}

/// 97_066_215_833 → "90.4 GB", like the app's `bytes()`.
fn size(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 || v >= 100.0 {
        format!("{v:.0} {}", UNITS[u])
    } else {
        let one = format!("{v:.1}");
        format!("{} {}", one.trim_end_matches(".0"), UNITS[u])
    }
}

/// What the overview shows for a folder: a full copy is all on this device.
fn info_of(b: &MockBackup) -> BackupInfo {
    let mut info = b.info.clone();
    if !info.on_demand && info.state != BackupState::Converting {
        info.local_bytes = info.bytes;
    }
    info
}

/// `SetBackupOnDemand`: starts switching modes, or changes how much stays on this device.
fn set_on_demand(
    b: &mut MockBackup,
    on_demand: bool,
    keep_bytes: Option<u64>,
) -> Result<(), RpcError> {
    let i = &mut b.info;
    if !i.on_demand_available {
        return Err(err(
            ErrorCode::BadRequest,
            "Saving space is available on Windows only for now.",
        ));
    }
    if i.state == BackupState::Converting {
        return Err(err(
            ErrorCode::BadRequest,
            &format!(
                "Drive is still switching {}. Try again when it's done.",
                i.name
            ),
        ));
    }
    let local = if i.on_demand { i.local_bytes } else { i.bytes };
    if on_demand {
        i.keep_bytes = keep_bytes.unwrap_or(i.keep_bytes);
        if i.on_demand {
            // A smaller limit frees space right away; a larger one fills up as files are opened.
            i.local_bytes = i.local_bytes.min(i.keep_bytes);
        } else {
            i.on_demand = true;
            i.state = BackupState::Converting;
            i.detail = Some(format!("Freeing up space in {}…", i.name));
            b.convert = Some(Convert {
                ticks: CONVERT_TICKS,
                from: local,
                to: i.bytes.min(i.keep_bytes),
            });
        }
    } else if i.on_demand {
        let missing = i.bytes.saturating_sub(local);
        if missing > FREE_BYTES {
            return Err(err(
                ErrorCode::BadRequest,
                &format!(
                    "Drive needs {} to download {} back, but this PC has only {} free. Free up space, then try again.",
                    size(missing),
                    i.name,
                    size(FREE_BYTES)
                ),
            ));
        }
        i.on_demand = false;
        i.state = BackupState::Converting;
        i.detail = Some(format!("Downloading {} back to this PC…", size(missing)));
        b.convert = Some(Convert {
            ticks: CONVERT_TICKS,
            from: local,
            to: i.bytes,
        });
    }
    Ok(())
}

fn rich_backups(now: i64) -> Vec<MockBackup> {
    let known = known_folders();
    let find = |k: KnownFolder| known.iter().find(|f| f.0 == k).unwrap();
    let mut out = Vec::new();

    let (k, name, path, _) = find(KnownFolder::Documents);
    let mut info = backup_info("b1", name, path, Some(*k), now - 3_600_000);
    info.state = BackupState::BackingUp;
    info.detail = None;
    info.files = 98_000;
    info.bytes = 14 * GB;
    info.done_files = 1_204;
    info.done_bytes = 900 * MB;
    info.pending = 98_000 - 1_204;
    info.skipped_online_only = 12;
    out.push(MockBackup {
        info,
        scan: 0,
        ticks: 400,
        total_files: 98_000,
        total_bytes: 14 * GB,
        cursor: 0,
        convert: None,
    });

    let (k, name, path, _) = find(KnownFolder::Desktop);
    let mut info = backup_info("b2", name, path, Some(*k), now - 2 * 86_400_000);
    info.state = BackupState::UpToDate;
    info.detail = None;
    info.files = 449;
    info.bytes = 410 * MB;
    info.done_files = 449;
    info.done_bytes = 410 * MB;
    info.last_complete_ms = Some(now - 5 * 60_000);
    out.push(MockBackup {
        info,
        scan: 0,
        ticks: 20,
        total_files: 449,
        total_bytes: 410 * MB,
        cursor: 0,
        convert: None,
    });

    let (k, name, path, _) = find(KnownFolder::Pictures);
    let mut info = backup_info("b3", name, path, Some(*k), now - 9 * 86_400_000);
    info.enabled = false;
    info.state = BackupState::Off;
    info.detail = None;
    info.files = 3_120;
    info.bytes = 6 * GB + 300 * MB;
    info.done_files = 3_120;
    info.done_bytes = 6 * GB + 300 * MB;
    info.last_complete_ms = Some(now - 3 * 86_400_000);
    out.push(MockBackup {
        info,
        scan: 0,
        ticks: 20,
        total_files: 3_120,
        total_bytes: 6 * GB + 300 * MB,
        cursor: 0,
        convert: None,
    });

    // Saves space on this device: most of it is only in the cloud.
    let (k, name, path, _) = find(KnownFolder::Downloads);
    let mut info = backup_info("b5", name, path, Some(*k), now - 6 * 86_400_000);
    let downloads = 95 * GB + 600 * MB;
    info.state = BackupState::UpToDate;
    info.detail = None;
    info.files = 2_310;
    info.bytes = downloads;
    info.done_files = 2_310;
    info.done_bytes = downloads;
    info.on_demand = info.on_demand_available;
    info.local_bytes = 4 * GB + 800 * MB;
    info.last_complete_ms = Some(now - 4 * 60_000);
    out.push(MockBackup {
        info,
        scan: 0,
        ticks: 20,
        total_files: 2_310,
        total_bytes: downloads,
        cursor: 0,
        convert: None,
    });

    let mut info = backup_info(
        "b4",
        "Old projects",
        Path::new("D:\\Old projects"),
        None,
        now - 30 * 86_400_000,
    );
    info.state = BackupState::Problem;
    info.detail =
        Some("Folder not found. If it's on a removable drive, plug the drive back in.".into());
    info.files = 12_400;
    info.bytes = 31 * GB;
    info.done_files = 12_400;
    info.done_bytes = 31 * GB;
    info.last_complete_ms = Some(now - 6 * 86_400_000);
    out.push(MockBackup {
        info,
        scan: 0,
        ticks: 20,
        total_files: 12_400,
        total_bytes: 31 * GB,
        cursor: 0,
        convert: None,
    });
    out
}

fn overview_of(st: &State) -> BackupOverview {
    let mut folders: Vec<BackupInfo> = st.backups.iter().map(info_of).collect();
    if st.paused {
        for f in &mut folders {
            if matches!(f.state, BackupState::Scanning | BackupState::BackingUp) {
                f.state = BackupState::Waiting;
                f.detail = Some("Syncing is paused. The backup continues when you resume.".into());
            }
        }
    }
    let drive = PathBuf::from(&st.settings.folder);
    let mut suggestions: Vec<FolderSuggestion> = known_folders()
        .into_iter()
        .filter(|(k, ..)| !st.backups.iter().any(|b| b.info.known == Some(*k)))
        .map(|(known, name, path, size)| FolderSuggestion {
            known,
            name: name.into(),
            blocked: drive
                .starts_with(&path)
                .then(|| "Contains your Drive folder".to_string()),
            path: path.to_string_lossy().into_owned(),
            files: size.map(|s| s.0),
            bytes: size.map(|s| s.1),
        })
        .collect();
    // Largest first; still measuring last.
    suggestions.sort_by_key(|s| std::cmp::Reverse(s.bytes.map(|b| b + 1).unwrap_or(0)));
    BackupOverview {
        folders,
        suggestions,
        suggestions_dismissed: st.suggestions_dismissed,
        cloud_folder: st
            .backups_ever
            .then(|| format!("Backups/{}", st.settings.device_name)),
        default_excludes: DEFAULT_EXCLUDES.iter().map(|s| s.to_string()).collect(),
    }
}

fn backup_mut<'a>(st: &'a mut State, id: &str) -> Result<&'a mut MockBackup, RpcError> {
    st.backups
        .iter_mut()
        .find(|b| b.info.id == id)
        .ok_or_else(|| err(ErrorCode::NotFound, "That folder isn't backed up anymore."))
}

fn clean_excludes(excludes: Vec<String>) -> Result<Vec<String>, RpcError> {
    let mut out: Vec<String> = Vec::new();
    for e in excludes {
        let e = e.trim().to_string();
        if e.is_empty() || out.contains(&e) {
            continue;
        }
        if e.contains(['/', '\\']) {
            return Err(err(
                ErrorCode::BadRequest,
                &format!(
                    "\"{e}\" has a slash. Enter just a name, like node_modules, or a pattern like *.tmp."
                ),
            ));
        }
        out.push(e);
    }
    Ok(out)
}

fn add_backup(st: &mut State, raw: &str) -> Result<BackupInfo, RpcError> {
    let path = PathBuf::from(raw.trim());
    if raw.trim().is_empty() || !path.is_absolute() {
        return Err(err(ErrorCode::BadRequest, "Choose a folder to back up."));
    }
    let drive = PathBuf::from(&st.settings.folder);
    if path.starts_with(&drive) {
        return Err(err(
            ErrorCode::BadRequest,
            "That folder is inside your Drive folder, so it's already in the cloud.",
        ));
    }
    if drive.starts_with(&path) {
        return Err(err(
            ErrorCode::BadRequest,
            "That folder contains your Drive folder. Choose a folder inside it instead, like Documents.",
        ));
    }
    for b in &st.backups {
        let other = Path::new(&b.info.path);
        if other == path {
            return Err(err(
                ErrorCode::BadRequest,
                "That folder is already backed up.",
            ));
        }
        if path.starts_with(other) {
            return Err(err(
                ErrorCode::BadRequest,
                &format!(
                    "That folder is inside {}, which is already backed up.",
                    b.info.name
                ),
            ));
        }
        if other.starts_with(&path) {
            return Err(err(
                ErrorCode::BadRequest,
                &format!(
                    "That folder contains {}, which is already backed up. Stop backing up {} first.",
                    b.info.name, b.info.name
                ),
            ));
        }
    }
    let known = known_folders().into_iter().find(|f| f.2 == path);
    let base = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| raw.trim().to_string());
    let mut name = base.clone();
    let mut n = 2;
    while st.backups.iter().any(|b| b.info.name == name) {
        name = format!("{base} ({n})");
        n += 1;
    }
    let (total_files, total_bytes) = match known.as_ref().and_then(|k| k.3) {
        Some(size) => size,
        None => {
            let mut rng = rand::thread_rng();
            (rng.gen_range(800..20_000), rng.gen_range(1..8) * GB)
        }
    };
    st.next_backup += 1;
    let info = backup_info(
        &format!("b{}", st.next_backup),
        &name,
        &path,
        known.map(|k| k.0),
        now_ms(),
    );
    st.backups.push(MockBackup {
        info: info.clone(),
        scan: 7,
        ticks: 22,
        total_files,
        total_bytes,
        cursor: 0,
        convert: None,
    });
    st.backups_ever = true;
    Ok(info)
}
