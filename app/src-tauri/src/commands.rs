//! Tauri commands the webviews call. They mirror the `ipc::Call` variants the UI needs, plus a
//! few app-side helpers (windows, dialogs, opening folders).

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use drive_core::ipc::{
    AccountCheck, AccountCredentials, Activity, BackupInfo, BackupOverview, Call, Issue,
    IssueAction, RecoveryKit, Settings, SettingsPatch, SetupRequest, SetupResult, Status,
    TransferList, TransferMode,
};
use serde::Serialize;
use tauri::{AppHandle, State};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;

use crate::backend::{Backend, CallError, ConnectionInfo, Link};
use crate::windows;

type Res<T> = Result<T, CallError>;
type Svc<'a> = State<'a, Arc<Backend>>;

pub const REPOSITORY: &str = "https://github.com/samcannas/drive";

#[derive(Serialize)]
pub struct AppInfo {
    version: &'static str,
    platform: &'static str,
    repository: &'static str,
    connection: ConnectionInfo,
}

#[tauri::command]
pub fn app_info(svc: Svc<'_>) -> AppInfo {
    AppInfo {
        version: env!("CARGO_PKG_VERSION"),
        platform: std::env::consts::OS,
        repository: REPOSITORY,
        connection: svc.connection(),
    }
}

#[tauri::command]
pub fn connection_state(svc: Svc<'_>) -> ConnectionInfo {
    svc.connection()
}

/// Latest status. Answers from the cache when the service is unreachable, so the UI can
/// always render something.
#[tauri::command]
pub async fn get_status(svc: Svc<'_>) -> Res<Status> {
    if svc.connection().state == Link::Connected {
        svc.refresh_status().await;
    }
    Ok(svc.status())
}

#[tauri::command]
pub async fn get_settings(svc: Svc<'_>) -> Res<Settings> {
    let s: Settings = svc.call(Call::GetSettings).await?;
    svc.set_settings(s.clone());
    Ok(s)
}

#[tauri::command]
pub async fn update_settings(svc: Svc<'_>, patch: SettingsPatch) -> Res<Settings> {
    let s: Settings = svc.call(Call::UpdateSettings(patch)).await?;
    // Also applies launch-at-login and tells other windows.
    svc.set_settings(s.clone());
    Ok(s)
}

#[tauri::command]
pub async fn recent_activity(svc: Svc<'_>, limit: u32) -> Res<Vec<Activity>> {
    svc.call(Call::RecentActivity {
        limit: limit.min(200),
    })
    .await
}

#[tauri::command]
pub async fn list_issues(svc: Svc<'_>) -> Res<Vec<Issue>> {
    svc.call(Call::ListIssues).await
}

#[tauri::command]
pub async fn resolve_issue(svc: Svc<'_>, id: i64, action: IssueAction) -> Res<()> {
    svc.call_value(Call::ResolveIssue { id, action }).await?;
    svc.refresh_status().await;
    Ok(())
}

/// Every upload and download that is running, queued or held.
#[tauri::command]
pub async fn transfers(svc: Svc<'_>) -> Res<TransferList> {
    svc.call(Call::Transfers).await
}

/// Pause, resume (`auto`), defer to tonight or force one upload. Returns the new list so the
/// popover can show the change right away.
#[tauri::command]
pub async fn set_transfer_mode(svc: Svc<'_>, id: i64, mode: TransferMode) -> Res<TransferList> {
    svc.call_value(Call::SetTransferMode { id, mode }).await?;
    svc.refresh_status().await;
    svc.call(Call::Transfers).await
}

#[tauri::command]
pub async fn pause(svc: Svc<'_>) -> Res<()> {
    svc.call_value(Call::Pause).await?;
    svc.refresh_status().await;
    Ok(())
}

#[tauri::command]
pub async fn resume(svc: Svc<'_>) -> Res<()> {
    svc.call_value(Call::Resume).await?;
    svc.refresh_status().await;
    Ok(())
}

#[tauri::command]
pub async fn sync_now(svc: Svc<'_>) -> Res<()> {
    svc.call_value(Call::SyncNow).await.map(|_| ())
}

