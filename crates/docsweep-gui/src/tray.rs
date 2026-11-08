//! The tray icon. "Scan" only opens a preview: nothing changes without a look. A
//! line in the menu tells whether an automatic run is active, or how the last went.

use std::sync::atomic::Ordering;

use chrono::{DateTime, Local};
use docsweep_core::index::{Index, ScanRun};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, Wry};

use crate::AppState;
use crate::i18n::text;

/// How many runs to look through for the last automatic one.
const RUNS_TO_SEARCH: u32 = 100;

/// The status line of the menu.
struct StatusLine(MenuItem<Wry>);

/// The menu entries with fixed text, kept to relabel them in another language.
struct Labels(Vec<(MenuItem<Wry>, &'static str)>);

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
    let status = MenuItem::with_id(app, "status", text("tray.status_none"), false, None::<&str>)?;
    let open = MenuItem::with_id(app, "open", text("tray.open"), true, None::<&str>)?;
    let preview = MenuItem::with_id(app, "preview", text("tray.preview"), true, None::<&str>)?;
    let first_separator = PredefinedMenuItem::separator(app)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", text("tray.quit"), true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &status,
            &first_separator,
            &open,
            &preview,
            &separator,
            &quit,
        ],
    )?;
    app.manage(StatusLine(status));
    app.manage(Labels(vec![
        (open, "tray.open"),
        (preview, "tray.preview"),
        (quit, "tray.quit"),
    ]));
    refresh_status(app);

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

fn show_status(app: &AppHandle, line: &str) {
    if let Some(status) = app.try_state::<StatusLine>() {
        let _ = status.0.set_text(line);
    }
}

/// Writes the menu again in the current language.
pub fn relabel(app: &AppHandle) {
    if let Some(labels) = app.try_state::<Labels>() {
        for (item, key) in &labels.0 {
            let _ = item.set_text(text(key));
        }
    }
    refresh_status(app);
}

/// Shows that an automatic run could not get going at all.
pub fn show_failure(app: &AppHandle) {
    show_status(app, &text("tray.status_error"));
}

/// Shows what is true right now.
pub fn refresh_status(app: &AppHandle) {
    let state = app.state::<AppState>();
    if state.automatic_run.load(Ordering::SeqCst) {
        show_status(app, &text("tray.status_running"));
        return;
    }
    let last = Index::open_read_only(&state.paths.index)
        .ok()
        .flatten()
        .and_then(|index| index.recent_runs(RUNS_TO_SEARCH).ok())
        .and_then(|runs| runs.into_iter().find(|run| run.automatic));
    show_status(app, &status_line(last.as_ref()));
}

fn status_line(last: Option<&ScanRun>) -> String {
    let Some(run) = last else {
        return text("tray.status_none");
    };
    let failed = serde_json::from_str::<serde_json::Value>(&run.summary)
        .ok()
        .and_then(|summary| summary["failed"].as_u64())
        .is_some_and(|failed| failed > 0);
    let time = DateTime::parse_from_rfc3339(&run.finished_at)
        .map(|time| {
            time.with_timezone(&Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|_| run.finished_at.clone());
    let key = if failed {
        "tray.status_last_failed"
    } else {
        "tray.status_last_ok"
    };
    text(key).replace("{time}", &time)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(summary: &str) -> ScanRun {
        ScanRun {
            id: 1,
            finished_at: "2026-11-06T08:00:00Z".into(),
            summary: summary.into(),
            report: "[]".into(),
            automatic: true,
        }
    }

    #[test]
    fn the_status_line_tells_how_the_last_automatic_run_went() {
        assert_eq!(status_line(None), text("tray.status_none"));
        assert!(status_line(Some(&run(r#"{"failed":0}"#))).contains("no problems"));
        let line = status_line(Some(&run(r#"{"failed":2}"#)));
        assert!(line.contains("with problems"), "{line}");
        assert!(line.contains("2026-11-0"), "{line}");
        assert!(!line.contains('{'), "{line}");
    }
}
