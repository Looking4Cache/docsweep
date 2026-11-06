//! Commands of the interface. Secrets only ever travel into the keychain: no
//! command returns one, and errors never contain one.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Local, NaiveDate, TimeZone};
use docsweep_browser::{
    BrowserFetcher, ChromeOptions, FetchOptions, FetchPaths, Recorder, WindowMode, fetch_company,
    read_page,
};
use docsweep_core::ai::generate::{generation_prompt, known_hosts, request_draft};
use docsweep_core::ai::provider::{self, ANTHROPIC_DEFAULT_MODEL};
use docsweep_core::ai::sanitize::{RawPage, SanitizedPrompt};
use docsweep_core::config::{
    Company, Config, ConfigError, Issue, MailAccount, SecretRef, system_downloads_dir,
};
use docsweep_core::index::{Index, IndexError, InvoiceRecord};
use docsweep_core::mail::execute::{self, AccountReport, ScanReport, ScanSummary, summarize};
use docsweep_core::names;
use docsweep_core::paths::AppDirs;
use docsweep_core::portal::credentials::{PortalCredentials, TotpGenerator};
use docsweep_core::portal::draft::{WriteError, draft_path, draft_text, write_draft};
use docsweep_core::portal::driver::{
    AiSendRequest, ConfirmationAnswer, ConfirmationHandler, ConfirmationRequest,
};
use docsweep_core::portal::engine::{FetchReport, FetchSummary, summarize_fetches};
use docsweep_core::portal::hosts::{HostChange, is_web_address};
use docsweep_core::portal::recording::build_draft;
use docsweep_core::review;
use docsweep_core::schedule::Schedule;
use docsweep_core::secrets::{KeyringStore, SecretStore, SecretString};
use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Emitter, State};
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_dialog::DialogExt;
use tokio::sync::oneshot;

use crate::automation::AutomationStatus;
use crate::{AppState, BUNDLE_ID};

const MAX_INVOICES: u32 = 1000;
const MAX_RUNS: u32 = 100;
const ALREADY_RUNNING: &str = "a scan or portal fetch is already running";

/// The files the app works with.
#[derive(Debug, Clone)]
pub struct Paths {
    pub config: PathBuf,
    pub index: PathBuf,
    /// Recipes and browser profiles.
    pub dirs: AppDirs,
}

/// An error as the interface sees it.
#[derive(Debug, Serialize)]
pub struct UiError {
    message: String,
    /// Validation problems, which the interface shows next to their fields.
    issues: Vec<Issue>,
}

impl UiError {
    pub fn new(message: impl ToString) -> Self {
        Self {
            message: message.to_string(),
            issues: Vec::new(),
        }
    }
}

impl From<ConfigError> for UiError {
    fn from(error: ConfigError) -> Self {
        let message = error.to_string();
        let issues = match error {
            ConfigError::Invalid { issues, .. } => issues,
            _ => Vec::new(),
        };
        Self { message, issues }
    }
}

impl From<IndexError> for UiError {
    fn from(error: IndexError) -> Self {
        Self::new(error)
    }
}

/// Facts about the application.
#[derive(Debug, Serialize)]
pub struct AppInfo {
    name: &'static str,
    version: &'static str,
    bundle_id: &'static str,
}

/// Everything the interface needs at start.
#[derive(Debug, Serialize)]
pub struct AppSnapshot {
    info: AppInfo,
    config_path: PathBuf,
    /// `None` on first start, or if the file cannot be used.
    config: Option<Config>,
    config_error: Option<UiError>,
    /// For each mailbox id, whether its password is in the keychain.
    stored_passwords: Vec<(String, bool)>,
    /// For each company with a portal, which of its secrets are in the keychain.
    stored_portal_secrets: Vec<(String, PortalSecrets)>,
    /// Whether the API key of the AI provider is in the keychain; never its value.
    llm_key_stored: bool,
    anthropic_default_model: &'static str,
    /// Where pick-ups look unless Settings say otherwise.
    system_downloads_dir: Option<PathBuf>,
}

/// Whether the secrets of a portal exist; never their values.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct PortalSecrets {
    credentials: bool,
    totp: bool,
}

fn snapshot(paths: &Paths, secrets: &dyn SecretStore) -> AppSnapshot {
    let info = AppInfo {
        name: names::DISPLAY_NAME,
        version: env!("CARGO_PKG_VERSION"),
        bundle_id: BUNDLE_ID,
    };
    let loaded = paths
        .config
        .exists()
        .then(|| Config::load(&paths.config))
        .transpose();
    let (config, config_error) = match loaded {
        Ok(config) => (config, None),
        Err(error) => (None, Some(UiError::from(error))),
    };
    let stored_passwords = config
        .iter()
        .flat_map(|config| &config.mail.accounts)
        .map(|account| {
            let stored = account
                .password
                .id()
                .is_some_and(|id| secrets.get(id).is_ok());
            (account.id.clone(), stored)
        })
        .collect();
    let is_stored = |reference: Option<&SecretRef>| {
        reference
            .and_then(SecretRef::id)
            .is_some_and(|id| secrets.get(id).is_ok())
    };
    let stored_portal_secrets = config
        .iter()
        .flat_map(|config| &config.companies)
        .filter_map(|company| {
            let portal = company.portal.as_ref()?;
            let stored = PortalSecrets {
                credentials: is_stored(portal.credentials.as_ref()),
                totp: is_stored(portal.totp.as_ref()),
            };
            Some((company.id.clone(), stored))
        })
        .collect();
    let llm_key_stored = is_stored(
        config
            .as_ref()
            .and_then(|config| config.llm.as_ref())
            .map(|llm| &llm.api_key),
    );
    AppSnapshot {
        info,
        config_path: paths.config.clone(),
        config,
        config_error,
        stored_passwords,
        stored_portal_secrets,
        llm_key_stored,
        anthropic_default_model: ANTHROPIC_DEFAULT_MODEL,
        system_downloads_dir: system_downloads_dir(),
    }
}