#[tauri::command]
pub async fn check_account(svc: Svc<'_>, login: String, password: String) -> Res<AccountCheck> {
    svc.call(Call::CheckAccount(AccountCredentials { login, password }))
        .await
}

#[tauri::command]
pub async fn setup(svc: Svc<'_>, request: SetupRequest) -> Res<SetupResult> {
    let result: SetupResult = svc.call(Call::Setup(request)).await?;
    svc.refresh_status().await;
    svc.refresh_settings().await;
    Ok(result)
}

#[tauri::command]
pub fn default_folder() -> String {
    crate::platform::default_folder()
}

#[tauri::command]
pub async fn pick_folder(app: AppHandle) -> Res<Option<String>> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_title("Choose where to keep your Drive folder")
        .pick_folder(move |p| {
            let _ = tx.send(p);
        });
    let picked = rx.await.ok().flatten();
    Ok(picked
        .and_then(|p| p.into_path().ok())
        .map(|p| p.to_string_lossy().into_owned()))
}

/// Asks the service for the recovery kit and writes it to a file the user picks. The secrets
/// never pass through the webview. Returns the saved path, or `None` if the user cancelled.
#[tauri::command]
pub async fn save_recovery_kit(app: AppHandle, svc: Svc<'_>) -> Res<Option<String>> {
    let kit: RecoveryKit = svc.call(Call::ExportRecovery).await?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_title("Save recovery kit")
        .set_file_name("Drive recovery kit.txt")
        .add_filter("Text file", &["txt"])
        .save_file(move |p| {
            let _ = tx.send(p);
        });
    let Some(path) = rx.await.ok().flatten().and_then(|p| p.into_path().ok()) else {
        return Ok(None);
    };
    let account = svc.status().account.unwrap_or_default();
    let text = recovery_text(&kit, &account);
    write_private(&path, text.as_bytes()).map_err(|e| {
        CallError::new(
            "internal",
            format!("Couldn't save the file: {e}. Choose another location."),
        )
    })?;
    Ok(Some(path.to_string_lossy().into_owned()))
}

fn recovery_text(kit: &RecoveryKit, account: &str) -> String {
    let nl = if cfg!(windows) { "\r\n" } else { "\n" };
    let body = format!(
        "DRIVE RECOVERY KIT\n\
         uloz.to account: {account}\n\
         \n\
         Anyone with this file can read your Drive files. Keep it somewhere private,\n\
         such as a password manager or a printed copy in a safe place.\n\
         \n\
         Encryption password: {password}\n\
         Encryption salt (password2): {salt}\n\
         \n\
         {instructions}\n\
         \n\
         rclone configuration\n\
         --------------------\n\
         {config}\n",
        password = kit.encryption_password,
        salt = kit.encryption_salt,
        instructions = kit.instructions.trim(),
        config = kit.rclone_config.trim(),
    );
    body.replace("\r\n", "\n").replace('\n', nl)
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

#[tauri::command]
pub async fn sign_out(svc: Svc<'_>) -> Res<()> {
    svc.call_value(Call::SignOut).await?;
    svc.refresh_status().await;
    Ok(())
}

pub fn open_path(app: &AppHandle, path: &str) {
    if let Err(e) = app.opener().open_path(path, None::<&str>) {
        tracing::warn!("could not open {path}: {e}");
    }
}

#[tauri::command]
pub fn open_folder(app: AppHandle, svc: Svc<'_>) -> Res<()> {
    let folder = svc
        .status()
        .folder
        .ok_or_else(|| CallError::new("not_set_up", "Drive isn't set up yet."))?;
    app.opener()
        .open_path(&folder, None::<&str>)
        .map_err(|e| CallError::new("internal", format!("Couldn't open the Drive folder: {e}")))
}

/// Shows an item from the activity list in the file manager. `path` is relative to the
/// Drive folder, or with `backup` (a `BackupInfo::id`) relative to that backed-up folder.
#[tauri::command]
pub async fn reveal_item(
    app: AppHandle,
    svc: Svc<'_>,
    path: String,
    backup: Option<String>,
) -> Res<()> {
    let (root, outside) = match backup {
        Some(id) => {
            let overview: BackupOverview = svc.call(Call::Backups).await?;
            let folder = overview
                .folders
                .into_iter()
                .find(|f| f.id == id)
                .ok_or_else(|| {
                    CallError::new("not_found", "That folder isn't backed up anymore.")
                })?;
            (
                PathBuf::from(folder.path),
                "That path isn't inside the backed-up folder.",
            )
        }
        None => {
            let folder = svc
                .status()
                .folder
                .ok_or_else(|| CallError::new("not_set_up", "Drive isn't set up yet."))?;
            (
                PathBuf::from(folder),
                "That path isn't inside the Drive folder.",
            )
        }
    };
    let rel = Path::new(&path);
    if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err(CallError::new("bad_request", outside));
    }
    let full = root.join(rel);
    let result = if full.exists() && full != root {
        app.opener().reveal_item_in_dir(&full)
    } else {
        // Deleted or moved since: open the closest folder that still exists.
        let dir = full
            .ancestors()
            .find(|p| p.starts_with(&root) && p.is_dir())
            .unwrap_or(&root)
            .to_path_buf();
        app.opener().open_path(dir.to_string_lossy(), None::<&str>)
    };
    result.map_err(|e| CallError::new("internal", format!("Couldn't show the file: {e}")))
}

