//! In-app updates through `tauri-plugin-updater`.
//!
//! A small state machine ([`UpdateStatus`]) that the webviews read with `update_state` and
//! follow through the `drive://update` event, and that the tray menu mirrors:
//!
//! * Checks the `latest.json` the release workflow publishes 15 s after startup, then hourly
//!   by the wall clock (10 minutes after a failed check), when the popover opens if the last
//!   check is over 15 minutes old, and whenever the user clicks "Check for updates".
//! * With the `auto_update` setting on (the default), a newer version downloads in the
//!   background and Drive offers "Restart to update". With it off, checks only tell the user;
//!   nothing downloads until they choose to update.
//! * Installing stops the `drive` service first (the Windows installer can't replace a running
//!   `drive.exe`), then installs and starts Drive again:
//!   - Windows (NSIS, passive): the plugin starts the installer with `/P /UPDATE /R /ARGS …`
//!     and exits this process. The installer's `.onInstSuccess` relaunches `drive-app.exe`
//!     because of `/R`, and the relaunched app starts the service again.
//!   - Linux (AppImage, `.deb` through `pkexec dpkg -i`): the plugin swaps the files and
//!     returns, then [`AppHandle::restart`] starts the new binary.
//!
//! An AppImage also installs an update that is already downloaded when Drive quits.
//!
//! Background checks are best effort: being offline is only logged. A check the user asked
//! for reports failures in plain language.
//!
//! In debug builds, `DRIVE_UI_MOCK_UPDATE=1` replaces the release feed with a made-up update
//! so the UI can be previewed; "installing" it only pretends.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_updater::{Update, UpdaterExt};

use crate::backend::{Backend, CallError};

pub const EV_UPDATE: &str = "drive://update";
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

const FIRST_CHECK_DELAY: Duration = Duration::from_secs(15);
/// Time between scheduled checks, by the wall clock: a computer that slept through a check
/// checks again soon after it wakes.
const CHECK_EVERY_MS: i64 = 60 * 60 * 1000;
/// After a failed check (offline, GitHub unreachable), try again sooner.
const RETRY_AFTER_MS: i64 = 10 * 60 * 1000;
/// Opening the popover checks too, unless a check ran this recently.
const CHECK_ON_OPEN_AFTER_MS: i64 = 15 * 60 * 1000;
/// How often the scheduler looks at the clock.
const SCHEDULER_TICK: Duration = Duration::from_secs(60);
const CHANGELOG_URL: &str =
    "https://github.com/samcannas/drive/releases/latest/download/changelog.json";
/// How often download progress is sent to the webviews.
const PROGRESS_EVERY: Duration = Duration::from_millis(150);

/// What the update UI shows. Serialized as `{"state": "available", "version": …}`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum UpdateStatus {
    /// This build or installation doesn't update itself; `reason` is a sentence for the UI.
    Disabled {
        reason: String,
    },
    /// Nothing checked yet.
    Idle,
    Checking,
    UpToDate {
        checked_ms: i64,
    },
    /// A newer version exists and hasn't been downloaded.
    Available {
        version: String,
        /// RFC 3339, from `latest.json`'s `pub_date`.
        date: Option<String>,
        notes: Vec<String>,
    },
    Downloading {
        version: String,
        done_bytes: u64,
        total_bytes: Option<u64>,
    },
    /// Downloaded and verified; waiting for the user to restart.
    Ready {
        version: String,
        date: Option<String>,
        notes: Vec<String>,
    },
    /// Stopping the service and running the installer; Drive restarts next.
    Installing {
        version: String,
    },
    /// `version` is set when an update is still known to be available (retry installs it);
    /// otherwise retry checks again.
    Error {
        message: String,
        version: Option<String>,
    },
}

/// One entry of `changelog.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Deserialize)]
struct Changelog {
    releases: Vec<Release>,
}

/// What a check found. `update` is `None` in the mock.
#[derive(Clone)]
struct Found {
    version: String,
    date: Option<String>,
    notes: Vec<String>,
    update: Option<Update>,
}

