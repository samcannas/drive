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
    let user = std::env::var("USERNAME").or_else(|_| std::env::var("USER")).unwrap_or_else(|_| "user".into());
    let user: String = user.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
    format!("drive-{user}.sock")
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
    /// Connect an account for the first time (or reconnect after sign-out).
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
    RecentActivity { limit: u32 },
    /// Returns `Vec<Issue>`.
    ListIssues,
    ResolveIssue { id: i64, action: IssueAction },
    /// Keep a file or folder (recursively) downloaded on this device, or release it.
    SetPinned { path: String, pinned: bool },
    /// Evict the downloaded copy of a file or folder (it stays in the cloud).
    FreeUpSpace { path: String },
    /// Returns [`RecoveryKit`]: an rclone config that opens the vault without Drive.
    ExportRecovery,
    /// Disconnects the account and forgets credentials. Local placeholders are removed.
    SignOut,
    /// Stops the service.
    Shutdown,
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
    IssuesChanged { count: u32 },
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
    pub upload_limit_kbps: u64,
    pub download_limit_kbps: u64,
    pub max_parallel_transfers: u32,
    /// Start the tray app (and service) when the user signs in.
    pub launch_at_login: bool,
    pub notifications: bool,
    pub device_name: String,
    pub app_token_custom: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SettingsPatch {
    pub cache_limit_bytes: Option<u64>,
    pub poll_interval_secs: Option<u64>,
    pub upload_limit_kbps: Option<u64>,
    pub download_limit_kbps: Option<u64>,
    pub max_parallel_transfers: Option<u32>,
    pub launch_at_login: Option<bool>,
    pub notifications: Option<bool>,
    pub device_name: Option<String>,
    /// `Some("")` resets to the shared default key.
    pub app_token: Option<String>,
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
    pub path: String,
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
    pub path: Option<String>,
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
        Self { id, result: Some(serde_json::to_value(value).unwrap_or(serde_json::Value::Null)), error: None }
    }
    pub fn err(id: u64, code: ErrorCode, message: impl Into<String>) -> Self {
        Self { id, result: None, error: Some(RpcError { code, message: message.into() }) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_format() {
        let r: Request = serde_json::from_str(r#"{"id":3,"method":"set_pinned","params":{"path":"a/b","pinned":true}}"#).unwrap();
        assert!(matches!(r.call, Call::SetPinned { pinned: true, .. }));
        let r: Request = serde_json::from_str(r#"{"id":4,"method":"status"}"#).unwrap();
        assert!(matches!(r.call, Call::Status));
        let e = serde_json::to_string(&Event::IssuesChanged { count: 2 }).unwrap();
        assert_eq!(e, r#"{"event":"issues_changed","data":{"count":2}}"#);
    }
}
