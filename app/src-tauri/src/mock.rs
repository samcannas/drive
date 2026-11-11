//! A fake Drive service for developing and screenshotting the UI without the real one.
//!
//! Enabled by `DRIVE_UI_MOCK=1`, or automatically in debug builds when the service can't be
//! reached. Release builds never use it: [`forced`] is always false there and the automatic
//! fallback in `backend` is debug-only.
//!
//! Optional knobs (debug builds only):
//! * `DRIVE_UI_MOCK=onboarding` starts in the not-set-up state.
//! * `DRIVE_UI_MOCK_MASS_DELETE=1` starts with a pending mass-delete confirmation.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use drive_core::ipc::{
    AccountCheck, Activity, ActivityKind, Call, ErrorCode, Event, Hello, Issue, IssueAction,
    IssueKind, RecoveryKit, RpcError, Settings, SetupResult, Status, SyncState,
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
            },
            activity,
            issues,
            pending_delete: mass.then_some(1_240),
            batch: Vec::new(),
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

    /// Cycles between "syncing" (about 10 s) and "up to date" (about 12 s).
    fn tick(&self) {
        let mut st = self.state.lock().unwrap();
        if !st.set_up || st.paused {
            return;
        }
        st.tick += 1;
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
                if let Some(v) = p.device_name { s.device_name = v }
                if let Some(v) = p.app_token { s.app_token_custom = !v.is_empty() }
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
    let syncing = !st.paused && (1..15).contains(&phase) && !st.batch.is_empty();
    let total: u64 = st.batch.iter().map(|b| b.1).sum();
    let done = if syncing { total * phase / 15 } else { 0 };
    let up = st.batch.iter().filter(|b| b.2).count() as u32;
    let down = st.batch.len() as u32 - up;
    let (state, headline) = if st.paused {
        (SyncState::Paused, "Syncing paused".to_string())
    } else if syncing {
        let n = st.batch.len();
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
        pending: if syncing { st.batch.len() as u32 } else { 0 },
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
