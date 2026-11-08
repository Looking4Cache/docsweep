//! Commands of assisted pick-ups, and the thread that looks at the downloads folder
//! during a session. Everything it decides lives in `docsweep_core::pickup`; this
//! only brings the clock, the default browser and the dialog.

use std::sync::MutexGuard;
use std::thread;
use std::time::Duration;

use chrono::{DateTime, Local, Utc};
use docsweep_core::config::Config;
use docsweep_core::index::Index;
use docsweep_core::pickup::{
    ImportOutcome, LocalFolder, PickupError, PickupItem, PickupSession, SessionView,
    dismiss_pickup as dismiss, pending_pickups,
};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::commands::{UiError, blocking};
use crate::{AppState, tray};

/// A download has come to rest once it looks the same from one look to the next.
const POLL: Duration = Duration::from_secs(1);

const EVENT_STATUS: &str = "pickup-status";

/// The one session there can be. The generation tells a watching thread whether
/// the session it was started for is still the current one.
#[derive(Debug, Default)]
pub struct PickupSlot {
    session: Option<PickupSession>,
    generation: u64,
}

impl PickupSlot {
    /// What the interface should show. A session that stopped watching and has
    /// nothing left to decide is over.
    fn view(&mut self, now: DateTime<Utc>) -> Option<SessionView> {
        let finished = self
            .session
            .as_ref()
            .is_some_and(|session| !session.is_watching(now) && session.candidates().is_empty());
        if finished {
            self.session = None;
        }
        self.session.as_ref().map(|session| session.view(now))
    }

    fn end(&mut self) {
        self.session = None;
        self.generation += 1;
    }

    fn session_mut(&mut self) -> Result<&mut PickupSession, UiError> {
        self.session
            .as_mut()
            .ok_or_else(|| UiError::new(PickupError::NotOffered))
    }
}

fn slot_of(state: &AppState) -> MutexGuard<'_, PickupSlot> {
    state.pickup.lock().expect("lock is never poisoned")
}

/// The invoices waiting to be picked up, for the Scan page and its badge.
#[tauri::command]
pub async fn list_pickups(state: State<'_, AppState>) -> Result<Vec<PickupItem>, UiError> {
    let paths = state.paths.clone();
    blocking(move || {
        // A file that cannot be used is already shown in the window.
        let Ok(config) = Config::load(&paths.config) else {
            return Ok(Vec::new());
        };
        match Index::open_read_only(&paths.index)? {
            Some(index) => Ok(pending_pickups(&config, &index)?),
            None => Ok(Vec::new()),
        }
    })
    .await
}

/// A person marked a pick-up as done, after the interface asked them to confirm.
/// It takes the index id of the link mail. The mailbox is not touched.
#[tauri::command]
pub async fn dismiss_pickup(state: State<'_, AppState>, id: i64) -> Result<(), UiError> {
    let paths = state.paths.clone();
    blocking(move || {
        let config = Config::load(&paths.config)?;
        let index = Index::open(&paths.index)?;
        dismiss(&config, &index, id).map_err(UiError::new)
    })
    .await
}

#[tauri::command]
pub fn pickup_status(state: State<'_, AppState>) -> Option<SessionView> {
    slot_of(&state).view(Utc::now())
}

/// Opens the company's portal address from the configuration in the default
/// browser, and starts looking at the downloads folder. Only ever a person's click
/// gets here; automatic runs do not.
#[tauri::command]
pub async fn start_pickup(app: AppHandle, company_id: String) -> Result<SessionView, UiError> {
    blocking(move || {
        let state = app.state::<AppState>();
        let config = Config::load(&state.paths.config)?;
        let dir = config
            .downloads_dir()
            .ok_or_else(|| UiError::new(PickupError::NoDownloadsFolder))?;
        let now = Utc::now();
        let mut slot = slot_of(&state);
        if slot.view(now).is_some() {
            return Err(UiError::new("a pick-up is still going on"));
        }
        let session = PickupSession::start(
            &config,
            &company_id,
            Box::new(LocalFolder::new(dir)),
            now,
            |url| tauri_plugin_opener::open_url(url, None::<&str>).map_err(|e| e.to_string()),
        )
        .map_err(UiError::new)?;
        let view = session.view(now);
        slot.session = Some(session);
        slot.generation += 1;
        watch(app.clone(), slot.generation, config);
        Ok(view)
    })
    .await
}

