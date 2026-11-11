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
//! * `DRIVE_UI_MOCK_UPDATE=1` offers a made-up update (see `updater`); installing it only
//!   pretends. Works with or without the mock service.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use drive_core::ipc::{
    AccountCheck, Activity, ActivityKind, Call, ErrorCode, Event, Hello, Issue, IssueAction,
    IssueKind, RecoveryKit, RpcError, Settings, SetupResult, Status, SyncState, TransferDirection,
    TransferItem, TransferList, TransferMode, TransferState,
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
}

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
            message:
                "Budget 2026.xlsx is open in another app. Drive will upload it after you close it."
                    .into(),
            created_ms: now - 4 * 60_000,
            actions: vec![IssueAction::Retry, IssueAction::Dismiss],
        }];
        if mass {
            issues.push(mass_issue(2, 1_240, now));
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

    /// Cycles between "syncing" (about 10 s) and "up to date" (about 12 s).
    fn tick(&self) {
        let mut st = self.state.lock().unwrap();
        if !st.set_up || st.paused {
            return;
        }
        st.tick += 1;
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
                Value::Null
            }
            Call::Resume => {
                st.paused = false;
                self.emit_status(&st);
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
        bytes,
        detail,
    }
}

fn mass_issue(id: i64, count: u32, now: i64) -> Issue {
    Issue {
        id,
        kind: IssueKind::MassDelete,
        path: Some("Photos".into()),
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
    let (state, headline) = if st.paused {
        (SyncState::Paused, "Syncing paused".to_string())
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
        uploading: if syncing { up } else { 0 },
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
        items,
    }
}