// ----- Backups of folders outside the Drive folder

/// Backed-up folders and suggestions. Changes also arrive as `drive://backups`.
#[tauri::command]
pub async fn backups(svc: Svc<'_>) -> Res<BackupOverview> {
    svc.call(Call::Backups).await
}

/// Starts backing up a folder (absolute path).
#[tauri::command]
pub async fn add_backup(svc: Svc<'_>, path: String) -> Res<BackupInfo> {
    svc.call(Call::AddBackup { path }).await
}

/// The per-folder off switch. Returns the new overview.
#[tauri::command]
pub async fn set_backup_enabled(svc: Svc<'_>, id: String, enabled: bool) -> Res<BackupOverview> {
    svc.call_value(Call::SetBackupEnabled { id, enabled })
        .await?;
    svc.call(Call::Backups).await
}

/// Stops backing up a folder. The folder itself is never touched; the cloud copy is kept
/// unless `delete_from_cloud` (it then goes to the uloz.to trash). Returns the new overview.
#[tauri::command]
pub async fn remove_backup(
    svc: Svc<'_>,
    id: String,
    delete_from_cloud: bool,
) -> Res<BackupOverview> {
    svc.call_value(Call::RemoveBackup {
        id,
        delete_from_cloud,
    })
    .await?;
    svc.call(Call::Backups).await
}

/// Replaces a backup's skip list.
#[tauri::command]
pub async fn set_backup_excludes(
    svc: Svc<'_>,
    id: String,
    excludes: Vec<String>,
) -> Res<BackupInfo> {
    svc.call(Call::SetBackupExcludes { id, excludes }).await
}

/// Makes a backed-up folder on-demand (files not used lately stay only in the cloud and download
/// when opened), or a full copy again (the service first downloads everything back and refuses if
/// there isn't enough free space). `keep_bytes` changes how much stays on this device.
#[tauri::command]
pub async fn set_backup_on_demand(
    svc: Svc<'_>,
    id: String,
    on_demand: bool,
    keep_bytes: Option<u64>,
) -> Res<BackupInfo> {
    svc.call(Call::SetBackupOnDemand {
        id,
        on_demand,
        keep_bytes,
    })
    .await
}

/// Hides the "these folders aren't backed up" suggestion. Returns the new overview.
#[tauri::command]
pub async fn dismiss_backup_suggestions(svc: Svc<'_>) -> Res<BackupOverview> {
    svc.call_value(Call::DismissBackupSuggestions).await?;
    svc.call(Call::Backups).await
}