#[tauri::command]
pub async fn get_state(state: State<'_, AppState>) -> Result<AppSnapshot, UiError> {
    let paths = state.paths.clone();
    blocking(move || Ok(snapshot(&paths, &KeyringStore))).await
}

#[tauri::command]
pub fn save_config(state: State<'_, AppState>, config: Config) -> Result<(), UiError> {
    config.save(&state.paths.config)?;
    Ok(())
}

/// Stores a secret. The value goes in once and cannot be read back.
#[tauri::command]
pub async fn set_secret(id: String, value: String) -> Result<(), UiError> {
    blocking(move || store_secret(&KeyringStore, &id, value)).await
}

fn store_secret(secrets: &dyn SecretStore, id: &str, value: String) -> Result<(), UiError> {
    if SecretRef::from_id(id).id().is_none() {
        return Err(UiError::new("invalid secret id"));
    }
    if value.is_empty() {
        return Err(UiError::new("the password must not be empty"));
    }
    secrets
        .set(id, &SecretString::from(value))
        .map_err(UiError::new)
}

/// Stores user name and password of a portal. They go in once and cannot be read
/// back.
#[tauri::command]
pub async fn set_portal_credentials(
    id: String,
    user: String,
    password: String,
) -> Result<(), UiError> {
    blocking(move || store_portal_credentials(&KeyringStore, &id, user, password)).await
}

fn store_portal_credentials(
    secrets: &dyn SecretStore,
    id: &str,
    user: String,
    password: String,
) -> Result<(), UiError> {
    if SecretRef::from_id(id).id().is_none() {
        return Err(UiError::new("invalid secret id"));
    }
    let credentials = PortalCredentials::new(user, password).map_err(UiError::new)?;
    secrets
        .set(id, &credentials.to_stored())
        .map_err(UiError::new)
}

/// Stores the TOTP setup key of a portal. It goes in once and cannot be read back.
#[tauri::command]
pub async fn set_totp_key(id: String, key: String) -> Result<(), UiError> {
    blocking(move || store_totp_key(&KeyringStore, &id, key)).await
}

fn store_totp_key(secrets: &dyn SecretStore, id: &str, key: String) -> Result<(), UiError> {
    if SecretRef::from_id(id).id().is_none() {
        return Err(UiError::new("invalid secret id"));
    }
    let key = SecretString::from(key);
    TotpGenerator::new(&key).map_err(UiError::new)?;
    secrets.set(id, &key).map_err(UiError::new)
}

/// Tries a mailbox without changing anything; returns how many mails it holds.
#[tauri::command]
pub async fn test_mailbox(account: MailAccount) -> Result<u32, UiError> {
    blocking(move || {
        let runtime = current_thread_runtime()?;
        runtime
            .block_on(execute::test_mailbox(&account, &KeyringStore))
            .map_err(UiError::new)
    })
    .await
}

/// A recipe draft on its way to a file. Between the commands, it stays here and
/// not in the interface, so that what was shown is also what gets sent or written.
#[derive(Debug, Default)]
pub struct RecipeWork {
    prompt: Option<PreparedPrompt>,
    draft: Option<PendingDraft>,
    stop_recording: Option<oneshot::Sender<()>>,
}

#[derive(Debug)]
struct PreparedPrompt {
    company_id: String,
    address: String,
    prompt: SanitizedPrompt,
}

#[derive(Debug)]
struct PendingDraft {
    path: PathBuf,
    text: String,
    host_changes: Vec<HostChange>,
}

/// The text a person has to agree to before anything is sent.
#[derive(Debug, Serialize)]
pub struct PromptView {
    provider: String,
    text: String,
}

/// A draft that has not been written yet.
#[derive(Debug, Serialize)]
pub struct DraftView {
    /// As it would go into the company's recipe field.
    file_name: String,
    text: String,
    host_changes: Vec<HostChange>,
    /// Writing would replace this file.
    exists: bool,
}

impl PendingDraft {
    fn view(&self) -> DraftView {
        DraftView {
            file_name: self
                .path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            text: self.text.clone(),
            host_changes: self.host_changes.clone(),
            exists: self.path.exists(),
        }
    }

    /// Nothing is written unless a person agreed to every new host and to
    /// replacing a file, whatever the interface says it has shown.
    fn write(&self, hosts_confirmed: bool, overwrite: bool) -> Result<(), UiError> {
        if !self.host_changes.is_empty() && !hosts_confirmed {
            return Err(UiError::new(
                "this draft leaves the portal's known host, which needs your confirmation",
            ));
        }
        write_draft(&self.path, &self.text, overwrite).map_err(|error| match error {
            WriteError::Exists(path) => UiError::new(format!(
                "{} already exists; it was left as it is",
                path.display()
            )),
            other => UiError::new(other),
        })
    }
}