/// Looks once a second, until the session ends or another one takes its place.
fn watch(app: AppHandle, generation: u64, config: Config) {
    thread::spawn(move || {
        loop {
            thread::sleep(POLL);
            let state = app.state::<AppState>();
            let now = Utc::now();
            let mut slot = slot_of(&state);
            if slot.generation != generation {
                return;
            }
            let Some(session) = slot.session.as_mut() else {
                return;
            };
            let waiting = session.candidates().len();
            let watching = session.poll(&config, &state.paths.index, now);
            let found = session.candidates().len() > waiting;
            let view = slot.view(now);
            drop(slot);
            if found {
                // The person started this and is waiting for it, so the question
                // may come to the front. Automatic runs never get here.
                tray::show_window(&app);
            }
            if found || !watching {
                let _ = app.emit(EVENT_STATUS, view);
            }
            if !watching {
                return;
            }
        }
    });
}

/// What a person's answer led to.
#[derive(Debug, Serialize)]
pub struct PickupAnswer {
    outcome: ImportOutcome,
    session: Option<SessionView>,
}

/// A person confirmed that this file belongs to the session's company. It takes a
/// name the session offered rather than a path.
#[tauri::command]
pub async fn confirm_pickup(app: AppHandle, file_name: String) -> Result<PickupAnswer, UiError> {
    blocking(move || {
        let state = app.state::<AppState>();
        let config = Config::load(&state.paths.config)?;
        let index = Index::open(&state.paths.index)?;
        let mut slot = slot_of(&state);
        let outcome = slot
            .session_mut()?
            .confirm(&config, &index, &file_name, Local::now().date_naive())
            .map_err(UiError::new)?;
        Ok(PickupAnswer {
            outcome,
            session: slot.view(Utc::now()),
        })
    })
    .await
}

/// A person said this file does not belong here. It stays where it is.
#[tauri::command]
pub fn ignore_pickup(
    state: State<'_, AppState>,
    file_name: String,
) -> Result<Option<SessionView>, UiError> {
    let mut slot = slot_of(&state);
    slot.session_mut()?
        .ignore(&file_name)
        .map_err(UiError::new)?;
    Ok(slot.view(Utc::now()))
}

/// Ends the session. Nothing looks at the folder after this, and files nobody
/// decided on stay where they are.
#[tauri::command]
pub fn end_pickup(state: State<'_, AppState>) {
    slot_of(&state).end();
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;

    const CONFIG: &str = r#"
mail:
  accounts:
    - { id: main, imap: "127.0.0.1:1", user: u, password: "keyring:p", watch: INBOX }
  disposal: { mode: all_delete }
companies:
  - id: acme
    name: Acme
    portal: { mode: assisted, url: "https://portal.example/" }
    output: { dir: invoices, filename: "{invoice_no}.pdf" }
"#;

    fn slot_with_session(dir: &std::path::Path, now: DateTime<Utc>) -> PickupSlot {
        let config = Config::from_yaml(CONFIG).unwrap();
        let session = PickupSession::start(
            &config,
            "acme",
            Box::new(LocalFolder::new(dir.to_path_buf())),
            now,
            |_| Ok(()),
        )
        .unwrap();
        PickupSlot {
            session: Some(session),
            generation: 1,
        }
    }

    #[test]
    fn a_session_is_over_at_the_limit_when_nothing_is_left_to_decide() {
        let dir = tempfile::tempdir().unwrap();
        let start = Utc::now();
        let mut slot = slot_with_session(dir.path(), start);

        assert!(slot.view(start).is_some());
        assert!(slot.view(start + TimeDelta::hours(1)).is_none());
        assert!(slot.session.is_none());
    }

    #[test]
    fn ending_a_session_tells_its_thread_to_stop() {
        let dir = tempfile::tempdir().unwrap();
        let start = Utc::now();
        let mut slot = slot_with_session(dir.path(), start);

        slot.end();

        assert!(slot.view(start).is_none());
        assert_eq!(slot.generation, 2);
    }

    #[test]
    fn answering_without_a_session_is_an_error() {
        let mut slot = PickupSlot::default();
        assert!(slot.session_mut().is_err());
    }
}
