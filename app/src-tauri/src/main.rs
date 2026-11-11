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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "drive_app=info,warn".into()),
        )
        .init();

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
        ])
        .setup(move |app| {
            let handle = app.handle().clone();
            let backend = Arc::new(Backend::new(handle.clone()));
            app.manage(backend.clone());
            app.manage(windows::PopoverState::default());
            tray::create(&handle)?;
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