fn company_of<'a>(config: &'a Config, company_id: &str) -> Result<&'a Company, UiError> {
    config
        .companies
        .iter()
        .find(|company| company.id == company_id)
        .ok_or_else(|| UiError::new(format!("no company `{company_id}` in the configuration")))
}

fn require_web_address(address: &str) -> Result<(), UiError> {
    if is_web_address(address) {
        Ok(())
    } else {
        Err(UiError::new("the address must start with https://"))
    }
}

fn locked(state: &AppState) -> std::sync::MutexGuard<'_, RecipeWork> {
    state.recipe_work.lock().expect("lock is never poisoned")
}

/// Looks at the page without logging in, and returns what would be sent. Nothing
/// has left this computer yet.
#[tauri::command]
pub async fn prepare_recipe(
    state: State<'_, AppState>,
    company_id: String,
    address: String,
    description: String,
) -> Result<PromptView, UiError> {
    require_web_address(&address)?;
    let Some(_permit) = state.scanning.try_start() else {
        return Err(UiError::new(ALREADY_RUNNING));
    };
    let paths = state.paths.clone();
    let company = company_id.clone();
    let target = address.clone();
    let (view, prompt) = blocking(move || {
        let config = Config::load(&paths.config)?;
        let company = company_of(&config, &company)?;
        let llm = config
            .llm
            .as_ref()
            .ok_or_else(|| UiError::new("no AI provider is set up under Settings"))?;
        let ai = provider::from_config(llm, &KeyringStore).map_err(UiError::new)?;
        let runtime = current_thread_runtime()?;
        let page = runtime
            .block_on(read_page(&paths.dirs.profile_dir(&company.id), &target))
            .map_err(UiError::new)?;
        let description = Some(description.trim()).filter(|text| !text.is_empty());
        let prompt = generation_prompt(
            &ai,
            company,
            RawPage {
                url: &page.address,
                html: &page.html,
            },
            description,
        )
        .map_err(UiError::new)?;
        let view = PromptView {
            provider: ai.label.clone(),
            text: prompt.text().to_owned(),
        };
        Ok((view, prompt))
    })
    .await?;
    locked(&state).prompt = Some(PreparedPrompt {
        company_id,
        address,
        prompt,
    });
    Ok(view)
}

/// Sends the text a person just saw, and checks the answer.
#[tauri::command]
pub async fn send_recipe_prompt(state: State<'_, AppState>) -> Result<DraftView, UiError> {
    let prepared = locked(&state)
        .prompt
        .take()
        .ok_or_else(|| UiError::new("nothing is waiting to be sent"))?;
    let paths = state.paths.clone();
    let pending = blocking(move || {
        let config = Config::load(&paths.config)?;
        let company = company_of(&config, &prepared.company_id)?;
        let llm = config
            .llm
            .as_ref()
            .ok_or_else(|| UiError::new("no AI provider is set up under Settings"))?;
        let ai = provider::from_config(llm, &KeyringStore).map_err(UiError::new)?;
        let recipes = paths.dirs.recipes_dir();
        let binding = known_hosts(company, &recipes, &prepared.address);
        let runtime = current_thread_runtime()?;
        let draft = runtime
            .block_on(request_draft(&ai, &prepared.prompt, &binding))
            .map_err(UiError::new)?;
        Ok(PendingDraft {
            path: draft_path(&recipes, &company.id),
            text: draft.text,
            host_changes: draft.host_changes,
        })
    })
    .await?;
    let view = pending.view();
    locked(&state).draft = Some(pending);
    Ok(view)
}

/// Writes the waiting draft.
#[tauri::command]
pub fn save_recipe_draft(
    state: State<'_, AppState>,
    hosts_confirmed: bool,
    overwrite: bool,
) -> Result<(), UiError> {
    let mut work = locked(&state);
    let pending = work
        .draft
        .as_ref()
        .ok_or_else(|| UiError::new("no draft is waiting"))?;
    pending.write(hosts_confirmed, overwrite)?;
    work.draft = None;
    Ok(())
}

/// Forgets what is waiting, after a person said no.
#[tauri::command]
pub fn discard_recipe_work(state: State<'_, AppState>) {
    let mut work = locked(&state);
    work.prompt = None;
    work.draft = None;
}

/// Opens a browser window and records until [`finish_recording`] or until the
/// person closes it. Then the draft waits for [`save_recipe_draft`].
#[tauri::command]
pub async fn record_recipe(
    state: State<'_, AppState>,
    company_id: String,
    address: String,
) -> Result<DraftView, UiError> {
    require_web_address(&address)?;
    let Some(_permit) = state.scanning.try_start() else {
        return Err(UiError::new(ALREADY_RUNNING));
    };
    let (stop, stopped) = oneshot::channel();
    locked(&state).stop_recording = Some(stop);
    let paths = state.paths.clone();
    let recorded = blocking(move || {
        let config = Config::load(&paths.config)?;
        let company = company_of(&config, &company_id)?;
        let runtime = current_thread_runtime()?;
        let events = runtime.block_on(async {
            let mut recorder = Recorder::start(
                ChromeOptions {
                    profile_dir: paths.dirs.profile_dir(&company.id),
                    window: WindowMode::Visible,
                },
                &address,
            )
            .await
            .map_err(UiError::new)?;
            recorder
                .wait_for_end(async {
                    let _ = stopped.await;
                })
                .await;
            Ok::<_, UiError>(recorder.finish().await)
        })?;
        let draft = build_draft(&address, &events).map_err(UiError::new)?;
        let text = draft_text("recorded in the browser", &draft.notes, &draft.recipe)
            .map_err(UiError::new)?;
        Ok(PendingDraft {
            path: draft_path(&paths.dirs.recipes_dir(), &company.id),
            text,
            host_changes: Vec::new(),
        })
    })
    .await;
    let mut work = locked(&state);
    work.stop_recording = None;
    let pending = recorded?;
    let view = pending.view();
    work.draft = Some(pending);
    Ok(view)
}

