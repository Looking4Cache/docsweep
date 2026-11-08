//! The loop behind automatic runs. Everything it decides lives in
//! `docsweep_core::automation`; this only brings the clock, the browser and the
//! dialog, and tells the tray and the interface what is going on.

use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use chrono::{DateTime, Local, Utc};
use docsweep_browser::{BrowserFetcher, WindowMode};
use docsweep_core::automation::{Tick, run_tick, worth_recording};
use docsweep_core::config::Config;
use docsweep_core::portal::engine::RunOrigin;
use docsweep_core::secrets::KeyringStore;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::commands::{self, GuiConfirmation};
use crate::{AppState, tray};

/// Gives the network and the window time to come up after a login.
const START_DELAY: Duration = Duration::from_secs(60);
/// A due run that had to wait starts this soon after the other one ended.
const TICK: Duration = Duration::from_secs(15);

const EVENT_STATUS: &str = "automation-status";

/// What the interface hears about automatic runs.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct AutomationStatus {
    pub running: bool,
}

/// Starts the loop. A run blocks this thread, which is fine: there is only ever
/// one, and manual runs are kept apart by the gate.
pub fn start(app: AppHandle) {
    thread::spawn(move || {
        thread::sleep(START_DELAY);
        let mut last_poll = None;
        loop {
            tick(&app, &mut last_poll);
            thread::sleep(TICK);
        }
    });
}

fn tick(app: &AppHandle, last_poll: &mut Option<DateTime<Utc>>) {
    let state = app.state::<AppState>();
    let paths = &state.paths;
    // Read every time, so that changes in Settings count at once. A broken file
    // is already shown in the window, and nothing can run without it.
    let Ok(config) = Config::load(&paths.config) else {
        return;
    };
    if !config.automation.enabled {
        return;
    }
    let Ok(runtime) = commands::current_thread_runtime() else {
        return;
    };
    let mut handler = GuiConfirmation::new(app.clone(), &state.confirmation);
    let mut fetcher = BrowserFetcher {
        config: &config,
        secrets: &KeyringStore,
        dirs: &paths.dirs,
        index: &paths.index,
        window: WindowMode::OnDemand,
        handler: &mut handler,
        // Every repair send of an automatic run waits for a person's yes.
        origin: RunOrigin::Automatic,
    };
    let result = runtime.block_on(run_tick(
        &config,
        &KeyringStore,
        &paths.index,
        &state.scanning,
        last_poll,
        Local::now(),
        &mut fetcher,
        || set_running(app, true),
    ));
    match result {
        Ok(Tick::Ran(report)) => {
            if worth_recording(&report)
                && let Err(error) = commands::record_scan(paths, &report, true)
            {
                eprintln!("{error}");
            }
            set_running(app, false);
        }
        Ok(Tick::Off | Tick::Busy | Tick::Idle) => {}
        Err(error) => {
            eprintln!("automatic run failed: {error}");
            set_running(app, false);
            tray::show_failure(app);
        }
    }
}

fn set_running(app: &AppHandle, running: bool) {
    let state = app.state::<AppState>();
    state.automatic_run.store(running, Ordering::SeqCst);
    tray::refresh_status(app);
    let _ = app.emit(EVENT_STATUS, AutomationStatus { running });
}
