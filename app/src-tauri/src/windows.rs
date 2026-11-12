//! The popover, settings and onboarding windows.
//!
//! All three load the same `index.html`; an initialization script tells the frontend which
//! view to render and which backdrop material (if any) is behind it.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tauri::{
    AppHandle, Emitter, Manager, PhysicalPosition, WebviewUrl, WebviewWindow, WebviewWindowBuilder,
    WindowEvent,
};

pub const POPOVER: &str = "popover";
pub const SETTINGS: &str = "settings";
pub const ONBOARDING: &str = "onboarding";

pub const EV_POPOVER_SHOWN: &str = "drive://popover-shown";

/// Tray icon rectangle in physical pixels: x, y, width, height.
#[derive(Debug, Clone, Copy)]
pub struct TrayRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

#[derive(Default)]
pub struct PopoverState {
    hidden_at: Mutex<Option<Instant>>,
    generation: AtomicU64,
    last_rect: Mutex<Option<TrayRect>>,
    /// Where the user dragged the popover to; it reopens there for the rest of the session.
    dragged_to: Mutex<Option<PhysicalPosition<i32>>>,
    /// When we last positioned the window ourselves (those moves aren't user drags).
    placed_at: Mutex<Option<Instant>>,
}

impl PopoverState {
    fn mark_placed(&self) {
        *self.placed_at.lock().unwrap() = Some(Instant::now());
    }
    fn recently_placed(&self) -> bool {
        self.placed_at
            .lock()
            .unwrap()
            .is_some_and(|t| t.elapsed() < Duration::from_millis(500))
    }
}

/// Popover is a frameless flyout on Windows. Elsewhere tray clicks aren't reliably delivered
/// and there's no tray rectangle to anchor to, so it's an ordinary small window.
const FLYOUT: bool = cfg!(windows);

/// How long a lost focus must last before the flyout hides.
const BLUR_GRACE: Duration = Duration::from_millis(150);

fn env_script(view: &str, material: &str) -> String {
    format!(
        "window.__DRIVE_ENV__ = {};",
        serde_json::json!({ "view": view, "material": material, "platform": std::env::consts::OS })
    )
}

fn backdrop(material: &'static str) -> Option<&'static str> {
    crate::platform::supports_backdrop().then_some(material)
}

#[cfg(windows)]
fn effects(material: &str) -> tauri::utils::config::WindowEffectsConfig {
    use tauri::window::Effect;
    tauri::utils::config::WindowEffectsConfig {
        effects: vec![if material == "mica" {
            Effect::Mica
        } else {
            Effect::Acrylic
        }],
        ..Default::default()
    }
}

fn builder<'a>(
    app: &'a AppHandle,
    label: &'a str,
    view: &str,
    material: Option<&str>,
) -> WebviewWindowBuilder<'a, tauri::Wry, AppHandle> {
    let b = WebviewWindowBuilder::new(app, label, WebviewUrl::App("index.html".into()))
        .initialization_script(env_script(view, material.unwrap_or("none")))
        .visible(false);
    #[cfg(windows)]
    let b = match material {
        Some(m) => b.transparent(true).effects(effects(m)),
        None => b,
    };
    b
}

fn popover(app: &AppHandle) -> tauri::Result<WebviewWindow> {
    if let Some(w) = app.get_webview_window(POPOVER) {
        return Ok(w);
    }
    let material = if FLYOUT { backdrop("acrylic") } else { None };
    let b = builder(app, POPOVER, "popover", material)
        .title("Drive")
        .inner_size(360.0, 520.0);
    let b = if FLYOUT {
        b.decorations(false)
            .resizable(false)
            .maximizable(false)
            .minimizable(false)
            .always_on_top(true)
            .skip_taskbar(true)
            .shadow(true)
    } else {
        b.resizable(true).min_inner_size(340.0, 420.0).center()
    };
    let win = b.build()?;
    if FLYOUT {
        crate::platform::make_flyout(&win);
    }
    let handle = app.clone();
    // Debug builds can keep the flyout open when it loses focus (UI work, automated checks).
    let hide_on_blur =
        FLYOUT && !(cfg!(debug_assertions) && std::env::var_os("DRIVE_UI_KEEP_OPEN").is_some());
    let this = win.clone();
    win.on_window_event(move |e| {
        tracing::trace!(event = ?e, "popover window event");
        match e {
            WindowEvent::Focused(false) if hide_on_blur => hide_after_blur(&handle, &this),
            WindowEvent::Moved(pos) => {
                let state = handle.state::<PopoverState>();
                let visible = handle
                    .get_webview_window(POPOVER)
                    .and_then(|w| w.is_visible().ok())
                    .unwrap_or(false);
                if visible && !state.recently_placed() {
                    *state.dragged_to.lock().unwrap() = Some(*pos);
                }
            }
            WindowEvent::CloseRequested { api, .. } => {
                // The native close button (Linux window) follows the same setting as our X.
                api.prevent_close();
                let handle = handle.clone();
                let this = this.clone();
                tauri::async_runtime::spawn(async move {
                    let svc = handle.state::<std::sync::Arc<crate::backend::Backend>>();
                    crate::commands::close_main_window(&handle, svc.inner(), Some(&this)).await;
                });
            }
            _ => {}
        }
    });
    Ok(win)
}