/// A downloaded, signature-checked update.
#[derive(Clone)]
struct Downloaded {
    found: Found,
    bytes: Arc<Vec<u8>>,
}

enum Mode {
    Real,
    Mock,
    Off(&'static str),
}

struct Inner {
    status: UpdateStatus,
    found: Option<Found>,
    ready: Option<Downloaded>,
    /// The newer releases for `found`, keyed by its version.
    changelog: Option<(String, Vec<Release>)>,
}

pub struct Updates {
    mode: Mode,
    inner: Mutex<Inner>,
    /// Serializes checks, downloads and installs.
    op: tokio::sync::Mutex<()>,
    /// Wall-clock ms of the last finished check (0: none yet), and when the next one is due.
    last_check_ms: AtomicI64,
    next_check_ms: AtomicI64,
}

fn mode() -> Mode {
    if crate::mock::update_forced() {
        Mode::Mock
    } else if cfg!(debug_assertions) {
        Mode::Off("This is a development build of Drive, so it doesn't update itself.")
    } else if std::env::var_os("DRIVE_NO_UPDATE").is_some() {
        Mode::Off("Updates are turned off on this computer by the DRIVE_NO_UPDATE setting.")
    } else if !installation_updates_itself() {
        Mode::Off("Drive was installed by your package manager, which keeps it up to date.")
    } else {
        Mode::Real
    }
}

/// On Linux only an AppImage or a `.deb` installed through dpkg updates itself (the updater
/// installs that with `pkexec dpkg -i`). Other installs, such as the Arch package, are
/// updated by their package manager.
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

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn start(app: &AppHandle) {
    let mode = mode();
    let status = match &mode {
        Mode::Off(reason) => UpdateStatus::Disabled {
            reason: (*reason).into(),
        },
        _ => UpdateStatus::Idle,
    };
    let first_delay = match mode {
        Mode::Off(_) => {
            tracing::debug!("updates are off for this installation");
            None
        }
        Mode::Mock => Some(Duration::from_secs(2)),
        Mode::Real => Some(FIRST_CHECK_DELAY),
    };
    app.manage(Updates {
        mode,
        inner: Mutex::new(Inner {
            status,
            found: None,
            ready: None,
            changelog: None,
        }),
        op: tokio::sync::Mutex::new(()),
        last_check_ms: AtomicI64::new(0),
        next_check_ms: AtomicI64::new(0),
    });
    let Some(first_delay) = first_delay else {
        return;
    };
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(first_delay).await;
        loop {
            let due =
                updates(&app).is_some_and(|u| now_ms() >= u.next_check_ms.load(Ordering::SeqCst));
            if due {
                background_check(&app).await;
            }
            // Short ticks against the wall clock instead of one long sleep: a monotonic
            // timer doesn't count time the computer spends asleep.
            tokio::time::sleep(SCHEDULER_TICK).await;
        }
    });
}

/// The popover opened: check now unless a check ran recently.
pub fn popover_opened(app: &AppHandle) {
    let Some(u) = updates(app) else { return };
    if matches!(u.mode, Mode::Off(_))
        || now_ms() - u.last_check_ms.load(Ordering::SeqCst) < CHECK_ON_OPEN_AFTER_MS
    {
        return;
    }
    // Claim this check so reopening the popover meanwhile doesn't start another.
    u.last_check_ms.store(now_ms(), Ordering::SeqCst);
    let app = app.clone();
    tauri::async_runtime::spawn(async move { background_check(&app).await });
}

/// Records a finished check and schedules the next one.
fn checked(app: &AppHandle, ok: bool) {
    let Some(u) = updates(app) else { return };
    let now = now_ms();
    u.last_check_ms.store(now, Ordering::SeqCst);
    let wait = if ok { CHECK_EVERY_MS } else { RETRY_AFTER_MS };
    u.next_check_ms.store(now + wait, Ordering::SeqCst);
}