/// Native folder picker for a new backup. Returns the path, or `None` if cancelled.
#[tauri::command]
pub async fn pick_backup_folder(app: AppHandle) -> Res<Option<String>> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut dialog = app.dialog().file().set_title("Choose a folder to back up");
    if let Some(home) = dirs::home_dir() {
        dialog = dialog.set_directory(home);
    }
    dialog.pick_folder(move |p| {
        let _ = tx.send(p);
    });
    let picked = rx.await.ok().flatten();
    Ok(picked
        .and_then(|p| p.into_path().ok())
        .map(|p| p.to_string_lossy().into_owned()))
}

#[tauri::command]
pub fn open_url(app: AppHandle, url: String) -> Res<()> {
    const ALLOWED: &[&str] = &[
        REPOSITORY,
        "https://uloz.to/",
        "https://ulozto.net/",
        "https://rclone.org/",
    ];
    if !ALLOWED
        .iter()
        .any(|p| url == p.trim_end_matches('/') || url.starts_with(p))
    {
        return Err(CallError::new(
            "bad_request",
            "That link can't be opened from Drive.",
        ));
    }
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|e| CallError::new("internal", e.to_string()))
}

// Window-creating commands must be async: a synchronous command runs on the main thread,
// and building a webview there deadlocks on Windows.
#[tauri::command]
pub async fn show_window(app: AppHandle, name: String, section: Option<String>) -> Res<()> {
    match name.as_str() {
        windows::SETTINGS => windows::open_settings_at(&app, section.as_deref()),
        windows::ONBOARDING => windows::open_onboarding(&app),
        windows::POPOVER => windows::show_main(&app, None),
        _ => {
            return Err(CallError::new(
                "bad_request",
                format!("Unknown window {name}"),
            ));
        }
    }
    Ok(())
}

/// Closes onboarding and shows the popover, so people see where Drive lives.
#[tauri::command]
pub async fn finish_onboarding(app: AppHandle) {
    windows::close_onboarding(&app);
    windows::show_main(&app, None);
}

#[tauri::command]
pub fn hide_popover(app: AppHandle) {
    windows::hide_popover(&app);
}

/// The window's close (X) button: hide to the tray, or quit, as set in Settings.
#[tauri::command]
pub async fn close_main(app: AppHandle, window: tauri::WebviewWindow, svc: Svc<'_>) -> Res<()> {
    close_main_window(&app, svc.inner(), Some(&window)).await;
    Ok(())
}

/// Closing the popover (its X or the taskbar): hide that window, or quit, as set in Settings.
pub async fn close_main_window(
    app: &AppHandle,
    svc: &Arc<Backend>,
    window: Option<&tauri::WebviewWindow>,
) {
    let action = svc
        .cached_settings()
        .map(|s| s.close_action)
        .unwrap_or_default();
    match action {
        drive_core::ipc::CloseAction::Minimize => match window {
            Some(w) => windows::hide_window(app, w),
            None => windows::hide_popover(app),
        },
        drive_core::ipc::CloseAction::Quit => quit_now(app, svc).await,
    }
}

#[tauri::command]
pub fn reconnect(svc: Svc<'_>) {
    svc.reconnect_now();
}

#[tauri::command]
pub async fn quit(app: AppHandle, svc: Svc<'_>) -> Res<()> {
    quit_now(&app, svc.inner()).await;
    Ok(())
}

/// Quitting Drive stops syncing: the service is asked to shut down, then the app exits.
pub async fn quit_now(app: &AppHandle, svc: &Arc<Backend>) {
    stop_service(svc, false).await;
    crate::updater::install_on_quit(app);
    app.exit(0);
}

/// Asks the service to shut down. With `wait`, also waits (up to 10 s) until it has stopped
/// listening, so that an update can replace its binary.
pub async fn stop_service(svc: &Arc<Backend>, wait: bool) {
    svc.stop_supervising();
    if svc.connection().state != Link::Connected || svc.connection().mock {
        return;
    }
    let _ = tokio::time::timeout(Duration::from_secs(3), svc.call_value(Call::Shutdown)).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while wait
        && tokio::time::Instant::now() < deadline
        && drive_core::ipc_client::Client::connect().await.is_ok()
    {
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}