/// Shows the main status surface: the popover anchored to the tray icon on Windows.
pub fn show_main(app: &AppHandle, rect: Option<TrayRect>) {
    let state = app.state::<PopoverState>();
    if let Some(r) = rect {
        *state.last_rect.lock().unwrap() = Some(r);
    }
    let rect = rect.or(*state.last_rect.lock().unwrap());
    let win = match popover(app) {
        Ok(w) => w,
        Err(e) => {
            tracing::error!("could not open the popover: {e}");
            return;
        }
    };
    state.generation.fetch_add(1, Ordering::SeqCst);
    if FLYOUT {
        // Positioned while hidden, so it appears in place. (It's a tool window, so window
        // managers don't move it afterwards; see `platform::make_flyout`.)
        place(app, &win, rect);
    }
    let _ = win.show();
    if FLYOUT {
        tracing::debug!(pos = ?win.outer_position().ok(), size = ?win.outer_size().ok(), "popover shown");
    } else {
        let _ = win.unminimize();
    }
    let _ = win.set_focus();
    let _ = win.emit_to(POPOVER, EV_POPOVER_SHOWN, ());
    crate::updater::popover_opened(app);
}

/// Creates the popover hidden at startup, so the first click shows a window that has already
/// loaded and drawn its content.
pub fn prepare_popover(app: &AppHandle) {
    if let Err(e) = popover(app) {
        tracing::warn!("could not prepare the popover: {e}");
    }
}

/// The popover lost focus: hide it if it is still unfocused a moment later. Focus moving
/// into the popover's own web content (on the first show of a new window, for one) reports a
/// blur followed at once by focus; hiding on that made the first click open and close it.
fn hide_after_blur(app: &AppHandle, win: &WebviewWindow) {
    let generation = app
        .state::<PopoverState>()
        .generation
        .load(Ordering::SeqCst);
    let (app, win) = (app.clone(), win.clone());
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(BLUR_GRACE).await;
        let state = app.state::<PopoverState>();
        // Shown again or hidden meanwhile: that decided it.
        if state.generation.load(Ordering::SeqCst) != generation {
            return;
        }
        if !win.is_focused().unwrap_or(false) {
            hide_window(&app, &win);
        }
    });
}

/// Left click on the tray icon.
pub fn toggle_popover(app: &AppHandle, rect: Option<TrayRect>) {
    let visible = app
        .get_webview_window(POPOVER)
        .and_then(|w| w.is_visible().ok())
        .unwrap_or(false);
    if visible {
        hide_popover(app);
        return;
    }
    // Clicking the tray icon while the popover is open first blurs it (which hides it), then
    // delivers the click. Treat that click as "close", not "open again".
    let recently_hidden = app
        .state::<PopoverState>()
        .hidden_at
        .lock()
        .unwrap()
        .is_some_and(|t| t.elapsed() < Duration::from_millis(300));
    if !recently_hidden {
        show_main(app, rect);
    }
}

pub fn hide_popover(app: &AppHandle) {
    if let Some(win) = app.get_webview_window(POPOVER) {
        hide_window(app, &win);
    }
}

/// Hides this popover window. The window is kept (not destroyed) so it can never end up on
/// screen without the app tracking it; a hidden webview costs a little memory, nothing else.
pub fn hide_window(app: &AppHandle, win: &WebviewWindow) {
    if !win.is_visible().unwrap_or(true) {
        return;
    }
    if let Err(e) = win.hide() {
        tracing::warn!("hiding the popover failed: {e}");
        // Last resort: at least get it out of the way.
        let _ = win.minimize();
    }
    let state = app.state::<PopoverState>();
    *state.hidden_at.lock().unwrap() = Some(Instant::now());
    state.generation.fetch_add(1, Ordering::SeqCst);
    tracing::debug!("popover hidden");
}

/// Puts the popover where the user last dragged it, or next to the tray icon.
fn place(app: &AppHandle, win: &WebviewWindow, rect: Option<TrayRect>) {
    let state = app.state::<PopoverState>();
    state.mark_placed();
    let dragged = *state.dragged_to.lock().unwrap();
    // Only reuse a dragged position that is still on a connected monitor.
    if let Some(pos) = dragged
        && app
            .monitor_from_point(pos.x as f64 + 40.0, pos.y as f64 + 20.0)
            .ok()
            .flatten()
            .is_some()
    {
        let _ = win.set_position(pos);
        return;
    }
    place_near_tray(app, win, rect);
}

