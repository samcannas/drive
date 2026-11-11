//! Tray icon, tooltip and menu.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use drive_core::ipc::{Call, Status, SyncState};
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager};

use crate::backend::{Backend, ConnectionInfo, Link};
use crate::tray_icon::{Glyph, SYNC_FRAMES, render};
use crate::windows::{self, TrayRect};

const TRAY_ID: &str = "main";

struct Items {
    /// Linux only: the headline as a disabled first item, since the tooltip isn't shown there.
    status: MenuItem<tauri::Wry>,
    pause: MenuItem<tauri::Wry>,
    menu: Menu<tauri::Wry>,
    /// "Restart to update", added to the menu once an update has been downloaded.
    update: MenuItem<tauri::Wry>,
    update_shown: AtomicBool,
}

#[derive(Default)]
struct Visual {
    glyph: Option<Glyph>,
    /// Animate the sync badge (only while bytes are actually moving).
    animating: bool,
    frame: u8,
    light: bool,
    icons: HashMap<(Glyph, bool), Image<'static>>,
}

fn visual() -> &'static Mutex<Visual> {
    static V: OnceLock<Mutex<Visual>> = OnceLock::new();
    V.get_or_init(|| {
        Mutex::new(Visual {
            light: crate::platform::tray_background_is_light(),
            ..Default::default()
        })
    })
}

fn icon(v: &mut Visual, g: Glyph) -> Image<'static> {
    let light = v.light;
    v.icons
        .entry((g, light))
        .or_insert_with(|| render(g, light))
        .clone()
}

pub fn create(app: &AppHandle) -> tauri::Result<()> {
    let status = MenuItem::with_id(app, "status", "Starting…", false, None::<&str>)?;
    let show = MenuItem::with_id(app, "show", "Show status", true, None::<&str>)?;
    let open = MenuItem::with_id(app, "open", "Open Drive folder", true, None::<&str>)?;
    let pause = MenuItem::with_id(app, "pause", "Pause syncing", false, None::<&str>)?;
    let settings = MenuItem::with_id(app, "settings", "Settings…", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit Drive", true, None::<&str>)?;
    let update = MenuItem::with_id(app, "update", "Restart to update", true, None::<&str>)?;
    let sep1 = PredefinedMenuItem::separator(app)?;
    let sep2 = PredefinedMenuItem::separator(app)?;
    let menu = if cfg!(windows) {
        Menu::with_items(app, &[&open, &pause, &settings, &sep1, &quit])?
    } else {
        // Linux trays don't deliver clicks, so everything must be reachable from the menu.
        Menu::with_items(
            app,
            &[
                &status, &show, &sep1, &open, &pause, &settings, &sep2, &quit,
            ],
        )?
    };

    let first = {
        let mut v = visual().lock().unwrap();
        v.glyph = Some(Glyph::Inactive);
        icon(&mut v, Glyph::Inactive)
    };
    TrayIconBuilder::with_id(TRAY_ID)
        .icon(first)
        .tooltip("Drive – Starting…")
        .menu(&menu)
        .show_menu_on_left_click(!cfg!(windows))
        .on_menu_event(|app, ev| on_menu(app, ev.id().as_ref()))
        .on_tray_icon_event(|tray, ev| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                rect,
                ..
            } = ev
            {
                let pos = rect.position.to_physical::<f64>(1.0);
                let size = rect.size.to_physical::<f64>(1.0);
                let r = TrayRect {
                    x: pos.x,
                    y: pos.y,
                    w: size.width,
                    h: size.height,
                };
                windows::toggle_popover(tray.app_handle(), Some(r));
            }
        })
        .build(app)?;
    app.manage(Items {
        status,
        pause,
        menu,
        update,
        update_shown: AtomicBool::new(false),
    });
    Ok(())
}

fn on_menu(app: &AppHandle, id: &str) {
    let backend = app.state::<Arc<Backend>>().inner().clone();
    match id {
        "show" => windows::show_main(app, None),
        "settings" => windows::open_settings(app),
        "open" => match backend.status().folder {
            Some(folder) => crate::commands::open_path(app, &folder),
            None => windows::open_onboarding(app),
        },
        "pause" => {
            let call = if backend.status().state == SyncState::Paused {
                Call::Resume
            } else {
                Call::Pause
            };
            tauri::async_runtime::spawn(async move {
                if backend.call_value(call).await.is_ok() {
                    backend.refresh_status().await;
                }
            });
        }
        "update" => {
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                crate::updater::restart_to_update(&app, &backend).await
            });
        }
        "quit" => {
            let app = app.clone();
            tauri::async_runtime::spawn(
                async move { crate::commands::quit_now(&app, &backend).await },
            );
        }
        _ => {}
    }
}