/// The person says the recording is complete.
#[tauri::command]
pub fn finish_recording(state: State<'_, AppState>) {
    if let Some(stop) = locked(&state).stop_recording.take() {
        let _ = stop.send(());
    }
}

/// The result of a preview or a real scan.
#[derive(Debug, Serialize)]
pub struct ScanResult {
    reports: Vec<AccountReport>,
    /// The portal downloads that link mails started.
    fetches: Vec<FetchReport>,
    summary: ScanSummary,
    /// The scan worked, but something around it did not.
    warning: Option<String>,
}

#[tauri::command]
pub async fn run_scan(
    app: AppHandle,
    state: State<'_, AppState>,
    dry_run: bool,
) -> Result<ScanResult, UiError> {
    let Some(_permit) = state.scanning.try_start() else {
        return Err(UiError::new(ALREADY_RUNNING));
    };
    let paths = state.paths.clone();
    let mut handler = GuiConfirmation::new(app, &state.confirmation);
    blocking(move || {
        if dry_run {
            preview(&paths, &KeyringStore)
        } else {
            scan(&paths, &KeyringStore, &mut handler)
        }
    })
    .await
}

/// The result of a preview or a real portal fetch. A list, like a scan's
/// reports, so the history treats them alike.
#[derive(Debug, Serialize)]
pub struct FetchResult {
    reports: Vec<FetchReport>,
    summary: FetchSummary,
    warning: Option<String>,
}

#[tauri::command]
pub async fn run_fetch(
    app: AppHandle,
    state: State<'_, AppState>,
    company_id: String,
    dry_run: bool,
    visible: bool,
) -> Result<FetchResult, UiError> {
    let Some(_permit) = state.scanning.try_start() else {
        return Err(UiError::new(ALREADY_RUNNING));
    };
    let paths = state.paths.clone();
    let mut handler = GuiConfirmation::new(app, &state.confirmation);
    let window = if visible {
        WindowMode::Visible
    } else {
        WindowMode::OnDemand
    };
    blocking(move || {
        fetch(
            &paths,
            &KeyringStore,
            &company_id,
            dry_run,
            window,
            &mut handler,
        )
    })
    .await
}

fn fetch<H: ConfirmationHandler>(
    paths: &Paths,
    secrets: &dyn SecretStore,
    company_id: &str,
    dry_run: bool,
    window: WindowMode,
    handler: &mut H,
) -> Result<FetchResult, UiError> {
    let config = Config::load(&paths.config)?;
    let company = config
        .companies
        .iter()
        .find(|company| company.id == company_id)
        .ok_or_else(|| UiError::new(format!("no company `{company_id}` in the configuration")))?;
    let fetch_paths = FetchPaths {
        index: paths.index.clone(),
        ..FetchPaths::of(&paths.dirs, company_id)
    };
    let options = FetchOptions {
        dry_run,
        window,
        recipe_file: None,
    };
    let runtime = current_thread_runtime()?;
    let report = runtime.block_on(fetch_company(
        &config,
        company,
        secrets,
        &fetch_paths,
        &options,
        handler,
    ));
    let reports = vec![report];
    let summary = summarize_fetches(&reports);
    let warning = if dry_run {
        None
    } else {
        record_run(paths, &reports, &summary, false).err()
    };
    Ok(FetchResult {
        reports,
        summary,
        warning,
    })
}

/// The one question a running fetch can have open.
#[derive(Debug, Default)]
pub struct ConfirmationSlot(Mutex<Option<oneshot::Sender<bool>>>);

impl ConfirmationSlot {
    fn open(&self) -> oneshot::Receiver<bool> {
        let (sender, receiver) = oneshot::channel();
        *self.0.lock().expect("lock is never poisoned") = Some(sender);
        receiver
    }

    fn answer(&self, done: bool) {
        if let Some(sender) = self.0.lock().expect("lock is never poisoned").take() {
            let _ = sender.send(done);
        }
    }

    fn close(&self) {
        self.0.lock().expect("lock is never poisoned").take();
    }
}

/// Asks through a dialog in the window: an event opens it, and the command
/// [`answer_confirmation`] brings the answer back.
pub struct GuiConfirmation {
    slot: Arc<ConfirmationSlot>,
    app: AppHandle,
}

impl GuiConfirmation {
    pub fn new(app: AppHandle, slot: &Arc<ConfirmationSlot>) -> Self {
        Self {
            slot: Arc::clone(slot),
            app,
        }
    }
}