fn place_near_tray(app: &AppHandle, win: &WebviewWindow, rect: Option<TrayRect>) {
    let monitor = match rect {
        Some(r) => app
            .monitor_from_point(r.x + r.w / 2.0, r.y + r.h / 2.0)
            .ok()
            .flatten(),
        None => None,
    }
    .or_else(|| app.primary_monitor().ok().flatten());
    let Some(monitor) = monitor else { return };
    let scale = monitor.scale_factor();
    // Content size in this monitor's pixels (the window may currently sit on another monitor).
    let _ = win.set_size(tauri::PhysicalSize::new(
        (360.0 * scale).round() as u32,
        (520.0 * scale).round() as u32,
    ));
    // The outer frame can include invisible borders; place by the outer size.
    let (w, h) = match win.outer_size() {
        Ok(s) => (s.width as i32, s.height as i32),
        Err(_) => ((360.0 * scale) as i32, (520.0 * scale) as i32),
    };
    let margin = (12.0 * scale).round() as i32;
    let area = monitor.work_area();
    let (ax, ay) = (area.position.x, area.position.y);
    let (aw, ah) = (area.size.width as i32, area.size.height as i32);
    let (cx, cy) = match rect {
        Some(r) => ((r.x + r.w / 2.0) as i32, (r.y + r.h / 2.0) as i32),
        None => {
            // No tray rectangle (second launch, menu): the corner next to the taskbar's
            // notification area, found from where the work area is inset from the monitor.
            let mon = monitor.position();
            let size = monitor.size();
            let top = ay > mon.y;
            let left = ax > mon.x && aw < size.width as i32;
            (
                if left { ax } else { ax + aw },
                if top { ay } else { ay + ah },
            )
        }
    };
    let x = (cx - w / 2).clamp(ax + margin, (ax + aw - w - margin).max(ax + margin));
    let y = if cy >= ay + ah / 2 {
        ay + ah - h - margin
    } else {
        ay + margin
    };
    tracing::debug!(?rect, scale, ax, ay, aw, ah, x, y, w, h, "placing popover");
    let _ = win.set_position(PhysicalPosition::new(x, y));
}

fn show_or_create(
    app: &AppHandle,
    label: &str,
    create: impl FnOnce() -> tauri::Result<WebviewWindow>,
) {
    let win = match app.get_webview_window(label) {
        Some(w) => Ok(w),
        None => create(),
    };
    match win {
        Ok(w) => {
            let _ = w.show();
            let _ = w.unminimize();
            let _ = w.set_focus();
        }
        Err(e) => tracing::error!("could not open the {label} window: {e}"),
    }
}

pub fn open_settings(app: &AppHandle) {
    open_settings_at(app, None);
}

/// The Settings section of the disk space advisor (tray menu, "Free up space…" in the popover).
pub const SECTION_SPACE: &str = "space";

/// Event telling an open settings window to switch to a section (e.g. `"backups"`).
pub const EV_SETTINGS_SECTION: &str = "drive://settings-section";

/// Opens settings, optionally at a section. A new window reads the section from
/// `window.__DRIVE_SECTION__`; an open one is told with [`EV_SETTINGS_SECTION`].
pub fn open_settings_at(app: &AppHandle, section: Option<&str>) {
    // Section ids are short lowercase words; anything else is ignored.
    let section = section.filter(|s| s.len() <= 32 && s.bytes().all(|b| b.is_ascii_lowercase()));
    hide_popover(app);
    if let (Some(sec), Some(_)) = (section, app.get_webview_window(SETTINGS)) {
        use tauri::Emitter;
        let _ = app.emit_to(SETTINGS, EV_SETTINGS_SECTION, sec);
    }
    show_or_create(app, SETTINGS, || {
        builder(app, SETTINGS, "settings", backdrop("mica"))
            .initialization_script(format!(
                "window.__DRIVE_SECTION__ = {};",
                serde_json::json!(section)
            ))
            .title("Drive settings")
            .inner_size(780.0, 600.0)
            .min_inner_size(520.0, 440.0)
            .center()
            .build()
    });
}

pub fn open_onboarding(app: &AppHandle) {
    hide_popover(app);
    show_or_create(app, ONBOARDING, || {
        builder(app, ONBOARDING, "onboarding", backdrop("mica"))
            .title("Set up Drive")
            .inner_size(600.0, 680.0)
            .min_inner_size(440.0, 560.0)
            .center()
            .build()
    });
}

pub fn close_onboarding(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(ONBOARDING) {
        let _ = w.close();
    }
}