fn updates(app: &AppHandle) -> Option<State<'_, Updates>> {
    app.try_state::<Updates>()
}

pub fn status(app: &AppHandle) -> UpdateStatus {
    updates(app)
        .map(|u| u.inner.lock().unwrap().status.clone())
        .unwrap_or(UpdateStatus::Idle)
}

fn set_status(app: &AppHandle, status: UpdateStatus) {
    let Some(u) = updates(app) else { return };
    {
        let mut inner = u.inner.lock().unwrap();
        if inner.status == status {
            return;
        }
        inner.status = status.clone();
    }
    let _ = app.emit(EV_UPDATE, &status);
    crate::tray::show_update(app, &status);
}

fn auto_update(app: &AppHandle) -> bool {
    app.try_state::<Arc<Backend>>()
        .and_then(|b| b.cached_settings())
        .is_none_or(|s| s.auto_update)
}

/// The scheduled check: quiet on failure, downloads when `auto_update` is on.
async fn background_check(app: &AppHandle) {
    let found = {
        let Some(u) = updates(app) else { return };
        let _op = u.op.lock().await;
        match check_locked(app, false).await {
            Ok(found) => found,
            Err(e) => {
                tracing::info!("update check failed, trying again in 10 minutes: {e}");
                return;
            }
        }
    };
    if found.is_some() && auto_update(app) {
        background_download(app).await;
    }
}

/// Downloads without the user asking. A failure leaves the update "available" so the user
/// can still choose to install it (which retries the download).
async fn background_download(app: &AppHandle) {
    let Some(u) = updates(app) else { return };
    let _op = u.op.lock().await;
    if let Err(e) = download_locked(app).await {
        tracing::warn!("downloading the update failed: {e}");
        let found = u.inner.lock().unwrap().found.clone();
        if let Some(f) = found {
            set_status(app, available(&f));
        }
    }
}

fn available(f: &Found) -> UpdateStatus {
    UpdateStatus::Available {
        version: f.version.clone(),
        date: f.date.clone(),
        notes: f.notes.clone(),
    }
}

fn ready_status(f: &Found) -> UpdateStatus {
    UpdateStatus::Ready {
        version: f.version.clone(),
        date: f.date.clone(),
        notes: f.notes.clone(),
    }
}

/// Asks the feed for a newer version. Call with `op` held. `manual` shows "checking" and
/// reports failures; a background check keeps the previous state on failure.
async fn check_locked(app: &AppHandle, manual: bool) -> Result<Option<Found>, String> {
    let Some(u) = updates(app) else {
        return Ok(None);
    };
    if matches!(u.mode, Mode::Off(_)) {
        return Ok(None);
    }
    if manual {
        set_status(app, UpdateStatus::Checking);
    }
    let result = match u.mode {
        Mode::Mock => mock_check().await,
        _ => real_check(app).await,
    };
    checked(app, result.is_ok());
    let found = match result {
        Ok(f) => f,
        Err(message) => {
            if manual {
                let version = u
                    .inner
                    .lock()
                    .unwrap()
                    .found
                    .as_ref()
                    .map(|f| f.version.clone());
                set_status(
                    app,
                    UpdateStatus::Error {
                        message: message.clone(),
                        version,
                    },
                );
            }
            return Err(message);
        }
    };
    let status = {
        let mut inner = u.inner.lock().unwrap();
        match &found {
            None => {
                inner.found = None;
                inner.ready = None;
                UpdateStatus::UpToDate {
                    checked_ms: now_ms(),
                }
            }
            Some(f) => {
                inner.found = Some(f.clone());
                let have = inner.ready.as_ref().map(|r| r.found.version.as_str());
                if have == Some(f.version.as_str()) {
                    ready_status(f)
                } else {
                    inner.ready = None;
                    available(f)
                }
            }
        }
    };
    if let Some(f) = &found {
        tracing::info!("Drive {} is available", f.version);
    }
    set_status(app, status);
    Ok(found)
}

