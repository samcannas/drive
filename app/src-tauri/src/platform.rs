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

/// Makes the popover a tool window, as Windows' own tray flyouts are: never in the taskbar or
/// Alt+Tab, and left alone by tiling window managers (GlazeWM, komorebi), which otherwise
/// manage it like an app window and center it each time it appears. Tauri rewrites a
/// window's extended styles whenever it shows it, so a subclass keeps the tool-window style
/// through every change. Also asks for the standard Windows 11 rounded corners, which tool
/// windows don't get by default.
#[cfg(windows)]
pub fn make_flyout(win: &tauri::WebviewWindow) {
    let Ok(handle) = win.hwnd() else { return };
    // Raw value: HWND isn't Send, and the subclass must be installed on the window's thread.
    let raw = handle.0 as isize;
    let _ = win.run_on_main_thread(move || flyout::install(raw));
}

#[cfg(windows)]
mod flyout {
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::Graphics::Dwm::{
        DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DwmSetWindowAttribute,
    };
    use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
    use windows::Win32::UI::WindowsAndMessaging::{
        GWL_EXSTYLE, GetWindowLongPtrW, STYLESTRUCT, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE,
        SWP_NOSIZE, SWP_NOZORDER, SetWindowLongPtrW, SetWindowPos, WM_STYLECHANGING,
        WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
    };

    const SUBCLASS_ID: usize = 0x4452_5631; // "DRV1"

    fn flyout_style(ex: u32) -> u32 {
        (ex | WS_EX_TOOLWINDOW.0) & !WS_EX_APPWINDOW.0
    }

    unsafe extern "system" fn proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        _id: usize,
        _data: usize,
    ) -> LRESULT {
        if msg == WM_STYLECHANGING && wparam.0 as i32 == GWL_EXSTYLE.0 {
            // SAFETY: for WM_STYLECHANGING, lparam points to a STYLESTRUCT we may change.
            let s = unsafe { &mut *(lparam.0 as *mut STYLESTRUCT) };
            s.styleNew = flyout_style(s.styleNew);
        }
        // SAFETY: forwarding the message unchanged to the next window procedure.
        unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
    }

    pub fn install(raw: isize) {
        let hwnd = HWND(raw as *mut _);
        // SAFETY: `hwnd` is this process's live window and we're on its thread; the calls
        // only change its styles and add a window procedure that lives as long as the program.
        unsafe {
            let _ = SetWindowSubclass(hwnd, Some(proc), SUBCLASS_ID, 0);
            let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
            if flyout_style(ex) != ex {
                SetWindowLongPtrW(hwnd, GWL_EXSTYLE, flyout_style(ex) as isize);
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
                );
            }
            let round = DWMWCP_ROUND;
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                (&raw const round).cast(),
                size_of_val(&round) as u32,
            );
        }
    }
}

#[cfg(not(windows))]
pub fn make_flyout(_win: &tauri::WebviewWindow) {}