impl ConfirmationHandler for GuiConfirmation {
    async fn ask(&mut self, request: &ConfirmationRequest) -> ConfirmationAnswer {
        let answer = self.slot.open();
        crate::tray::show_window(&self.app);
        if self.app.emit("portal-confirmation", request).is_err() {
            return ConfirmationAnswer::Cancelled;
        }
        match answer.await {
            Ok(true) => ConfirmationAnswer::Done,
            _ => ConfirmationAnswer::Cancelled,
        }
    }

    fn expired(&mut self) {
        self.slot.close();
        let _ = self.app.emit("portal-confirmation-ended", ());
    }

    async fn approve_ai_send(&mut self, request: &AiSendRequest) -> bool {
        let answer = self.slot.open();
        crate::tray::show_window(&self.app);
        if self.app.emit("ai-send-confirmation", request).is_err() {
            return false;
        }
        matches!(answer.await, Ok(true))
    }
}

/// The person's answer to the dialog of a running fetch.
#[tauri::command]
pub fn answer_confirmation(state: State<'_, AppState>, done: bool) {
    state.confirmation.answer(done);
}

fn preview(paths: &Paths, secrets: &dyn SecretStore) -> Result<ScanResult, UiError> {
    let config = Config::load(&paths.config)?;
    let runtime = current_thread_runtime()?;
    let reports = runtime
        .block_on(execute::preview_mail_scan(
            &config,
            secrets,
            &paths.index,
            Local::now().date_naive(),
        ))
        .map_err(UiError::new)?;
    let summary = summarize(&reports);
    Ok(ScanResult {
        reports,
        fetches: Vec::new(),
        summary,
        warning: None,
    })
}

fn scan<H: ConfirmationHandler>(
    paths: &Paths,
    secrets: &dyn SecretStore,
    handler: &mut H,
) -> Result<ScanResult, UiError> {
    let config = Config::load(&paths.config)?;
    let runtime = current_thread_runtime()?;
    let mut fetcher = BrowserFetcher {
        config: &config,
        secrets,
        dirs: &paths.dirs,
        index: &paths.index,
        window: WindowMode::OnDemand,
        handler,
    };
    let report = runtime
        .block_on(execute::run_mail_scan(
            &config,
            secrets,
            &paths.index,
            Local::now().date_naive(),
            &mut fetcher,
        ))
        .map_err(UiError::new)?;
    let summary = report.summary();
    let warning = record_scan(paths, &report, false).err();
    let ScanReport { accounts, fetches } = report;
    Ok(ScanResult {
        reports: accounts,
        fetches,
        summary,
        warning,
    })
}

/// Adds a scan to the history: mailboxes and portal downloads in one list, which
/// the interface tells apart by their fields.
pub fn record_scan(paths: &Paths, report: &ScanReport, automatic: bool) -> Result<(), String> {
    let values = report
        .accounts
        .iter()
        .map(serde_json::to_value)
        .chain(report.fetches.iter().map(serde_json::to_value))
        .collect::<Result<Vec<Value>, _>>()
        .map_err(|e| e.to_string())?;
    record_run(paths, &values, &report.summary(), automatic)
}

fn record_run<R: Serialize, S: Serialize>(
    paths: &Paths,
    reports: &[R],
    summary: &S,
    automatic: bool,
) -> Result<(), String> {
    let summary = serde_json::to_string(summary).map_err(|e| e.to_string())?;
    let report = serde_json::to_string(reports).map_err(|e| e.to_string())?;
    Index::open(&paths.index)
        .and_then(|index| index.record_run(&summary, &report, automatic))
        .map_err(|e| format!("the run could not be added to the history: {e}"))
}

#[tauri::command]
pub async fn list_invoices(state: State<'_, AppState>) -> Result<Vec<InvoiceRecord>, UiError> {
    let path = state.paths.index.clone();
    blocking(move || invoices(&path)).await
}

fn invoices(path: &Path) -> Result<Vec<InvoiceRecord>, UiError> {
    match Index::open_read_only(path)? {
        Some(index) => Ok(index.recent_invoices(MAX_INVOICES)?),
        None => Ok(Vec::new()),
    }
}

/// A run of the history, with its stored JSON unpacked.
#[derive(Debug, Serialize)]
pub struct RunView {
    id: i64,
    finished_at: String,
    automatic: bool,
    summary: Value,
    reports: Value,
}

#[tauri::command]
pub async fn list_runs(state: State<'_, AppState>) -> Result<Vec<RunView>, UiError> {
    let path = state.paths.index.clone();
    blocking(move || runs(&path)).await
}

fn runs(path: &Path) -> Result<Vec<RunView>, UiError> {
    let Some(index) = Index::open_read_only(path)? else {
        return Ok(Vec::new());
    };
    Ok(index
        .recent_runs(MAX_RUNS)?
        .into_iter()
        .map(|run| RunView {
            id: run.id,
            finished_at: run.finished_at,
            automatic: run.automatic,
            summary: serde_json::from_str(&run.summary).unwrap_or(Value::Null),
            reports: serde_json::from_str(&run.report).unwrap_or(Value::Null),
        })
        .collect())
}

#[tauri::command]
pub async fn complete_review(
    state: State<'_, AppState>,
    id: i64,
    invoice_no: String,
    date: NaiveDate,
) -> Result<InvoiceRecord, UiError> {
    let paths = state.paths.clone();
    blocking(move || {
        let config = Config::load(&paths.config)?;
        let index = Index::open(&paths.index)?;
        review::complete_review(&config, &index, id, &invoice_no, date).map_err(UiError::new)
    })
    .await
}