async fn real_check(app: &AppHandle) -> Result<Option<Found>, String> {
    let handle = app.clone();
    let updater = app
        .updater_builder()
        // Windows only: the installer is started and this process exits right away, which
        // would leave a dead tray icon behind until the pointer passes over it.
        .on_before_exit(move || {
            crate::tray::hide(&handle);
            for w in handle.webview_windows().values() {
                let _ = w.hide();
            }
        })
        .restart_after_install(true)
        .build()
        .map_err(|e| plain(&e, Stage::Check))?;
    let Some(update) = updater.check().await.map_err(|e| plain(&e, Stage::Check))? else {
        return Ok(None);
    };
    let date = update
        .raw_json
        .get("pub_date")
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    Ok(Some(Found {
        version: update.version.clone(),
        date,
        notes: bullets(update.body.as_deref().unwrap_or_default()),
        update: Some(update),
    }))
}

/// Downloads the update found by the last check, checking first if needed. Call with `op`
/// held. Returns the downloaded update.
async fn download_locked(app: &AppHandle) -> Result<Downloaded, String> {
    let u = updates(app).ok_or_else(|| "Updates aren't available.".to_string())?;
    let (found, ready) = {
        let inner = u.inner.lock().unwrap();
        (inner.found.clone(), inner.ready.clone())
    };
    let found = match found {
        Some(f) => f,
        None => check_locked(app, true)
            .await?
            .ok_or_else(|| "Drive is already up to date.".to_string())?,
    };
    if let Some(r) = ready.filter(|r| r.found.version == found.version) {
        return Ok(r);
    }
    let version = found.version.clone();
    tracing::info!("downloading Drive {version}");
    set_status(
        app,
        UpdateStatus::Downloading {
            version: version.clone(),
            done_bytes: 0,
            total_bytes: None,
        },
    );
    let mut done: u64 = 0;
    let mut last = Instant::now();
    let mut progress = |chunk: usize, total: Option<u64>| {
        done += chunk as u64;
        if last.elapsed() >= PROGRESS_EVERY || total.is_some_and(|t| done >= t) {
            last = Instant::now();
            set_status(
                app,
                UpdateStatus::Downloading {
                    version: version.clone(),
                    done_bytes: done,
                    total_bytes: total,
                },
            );
        }
    };
    let bytes = match &found.update {
        Some(update) => update.download(&mut progress, || {}).await,
        None => Ok(mock_download(&mut progress).await),
    };
    let bytes = match bytes {
        Ok(b) => b,
        Err(e) => {
            let message = plain(&e, Stage::Download);
            set_status(
                app,
                UpdateStatus::Error {
                    message: message.clone(),
                    version: Some(found.version.clone()),
                },
            );
            return Err(message);
        }
    };
    let downloaded = Downloaded {
        found: found.clone(),
        bytes: Arc::new(bytes),
    };
    u.inner.lock().unwrap().ready = Some(downloaded.clone());
    tracing::info!("Drive {} is ready to install", found.version);
    set_status(app, ready_status(&found));
    Ok(downloaded)
}

/// How long installing waits for the last-moment check for a newer version.
const INSTALL_CHECK_TIMEOUT: Duration = Duration::from_secs(10);

/// Asks the feed once more right before installing. A download made hours ago may already be
/// outdated (it once installed 0.1.19 and then 0.1.29 straight after); if a newer version is
/// out, the check forgets the old download and `download_locked` fetches the new one. When
/// the check fails or times out (offline), the download already made is installed.
async fn refresh_before_install(app: &AppHandle) {
    match tokio::time::timeout(INSTALL_CHECK_TIMEOUT, check_locked(app, false)).await {
        Ok(Ok(Some(f))) => tracing::info!("latest version before installing: {}", f.version),
        Ok(Ok(None)) => tracing::info!("no update on the feed any more"),
        Ok(Err(e)) => tracing::info!("checking before installing failed, installing anyway: {e}"),
        Err(_) => tracing::info!("checking before installing timed out, installing anyway"),
    }
}

