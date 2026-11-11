//! Background updates through `tauri-plugin-updater`.
//!
//! Checks shortly after startup and then every 6 hours against the `latest.json` the release
//! workflow publishes, downloads a newer version in the background and then offers
//! "Restart to update" in the tray menu. An AppImage also installs an update that is already
//! downloaded when Drive quits (it only swaps a file, so there is nothing to ask).
//!
//! Update checks are best effort: being offline, or the release feed being unreachable (it
//! answers 404 while the repository is private), is only logged at debug level.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tauri::{AppHandle, Manager};
use tauri_plugin_updater::{Update, UpdaterExt};

use crate::backend::Backend;

const FIRST_CHECK_DELAY: Duration = Duration::from_secs(15);
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// A downloaded, signature-checked update waiting to be installed.
struct Ready {
    update: Update,
    bytes: Vec<u8>,
}

#[derive(Default)]
pub struct UpdateState {
    ready: Mutex<Option<Ready>>,
}

/// Whether this installation updates itself. Release builds only; on Linux only an AppImage
/// or a `.deb` installed through dpkg (the updater installs that with `pkexec dpkg -i`).
/// Other installs, such as the Arch package, are updated by their package manager.
fn enabled() -> bool {
    !cfg!(debug_assertions)
        && std::env::var_os("DRIVE_NO_UPDATE").is_none()
        && installation_updates_itself()
}

#[cfg(target_os = "linux")]
fn installation_updates_itself() -> bool {
    use tauri::utils::config::BundleType;
    std::env::var_os("APPIMAGE").is_some()
        || (matches!(tauri::utils::platform::bundle_type(), Some(BundleType::Deb))
            && std::path::Path::new("/var/lib/dpkg/info/drive.list").exists())
}

#[cfg(not(target_os = "linux"))]
fn installation_updates_itself() -> bool {
    true
}

pub fn start(app: &AppHandle) {
    app.manage(UpdateState::default());
    if !enabled() {
        tracing::debug!("automatic updates are off for this installation");
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(FIRST_CHECK_DELAY).await;
        loop {
            if let Err(e) = check_and_download(&app).await {
                tracing::debug!("update check failed: {e}");
            }
            tokio::time::sleep(CHECK_INTERVAL).await;
        }
    });
}

async fn check_and_download(app: &AppHandle) -> tauri_plugin_updater::Result<()> {
    let Some(update) = app.updater()?.check().await? else {
        tracing::debug!("no update available");
        return Ok(());
    };
    let state = app.state::<UpdateState>();
    let have = state
        .ready
        .lock()
        .unwrap()
        .as_ref()
        .map(|r| r.update.version.clone());
    if have.as_deref() == Some(update.version.as_str()) {
        return Ok(());
    }
    tracing::info!("downloading Drive {}", update.version);
    let bytes = update.download(|_, _| {}, || {}).await?;
    let version = update.version.clone();
    *state.ready.lock().unwrap() = Some(Ready { update, bytes });
    tracing::info!("Drive {version} is ready to install");
    crate::tray::show_update(app, &version);
    Ok(())
}

fn take_ready(app: &AppHandle) -> Option<Ready> {
    app.try_state::<UpdateState>()?.ready.lock().unwrap().take()
}

/// Tray "Restart to update": stops the service (so its binary can be replaced), installs the
/// update and starts Drive again. On Windows the installer exits this process and relaunches
/// Drive when it's done.
pub async fn restart_to_update(app: &AppHandle, backend: &Arc<Backend>) {
    let Some(ready) = take_ready(app) else {
        return;
    };
    crate::commands::stop_service(backend, true).await;
    if let Err(e) = ready.update.install(&ready.bytes) {
        tracing::warn!("installing the update failed: {e}");
    }
    app.restart();
}

/// Called while quitting, after the service has been stopped: an AppImage swaps in an update
/// that is already downloaded. Other bundles keep it for "Restart to update" next time.
pub fn install_on_quit(app: &AppHandle) {
    if std::env::var_os("APPIMAGE").is_none() {
        return;
    }
    if let Some(ready) = take_ready(app) {
        match ready.update.install(&ready.bytes) {
            Ok(()) => tracing::info!("installed Drive {}", ready.update.version),
            Err(e) => tracing::warn!("installing the update failed: {e}"),
        }
    }
}
