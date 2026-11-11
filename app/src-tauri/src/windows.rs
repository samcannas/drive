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

/// The popover is destroyed after staying hidden this long, to give the webview's memory back.
const DESTROY_HIDDEN_AFTER: Duration = Duration::from_secs(90);

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
}

/// Popover is a frameless flyout on Windows. Elsewhere tray clicks aren't reliably delivered
/// and there's no tray rectangle to anchor to, so it's an ordinary small window.
const FLYOUT: bool = cfg!(windows);

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
        let handle = app.clone();
        win.on_window_event(move |e| {
            if let WindowEvent::Focused(false) = e {
                hide_popover(&handle);
            }
        });
    }
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
        place_near_tray(app, &win, rect);
    }
    let _ = win.show();
    if FLYOUT {
        // The first show of a new window can override the position set while hidden.
        place_near_tray(app, &win, rect);
        tracing::debug!(pos = ?win.outer_position().ok(), size = ?win.outer_size().ok(), "popover shown");
    }
    let _ = win.unminimize();
    let _ = win.set_focus();
    let _ = win.emit_to(POPOVER, EV_POPOVER_SHOWN, ());
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
    let Some(win) = app.get_webview_window(POPOVER) else {
        return;
    };
    if !win.is_visible().unwrap_or(false) {
        return;
    }
    let _ = win.hide();
    let state = app.state::<PopoverState>();
    *state.hidden_at.lock().unwrap() = Some(Instant::now());
    let generation = state.generation.fetch_add(1, Ordering::SeqCst) + 1;
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(DESTROY_HIDDEN_AFTER).await;
        let state = handle.state::<PopoverState>();
        if state.generation.load(Ordering::SeqCst) != generation {
            return;
        }
        if let Some(w) = handle.get_webview_window(POPOVER)
            && !w.is_visible().unwrap_or(true)
        {
            let _ = w.destroy();
        }
    });
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
    hide_popover(app);
    show_or_create(app, SETTINGS, || {
        builder(app, SETTINGS, "settings", backdrop("mica"))
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