/// Downloads if needed, stops the service, installs and starts Drive again. Only returns
/// when something failed (or in the mock).
pub async fn install(app: &AppHandle, backend: &Arc<Backend>) -> Result<(), String> {
    let u = updates(app).ok_or_else(|| "Updates aren't available.".to_string())?;
    if let Mode::Off(reason) = u.mode {
        return Err(reason.into());
    }
    let _op = u.op.lock().await;
    refresh_before_install(app).await;
    let ready = download_locked(app).await?;
    let version = ready.found.version.clone();
    set_status(
        app,
        UpdateStatus::Installing {
            version: version.clone(),
        },
    );
    let Some(update) = ready.found.update.clone() else {
        // The mock pretends the restart happened.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        tracing::info!("mock: would install Drive {version} and restart now");
        {
            let mut inner = u.inner.lock().unwrap();
            inner.found = None;
            inner.ready = None;
        }
        set_status(
            app,
            UpdateStatus::UpToDate {
                checked_ms: now_ms(),
            },
        );
        return Ok(());
    };

    tracing::info!("installing Drive {version}");
    crate::commands::stop_service(backend, true).await;
    let bytes = ready.bytes.clone();
    // On Windows this starts the installer and exits the process; it only returns on failure.
    let result = tokio::task::spawn_blocking(move || update.install(bytes.as_slice()))
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r.map_err(|e| plain(&e, Stage::Install)));
    match result {
        Ok(()) => {
            tracing::info!("installed Drive {version}; restarting");
            app.restart();
        }
        Err(message) => {
            tracing::warn!("installing the update failed: {message}");
            backend.resume_supervising();
            set_status(
                app,
                UpdateStatus::Error {
                    message: message.clone(),
                    version: Some(version),
                },
            );
            Err(message)
        }
    }
}

/// Tray menu: same as the UI's "Update and restart".
pub async fn install_from_tray(app: &AppHandle, backend: &Arc<Backend>) {
    if let Err(e) = install(app, backend).await {
        tracing::warn!("update from the tray menu failed: {e}");
    }
}

/// Called while quitting, after the service has been stopped: an AppImage swaps in an update
/// that is already downloaded. Other bundles keep it for "Restart to update" next time.
pub fn install_on_quit(app: &AppHandle) {
    if std::env::var_os("APPIMAGE").is_none() {
        return;
    }
    let Some(u) = updates(app) else { return };
    let ready = u.inner.lock().unwrap().ready.take();
    if let Some(Downloaded {
        found:
            Found {
                update: Some(update),
                version,
                ..
            },
        bytes,
    }) = ready
    {
        match update.install(bytes.as_slice()) {
            Ok(()) => tracing::info!("installed Drive {version}"),
            Err(e) => tracing::warn!("installing the update failed: {e}"),
        }
    }
}

/// Settings changed: turning on automatic updates downloads an update that is waiting.
pub fn settings_changed(app: &AppHandle, auto_update: bool) {
    if auto_update && matches!(status(app), UpdateStatus::Available { .. }) {
        let app = app.clone();
        tauri::async_runtime::spawn(async move { background_download(&app).await });
    }
}

// ----- Changelog

/// Releases newer than the running version, newest first, up to the version on offer. Uses
/// `changelog.json` from the latest release; falls back to `latest.json`'s notes.
async fn changelog(app: &AppHandle) -> Vec<Release> {
    let Some(u) = updates(app) else {
        return Vec::new();
    };
    let (found, cached, mock) = {
        let inner = u.inner.lock().unwrap();
        (
            inner.found.clone(),
            inner.changelog.clone(),
            matches!(u.mode, Mode::Mock),
        )
    };
    let Some(found) = found else {
        return Vec::new();
    };
    if let Some((v, releases)) = cached
        && v == found.version
    {
        return releases;
    }
    let all = if mock {
        Ok(mock_changelog())
    } else {
        fetch_changelog().await
    };
    let mut releases: Vec<Release> = match all {
        Ok(all) => all
            .into_iter()
            .filter(|r| newer(&r.version, CURRENT_VERSION) && !newer(&r.version, &found.version))
            .collect(),
        Err(e) => {
            tracing::debug!("could not load the changelog: {e}");
            Vec::new()
        }
    };
    releases.sort_by(|a, b| version_key(&b.version).cmp(&version_key(&a.version)));
    if releases.is_empty() {
        releases.push(Release {
            version: found.version.clone(),
            date: found.date.clone(),
            url: None,
            notes: found.notes.clone(),
        });
    }
    u.inner.lock().unwrap().changelog = Some((found.version, releases.clone()));
    releases
}

