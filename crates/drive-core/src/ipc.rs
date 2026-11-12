//! Protocol between the background service (`drive daemon`) and its clients (tray app, CLI).
//!
//! Transport: a per-user local socket (a named pipe on Windows, a Unix socket in
//! `$XDG_RUNTIME_DIR` on Linux) carrying newline-delimited JSON.
//!
//! * Client → service: one [`Request`] per line.
//! * Service → client: [`Response`] lines (matched by `id`) and, after `subscribe`,
//!   [`Event`] lines (`{"event": ...}`) at any time.

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

/// Name of the per-user socket. Callers turn it into a platform socket name.
pub fn socket_name() -> String {
    let user = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "user".into());
    let user: String = user
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    // A separate instance (`DRIVE_HOME`, used for development and tests) gets its own socket,
    // so it never talks to — or is blocked by — the real Drive of the same user.
    match std::env::var("DRIVE_HOME") {
        Ok(home) if !home.is_empty() => {
            use sha2::Digest as _;
            let h = sha2::Sha256::digest(home.as_bytes());
            format!("drive-{user}-{}.sock", hex::encode(&h[..4]))
        }
        _ => format!("drive-{user}.sock"),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    #[serde(flatten)]
    pub call: Call,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Call {
    /// Handshake; returns [`Hello`].
    Hello,
    /// Current [`Status`].
    Status,
    /// Start streaming [`Event`]s on this connection.
    Subscribe,
    /// Connect an account for the first time. Also used to sign in again when the state is
    /// `signed_out` (same login: new uloz.to password; the stored encryption password is kept
    /// unless a new one is given).
    Setup(SetupRequest),
    /// Checks credentials without saving anything; returns [`AccountCheck`].
    CheckAccount(AccountCredentials),
    Pause,
    Resume,
    /// Rescan the remote now instead of waiting for the next poll.
    SyncNow,
    GetSettings,
    /// Partial update; omitted fields are unchanged. Returns the new [`Settings`].
    UpdateSettings(SettingsPatch),
    /// Most recent first; returns `Vec<Activity>`.
    RecentActivity {
        limit: u32,
    },
    /// Returns `Vec<Issue>`.
    ListIssues,
    ResolveIssue {
        id: i64,
        action: IssueAction,
    },
    /// Keep a file or folder (recursively) downloaded on this device, or release it.
    SetPinned {
        path: String,
        pinned: bool,
    },
    /// Evict the downloaded copy of a file or folder (it stays in the cloud).
    FreeUpSpace {
        path: String,
    },
    /// Returns [`RecoveryKit`]: an rclone config that opens the vault without Drive.
    ExportRecovery,
    /// Disconnects the account and forgets credentials. Local placeholders are removed.
    SignOut,
    /// Stops the service.
    Shutdown,
    /// Every upload and download that is running, queued or held, with the reason for anything
    /// that isn't moving. Returns [`TransferList`].
    Transfers,
    /// Pause, resume, defer to tonight or force a single upload (by its [`TransferItem::id`]).
    SetTransferMode {
        id: i64,
        mode: TransferMode,
    },
    /// Backed-up folders and suggestions. Returns [`BackupOverview`].
    Backups,
    /// Starts backing up a folder (absolute path). Returns [`BackupInfo`].
    AddBackup {
        path: String,
    },
    /// The per-folder off switch: pauses (keeps the cloud copy) or resumes a backup.
    SetBackupEnabled {
        id: String,
        enabled: bool,
    },
    /// Stops backing up a folder for good. The folder on this device is never touched; the
    /// cloud copy is kept unless `delete_from_cloud` (then it goes to the uloz.to trash).
    RemoveBackup {
        id: String,
        #[serde(default)]
        delete_from_cloud: bool,
    },
    /// Replaces a backup's skip list (see [`BackupInfo::excludes`]). Returns [`BackupInfo`].
    SetBackupExcludes {
        id: String,
        excludes: Vec<String>,
    },
    /// Hides the "these folders aren't backed up" suggestion.
    DismissBackupSuggestions,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RpcError {
    pub code: ErrorCode,
    pub message: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    BadRequest,
    NotSetUp,
    AlreadySetUp,
    AuthFailed,
    WrongEncryptionPassword,
    Network,
    NotFound,
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
pub enum Event {
    Status(Status),
    Activity(Activity),
    IssuesChanged {
        count: u32,
    },
    /// Sent about once a second while anything is transferring or queued, and once more when the
    /// list becomes empty. Only to subscribed clients.
    Transfers(TransferList),
    /// Sent when backups are added, removed, toggled, or their progress changes.
    Backups(BackupOverview),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u32,
    pub version: String,
    pub platform: String,
}

/// Overall state, in priority order for the tray icon.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum SyncState {
    /// No account connected yet.
    #[default]
    NotSetUp,
    /// Credentials stopped working; the user must sign in again.
    SignedOut,
    /// Something needs the user's attention (see `issues`).
    Error,
    /// No connection to uloz.to; local changes are queued.
    Offline,
    Paused,
    Syncing,
    UpToDate,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Status {
    pub state: SyncState,
    /// One short, plain-language line for the tray tooltip and popover header.
    pub headline: String,
    pub account: Option<String>,
    /// Local folder (Windows) or mount point (Linux).
    pub folder: Option<String>,
    pub uploading: u32,
    pub downloading: u32,
    /// Items waiting to be uploaded, created, moved or deleted remotely.
    pub pending: u32,
    /// Bytes still to transfer for in-flight items.
    pub transfer_bytes_total: u64,
    pub transfer_bytes_done: u64,
    pub issues: u32,
    /// Bytes held locally for files that are not pinned (the evictable cache).
    pub cache_bytes: u64,
    pub cache_limit_bytes: u64,
    /// Bytes held locally for pinned files.
    pub pinned_bytes: u64,
    /// Total plaintext size of everything in the vault.
    pub cloud_bytes: u64,
    pub cloud_files: u64,
    /// Unix ms of the last completed remote scan.
    pub last_sync_ms: Option<i64>,
    /// Deletions waiting for confirmation by the mass-delete guard.
    pub pending_delete_confirmation: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountCredentials {
    pub login: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountCheck {
    pub login: String,
    /// Whether a Drive vault already exists in this account.
    pub vault_exists: bool,
    /// Whether the existing vault already contains files.
    pub vault_has_files: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetupRequest {
    pub login: String,
    pub password: String,
    /// Encryption password. For a new vault, `None` generates a strong random one
    /// (returned once in [`SetupResult`] — the user must save it).
    pub encryption_password: Option<String>,
    /// Local folder (Windows sync root) or mount point (Linux). `None` = platform default.
    pub folder: Option<String>,
    /// Optional dedicated uloz.to API key; `None` uses the shared default key.
    pub app_token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetupResult {
    pub folder: String,
    /// Set only when the service generated the encryption password.
    pub generated_encryption_password: Option<String>,
    pub created_vault: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Settings {
    pub folder: String,
    /// Upper bound for downloaded-but-not-pinned files; the oldest are evicted beyond it.
    pub cache_limit_bytes: u64,
    /// Seconds between remote scans.
    pub poll_interval_secs: u64,
    /// 0 = unlimited.
    /// KiB/s (kilobytes, not kilobits). 0 = unlimited.
    pub upload_limit_kbps: u64,
    /// KiB/s. 0 = unlimited.
    pub download_limit_kbps: u64,
    /// Slow uploads down automatically while they make the connection sluggish for everything
    /// else (and only then). Applies below `upload_limit_kbps`.
    #[serde(default = "default_true")]
    pub adaptive_upload: bool,
    pub max_parallel_transfers: u32,
    /// Start the tray app (and service) when the user signs in.
    pub launch_at_login: bool,
    pub notifications: bool,
    pub device_name: String,
    pub app_token_custom: bool,
    /// What the window's close (X) button does.
    #[serde(default)]
    pub close_action: CloseAction,
    /// Download updates in the background and offer "Restart to update". When off, the app
    /// still checks for updates but only tells the user about them.
    #[serde(default = "default_true")]
    pub auto_update: bool,
    /// Hold uploads of large files until the overnight window.
    #[serde(default)]
    pub night_uploads: bool,
    /// Window start and end, local time, "HH:MM" (end may be earlier than start: it wraps
    /// past midnight).
    #[serde(default = "default_night_start")]
    pub night_start: String,
    #[serde(default = "default_night_end")]
    pub night_end: String,
    /// Files at least this large wait for the window when `night_uploads` is on.
    #[serde(default = "default_night_min_bytes")]
    pub night_min_bytes: u64,
}

pub fn default_night_start() -> String {
    "01:00".into()
}
pub fn default_night_end() -> String {
    "07:00".into()
}
pub fn default_night_min_bytes() -> u64 {
    200 * 1024 * 1024
}

fn default_true() -> bool {
    true
}

/// What closing the Drive window does.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum CloseAction {
    /// Hide the window; Drive keeps running (and syncing) in the tray.
    #[default]
    Minimize,
    /// Quit Drive, which also stops syncing.
    Quit,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SettingsPatch {
    pub cache_limit_bytes: Option<u64>,
    pub poll_interval_secs: Option<u64>,
    pub upload_limit_kbps: Option<u64>,
    pub download_limit_kbps: Option<u64>,
    pub adaptive_upload: Option<bool>,
    pub max_parallel_transfers: Option<u32>,
    pub launch_at_login: Option<bool>,
    pub notifications: Option<bool>,
    pub device_name: Option<String>,
    /// `Some("")` resets to the shared default key.
    pub app_token: Option<String>,
    pub close_action: Option<CloseAction>,
    pub auto_update: Option<bool>,
    pub night_uploads: Option<bool>,
    pub night_start: Option<String>,
    pub night_end: Option<String>,
    pub night_min_bytes: Option<u64>,
}

// ---- transfers ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TransferDirection {
    Upload,
    Download,
}

/// What a transfer is doing right now.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TransferState {
    /// Reading and hashing the file before the first byte goes out.
    Preparing,
    /// Bytes are moving.
    Active,
    /// All bytes sent; the server is assembling and checking the file.
    Verifying,
    /// Ready to go; waiting for a free slot.
    Queued,
    /// Can't start yet; `reason` says why (file open in another app, offline, retrying…).
    Waiting,
    /// Paused by the user.
    Paused,
    /// Held for the overnight window.
    Scheduled,
}

/// The user's choice for one upload.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum TransferMode {
    /// Follow the settings (large files may wait for the overnight window).
    #[default]
    Auto,
    Paused,
    /// Wait for the overnight window regardless of size.
    Tonight,
    /// Upload now, even if the overnight window applies.
    Now,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TransferItem {
    /// Stable id for actions (the file's node id).
    pub id: i64,
    /// Relative to the Drive folder, `/`-separated.
    pub path: String,
    pub direction: TransferDirection,
    pub state: TransferState,
    /// Plain-language explanation for `waiting` / `scheduled` (and a hint for others).
    pub reason: Option<String>,
    /// Bytes already safely transferred (for uploads: confirmed by the server, so they survive
    /// a restart when `resumable` is true).
    pub bytes_done: u64,
    pub bytes_total: u64,
    /// Smoothed bytes per second while active.
    pub rate_bps: u64,
    pub eta_secs: Option<u64>,
    /// Unix ms of the next automatic retry, when waiting after a failure.
    pub retry_at_ms: Option<i64>,
    /// Progress is kept if Drive restarts or the connection drops.
    pub resumable: bool,
    pub mode: TransferMode,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct TransferList {
    /// Active first, then queued/waiting, then paused/scheduled; at most 200 entries.
    pub items: Vec<TransferItem>,
    /// Total number of transfers (may exceed `items.len()`).
    pub total_count: u32,
    pub upload_rate_bps: u64,
    pub download_rate_bps: u64,
    /// Bytes still to upload for everything that isn't paused or scheduled.
    pub upload_remaining_bytes: u64,
    /// Estimated time until everything that isn't paused or scheduled is uploaded.
    pub upload_eta_secs: Option<u64>,
    /// Unix ms when the overnight window next opens (only when night uploads are on and the
    /// window is closed).
    pub night_window_opens_ms: Option<i64>,
    pub night_window_open: bool,
    /// Uploads are slowed automatically to this many bytes/s to keep the connection
    /// responsive (see `Settings::adaptive_upload`).
    #[serde(default)]
    pub auto_upload_limit_bps: Option<u64>,
    /// Uploads are paused for a few seconds to check whether they slow the connection.
    #[serde(default)]
    pub auto_upload_checking: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActivityKind {
    Uploaded,
    Downloaded,
    CreatedFolder,
    Moved,
    Deleted,
    Conflict,
    Evicted,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Activity {
    pub id: i64,
    pub at_ms: i64,
    pub kind: ActivityKind,
    /// Relative to the Drive folder, `/`-separated. For backups ([`Activity::backup`] set):
    /// relative to that backed-up folder.
    pub path: String,
    /// The [`BackupInfo::id`] this happened in, if it's about a backed-up folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<String>,
    pub bytes: u64,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IssueKind {
    /// An upload or download keeps failing.
    TransferFailed,
    /// Name can't be stored (too long once encrypted, or invalid on another OS).
    InvalidName,
    /// Both sides changed a file; a conflicted copy was created.
    Conflict,
    /// Many deletions at once are waiting for confirmation.
    MassDelete,
    /// The remote vault could not be decrypted with the saved password.
    WrongPassword,
    /// Signed out / credentials rejected.
    Auth,
    /// Local disk almost full; downloads paused.
    DiskFull,
    /// File is open in another app and can't be read for upload.
    FileLocked,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Issue {
    pub id: i64,
    pub kind: IssueKind,
    /// Like [`Activity::path`].
    pub path: Option<String>,
    /// Like [`Activity::backup`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<String>,
    /// Plain-language explanation for the user.
    pub message: String,
    pub created_ms: i64,
    /// Actions the UI may offer for this issue.
    pub actions: Vec<IssueAction>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IssueAction {
    Retry,
    Dismiss,
    /// Mass-delete guard: go ahead and delete remotely.
    ConfirmDelete,
    /// Mass-delete guard: restore the deleted items locally from the cloud.
    RestoreDeleted,
    SignIn,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryKit {
    /// Ready-to-paste `rclone.conf` content (passwords obscured the way rclone expects).
    pub rclone_config: String,
    /// The plain encryption password and salt, for writing down.
    pub encryption_password: String,
    pub encryption_salt: String,
    pub instructions: String,
}

impl Response {
    pub fn ok(id: u64, value: impl Serialize) -> Self {
        Self {
            id,
            result: Some(serde_json::to_value(value).unwrap_or(serde_json::Value::Null)),
            error: None,
        }
    }
    pub fn err(id: u64, code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
            }),
        }
    }
}

/// Backups of folders outside the Drive folder (Desktop, Documents, …).
///
/// Each backed-up folder is mirrored one way into the vault under
/// `Backups/<device>/<name>`: Drive only ever reads the folder on this device, never moves,
/// changes or deletes anything in it. Files deleted from it are removed from the cloud copy
/// too (they stay in the uloz.to trash for 30 days).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct BackupOverview {
    pub folders: Vec<BackupInfo>,
    /// Well-known folders that aren't backed up yet, largest first.
    pub suggestions: Vec<FolderSuggestion>,
    pub suggestions_dismissed: bool,
    /// Where backups appear in the vault, e.g. `Backups/DESKTOP-FA1NIS8`. `None` until the
    /// first backup is added.
    pub cloud_folder: Option<String>,
    /// The skip list new backups start with.
    pub default_excludes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BackupInfo {
    /// Stable id (survives renames of the display name).
    pub id: String,
    /// Display name, also the folder name in the vault (e.g. "Documents").
    pub name: String,
    /// The folder on this device (absolute).
    pub path: String,
    /// Which well-known folder this is, if any.
    pub known: Option<KnownFolder>,
    /// The per-folder off switch.
    pub enabled: bool,
    pub state: BackupState,
    /// Plain-language detail for the state ("Scanning 12,400 files…", "Folder not found").
    pub detail: Option<String>,
    /// Names or `*` patterns skipped anywhere in the folder (files and folders), e.g.
    /// `node_modules`, `*.tmp`. Folders tagged as caches (`CACHEDIR.TAG`) and Python virtual
    /// environments are always skipped.
    pub excludes: Vec<String>,
    /// Files and bytes in the folder that are backed up (after skips).
    pub files: u64,
    pub bytes: u64,
    /// Of those, how many are safely in the cloud.
    pub done_files: u64,
    pub done_bytes: u64,
    /// Files waiting to upload, move or be removed from the cloud copy.
    pub pending: u32,
    /// Online-only files of other cloud apps (e.g. OneDrive) that were skipped.
    pub skipped_online_only: u64,
    pub added_ms: i64,
    /// When the backup was last fully up to date.
    pub last_complete_ms: Option<i64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BackupState {
    /// Looking through the folder for the first time (or after a restart).
    Scanning,
    /// Uploading or updating the cloud copy.
    BackingUp,
    UpToDate,
    /// Switched off by the user; the cloud copy is kept.
    Off,
    /// Drive is paused, offline or signed out.
    Waiting,
    /// The folder is missing or unreadable (see `detail`).
    Problem,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum KnownFolder {
    Desktop,
    Documents,
    Downloads,
    Pictures,
    Videos,
    Music,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FolderSuggestion {
    pub known: KnownFolder,
    pub name: String,
    pub path: String,
    /// Size after the default skips; `None` while still measuring.
    pub files: Option<u64>,
    pub bytes: Option<u64>,
    /// Why it can't be backed up as is (e.g. "Contains your Drive folder"); the UI shows it
    /// instead of an Add button.
    pub blocked: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_wire_format() {
        let r: Request = serde_json::from_str(
            r#"{"id":1,"method":"set_transfer_mode","params":{"id":7,"mode":"tonight"}}"#,
        )
        .unwrap();
        assert!(matches!(
            r.call,
            Call::SetTransferMode {
                id: 7,
                mode: TransferMode::Tonight
            }
        ));
        let e = serde_json::to_value(Event::Transfers(TransferList::default())).unwrap();
        assert_eq!(e["event"], "transfers");
        // Older settings files without the new fields still parse.
        let s: Settings = serde_json::from_str(
            r#"{"folder":"x","cache_limit_bytes":1,"poll_interval_secs":1,"upload_limit_kbps":0,
                "download_limit_kbps":0,"max_parallel_transfers":3,"launch_at_login":true,
                "notifications":true,"device_name":"pc","app_token_custom":false}"#,
        )
        .unwrap();
        assert_eq!(s.night_start, "01:00");
        assert!(!s.night_uploads);
    }

    #[test]
    fn wire_format() {
        let r: Request = serde_json::from_str(
            r#"{"id":3,"method":"set_pinned","params":{"path":"a/b","pinned":true}}"#,
        )
        .unwrap();
        assert!(matches!(r.call, Call::SetPinned { pinned: true, .. }));
        let r: Request = serde_json::from_str(r#"{"id":4,"method":"status"}"#).unwrap();
        assert!(matches!(r.call, Call::Status));
        let e = serde_json::to_string(&Event::IssuesChanged { count: 2 }).unwrap();
        assert_eq!(e, r#"{"event":"issues_changed","data":{"count":2}}"#);
    }
}
