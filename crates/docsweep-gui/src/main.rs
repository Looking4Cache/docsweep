//! Desktop app of Docsweep. Only a stub until the GUI milestone.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde::Serialize;

/// Bundle ID, read from `tauri.conf.json` by the build script.
const BUNDLE_ID: &str = env!("DOCSWEEP_BUNDLE_ID");

/// Application facts shown by the frontend.
#[derive(Debug, Serialize)]
struct AppInfo {
    name: &'static str,
    version: &'static str,
    bundle_id: &'static str,
}

fn app_info() -> AppInfo {
    AppInfo {
        name: docsweep_core::names::DISPLAY_NAME,
        version: env!("CARGO_PKG_VERSION"),
        bundle_id: BUNDLE_ID,
    }
}

#[tauri::command]
fn get_app_info() -> AppInfo {
    app_info()
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![get_app_info])
        .run(tauri::generate_context!())
        .expect("failed to start the Docsweep window");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_info_uses_the_binding_names() {
        let info = app_info();
        assert_eq!(info.name, "Docsweep");
        assert!(!info.bundle_id.is_empty());
    }
}