async fn fetch_changelog() -> Result<Vec<Release>, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent(concat!("Drive/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| e.to_string())?;
    let res = client
        .get(CHANGELOG_URL)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| e.to_string())?;
    let log: Changelog = res.json().await.map_err(|e| e.to_string())?;
    Ok(log.releases)
}

/// `"0.1.10"`, `"v0.1.10"` or `"0.1.10-rc.1"` → `[0, 1, 10]`. Pre-release tags are ignored.
fn version_key(v: &str) -> Vec<u64> {
    let v = v.trim().trim_start_matches(['v', 'V']);
    let core = v.split(['-', '+']).next().unwrap_or_default();
    let mut key: Vec<u64> = core.split('.').map(|p| p.parse().unwrap_or(0)).collect();
    while key.last() == Some(&0) {
        key.pop();
    }
    key
}

fn newer(a: &str, b: &str) -> bool {
    version_key(a) > version_key(b)
}

/// `latest.json` notes are a markdown bullet list; one note per bullet (or per line).
fn bullets(body: &str) -> Vec<String> {
    body.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| {
            // A bare marker ("- " trimmed to "-") is an empty bullet.
            if matches!(l, "-" | "*" | "•") {
                return String::new();
            }
            l.strip_prefix("- ")
                .or_else(|| l.strip_prefix("* "))
                .or_else(|| l.strip_prefix("• "))
                .unwrap_or(l)
                .trim()
                .to_string()
        })
        .filter(|l| !l.is_empty())
        .collect()
}

// ----- Errors

#[derive(Clone, Copy)]
enum Stage {
    Check,
    Download,
    Install,
}

/// The plugin's errors, in words a person can act on.
fn plain(e: &tauri_plugin_updater::Error, stage: Stage) -> String {
    use tauri_plugin_updater::Error as E;
    tracing::debug!("updater error: {e:?}");
    match e {
        E::Reqwest(_) | E::Network(_) => match stage {
            Stage::Download => {
                "The download stopped. Check your internet connection and try again.".into()
            }
            _ => "Couldn't reach GitHub to check for updates. Check your internet connection and try again.".into(),
        },
        E::ReleaseNotFound | E::Serialization(_) | E::TargetNotFound(_) | E::TargetsNotFound(_) => {
            "Couldn't read the update information from GitHub. Try again later.".into()
        }
        E::Minisign(_) | E::Base64(_) | E::SignatureUtf8(_) => {
            "The update couldn't be verified, so Drive didn't install it. Try again later.".into()
        }
        E::AuthenticationFailed => {
            "Installing the update needs your password. Try again and enter it when asked.".into()
        }
        E::Io(io) => match stage {
            Stage::Install => format!("Couldn't install the update ({io}). Try again."),
            _ => format!("Couldn't save the update ({io}). Check that there's free disk space and try again."),
        },
        _ => match stage {
            Stage::Check => format!("Couldn't check for updates ({e}). Try again later."),
            Stage::Download => format!("Couldn't download the update ({e}). Try again."),
            Stage::Install => format!("Couldn't install the update ({e}). Try again."),
        },
    }
}

// ----- Mock (debug builds, DRIVE_UI_MOCK_UPDATE=1)

const MOCK_VERSION: &str = "0.1.2";

