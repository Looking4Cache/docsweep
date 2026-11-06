//! The tray icon. "Scan" only opens a preview: nothing changes without a look.

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager};

use crate::i18n::text;

/// Event telling the interface to start a preview.
const EVENT_PREVIEW: &str = "tray-preview";

/// Brings the main window to the front.
pub fn show_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.set_focus();
    }
}

/// Creates the tray icon with its menu.
pub fn create(app: &AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", text("tray.open"), true, None::<&str>)?;
    let preview = MenuItem::with_id(app, "preview", text("tray.preview"), true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", text("tray.quit"), true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &preview, &separator, &quit])?;

    let mut builder = TrayIconBuilder::with_id("main")
        .tooltip(docsweep_core::names::DISPLAY_NAME)
        .menu(&menu)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "open" => show_window(app),
            "preview" => {
                show_window(app);
                let _ = app.emit(EVENT_PREVIEW, ());
            }
            "quit" => app.exit(0),
            _ => {}
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;
    Ok(())
}
