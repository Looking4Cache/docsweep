//! Desktop app of Docsweep: window, tray and the bridge to `docsweep-core`.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod i18n;
mod tray;

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use docsweep_core::names;
use docsweep_core::paths::AppDirs;
use tauri::{Manager, WindowEvent};

/// Bundle ID, read from `tauri.conf.json` by the build script.
const BUNDLE_ID: &str = env!("DOCSWEEP_BUNDLE_ID");

/// What all commands share.
pub struct AppState {
    pub paths: commands::Paths,
    /// Set while a scan runs, so that two scans never touch a mailbox at once.
    pub scanning: AtomicBool,
}

fn resolve_paths() -> Option<commands::Paths> {
    let dirs = AppDirs::discover()?;
    let config = std::env::var_os(names::ENV_CONFIG)
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs.config_file());
    Some(commands::Paths {
        config,
        index: dirs.index_file(),
    })
}

fn main() {
    let paths = resolve_paths().expect("Docsweep needs a home directory");
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState {
            paths,
            scanning: AtomicBool::new(false),
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_state,
            commands::save_config,
            commands::set_secret,
            commands::test_mailbox,
            commands::run_scan,
            commands::list_invoices,
            commands::list_runs,
            commands::complete_review,
            commands::reveal_invoice,
            commands::pick_folder,
        ])
        .setup(|app| {
            // Without a tray there would be no way back to a hidden window, so
            // closing only hides the window if the tray could be created.
            match tray::create(app.handle()) {
                Ok(()) => {
                    let handle = app.handle().clone();
                    if let Some(window) = app.get_webview_window("main") {
                        window.on_window_event(move |event| {
                            if let WindowEvent::CloseRequested { api, .. } = event {
                                api.prevent_close();
                                if let Some(window) = handle.get_webview_window("main") {
                                    let _ = window.hide();
                                }
                            }
                        });
                    }
                }
                Err(error) => eprintln!("no tray available: {error}"),
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("failed to start the Docsweep window");

    app.run(|handle, event| {
        // macOS: clicking the Dock icon brings a hidden window back.
        #[cfg(target_os = "macos")]
        if let tauri::RunEvent::Reopen { .. } = event {
            tray::show_window(handle);
        }
        #[cfg(not(target_os = "macos"))]
        let _ = (handle, event);
    });
}