fn mock_changelog() -> Vec<Release> {
    vec![
        Release {
            version: MOCK_VERSION.into(),
            date: Some("2026-11-11T05:00:00Z".into()),
            url: Some("https://github.com/samcannas/drive/releases/tag/v0.1.2".into()),
            notes: vec![
                "Update Drive from the status window or Settings".into(),
                "Choose whether updates install automatically".into(),
                "Faster retries for uploads that had to wait".into(),
            ],
        },
        Release {
            version: "0.1.1".into(),
            date: Some("2026-11-08T17:30:00Z".into()),
            url: Some("https://github.com/samcannas/drive/releases/tag/v0.1.1".into()),
            notes: vec![
                "Drag the status window anywhere, and close it with the X button".into(),
                "Choose what closing the window does".into(),
            ],
        },
    ]
}

async fn mock_check() -> Result<Option<Found>, String> {
    tokio::time::sleep(Duration::from_millis(800)).await;
    let latest = &mock_changelog()[0];
    // Debug builds: DRIVE_UI_MOCK_UPDATE_VERSION_FILE names a file holding the version the
    // mock feed offers, read on every check, so a test can publish a newer version between
    // the download and the install.
    let version = std::env::var_os("DRIVE_UI_MOCK_UPDATE_VERSION_FILE")
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| latest.version.clone());
    // "offline" in that file makes the check fail, as if the feed were unreachable.
    if version == "offline" {
        return Err("Couldn't reach the update server.".into());
    }
    Ok(Some(Found {
        version,
        date: latest.date.clone(),
        notes: latest.notes.clone(),
        update: None,
    }))
}

async fn mock_download(progress: &mut impl FnMut(usize, Option<u64>)) -> Vec<u8> {
    const TOTAL: u64 = 18 * 1024 * 1024;
    const STEPS: u64 = 30;
    for _ in 0..STEPS {
        tokio::time::sleep(Duration::from_millis(100)).await;
        progress((TOTAL / STEPS) as usize, Some(TOTAL));
    }
    Vec::new()
}

// ----- Commands

#[tauri::command]
pub fn update_state(app: AppHandle) -> UpdateStatus {
    status(&app)
}

/// "Check for updates". Returns the state afterwards; with `auto_update` on, a found update
/// starts downloading in the background.
#[tauri::command]
pub async fn check_for_updates(app: AppHandle) -> Result<UpdateStatus, CallError> {
    let Some(u) = updates(&app) else {
        return Ok(UpdateStatus::Idle);
    };
    // A download or install is already running: its state is the answer. (A background
    // check that happens to be running is simply waited for.)
    if matches!(
        status(&app),
        UpdateStatus::Downloading { .. } | UpdateStatus::Installing { .. }
    ) {
        return Ok(status(&app));
    }
    let op = u.op.lock().await;
    let found = check_locked(&app, true)
        .await
        .map_err(|m| CallError::new("update_failed", m))?;
    drop(op);
    if found.is_some() && auto_update(&app) {
        let app2 = app.clone();
        tauri::async_runtime::spawn(async move { background_download(&app2).await });
    }
    Ok(status(&app))
}

/// "Update and restart": downloads if needed, installs and restarts Drive.
#[tauri::command]
pub async fn install_update(app: AppHandle, svc: State<'_, Arc<Backend>>) -> Result<(), CallError> {
    install(&app, svc.inner())
        .await
        .map_err(|m| CallError::new("update_failed", m))
}

#[tauri::command]
pub async fn update_changelog(app: AppHandle) -> Vec<Release> {
    changelog(&app).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert!(newer("0.1.10", "0.1.9"));
        assert!(newer("v0.2.0", "0.1.10"));
        assert!(!newer("0.1.0", "0.1"));
        assert!(!newer("0.1.0-rc.1", "0.1.0"));
    }

    #[test]
    fn splits_bullets() {
        assert_eq!(
            bullets("## Changes\n- One\n* Two\n\nThree\n- "),
            vec!["One", "Two", "Three"]
        );
    }
}