/// Shows an invoice in the file manager. It takes an index id rather than a
/// path, so the interface cannot point this at arbitrary files.
#[tauri::command]
pub async fn reveal_invoice(state: State<'_, AppState>, id: i64) -> Result<(), UiError> {
    let path = state.paths.index.clone();
    blocking(move || {
        let index =
            Index::open_read_only(&path)?.ok_or_else(|| UiError::new("the index is empty"))?;
        let file = index
            .invoice(id)?
            .and_then(|record| record.file_path)
            .ok_or_else(|| UiError::new("this entry has no file"))?;
        tauri_plugin_opener::reveal_item_in_dir(&file).map_err(UiError::new)
    })
    .await
}

/// Asks for a folder with the system dialog.
#[tauri::command]
pub async fn pick_folder(app: AppHandle) -> Result<Option<String>, UiError> {
    blocking(move || {
        Ok(app
            .dialog()
            .file()
            .blocking_pick_folder()
            .and_then(|picked| picked.into_path().ok())
            .map(|path| path.display().to_string()))
    })
    .await
}

/// Whether an automatic run is active right now.
#[tauri::command]
pub fn automation_status(state: State<'_, AppState>) -> AutomationStatus {
    AutomationStatus {
        running: state.automatic_run.load(Ordering::SeqCst),
    }
}

/// When a schedule next comes up, or what is wrong with it.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct ScheduleView {
    next_run: Option<String>,
    error: Option<String>,
}

fn schedule_view<Tz: TimeZone>(expression: &str, now: &DateTime<Tz>) -> ScheduleView {
    match Schedule::parse(expression) {
        Ok(schedule) => ScheduleView {
            next_run: schedule.next_after(now).map(|next| next.to_rfc3339()),
            error: None,
        },
        Err(error) => ScheduleView {
            next_run: None,
            error: Some(error),
        },
    }
}

#[tauri::command]
pub fn describe_schedule(expression: String) -> ScheduleView {
    schedule_view(&expression, &Local::now())
}

/// Whether Docsweep starts at login. The system knows that, not the configuration.
#[tauri::command]
pub fn autostart_enabled(app: AppHandle) -> Result<bool, UiError> {
    app.autolaunch().is_enabled().map_err(UiError::new)
}

#[tauri::command]
pub fn set_autostart(app: AppHandle, enabled: bool) -> Result<(), UiError> {
    let autostart = app.autolaunch();
    if enabled {
        autostart.enable().map_err(UiError::new)
    } else {
        autostart.disable().map_err(UiError::new)
    }
}

/// Runs blocking work off the main thread. SQLite connections and parsers are
/// not `Sync`, so the scan keeps to one thread with its own small runtime.
pub async fn blocking<T, F>(work: F) -> Result<T, UiError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, UiError> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(work)
        .await
        .map_err(UiError::new)?
}

