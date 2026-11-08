//! Desktop app of Docsweep: window, tray and the bridge to `docsweep-core`.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod automation;
mod commands;
mod i18n;
mod pickup;
mod tray;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use docsweep_core::automation::RunGate;
use docsweep_core::names;
use docsweep_core::paths::AppDirs;
use tauri::{Manager, WindowEvent};
use tauri_plugin_autostart::MacosLauncher;

/// Bundle ID, read from `tauri.conf.json` by the build script.
const BUNDLE_ID: &str = env!("DOCSWEEP_BUNDLE_ID");

/// With this argument the app starts in the tray; a start at login passes it.
const START_HIDDEN: &str = "--hidden";

/// What all commands share.
pub struct AppState {
    pub paths: commands::Paths,
    /// Held by whatever runs, so that two runs never touch a mailbox or a portal
    /// at once: scans, previews, fetches by hand and automatic runs.
    pub scanning: RunGate,
    /// Set while the run holding the gate is an automatic one.
    pub automatic_run: AtomicBool,
    /// The question to a person that a portal fetch is waiting on.
    pub confirmation: Arc<commands::ConfirmationSlot>,
    /// A recipe draft that is waiting for a person's decision.
    pub recipe_work: std::sync::Mutex<commands::RecipeWork>,
    /// The assisted pick-up that is going on, if any.
    pub pickup: std::sync::Mutex<pickup::PickupSlot>,
}

fn resolve_paths() -> Option<commands::Paths> {
    let dirs = AppDirs::discover()?;
    let config = std::env::var_os(names::ENV_CONFIG)
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs.config_file());
    Some(commands::Paths {
        config,
        index: dirs.index_file(),
        dirs,
    })
}

fn main() {
    let paths = resolve_paths().expect("Docsweep needs a home directory");
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        // Only ever driven by the app's own commands, so the web view gets no
        // permission for it.
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec![START_HIDDEN]),
        ))
        .manage(AppState {
            paths,
            scanning: RunGate::default(),
            automatic_run: AtomicBool::new(false),
            confirmation: Arc::default(),
            recipe_work: std::sync::Mutex::default(),
            pickup: std::sync::Mutex::default(),
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_state,
            commands::save_config,
            commands::set_secret,
            commands::test_mailbox,
            commands::set_portal_credentials,
            commands::set_totp_key,
            commands::run_scan,
            commands::run_fetch,
            commands::answer_confirmation,
            commands::prepare_recipe,
            commands::send_recipe_prompt,
            commands::save_recipe_draft,
            commands::discard_recipe_work,
            commands::record_recipe,
            commands::finish_recording,
            commands::list_invoices,
            commands::list_runs,
            commands::complete_review,
            commands::reveal_invoice,
            commands::pick_folder,
            commands::automation_status,
            commands::describe_schedule,
            commands::autostart_enabled,
            commands::set_autostart,
            commands::set_language,
            pickup::list_pickups,
            pickup::dismiss_pickup,
            pickup::pickup_status,
            pickup::start_pickup,
            pickup::confirm_pickup,
            pickup::ignore_pickup,
            pickup::end_pickup,
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
                        if std::env::args().any(|argument| argument == START_HIDDEN) {
                            let _ = window.hide();
                        }
                    }
                }
                Err(error) => eprintln!("no tray available: {error}"),
            }
            automation::start(app.handle().clone());
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