/// Offers "Restart to update" for a downloaded update: just above "Quit Drive".
pub fn show_update(app: &AppHandle, version: &str) {
    let Some(items) = app.try_state::<Items>() else {
        return;
    };
    let _ = items
        .update
        .set_text(format!("Restart to update to {version}"));
    if !items.update_shown.swap(true, Ordering::SeqCst) {
        let quit_at = items
            .menu
            .items()
            .map(|i| i.len())
            .unwrap_or(1)
            .saturating_sub(1);
        if let Err(e) = items.menu.insert(&items.update, quit_at) {
            tracing::debug!("could not add the update menu item: {e}");
            items.update_shown.store(false, Ordering::SeqCst);
        }
    }
}

fn glyph_for(status: &Status, conn: &ConnectionInfo) -> (Glyph, bool) {
    if conn.state != Link::Connected {
        return (
            if conn.state == Link::Unavailable {
                Glyph::Error
            } else {
                Glyph::Inactive
            },
            false,
        );
    }
    if status.pending_delete_confirmation.is_some() {
        return (Glyph::Attention, false);
    }
    match status.state {
        SyncState::NotSetUp => (Glyph::Inactive, false),
        SyncState::SignedOut => (Glyph::Attention, false),
        SyncState::Error => (Glyph::Error, false),
        SyncState::Offline => (Glyph::Offline, false),
        SyncState::Paused => (Glyph::Paused, false),
        SyncState::Syncing => (Glyph::Syncing(0), status.uploading + status.downloading > 0),
        SyncState::UpToDate => (Glyph::UpToDate, false),
    }
}

fn headline(status: &Status, conn: &ConnectionInfo) -> String {
    match conn.state {
        Link::Connected => status.headline.clone(),
        Link::Connecting => "Starting…".into(),
        Link::Unavailable => "Drive isn't running".into(),
    }
}

pub fn update(app: &AppHandle, status: &Status, conn: &ConnectionInfo) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    let (glyph, animating) = glyph_for(status, conn);
    let image = {
        let mut v = visual().lock().unwrap();
        let same = v
            .glyph
            .map(|g| matches!((g, glyph), (Glyph::Syncing(_), Glyph::Syncing(_))) || g == glyph)
            == Some(true);
        v.animating = animating;
        if same {
            None
        } else {
            v.glyph = Some(glyph);
            v.frame = 0;
            Some(icon(&mut v, glyph))
        }
    };
    if let Some(image) = image {
        let _ = tray.set_icon(Some(image));
    }
    let line = headline(status, conn);
    // Windows truncates tooltips at 127 characters.
    let tooltip: String = format!("Drive – {line}").chars().take(127).collect();
    let _ = tray.set_tooltip(Some(&tooltip));

    if let Some(items) = app.try_state::<Items>() {
        let connected = conn.state == Link::Connected;
        let _ = items.status.set_text(&line);
        let paused = status.state == SyncState::Paused;
        let _ = items.pause.set_text(if paused {
            "Resume syncing"
        } else {
            "Pause syncing"
        });
        let can_pause = connected
            && matches!(
                status.state,
                SyncState::Syncing
                    | SyncState::UpToDate
                    | SyncState::Paused
                    | SyncState::Offline
                    | SyncState::Error
            );
        let _ = items.pause.set_enabled(can_pause);
    }
}

/// Advances the sync badge while transferring, and follows taskbar theme changes.
pub fn start_animation(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut iv = tokio::time::interval(Duration::from_millis(400));
        let mut ticks: u32 = 0;
        loop {
            iv.tick().await;
            ticks = ticks.wrapping_add(1);
            let theme_check = ticks.is_multiple_of(10);
            let image = {
                let mut v = visual().lock().unwrap();
                let mut redraw = false;
                if theme_check {
                    let light = crate::platform::tray_background_is_light();
                    if light != v.light {
                        v.light = light;
                        redraw = true;
                    }
                }
                if v.animating && matches!(v.glyph, Some(Glyph::Syncing(_))) {
                    v.frame = (v.frame + 1) % SYNC_FRAMES;
                    v.glyph = Some(Glyph::Syncing(v.frame));
                    redraw = true;
                } else if matches!(v.glyph, Some(Glyph::Syncing(f)) if f != 0) {
                    // Settle on the first frame once transfers stop.
                    v.frame = 0;
                    v.glyph = Some(Glyph::Syncing(0));
                    redraw = true;
                }
                match (redraw, v.glyph) {
                    (true, Some(g)) => Some(icon(&mut v, g)),
                    _ => None,
                }
            };
            if let (Some(image), Some(tray)) = (image, app.tray_by_id(TRAY_ID)) {
                let _ = tray.set_icon(Some(image));
            }
        }
    });
}