pub fn current_thread_runtime() -> Result<tokio::runtime::Runtime, UiError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(UiError::new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use docsweep_core::secrets::{ExposeSecret, MemoryStore};

    const VALID: &str = r#"
mail:
  accounts:
    - { id: main, imap: "127.0.0.1:1", user: u, password: "keyring:mail-main", watch: INBOX }
  disposal: { mode: all_delete }
"#;

    fn paths(dir: &Path) -> Paths {
        Paths {
            config: dir.join("docsweep.yaml"),
            index: dir.join("data").join("docsweep.db"),
            dirs: AppDirs::from_base(&dir.join("config"), &dir.join("data")),
        }
    }

    #[test]
    fn first_start_has_no_configuration_and_no_error() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = snapshot(&paths(dir.path()), &MemoryStore::default());
        assert!(snapshot.config.is_none());
        assert!(snapshot.config_error.is_none());
        assert_eq!(snapshot.info.name, "Docsweep");
        assert!(!snapshot.info.bundle_id.is_empty());
    }

    #[test]
    fn snapshot_tells_whether_passwords_exist_but_never_their_value() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        std::fs::write(&paths.config, VALID).unwrap();
        let secrets = MemoryStore::with("mail-main", "super-secret-value");

        let snapshot = snapshot(&paths, &secrets);

        assert_eq!(snapshot.stored_passwords, [("main".to_owned(), true)]);
        let json = serde_json::to_string(&snapshot).unwrap();
        assert!(!json.contains("super-secret-value"), "{json}");
        assert!(json.contains("keyring:mail-main"), "{json}");
    }

    #[test]
    fn missing_password_is_reported_as_missing() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        std::fs::write(&paths.config, VALID).unwrap();
        let snapshot = snapshot(&paths, &MemoryStore::default());
        assert_eq!(snapshot.stored_passwords, [("main".to_owned(), false)]);
    }

    #[test]
    fn broken_configuration_is_shown_with_its_issues() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        std::fs::write(&paths.config, VALID.replace("keyring:mail-main", "plain")).unwrap();

        let snapshot = snapshot(&paths, &MemoryStore::default());

        let error = snapshot.config_error.unwrap();
        assert_eq!(error.issues[0].path, "mail.accounts[0].password");
        assert!(snapshot.config.is_none());
    }

    #[test]
    fn secrets_are_stored_and_bad_input_is_refused() {
        let secrets = MemoryStore::default();
        store_secret(&secrets, "mail-main", "value".into()).unwrap();
        assert_eq!(secrets.get("mail-main").unwrap().expose_secret(), "value");
        assert!(store_secret(&secrets, "Bad Id", "value".into()).is_err());
        assert!(store_secret(&secrets, "mail-main", String::new()).is_err());
    }

    #[test]
    fn dry_run_creates_no_index_and_no_history() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        std::fs::write(&paths.config, VALID).unwrap();

        let result = preview(&paths, &MemoryStore::default()).unwrap();

        assert_eq!(result.summary.failed, 1, "no password is stored");
        assert!(!paths.index.exists());
        assert!(runs(&paths.index).unwrap().is_empty());
        assert!(invoices(&paths.index).unwrap().is_empty());
    }

    #[test]
    fn real_run_is_added_to_the_history_even_if_an_account_fails() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        std::fs::write(&paths.config, VALID).unwrap();

        let result = scan(&paths, &MemoryStore::default(), &mut NobodyAsked).unwrap();

        assert!(result.warning.is_none());
        let history = runs(&paths.index).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].summary["failed"], 1);
        assert_eq!(history[0].reports[0]["account_id"], "main");
    }

    #[test]
    fn scan_without_configuration_is_an_understandable_error() {
        let dir = tempfile::tempdir().unwrap();
        let error = preview(&paths(dir.path()), &MemoryStore::default()).unwrap_err();
        assert!(error.message.contains("docsweep.yaml"), "{}", error.message);
    }

    #[test]
    fn a_schedule_is_described_by_its_next_run_or_its_problem() {
        let now = DateTime::parse_from_rfc3339("2026-11-06T12:00:00+01:00").unwrap();

        let valid = schedule_view("0 9 10 * *", &now);
        assert_eq!(valid.next_run.as_deref(), Some("2026-11-10T09:00:00+01:00"));
        assert_eq!(valid.error, None);

        let invalid = schedule_view("every day", &now);
        assert_eq!(invalid.next_run, None);
        assert!(invalid.error.unwrap().contains("five fields"));
    }

    #[test]
    fn an_automatic_scan_is_marked_in_the_history_with_its_portal_downloads() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        std::fs::write(&paths.config, WITH_PORTAL).unwrap();
        let config = Config::load(&paths.config).unwrap();
        let report = ScanReport {
            accounts: vec![AccountReport {
                account_id: "main".into(),
                mails: Vec::new(),
                error: None,
            }],
            fetches: vec![FetchReport::failed(
                &config.companies[0],
                "nobody answered in time".into(),
            )],
        };

        record_scan(&paths, &report, true).unwrap();
        record_scan(&paths, &ScanReport::default(), false).unwrap();

        let history = runs(&paths.index).unwrap();
        assert!(!history[0].automatic);
        assert!(history[1].automatic);
        assert_eq!(history[1].summary["failed"], 1);
        assert_eq!(history[1].reports[0]["account_id"], "main");
        assert_eq!(history[1].reports[1]["company_id"], "acme");
    }

    #[test]
    fn saved_configuration_issues_reach_the_interface() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let mut config = Config::from_yaml(VALID).unwrap();
        config.mail.accounts[0].user = String::new();

        let error = UiError::from(config.save(&paths.config).unwrap_err());

        assert_eq!(error.issues[0].path, "mail.accounts[0].user");
        assert!(!paths.config.exists());
    }

    const WITH_PORTAL: &str = r#"
mail:
  accounts:
    - { id: main, imap: "127.0.0.1:1", user: u, password: "keyring:mail-main", watch: INBOX }
  disposal: { mode: all_delete }
companies:
  - id: acme
    name: Acme
    portal: { credentials: "keyring:acme", totp: "keyring:acme-totp", recipe: acme.yaml }
    output: { dir: invoices, filename: "{invoice_no}.pdf" }
