//! Drive tray app: a tray icon, a status popover and the onboarding and settings windows.
//!
//! All sync logic lives in the `drive daemon` service. This process only talks to it over
//! the local socket (see `drive_core::ipc`) and forwards its events to the webviews.

// No console window in release builds on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod backend;
mod commands;
mod mock;
mod notify;
mod platform;
mod tray;
mod tray_icon;
mod updater;
mod windows;

use std::sync::Arc;

use tauri::Manager;

use crate::backend::Backend;

fn main() {
    let _log = init_logging();

    let autostarted = std::env::args().any(|a| a == "--autostart");

    tauri::Builder::default()
        // Must be registered first: a second launch just brings the running instance forward.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            windows::show_main(app, None);
        }))
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--autostart"]),
        ))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .invoke_handler(tauri::generate_handler![
            commands::app_info,
            commands::connection_state,
            commands::get_status,
            commands::get_settings,
            commands::update_settings,
            commands::recent_activity,
            commands::list_issues,
            commands::resolve_issue,
            commands::transfers,
            commands::set_transfer_mode,
            commands::pause,
            commands::resume,
            commands::sync_now,
            commands::check_account,
            commands::setup,
            commands::default_folder,
            commands::pick_folder,
            commands::save_recovery_kit,
            commands::sign_out,
            commands::open_folder,
            commands::reveal_item,
            commands::open_url,
            commands::show_window,
            commands::finish_onboarding,
            commands::hide_popover,
            commands::close_main,
            commands::reconnect,
            commands::quit,
            updater::update_state,
            updater::check_for_updates,
            updater::install_update,
            updater::update_changelog,
        ])
        .setup(move |app| {
            let handle = app.handle().clone();
            let backend = Arc::new(Backend::new(handle.clone()));
            app.manage(backend.clone());
            app.manage(windows::PopoverState::default());
            tray::create(&handle)?;
            windows::prepare_popover(&handle);
            backend.start(autostarted);
            updater::start(&handle);
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("failed to start Drive")
        .run(|_app, event| {
            // Closing every window keeps the app running in the tray.
            if let tauri::RunEvent::ExitRequested {
                code: None, api, ..
            } = event
            {
                api.prevent_exit();
            }
        });
}

/// Logs to the console and, in release builds (which have no console), to
/// `%LOCALAPPDATA%\Drive\logs\app.*.log` / `~/.local/share/Drive/logs/app.*.log` next to the
/// service's logs, kept for a week.
fn init_logging() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::prelude::*;
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "drive_app=info,warn".into());
    let dir = dirs::data_local_dir().map(|d| d.join("Drive").join("logs"));
    let file = dir.and_then(|d| {
        std::fs::create_dir_all(&d).ok()?;
        tracing_appender::rolling::Builder::new()
            .rotation(tracing_appender::rolling::Rotation::DAILY)
            // Development builds keep their own log so test runs never mix into a real install's.
            .filename_prefix(if cfg!(debug_assertions) {
                "app-dev"
            } else {
                "app"
            })
            .filename_suffix("log")
            .max_log_files(7)
            .build(d)
            .ok()
    });
    let (layer, guard) = match file {
        Some(f) => {
            let (w, g) = tracing_appender::non_blocking(f);
            (
                Some(
                    tracing_subscriber::fmt::layer()
                        .with_writer(w)
                        .with_ansi(false),
                ),
                Some(g),
            )
        }
        None => (None, None),
    };
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .with(layer)
        .init();
    guard
}
