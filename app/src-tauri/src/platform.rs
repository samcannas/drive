//! Small platform-specific helpers: starting the service, autostart, theme detection.

use std::path::PathBuf;
use std::process::{Command, Stdio};

/// Default sync folder (Windows) or mount point (Linux) shown during onboarding.
/// The service decides the real default when `SetupRequest.folder` is `None`; this mirrors it.
pub fn default_folder() -> String {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Drive")
        .to_string_lossy()
        .into_owned()
}

/// Path of the bundled `drive` service binary: next to this executable (installed bundles and
/// `target/<profile>` during development), falling back to `PATH`.
fn daemon_path() -> PathBuf {
    let name = if cfg!(windows) { "drive.exe" } else { "drive" };
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|d| d.join(name)))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from(name))
}

/// Starts `drive daemon` detached from this process, without a console window.
pub fn spawn_daemon() -> Result<(), String> {
    let path = daemon_path();
    tracing::info!("starting the Drive service: {}", path.display());
    let mut cmd = Command::new(&path);
    cmd.arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Own process group, so a terminal Ctrl+C aimed at the app doesn't stop the service.
        cmd.process_group(0);
    }
    cmd.spawn()
        .map(|_| ())
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Registers or removes the login item. Skipped in debug builds so development binaries
/// never end up in the user's startup list.
pub fn apply_autostart(app: &tauri::AppHandle, enabled: bool) {
    if cfg!(debug_assertions) {
        return;
    }
    use tauri_plugin_autostart::ManagerExt;
    let launcher = app.autolaunch();
    let current = launcher.is_enabled().unwrap_or(!enabled);
    let result = match (enabled, current) {
        (true, false) => launcher.enable(),
        (false, true) => launcher.disable(),
        _ => Ok(()),
    };
    if let Err(e) = result {
        tracing::warn!("could not update launch at login: {e}");
    }
}

/// Whether the taskbar / panel behind the tray icon is light, so the glyph should be dark.
pub fn tray_background_is_light() -> bool {
    #[cfg(windows)]
    {
        windows_registry::CURRENT_USER
            .open(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize")
            .and_then(|k| k.get_u32("SystemUsesLightTheme"))
            .map(|v| v == 1)
            .unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        // GNOME's top bar is dark in both styles. KDE panels follow the colour scheme.
        let desktop = std::env::var("XDG_CURRENT_DESKTOP")
            .unwrap_or_default()
            .to_ascii_lowercase();
        if desktop.contains("kde") {
            let path = dirs::config_dir().map(|d| d.join("kdeglobals"));
            if let Some(text) = path.and_then(|p| std::fs::read_to_string(p).ok()) {
                let scheme = text
                    .lines()
                    .find_map(|l| l.strip_prefix("ColorScheme="))
                    .unwrap_or("BreezeLight");
                return !scheme.to_ascii_lowercase().contains("dark");
            }
        }
        false
    }
}

/// Windows 11 (build 22000+) supports Mica and the modern Acrylic backdrop.
pub fn supports_backdrop() -> bool {
    #[cfg(windows)]
    {
        windows_registry::LOCAL_MACHINE
            .open(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion")
            .and_then(|k| k.get_string("CurrentBuildNumber"))
            .ok()
            .and_then(|b| b.parse::<u32>().ok())
            .is_some_and(|b| b >= 22000)
    }
    #[cfg(not(windows))]
    {
        false
    }
}