"#;

    struct NobodyAsked;

    impl ConfirmationHandler for NobodyAsked {
        async fn ask(&mut self, _request: &ConfirmationRequest) -> ConfirmationAnswer {
            panic!("no browser runs in these tests");
        }
    }

    #[test]
    fn portal_secrets_are_stored_and_bad_input_is_refused_without_quoting_it() {
        let secrets = MemoryStore::default();
        store_portal_credentials(&secrets, "acme", "me".into(), "secret-value".into()).unwrap();
        let stored = secrets.get("acme").unwrap();
        assert!(stored.expose_secret().contains("secret-value"));
        assert!(store_portal_credentials(&secrets, "Bad Id", "me".into(), "x".into()).is_err());
        let empty = store_portal_credentials(&secrets, "acme", "".into(), "secret-value".into());
        assert!(!empty.unwrap_err().message.contains("secret-value"));

        store_totp_key(&secrets, "acme-totp", "gezd gnbv".into()).unwrap();
        let bad = store_totp_key(&secrets, "other", "!secret-value!".into()).unwrap_err();
        assert!(!bad.message.contains("secret-value"));
        assert!(secrets.get("other").is_err());
    }

    #[test]
    fn snapshot_tells_which_portal_secrets_exist_but_never_their_value() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        std::fs::write(&paths.config, WITH_PORTAL).unwrap();
        let secrets = MemoryStore::with("acme", "super-secret-value");

        let snapshot = snapshot(&paths, &secrets);

        assert_eq!(
            snapshot.stored_portal_secrets,
            [(
                "acme".to_owned(),
                PortalSecrets {
                    credentials: true,
                    totp: false
                }
            )]
        );
        let json = serde_json::to_string(&snapshot).unwrap();
        assert!(!json.contains("super-secret-value"), "{json}");
    }

    #[test]
    fn an_assisted_company_has_no_portal_secrets_and_cannot_be_fetched() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        std::fs::write(
            &paths.config,
            WITH_PORTAL.replace(
                r#"portal: { credentials: "keyring:acme", totp: "keyring:acme-totp", recipe: acme.yaml }"#,
                r#"portal: { mode: assisted, url: "https://portal.example/", recipe: acme.yaml }"#,
            ),
        )
        .unwrap();

        let secrets = MemoryStore::with("acme", "left-over");
        assert_eq!(
            snapshot(&paths, &secrets).stored_portal_secrets,
            [(
                "acme".to_owned(),
                PortalSecrets {
                    credentials: false,
                    totp: false
                }
            )]
        );

        let result = fetch(
            &paths,
            &MemoryStore::default(),
            "acme",
            false,
            WindowMode::Hidden,
            &mut NobodyAsked,
        )
        .unwrap();
        let error = result.reports[0].error.as_deref().unwrap();
        assert!(error.contains("assisted"), "{error}");
    }

    #[test]
    fn a_real_fetch_reaches_the_history_and_a_preview_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        std::fs::write(&paths.config, WITH_PORTAL).unwrap();
        let secrets = MemoryStore::default();
        let run = |dry_run| {
            fetch(
                &paths,
                &secrets,
                "acme",
                dry_run,
                WindowMode::Hidden,
                &mut NobodyAsked,
            )
            .unwrap()
        };

        // The recipe file does not exist, so no browser starts.
        let preview = run(true);
        assert!(preview.reports[0].error.is_some());
        assert_eq!(preview.summary.failed, 1);
        assert!(!paths.index.exists());

        let real = run(false);
        assert!(real.warning.is_none());
        let history = runs(&paths.index).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].summary["failed"], 1);
        assert_eq!(history[0].reports[0]["company_id"], "acme");
    }

    #[test]
    fn fetching_an_unknown_company_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        std::fs::write(&paths.config, WITH_PORTAL).unwrap();
        let error = fetch(
            &paths,
            &MemoryStore::default(),
            "nope",
            true,
            WindowMode::Hidden,
            &mut NobodyAsked,
        )
        .unwrap_err();
        assert!(error.message.contains("nope"), "{}", error.message);
    }

    #[test]
    fn snapshot_tells_whether_the_api_key_exists_but_never_its_value() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let with_llm =
            format!("{VALID}\nllm: {{ provider: anthropic, api_key: \"keyring:llm-key\" }}\n");
        std::fs::write(&paths.config, with_llm).unwrap();

        assert!(!snapshot(&paths, &MemoryStore::default()).llm_key_stored);
        let secrets = MemoryStore::with("llm-key", "sk-super-secret-value");
        let snapshot = snapshot(&paths, &secrets);
        assert!(snapshot.llm_key_stored);
        let json = serde_json::to_string(&snapshot).unwrap();
        assert!(!json.contains("sk-super-secret-value"), "{json}");
    }

    fn pending(dir: &Path, host_changes: Vec<HostChange>) -> PendingDraft {
        PendingDraft {
            path: dir.join("acme.draft.yaml"),
            text: "steps: []".to_owned(),
            host_changes,
        }
    }

    #[test]
    fn a_new_host_is_never_written_without_confirmation() {
        let dir = tempfile::tempdir().unwrap();
        let change = HostChange {
            step: "login".to_owned(),
            old: "portal.test".to_owned(),
            new: "evil.example".to_owned(),
        };
        let draft = pending(dir.path(), vec![change]);

        let refused = draft.write(false, true).unwrap_err();
        assert!(
            refused.message.contains("confirmation"),
            "{}",
            refused.message
        );
        assert!(!draft.path.exists());

        draft.write(true, false).unwrap();
        assert!(draft.path.exists());
    }

    #[test]
    fn an_existing_draft_is_only_replaced_on_request() {
        let dir = tempfile::tempdir().unwrap();
        let draft = pending(dir.path(), Vec::new());
        assert!(!draft.view().exists);
        assert_eq!(draft.view().file_name, "acme.draft.yaml");

        std::fs::write(&draft.path, "mine").unwrap();
        assert!(draft.view().exists);
        assert!(draft.write(false, false).is_err());
        assert_eq!(std::fs::read_to_string(&draft.path).unwrap(), "mine");

        draft.write(false, true).unwrap();
        assert_eq!(std::fs::read_to_string(&draft.path).unwrap(), "steps: []");
    }

    #[test]
    fn a_confirmation_answer_reaches_the_waiting_fetch_once() {
        let slot = ConfirmationSlot::default();
        slot.answer(true); // Nobody waits: nothing happens.

        let mut waiting = slot.open();
        assert!(waiting.try_recv().is_err());
        slot.answer(true);
        assert_eq!(waiting.try_recv(), Ok(true));

        let mut closed = slot.open();
        slot.close();
        assert!(closed.try_recv().is_err());
        slot.answer(false);
    }
}
