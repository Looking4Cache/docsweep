//! Runs a recipe: log in, extract the list, compare it with the index, and store
//! what is new. A dry run stops after the comparison and downloads nothing.
//!
//! Storing follows the mail pipeline: the same names, the same duplicate check,
//! the same `needs_review` rules. A failure with one document does not stop the
//! others. Nothing here retries a login, so wrong credentials cannot lock an
//! account, and nothing tries to get around a captcha: a person is asked.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::NaiveDate;
use serde::Serialize;
use tokio::time::{Instant, sleep, timeout_at};
use url::Url;

use super::credentials::{CredentialError, PortalCredentials, TotpGenerator};
use super::driver::{
    BrowserDriver, ConfirmationAnswer, ConfirmationHandler, ConfirmationReason,
    ConfirmationRequest, DriverError, RowValues,
};
use super::recipe::{
    CookieBanner, DownloadNew, ExtractList, FieldSelector, LoginStep, OpenDetail, Recipe, Step,
};
use crate::config::{Company, Config};
use crate::document::{self, looks_like_pdf};
use crate::index::{DuplicateCheck, Index, IndexError, NewInvoice, Source, Status};
use crate::mail::plan::{Disposition, PlanError, ReviewReason, disposition_of, target_of};
use crate::naming::sanitize_invoice_no;
use crate::secrets::{SecretError, SecretStore};
use crate::storage::{self, StorageError, sha256_hex};

/// How often the page is checked while waiting.
const POLL: Duration = Duration::from_millis(200);
/// How long a cookie banner gets to appear.
const COOKIE_BANNER_WAIT: Duration = Duration::from_secs(2);
/// How long a detail page gets to show its documents before they count as missing.
const DETAIL_WAIT: Duration = Duration::from_secs(10);
/// How long the page gets after a person said they are done.
const SETTLE: Duration = Duration::from_secs(1);
/// Longer texts on a page are not invoice data.
const MAX_FIELD_CHARS: usize = 500;

/// The buttons of the most common consent tools.
const COMMON_COOKIE_BUTTONS: &str = "#onetrust-accept-btn-handler, \
    #CybotCookiebotDialogBodyLevelButtonLevelOptinAllowAll, \
    #sp-cc-accept, \
    [data-testid=\"uc-accept-all-button\"], \
    .cc-allow, \
    #accept-cookies";

/// Why a run could not start or had to end.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("the company `{0}` has no portal")]
    NoPortal(String),
    #[error(
        "no portal credentials `{id}` in the keychain; store them with `docsweep secret set-portal {id}`"
    )]
    CredentialsNotFound { id: String },
    #[error("the portal credentials `{id}` cannot be used: {source}")]
    Credentials { id: String, source: CredentialError },
    #[error(
        "the recipe asks for a TOTP code, but the company has no `portal.totp`; add it and store the key with `docsweep secret set-totp <id>`"
    )]
    TotpNotConfigured,
    #[error(
        "no TOTP setup key `{id}` in the keychain; store it with `docsweep secret set-totp {id}`"
    )]
    TotpNotFound { id: String },
    #[error("the TOTP setup key `{id}` cannot be used: {source}")]
    Totp { id: String, source: CredentialError },
    #[error("the company has no valid `keyring:<id>` reference for its portal")]
    InvalidReference,
    #[error(transparent)]
    Secret(#[from] SecretError),
    #[error("{0}")]
    Driver(#[from] DriverError),
    #[error("{step}: the page did not get there in time")]
    StepFailed { step: String },
    #[error("refusing to send credentials over an unencrypted connection")]
    InsecureLogin,
    #[error("nobody answered the confirmation in time; the run was ended")]
    ConfirmationTimedOut,
    #[error("the confirmation was cancelled; the run was ended")]
    ConfirmationCancelled,
}

/// Credentials and TOTP generator for the login step.
#[derive(Debug)]
pub struct LoginSecrets {
    credentials: PortalCredentials,
    totp: Option<TotpGenerator>,
}

/// Everything a run depends on.
pub struct FetchJob<'a> {
    pub config: &'a Config,
    pub company: &'a Company,
    /// With the company's parameters filled in.
    pub recipe: &'a Recipe,
    login: Option<LoginSecrets>,
    /// `None` when no index exists yet, which only happens in a first dry run.
    pub index: Option<&'a Index>,
    /// Date for files whose document and list entry both have no date.
    pub today: NaiveDate,
    pub dry_run: bool,
}

impl<'a> FetchJob<'a> {
    /// Reads the login secrets, so that their problems show up before a browser
    /// starts.
    pub fn new(
        config: &'a Config,
        company: &'a Company,
        recipe: &'a Recipe,
        secrets: &dyn SecretStore,
        index: Option<&'a Index>,
        today: NaiveDate,
        dry_run: bool,
    ) -> Result<Self, FetchError> {
        let login = recipe
            .login()
            .map(|step| load_login_secrets(company, step, secrets))
            .transpose()?;
        Ok(Self {
            config,
            company,
            recipe,
            login,
            index,
            today,
            dry_run,
        })
    }
}

fn load_login_secrets(
    company: &Company,
    step: &LoginStep,
    secrets: &dyn SecretStore,
) -> Result<LoginSecrets, FetchError> {
    let portal = company
        .portal
        .as_ref()
        .ok_or_else(|| FetchError::NoPortal(company.id.clone()))?;

    let id = portal
        .credentials
        .id()
        .ok_or(FetchError::InvalidReference)?;
    let stored = secrets.get(id).map_err(|error| match error {
        SecretError::NotFound { id } => FetchError::CredentialsNotFound { id },
        other => other.into(),
    })?;
    let credentials =
        PortalCredentials::parse(&stored).map_err(|source| FetchError::Credentials {
            id: id.to_owned(),
            source,
        })?;

    let totp = match &step.totp {
        None => None,
        Some(_) => {
            let reference = portal.totp.as_ref().ok_or(FetchError::TotpNotConfigured)?;
            let id = reference.id().ok_or(FetchError::InvalidReference)?;
            let key = secrets.get(id).map_err(|error| match error {
                SecretError::NotFound { id } => FetchError::TotpNotFound { id },
                other => other.into(),
            })?;
            let generator = TotpGenerator::new(&key).map_err(|source| FetchError::Totp {
                id: id.to_owned(),
                source,
            })?;
            Some(generator)
        }
    };
    Ok(LoginSecrets { credentials, totp })
}

/// What came of one list entry. Links are left out of reports on purpose: they
/// can carry tokens, and reports end up in the history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EntryOutcome {
    /// Dry run: this would be downloaded. The final name depends on the file.
    WouldDownload {
        file_name: Option<String>,
    },
    AlreadyKnown,
    Stored {
        path: PathBuf,
    },
    StoredForReview {
        path: PathBuf,
        reason: ReviewReason,
    },
    /// The identical file is known, but not safe to rely on.
    Unresolved {
        reason: String,
    },
    /// The portal lists it, but offers no document yet.
    Missing,
    Failed {
        message: String,
    },
}

/// One list entry in a report. All texts come from the portal and are untrusted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EntryReport {
    pub key: String,
    pub date: Option<String>,
    pub invoice_no: Option<String>,
    pub amount: Option<String>,
    pub outcome: EntryOutcome,
}

/// The result of a run for one company.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FetchReport {
    pub company_id: String,
    pub company_name: String,
    pub entries: Vec<EntryReport>,
    /// Set if the run ended early. Entries before that stay valid.
    pub error: Option<String>,
}

impl FetchReport {
    /// A report for a run that could not even start.
    pub fn failed(company: &Company, error: String) -> Self {
        Self {
            company_id: company.id.clone(),
            company_name: company.name.clone(),
            entries: Vec::new(),
            error: Some(error),
        }
    }

    /// Whether anything went wrong.
    pub fn has_failures(&self) -> bool {
        self.error.is_some()
            || self
                .entries
                .iter()
                .any(|entry| matches!(entry.outcome, EntryOutcome::Failed { .. }))
    }
}

/// Counts of a run, for headlines and the history.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct FetchSummary {
    /// Documents saved as complete invoices (or planned to be).
    pub saved: u32,
    pub for_review: u32,
    pub already_known: u32,
    pub missing: u32,
    pub failed: u32,
}

/// Summarises reports.
pub fn summarize_fetches(reports: &[FetchReport]) -> FetchSummary {
    let mut summary = FetchSummary::default();
    for report in reports {
        summary.failed += u32::from(report.error.is_some());
        for entry in &report.entries {
            match entry.outcome {
                EntryOutcome::WouldDownload { .. } | EntryOutcome::Stored { .. } => {
                    summary.saved += 1;
                }
                EntryOutcome::StoredForReview { .. } => summary.for_review += 1,
                EntryOutcome::AlreadyKnown | EntryOutcome::Unresolved { .. } => {
                    summary.already_known += 1;
                }
                EntryOutcome::Missing => summary.missing += 1,
                EntryOutcome::Failed { .. } => summary.failed += 1,
            }
        }
    }
    summary
}

/// Runs the recipe. Whatever happens, entries that were handled before an error
/// are in the report, and everything in the index has its file.
pub async fn run_recipe<D: BrowserDriver, H: ConfirmationHandler>(
    job: &FetchJob<'_>,
    driver: &mut D,
    handler: &mut H,
) -> FetchReport {
    let mut run = Run {
        job,
        driver,
        handler,
        pending: Vec::new(),
        seen_keys: HashSet::new(),
        entries: Vec::new(),
    };
    let error = run.all_steps().await.err().map(|error| error.to_string());
    FetchReport {
        company_id: job.company.id.clone(),
        company_name: job.company.name.clone(),
        entries: run.entries,
        error,
    }
}

/// A list entry on its way to the download step.
struct Row {
    values: RowValues,
    page_url: String,
}

/// What a wait step waits for.
enum Wait<'s> {
    /// Any of these is on the page.
    AnyVisible(Vec<&'s str>),
    /// The first is on the page; without a first, the second has left it.
    VisibleOrGone(Option<&'s str>, &'s str),
}

struct Run<'a, 'j, D, H> {
    job: &'a FetchJob<'j>,
    driver: &'a mut D,
    handler: &'a mut H,
    pending: Vec<Row>,
    seen_keys: HashSet<String>,
    entries: Vec<EntryReport>,
}

impl<D: BrowserDriver, H: ConfirmationHandler> Run<'_, '_, D, H> {
    fn step_timeout(&self) -> Duration {
        Duration::from_secs(self.job.recipe.step_timeout_secs)
    }

    async fn all_steps(&mut self) -> Result<(), FetchError> {
        let job = self.job;
        for step in &job.recipe.steps {
            match step {
                Step::Goto(url) => self.goto(url).await?,
                Step::DismissCookieBanner(banner) => self.dismiss_cookie_banner(banner).await,
                Step::Login(login) => self.login(login).await?,
                Step::ExtractList(list) => self.extract_list(list).await?,
                Step::DownloadNew(download) => self.download_new(download).await,
            }
            self.check_for_confirmation().await?;
        }
        Ok(())
    }

    async fn goto(&mut self, url: &str) -> Result<(), FetchError> {
        match self.driver.goto(url).await {
            Err(DriverError::Timeout) => {
                self.ask_person(ConfirmationReason::StepTimedOut {
                    step: "opening the page".to_owned(),
                })
                .await?;
                Ok(self.driver.goto(url).await?)
            }
            other => Ok(other?),
        }
    }

    /// Never fails a run: a banner that is not there needs no dismissing.
    async fn dismiss_cookie_banner(&mut self, banner: &CookieBanner) {
        let selector = match banner {
            CookieBanner::Enabled(false) => return,
            CookieBanner::Enabled(true) => COMMON_COOKIE_BUTTONS,
            CookieBanner::Selector(selector) => selector,
        };
        let deadline = Instant::now() + COOKIE_BANNER_WAIT;
        while Instant::now() < deadline {
            if matches!(self.driver.is_visible(selector).await, Ok(true)) {
                let _ = self.driver.click(selector).await;
                return;
            }
            sleep(POLL).await;
        }
    }

    /// Submits at most once per run. After a time-out a person may finish the
    /// login, but the run never sends the credentials a second time.
    async fn login(&mut self, step: &LoginStep) -> Result<(), FetchError> {
        let job = self.job;
        let secrets = job
            .login
            .as_ref()
            .expect("a job loads them for a login step");
        let logged_in = step.logged_in.as_deref();

        let mut first = vec![step.user.as_str()];
        first.extend(logged_in);
        self.wait("waiting for the login form", Wait::AnyVisible(first))
            .await?;
        if self.shows(logged_in).await? {
            return Ok(());
        }

        self.require_secure_page().await?;
        self.driver
            .fill(&step.user, &secrets.credentials.user)
            .await?;
        if let Some(next) = &step.next {
            self.driver.click(next).await?;
            self.wait(
                "waiting for the password field",
                Wait::AnyVisible(vec![&step.password]),
            )
            .await?;
            self.require_secure_page().await?;
        }
        self.driver
            .fill(&step.password, &secrets.credentials.password)
            .await?;
        self.driver.click(&step.submit).await?;

        let Some(totp_field) = &step.totp else {
            return self
                .wait(
                    "waiting for the login to finish",
                    Wait::VisibleOrGone(logged_in, &step.password),
                )
                .await;
        };
        // A trusted device may not be asked for a code at all.
        let mut second = vec![totp_field.as_str()];
        second.extend(logged_in);
        self.wait("waiting for the TOTP field", Wait::AnyVisible(second))
            .await?;
        if self.shows(logged_in).await? {
            return Ok(());
        }
        self.require_secure_page().await?;
        let generator = secrets
            .totp
            .as_ref()
            .expect("a job loads it for a TOTP field");
        self.driver
            .fill(totp_field, &generator.current_code())
            .await?;
        let submit = step.totp_submit.as_deref().unwrap_or(&step.submit);
        self.driver.click(submit).await?;
        self.wait(
            "waiting for the login to finish",
            Wait::VisibleOrGone(logged_in, totp_field),
        )
        .await
    }

    async fn shows(&mut self, selector: Option<&str>) -> Result<bool, DriverError> {
        match selector {
            Some(selector) => self.driver.is_visible(selector).await,
            None => Ok(false),
        }
    }

    /// Credentials only ever go to encrypted pages or to this computer.
    async fn require_secure_page(&mut self) -> Result<(), FetchError> {
        let address = self.driver.current_url().await?;
        let parsed = Url::parse(&address).map_err(|_| FetchError::InsecureLogin)?;
        let local = matches!(parsed.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
        if parsed.scheme() == "https" || local {
            Ok(())
        } else {
            Err(FetchError::InsecureLogin)
        }
    }

    /// Waits for the page. A confirmation that shows up is handled on the way,
    /// and a person gets one chance when time runs out.
    async fn wait(&mut self, step: &str, wait: Wait<'_>) -> Result<(), FetchError> {
        let mut deadline = Instant::now() + self.step_timeout();
        let mut person_asked = false;
        loop {
            if self.holds(&wait).await? {
                return Ok(());
            }
            if self.confirmation_shown().await? {
                self.ask_person(ConfirmationReason::Detected).await?;
                deadline = Instant::now() + self.step_timeout();
                continue;
            }
            if Instant::now() >= deadline {
                if person_asked {
                    return Err(FetchError::StepFailed {
                        step: step.to_owned(),
                    });
                }
                person_asked = true;
                self.ask_person(ConfirmationReason::StepTimedOut {
                    step: step.to_owned(),
                })
                .await?;
                deadline = Instant::now() + SETTLE;
                continue;
            }
            sleep(POLL).await;
        }
    }

    async fn holds(&mut self, wait: &Wait<'_>) -> Result<bool, DriverError> {
        match wait {
            Wait::AnyVisible(selectors) => {
                for selector in selectors {
                    if self.driver.is_visible(selector).await? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            Wait::VisibleOrGone(visible, gone) => {
                if self.shows(*visible).await? {
                    return Ok(true);
                }
                Ok(visible.is_none() && !self.driver.is_visible(gone).await?)
            }
        }
    }

    async fn confirmation_shown(&mut self) -> Result<bool, DriverError> {
        let detect = self.job.recipe.confirmation.detect.as_deref();
        self.shows(detect).await
    }

    async fn check_for_confirmation(&mut self) -> Result<(), FetchError> {
        if self.confirmation_shown().await? {
            self.ask_person(ConfirmationReason::Detected).await?;
        }
        Ok(())
    }

    /// Shows the browser and waits for a person, within the recipe's time limit.
    /// A detected confirmation must really be gone before the run goes on.
    async fn ask_person(&mut self, reason: ConfirmationReason) -> Result<(), FetchError> {
        self.driver.show_window().await?;
        let limit = self.job.recipe.confirmation.timeout_secs;
        let deadline = Instant::now() + Duration::from_secs(limit);
        loop {
            let request = ConfirmationRequest {
                company_name: self.job.company.name.clone(),
                reason: reason.clone(),
                timeout_secs: deadline.saturating_duration_since(Instant::now()).as_secs(),
            };
            match timeout_at(deadline, self.handler.ask(&request)).await {
                Err(_) => {
                    self.handler.expired();
                    return Err(FetchError::ConfirmationTimedOut);
                }
                Ok(ConfirmationAnswer::Cancelled) => {
                    return Err(FetchError::ConfirmationCancelled);
                }
                Ok(ConfirmationAnswer::Done) => {}
            }
            sleep(SETTLE).await;
            if !self.confirmation_shown().await? {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(FetchError::ConfirmationTimedOut);
            }
        }
    }

    async fn extract_list(&mut self, list: &ExtractList) -> Result<(), FetchError> {
        let mut first = vec![list.rows.as_str()];
        first.extend(list.empty.as_deref());
        self.wait("waiting for the list", Wait::AnyVisible(first))
            .await?;

        let fields = FieldSelector::parse_all(&list.fields);
        let rows = self.pages_of_list(list, &fields).await?;
        match &list.open_detail {
            None => self.pending.extend(rows),
            Some(detail) => {
                for row in rows {
                    self.open_detail(row, detail).await?;
                }
            }
        }
        Ok(())
    }

    /// Reads all pages. If the next page does not come, what was found so far
    /// still gets processed, and the report says that the list is incomplete.
    async fn pages_of_list(
        &mut self,
        list: &ExtractList,
        fields: &BTreeMap<String, FieldSelector>,
    ) -> Result<Vec<Row>, FetchError> {
        let mut rows = Vec::new();
        for page in 1..=list.max_pages {
            let values = self.read_rows(&list.rows, fields).await?;
            let page_url = self.driver.current_url().await?;
            rows.extend(values.iter().map(|values| Row {
                values: values.clone(),
                page_url: page_url.clone(),
            }));

            let Some(next) = &list.next_page else {
                break;
            };
            if !self.driver.is_visible(next).await? {
                break;
            }
            if page == list.max_pages {
                self.record_list_problem(format!(
                    "the list has more pages than the {} this recipe allows",
                    list.max_pages
                ));
                break;
            }
            self.driver.click(next).await?;
            if !self.next_page_arrived(&values, &list.rows, fields).await {
                self.record_list_problem(format!("page {} of the list did not load", page + 1));
                break;
            }
            self.check_for_confirmation().await?;
        }
        Ok(rows)
    }

    async fn next_page_arrived(
        &mut self,
        previous: &[RowValues],
        rows: &str,
        fields: &BTreeMap<String, FieldSelector>,
    ) -> bool {
        let deadline = Instant::now() + self.step_timeout();
        while Instant::now() < deadline {
            sleep(POLL).await;
            // Reading can fail while the page changes.
            if let Ok(values) = self.read_rows(rows, fields).await
                && !values.is_empty()
                && values != previous
            {
                return true;
            }
        }
        false
    }

    fn record_list_problem(&mut self, message: String) {
        self.entries.push(EntryReport {
            key: self.job.company.name.clone(),
            date: None,
            invoice_no: None,
            amount: None,
            outcome: EntryOutcome::Failed { message },
        });
    }

    /// Page content is untrusted: whitespace and control characters go, long
    /// texts are cut off, and fields without a value do not exist.
    async fn read_rows(
        &mut self,
        rows: &str,
        fields: &BTreeMap<String, FieldSelector>,
    ) -> Result<Vec<RowValues>, DriverError> {
        let mut read = self.driver.extract_rows(rows, fields).await?;
        for values in &mut read {
            let cleaned = std::mem::take(values)
                .into_iter()
                .map(|(name, value)| (name, clean_text(&value)))
                .filter(|(_, value)| !value.is_empty())
                .collect();
            *values = cleaned;
        }
        Ok(read)
    }

    /// Opens the page of a list row and queues its documents. Only a person
    /// failing to answer ends the run; other problems stay with this row.
    async fn open_detail(&mut self, row: Row, detail: &OpenDetail) -> Result<(), FetchError> {
        let label = label_of(&row.values, &detail.url);
        let Some(url) = row
            .values
            .get(&detail.url)
            .and_then(|link| resolve_link(&row.page_url, link))
        else {
            self.record_detail(&row.values, label, EntryOutcome::Missing);
            return Ok(());
        };
        if let Err(error) = self.driver.goto(&url).await {
            let outcome = EntryOutcome::Failed {
                message: error.to_string(),
            };
            self.record_detail(&row.values, label, outcome);
            return Ok(());
        }
        self.check_for_confirmation().await?;

        let fields = FieldSelector::parse_all(&detail.fields);
        match self.documents_on_page(detail, &fields).await {
            Ok(documents) if documents.is_empty() => {
                self.record_detail(&row.values, label, EntryOutcome::Missing);
            }
            Ok(documents) => {
                let page_url = self.driver.current_url().await?;
                for document in documents {
                    let mut values = row.values.clone();
                    values.extend(document);
                    self.pending.push(Row {
                        values,
                        page_url: page_url.clone(),
                    });
                }
            }
            Err(error) => {
                let outcome = EntryOutcome::Failed {
                    message: error.to_string(),
                };
                self.record_detail(&row.values, label, outcome);
            }
        }
        Ok(())
    }

    async fn documents_on_page(
        &mut self,
        detail: &OpenDetail,
        fields: &BTreeMap<String, FieldSelector>,
    ) -> Result<Vec<RowValues>, DriverError> {
        let deadline = Instant::now() + DETAIL_WAIT;
        while Instant::now() < deadline {
            if self.driver.is_visible(&detail.rows).await? {
                return self.read_rows(&detail.rows, fields).await;
            }
            if self.shows(detail.missing.as_deref()).await? {
                return Ok(Vec::new());
            }
            sleep(POLL).await;
        }
        Ok(Vec::new())
    }

    fn record_detail(&mut self, values: &RowValues, key: String, outcome: EntryOutcome) {
        self.entries.push(entry_report(key, values, outcome));
    }

    async fn download_new(&mut self, download: &DownloadNew) {
        for row in std::mem::take(&mut self.pending) {
            let outcome = self.handle_row(&row, download).await;
            let key = row
                .values
                .get(&download.key)
                .cloned()
                .unwrap_or_else(|| label_of(&row.values, "url"));
            self.entries.push(entry_report(key, &row.values, outcome));
        }
    }

    async fn handle_row(&mut self, row: &Row, download: &DownloadNew) -> EntryOutcome {
        let link = row.values.get("url");
        let Some(key) = row.values.get(&download.key) else {
            return match link {
                None => EntryOutcome::Missing,
                Some(_) => EntryOutcome::Failed {
                    message: format!("this entry has no value for `{}`", download.key),
                },
            };
        };
        match self.is_known(key) {
            Ok(true) => return EntryOutcome::AlreadyKnown,
            Ok(false) => {}
            Err(error) => {
                return EntryOutcome::Failed {
                    message: error.to_string(),
                };
            }
        }
        let Some(link) = link else {
            return EntryOutcome::Missing;
        };
        let Some(url) = resolve_link(&row.page_url, link) else {
            return EntryOutcome::Failed {
                message: "the document link is not an `http` or `https` address".to_owned(),
            };
        };
        if self.job.dry_run {
            return EntryOutcome::WouldDownload {
                file_name: self.predicted_file_name(row),
            };
        }
        match self.fetch_document(&url, download).await {
            Ok(bytes) => self.store(&bytes, row, key),
            Err(message) => EntryOutcome::Failed { message },
        }
    }

    /// Whether this run or an earlier one has dealt with the key.
    fn is_known(&mut self, key: &str) -> Result<bool, IndexError> {
        if !self.seen_keys.insert(key.to_owned()) {
            return Ok(true);
        }
        match self.job.index {
            Some(index) => index.is_portal_entry_known(&self.job.company.id, key),
            None => Ok(false),
        }
    }

    /// Only possible if the list shows date and number; the file may still say
    /// otherwise.
    fn predicted_file_name(&self, row: &Row) -> Option<String> {
        let date = row
            .values
            .get("date")
            .and_then(|text| document::parse_date(text))?;
        let number = list_number(row)?;
        let target = target_of(self.job.config, self.job.company, date, Some(number), "").ok()?;
        Some(target.file_name)
    }

    async fn fetch_document(
        &mut self,
        url: &str,
        download: &DownloadNew,
    ) -> Result<Vec<u8>, String> {
        let bytes = self.driver.download(url).await.map_err(|e| e.to_string())?;
        if looks_like_pdf(&bytes) {
            return Ok(bytes);
        }
        if !download.html_to_pdf {
            return Err("the portal returned something other than a PDF".to_owned());
        }
        self.driver.goto(url).await.map_err(|e| e.to_string())?;
        // An expired session would otherwise be printed and filed as the invoice.
        if let Some(login) = self.job.recipe.login()
            && self.driver.is_visible(&login.user).await.unwrap_or(false)
        {
            return Err("the portal asked for a login instead of the document".to_owned());
        }
        let printed = self
            .driver
            .print_to_pdf()
            .await
            .map_err(|e| e.to_string())?;
        if looks_like_pdf(&printed) {
            Ok(printed)
        } else {
            Err("printing the document did not produce a PDF".to_owned())
        }
    }

    /// The same decisions as for a mail attachment. The number and date in the
    /// file win, so an invoice that also arrives by mail is recognised; those of
    /// the list fill the gaps.
    fn store(&self, bytes: &[u8], row: &Row, key: &str) -> EntryOutcome {
        match self.try_store(bytes, row, key) {
            Ok(outcome) => outcome,
            Err(error) => EntryOutcome::Failed {
                message: error.to_string(),
            },
        }
    }

    fn try_store(&self, bytes: &[u8], row: &Row, key: &str) -> Result<EntryOutcome, StoreError> {
        let job = self.job;
        let company = job.company;
        let index = job.index.expect("a real run always has an index");

        let hash = sha256_hex(bytes);
        let facts = document::extract_facts(bytes);
        let number = facts
            .invoice_no
            .clone()
            .or_else(|| list_number(row).map(str::to_owned));
        let date = facts
            .date
            .or_else(|| {
                row.values
                    .get("date")
                    .and_then(|text| document::parse_date(text))
            })
            .unwrap_or(job.today);

        let check = index.check_duplicate(&company.id, number.as_deref(), &hash)?;
        if let DuplicateCheck::SameFile(known) = &check {
            index.note_portal_key(known.id, key)?;
        }
        let (status, review) = match disposition_of(check, number.is_some()) {
            Disposition::Store => (Status::Stored, None),
            Disposition::StoreForReview(reason) => (Status::NeedsReview, Some(reason)),
            Disposition::AlreadyStored { .. } => return Ok(EntryOutcome::AlreadyKnown),
            Disposition::Unresolved { reason } => return Ok(EntryOutcome::Unresolved { reason }),
        };

        let target = target_of(job.config, company, date, number.as_deref(), &hash)?;
        let path = storage::store_verified(&target.dir, &target.file_name, bytes)?;
        let inserted = index.insert(&NewInvoice {
            company_id: company.id.clone(),
            invoice_date: Some(date),
            invoice_no: number,
            amount: facts.amount,
            currency: facts.currency,
            file_path: path.clone(),
            file_hash: hash,
            source: Source::Portal,
            mail_account: None,
            mail_id: None,
            portal_key: Some(key.to_owned()),
            status,
        });
        if let Err(error) = inserted {
            remove_unindexed(&path);
            return Err(error.into());
        }
        Ok(match review {
            None => EntryOutcome::Stored { path },
            Some(reason) => EntryOutcome::StoredForReview { path, reason },
        })
    }
}

/// Without an index entry a retry could not recognise this file, and would store
/// a second copy of it.
fn remove_unindexed(path: &Path) {
    let _ = fs::remove_file(path);
}

/// Why storing a document failed.
#[derive(Debug, thiserror::Error)]
enum StoreError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Index(#[from] IndexError),
    #[error(transparent)]
    Plan(#[from] PlanError),
}

/// The number the list shows, if it could be part of a file name at all.
fn list_number(row: &Row) -> Option<&str> {
    row.values
        .get("invoice_no")
        .map(String::as_str)
        .filter(|number| !sanitize_invoice_no(number).is_empty())
}

fn entry_report(key: String, values: &RowValues, outcome: EntryOutcome) -> EntryReport {
    EntryReport {
        key,
        date: values.get("date").cloned(),
        invoice_no: values.get("invoice_no").cloned(),
        amount: values.get("amount").cloned(),
        outcome,
    }
}

/// A name for a row that has no key, from whatever else it shows.
fn label_of(values: &RowValues, link_field: &str) -> String {
    values
        .iter()
        .filter(|(name, _)| name.as_str() != link_field && name.as_str() != "url")
        .map(|(_, value)| value.as_str())
        .collect::<Vec<_>>()
        .join(" / ")
}

fn clean_text(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .filter(|c| !c.is_control() || c.is_whitespace())
        .collect();
    cleaned
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(MAX_FIELD_CHARS)
        .collect()
}

/// Links come from the page. Only web addresses are followed; `file:` and
/// `javascript:` links are not.
fn resolve_link(page_url: &str, link: &str) -> Option<String> {
    let url = Url::parse(page_url).ok()?.join(link).ok()?;
    matches!(url.scheme(), "http" | "https").then(|| url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::test_pdf;
    use crate::secrets::{ExposeSecret, MemoryStore, SecretString};
    use std::collections::HashMap;

    const LOGIN_JSON: &str = r#"{"user":"me@portal.test","password":"hunter2-password"}"#;
    const TOTP_KEY: &str = "GEZDGNBVGY3TQOJQ";

    /// A page of the simulated portal.
    #[derive(Default, Clone)]
    struct FakePage {
        visible: HashSet<String>,
        rows: HashMap<String, Vec<RowValues>>,
        /// Clicking this selector leads to that address.
        links: HashMap<String, String>,
    }

    impl FakePage {
        fn showing(selectors: &[&str]) -> Self {
            Self {
                visible: selectors.iter().map(|s| (*s).to_owned()).collect(),
                ..Self::default()
            }
        }

        fn with_rows(mut self, selector: &str, rows: Vec<RowValues>) -> Self {
            self.visible.insert(selector.to_owned());
            self.rows.insert(selector.to_owned(), rows);
            self
        }

        fn link(mut self, selector: &str, target: &str) -> Self {
            self.visible.insert(selector.to_owned());
            self.links.insert(selector.to_owned(), target.to_owned());
            self
        }
    }

    #[derive(Default)]
    struct FakeDriver {
        pages: HashMap<String, FakePage>,
        current: String,
        files: HashMap<String, Result<Vec<u8>, DriverError>>,
        printed: Option<Vec<u8>>,
        visits: Vec<String>,
        downloads: Vec<String>,
        filled: Vec<(String, String)>,
        clicks: Vec<String>,
        window_shown: u32,
        fail_fill: bool,
    }

    impl FakeDriver {
        fn page(mut self, url: &str, page: FakePage) -> Self {
            self.pages.insert(url.to_owned(), page);
            self
        }

        fn file(mut self, url: &str, bytes: Vec<u8>) -> Self {
            self.files.insert(url.to_owned(), Ok(bytes));
            self
        }

        fn broken_file(mut self, url: &str) -> Self {
            self.files
                .insert(url.to_owned(), Err(DriverError::Failed("HTTP 500".into())));
            self
        }

        fn current_page(&self) -> FakePage {
            self.pages.get(&self.current).cloned().unwrap_or_default()
        }
    }

    impl BrowserDriver for FakeDriver {
        async fn goto(&mut self, url: &str) -> Result<(), DriverError> {
            self.visits.push(url.to_owned());
            self.current = url.to_owned();
            Ok(())
        }
        async fn current_url(&mut self) -> Result<String, DriverError> {
            Ok(self.current.clone())
        }
        async fn is_visible(&mut self, selector: &str) -> Result<bool, DriverError> {
            Ok(self.current_page().visible.contains(selector))
        }
        async fn click(&mut self, selector: &str) -> Result<(), DriverError> {
            self.clicks.push(selector.to_owned());
            if let Some(target) = self.current_page().links.get(selector) {
                self.current = target.clone();
            }
            Ok(())
        }
        async fn fill(&mut self, selector: &str, value: &SecretString) -> Result<(), DriverError> {
            if self.fail_fill {
                return Err(DriverError::Failed(format!("could not fill {selector}")));
            }
            self.filled
                .push((selector.to_owned(), value.expose_secret().to_owned()));
            Ok(())
        }
        async fn extract_rows(
            &mut self,
            rows: &str,
            _fields: &BTreeMap<String, FieldSelector>,
        ) -> Result<Vec<RowValues>, DriverError> {
            Ok(self
                .current_page()
                .rows
                .get(rows)
                .cloned()
                .unwrap_or_default())
        }
        async fn download(&mut self, url: &str) -> Result<Vec<u8>, DriverError> {
            self.downloads.push(url.to_owned());
            self.files
                .get(url)
                .cloned()
                .unwrap_or_else(|| Err(DriverError::Failed("HTTP 404".into())))
        }
        async fn print_to_pdf(&mut self) -> Result<Vec<u8>, DriverError> {
            self.printed
                .clone()
                .ok_or_else(|| DriverError::Failed("cannot print".into()))
        }
        async fn show_window(&mut self) -> Result<(), DriverError> {
            self.window_shown += 1;
            Ok(())
        }
    }

    /// Answers like a person. `Never` lets the time limit run out.
    enum Person {
        Never,
        Done,
        Cancels,
        /// Makes the confirmation disappear from the page, then says done.
        Solves,
    }

    struct FakeHandler {
        person: Person,
        asked: Vec<ConfirmationRequest>,
        expired: bool,
    }

    impl FakeHandler {
        fn new(person: Person) -> Self {
            Self {
                person,
                asked: Vec::new(),
                expired: false,
            }
        }
    }

    impl ConfirmationHandler for FakeHandler {
        async fn ask(&mut self, request: &ConfirmationRequest) -> ConfirmationAnswer {
            self.asked.push(request.clone());
            match self.person {
                Person::Never => std::future::pending().await,
                Person::Done | Person::Solves => ConfirmationAnswer::Done,
                Person::Cancels => ConfirmationAnswer::Cancelled,
            }
        }
        fn expired(&mut self) {
            self.expired = true;
        }
    }

    struct Setup {
        dir: tempfile::TempDir,
        config: Config,
        index: Index,
        secrets: MemoryStore,
    }

    fn setup() -> Setup {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::from_yaml(&format!(
            r#"
mail:
  accounts:
    - {{ id: main, imap: "i.example:993", user: u, password: "keyring:p", watch: INBOX }}
  disposal: {{ mode: all_delete }}
companies:
  - id: acme
    name: Acme
    portal: {{ credentials: "keyring:acme", totp: "keyring:acme-totp", recipe: acme.yaml }}
    output: {{ dir: '{}', filename: "{{date:%Y-%m-%d}} - Acme - {{invoice_no}}.pdf" }}
"#,
            dir.path().display()
        ))
        .unwrap();
        let secrets = MemoryStore::with("acme", LOGIN_JSON);
        secrets
            .set("acme-totp", &SecretString::from(TOTP_KEY))
            .unwrap();
        Setup {
            dir,
            config,
            index: Index::open_in_memory().unwrap(),
            secrets,
        }
    }

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2030, 1, 1).unwrap()
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        // Paused time jumps ahead whenever everything waits, so time limits
        // take no real time.
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .unwrap()
            .block_on(future)
    }

    fn run_with(
        setup: &Setup,
        recipe: &str,
        index: Option<&Index>,
        dry_run: bool,
        driver: &mut FakeDriver,
        handler: &mut FakeHandler,
    ) -> FetchReport {
        let recipe = Recipe::from_yaml(recipe)
            .unwrap()
            .with_params(&BTreeMap::new())
            .unwrap();
        let job = FetchJob::new(
            &setup.config,
            &setup.config.companies[0],
            &recipe,
            &setup.secrets,
            index,
            today(),
            dry_run,
        )
        .unwrap();
        block_on(run_recipe(&job, driver, handler))
    }

    fn run(setup: &Setup, recipe: &str, driver: &mut FakeDriver) -> FetchReport {
        run_with(
            setup,
            recipe,
            Some(&setup.index),
            false,
            driver,
            &mut FakeHandler::new(Person::Never),
        )
    }

    fn row(number: &str, link: &str) -> RowValues {
        RowValues::from([
            ("invoice_no".to_owned(), number.to_owned()),
            ("url".to_owned(), link.to_owned()),
        ])
    }

    fn invoice_pdf(number: &str) -> Vec<u8> {
        test_pdf::with_text(&[
            &format!("Rechnungsnummer: {number}"),
            "Rechnungsdatum: 03.11.2026",
        ])
    }

    const LIST_URL: &str = "https://portal.test/invoices";

    const LIST_RECIPE: &str = r#"
steps:
  - goto: https://portal.test/invoices
  - extract_list:
      rows: tr
      fields: { invoice_no: td, url: a@href }
  - download_new: { key: invoice_no }
"#;

    fn list_portal(rows: Vec<RowValues>) -> FakeDriver {
        FakeDriver::default().page(LIST_URL, FakePage::default().with_rows("tr", rows))
    }

    fn files_in(setup: &Setup) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(setup.dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn outcomes(report: &FetchReport) -> Vec<&EntryOutcome> {
        report.entries.iter().map(|entry| &entry.outcome).collect()
    }

    #[test]
    fn new_entries_are_downloaded_stored_and_indexed() {
        let setup = setup();
        let mut driver = list_portal(vec![row("R-1", "/doc/1.pdf")])
            .file("https://portal.test/doc/1.pdf", invoice_pdf("R-1"));

        let report = run(&setup, LIST_RECIPE, &mut driver);

        assert_eq!(report.error, None);
        assert!(matches!(
            report.entries[0].outcome,
            EntryOutcome::Stored { .. }
        ));
        assert_eq!(files_in(&setup), ["2026-11-03 - Acme - R-1.pdf"]);
        let stored = setup.index.invoices_of("acme").unwrap().remove(0);
        assert_eq!(stored.source, Source::Portal);
        assert_eq!(stored.status, Status::Stored);
        assert_eq!(stored.invoice_no.as_deref(), Some("R-1"));
        assert_eq!(stored.mail_id, None);
        assert_eq!(summarize_fetches(&[report]).saved, 1);
    }

    #[test]
    fn a_repeated_run_downloads_nothing() {
        let setup = setup();
        let portal = || {
            list_portal(vec![row("R-1", "/doc/1.pdf")])
                .file("https://portal.test/doc/1.pdf", invoice_pdf("R-1"))
        };
        run(&setup, LIST_RECIPE, &mut portal());

        let mut second = portal();
        let report = run(&setup, LIST_RECIPE, &mut second);

        assert_eq!(outcomes(&report), [&EntryOutcome::AlreadyKnown]);
        assert!(second.downloads.is_empty());
        assert_eq!(files_in(&setup).len(), 1);
    }

    #[test]
    fn an_entry_known_by_key_is_skipped_even_if_the_file_says_another_number() {
        let setup = setup();
        let portal = || {
            list_portal(vec![row("LIST-1", "/doc/1.pdf")])
                .file("https://portal.test/doc/1.pdf", invoice_pdf("FILE-1"))
        };
        run(&setup, LIST_RECIPE, &mut portal());
        assert_eq!(
            setup.index.invoices_of("acme").unwrap()[0]
                .invoice_no
                .as_deref(),
            Some("FILE-1")
        );

        let mut second = portal();
        run(&setup, LIST_RECIPE, &mut second);
        assert!(second.downloads.is_empty());
    }

    #[test]
    fn a_failure_with_one_invoice_does_not_stop_the_rest() {
        let setup = setup();
        let mut driver = list_portal(vec![
            row("R-1", "/doc/1.pdf"),
            row("R-2", "/doc/2.pdf"),
            row("R-3", "/doc/3.pdf"),
        ])
        .file("https://portal.test/doc/1.pdf", invoice_pdf("R-1"))
        .broken_file("https://portal.test/doc/2.pdf")
        .file("https://portal.test/doc/3.pdf", invoice_pdf("R-3"));

        let report = run(&setup, LIST_RECIPE, &mut driver);

        assert_eq!(report.error, None);
        assert!(report.has_failures());
        assert!(matches!(
            report.entries[1].outcome,
            EntryOutcome::Failed { .. }
        ));
        assert_eq!(files_in(&setup).len(), 2);
        let summary = summarize_fetches(&[report]);
        assert_eq!((summary.saved, summary.failed), (2, 1));

        // The failed one is not in the index, so the next run gets it.
        let mut retry = list_portal(vec![
            row("R-1", "/doc/1.pdf"),
            row("R-2", "/doc/2.pdf"),
            row("R-3", "/doc/3.pdf"),
        ])
        .file("https://portal.test/doc/2.pdf", invoice_pdf("R-2"));
        run(&setup, LIST_RECIPE, &mut retry);
        assert_eq!(retry.downloads, ["https://portal.test/doc/2.pdf"]);
        assert_eq!(files_in(&setup).len(), 3);
    }

    #[test]
    fn a_dry_run_lists_new_entries_and_writes_nothing() {
        let setup = setup();
        let mut with_date = row("R-1", "/doc/1.pdf");
        with_date.insert("date".into(), "03.11.2026".into());
        let mut driver = list_portal(vec![
            with_date,
            row("R-2", "/doc/2.pdf"),
            row("R-2", "/doc/2-again.pdf"),
        ]);

        let report = run_with(
            &setup,
            LIST_RECIPE,
            None,
            true,
            &mut driver,
            &mut FakeHandler::new(Person::Never),
        );

        assert_eq!(
            outcomes(&report),
            [
                &EntryOutcome::WouldDownload {
                    file_name: Some("2026-11-03 - Acme - R-1.pdf".into())
                },
                &EntryOutcome::WouldDownload { file_name: None },
                &EntryOutcome::AlreadyKnown,
            ]
        );
        assert!(driver.downloads.is_empty());
        assert!(files_in(&setup).is_empty());
        assert_eq!(summarize_fetches(&[report]).saved, 2);
    }

    #[test]
    fn a_dry_run_compares_with_the_index() {
        let setup = setup();
        run(
            &setup,
            LIST_RECIPE,
            &mut list_portal(vec![row("R-1", "/doc/1.pdf")])
                .file("https://portal.test/doc/1.pdf", invoice_pdf("R-1")),
        );

        let mut driver = list_portal(vec![row("R-1", "/doc/1.pdf"), row("R-2", "/doc/2.pdf")]);
        let report = run_with(
            &setup,
            LIST_RECIPE,
            Some(&setup.index),
            true,
            &mut driver,
            &mut FakeHandler::new(Person::Never),
        );

        assert_eq!(report.entries[0].outcome, EntryOutcome::AlreadyKnown);
        assert!(matches!(
            report.entries[1].outcome,
            EntryOutcome::WouldDownload { .. }
        ));
        assert!(driver.downloads.is_empty());
    }

    #[test]
    fn crafted_numbers_stay_inside_the_target_folder() {
        let setup = setup();
        let evil = "../../../escaped";
        let mut driver = list_portal(vec![
            row(evil, "/doc/1.pdf"),
            row("safe", "/doc/2.pdf"),
            row("..\\..\\windows", "/doc/3.pdf"),
        ])
        // The list number is used because this file has none.
        .file(
            "https://portal.test/doc/1.pdf",
            test_pdf::with_text(&["no number here"]),
        )
        // This file itself carries a crafted number.
        .file(
            "https://portal.test/doc/2.pdf",
            test_pdf::with_text(&["Rechnungsnummer: ../../R-9/evil"]),
        )
        .file(
            "https://portal.test/doc/3.pdf",
            test_pdf::with_text(&["no number, second file"]),
        );

        let report = run(&setup, LIST_RECIPE, &mut driver);

        assert_eq!(report.error, None);
        assert_eq!(files_in(&setup).len(), 3, "{report:?}");
        let root = setup.dir.path().canonicalize().unwrap();
        for record in setup.index.invoices_of("acme").unwrap() {
            let path = record.file_path.unwrap().canonicalize().unwrap();
            assert_eq!(path.parent().unwrap(), root);
        }
        let outside = setup.dir.path().parent().unwrap().join("escaped.pdf");
        assert!(!outside.exists());
    }

    #[test]
    fn crafted_links_are_not_followed() {
        let setup = setup();
        let mut driver = list_portal(vec![
            row("R-1", "file:///etc/passwd"),
            row("R-2", "javascript:alert(1)"),
        ]);

        let report = run(&setup, LIST_RECIPE, &mut driver);

        assert!(driver.downloads.is_empty());
        for outcome in outcomes(&report) {
            assert!(matches!(outcome, EntryOutcome::Failed { .. }));
        }
    }

    #[test]
    fn page_text_is_cleaned_before_it_is_used() {
        let setup = setup();
        let mut driver = list_portal(vec![row(" R\n-\u{1b}[31m 1 ", "/doc/1.pdf")]);
        let report = run_with(
            &setup,
            LIST_RECIPE,
            None,
            true,
            &mut driver,
            &mut FakeHandler::new(Person::Never),
        );
        assert_eq!(report.entries[0].key, "R -[31m 1");
    }

    #[test]
    fn a_file_without_any_number_needs_review() {
        let setup = setup();
        let rows = vec![RowValues::from([
            ("id".to_owned(), "K-1".to_owned()),
            ("url".to_owned(), "/doc/1.pdf".to_owned()),
        ])];
        let recipe = r#"
steps:
  - goto: https://portal.test/invoices
  - extract_list: { rows: tr, fields: { id: "@data-id", url: a@href } }
  - download_new: { key: id }
"#;
        let portal = || {
            list_portal(rows.clone()).file(
                "https://portal.test/doc/1.pdf",
                test_pdf::with_text(&["Keine Nummer"]),
            )
        };

        let report = run(&setup, recipe, &mut portal());

        assert!(matches!(
            report.entries[0].outcome,
            EntryOutcome::StoredForReview {
                reason: ReviewReason::NumberNotFound,
                ..
            }
        ));
        let record = setup.index.invoices_of("acme").unwrap().remove(0);
        assert_eq!(record.status, Status::NeedsReview);
        assert_eq!(summarize_fetches(&[report]).for_review, 1);

        // The same list entry does not come back as new.
        let mut second = portal();
        run(&setup, recipe, &mut second);
        assert!(second.downloads.is_empty());
    }

    #[test]
    fn the_list_fills_gaps_the_file_leaves() {
        let setup = setup();
        let mut entry = row("R-7", "/doc/1.pdf");
        entry.insert("date".into(), "15.03.2026".into());
        let mut driver = list_portal(vec![entry]).file(
            "https://portal.test/doc/1.pdf",
            test_pdf::with_text(&["Keine Nummer"]),
        );

        let report = run(&setup, LIST_RECIPE, &mut driver);

        assert!(matches!(
            report.entries[0].outcome,
            EntryOutcome::Stored { .. }
        ));
        assert_eq!(files_in(&setup), ["2026-03-15 - Acme - R-7.pdf"]);
    }

    #[test]
    fn a_known_number_with_another_file_needs_review() {
        let setup = setup();
        run(
            &setup,
            LIST_RECIPE,
            &mut list_portal(vec![row("K-1", "/doc/1.pdf")])
                .file("https://portal.test/doc/1.pdf", invoice_pdf("R-1")),
        );

        // Same number in the file, but different content and a new list key.
        let corrected = test_pdf::with_text(&["Rechnungsnummer: R-1", "Korrigiert"]);
        let report = run(
            &setup,
            LIST_RECIPE,
            &mut list_portal(vec![row("K-2", "/doc/2.pdf")])
                .file("https://portal.test/doc/2.pdf", corrected),
        );

        assert!(matches!(
            report.entries[0].outcome,
            EntryOutcome::StoredForReview {
                reason: ReviewReason::NumberConflict { .. },
                ..
            }
        ));
        assert_eq!(setup.index.invoices_of("acme").unwrap().len(), 2);
    }

    #[test]
    fn an_invoice_that_came_by_mail_is_not_stored_again() {
        let setup = setup();
        let pdf = invoice_pdf("R-1");
        let path = setup.dir.path().join("from-mail.pdf");
        fs::write(&path, &pdf).unwrap();
        setup
            .index
            .insert(&NewInvoice {
                company_id: "acme".into(),
                invoice_date: None,
                invoice_no: Some("R-1".into()),
                amount: None,
                currency: None,
                file_path: path,
                file_hash: sha256_hex(&pdf),
                source: Source::Mail,
                mail_account: Some("main".into()),
                mail_id: Some("<1@x>".into()),
                portal_key: None,
                status: Status::Stored,
            })
            .unwrap();
        let portal = || {
            list_portal(vec![row("K-1", "/doc/1.pdf")])
                .file("https://portal.test/doc/1.pdf", pdf.clone())
        };

        let report = run(&setup, LIST_RECIPE, &mut portal());

        assert_eq!(outcomes(&report), [&EntryOutcome::AlreadyKnown]);
        assert_eq!(setup.index.invoices_of("acme").unwrap().len(), 1);
        assert_eq!(files_in(&setup), ["from-mail.pdf"]);

        // The list entry is now tied to that row and is not fetched again.
        let mut second = portal();
        run(&setup, LIST_RECIPE, &mut second);
        assert!(second.downloads.is_empty());
    }

    #[test]
    fn an_index_failure_removes_the_file() {
        let setup = setup();
        let broken = Index::open_in_memory().unwrap();
        let mut driver = list_portal(vec![row("R-1", "/doc/1.pdf")])
            .file("https://portal.test/doc/1.pdf", invoice_pdf("R-1"));
        let recipe = Recipe::from_yaml(LIST_RECIPE).unwrap();
        let job = FetchJob::new(
            &setup.config,
            &setup.config.companies[0],
            &recipe,
            &setup.secrets,
            Some(&broken),
            today(),
            false,
        )
        .unwrap();
        let run = Run {
            job: &job,
            driver: &mut driver,
            handler: &mut FakeHandler::new(Person::Never),
            pending: Vec::new(),
            seen_keys: HashSet::new(),
            entries: Vec::new(),
        };
        let row = Row {
            values: RowValues::from([("invoice_no".to_owned(), "R-1".to_owned())]),
            page_url: LIST_URL.to_owned(),
        };
        broken.break_for_test();

        let outcome = run.store(&invoice_pdf("R-1"), &row, "R-1");

        assert!(matches!(outcome, EntryOutcome::Failed { .. }));
        assert!(files_in(&setup).is_empty());
    }

    #[test]
    fn a_broken_target_folder_fails_only_that_entry() {
        let setup = setup();
        let blocked = setup.dir.path().join("blocked");
        fs::write(&blocked, b"x").unwrap();
        let mut config = setup.config.clone();
        config.companies[0].output.dir = Some(blocked.display().to_string());
        let setup = Setup { config, ..setup };
        let mut driver = list_portal(vec![row("R-1", "/doc/1.pdf")])
            .file("https://portal.test/doc/1.pdf", invoice_pdf("R-1"));

        let report = run(&setup, LIST_RECIPE, &mut driver);

        assert!(report.has_failures());
        assert!(setup.index.invoices_of("acme").unwrap().is_empty());
    }

    const HTML_RECIPE: &str = r#"
steps:
  - goto: https://portal.test/invoices
  - extract_list: { rows: tr, fields: { invoice_no: td, url: a@href } }
  - download_new: { key: invoice_no, html_to_pdf: true }
"#;

    #[test]
    fn html_invoices_are_printed_only_when_the_recipe_says_so() {
        let html = b"<html>Rechnungsnummer: R-1</html>".to_vec();
        let setup_a = setup();
        let mut driver = list_portal(vec![row("R-1", "/doc/1.html")])
            .file("https://portal.test/doc/1.html", html.clone());
        driver.printed = Some(invoice_pdf("R-1"));
        let report = run(&setup_a, HTML_RECIPE, &mut driver);
        assert!(matches!(
            report.entries[0].outcome,
            EntryOutcome::Stored { .. }
        ));
        assert!(
            driver
                .visits
                .contains(&"https://portal.test/doc/1.html".to_owned())
        );

        let setup_b = setup();
        let mut driver = list_portal(vec![row("R-1", "/doc/1.html")])
            .file("https://portal.test/doc/1.html", html);
        driver.printed = Some(invoice_pdf("R-1"));
        let report = run(&setup_b, LIST_RECIPE, &mut driver);
        assert!(matches!(
            report.entries[0].outcome,
            EntryOutcome::Failed { .. }
        ));
        assert!(files_in(&setup_b).is_empty());
    }

    const LOGIN_RECIPE: &str = r##"
step_timeout_secs: 5
steps:
  - goto: https://portal.test/login
  - login:
      user: "#user"
      password: "#password"
      submit: "#send"
      totp: "#otp"
      logged_in: "#logout"
  - goto: https://portal.test/invoices
  - extract_list: { rows: tr, fields: { invoice_no: td, url: a@href } }
  - download_new: { key: invoice_no }
"##;

    fn login_portal() -> FakeDriver {
        FakeDriver::default()
            .page(
                "https://portal.test/login",
                FakePage::showing(&["#user", "#password"]).link("#send", "https://portal.test/otp"),
            )
            .page(
                "https://portal.test/otp",
                FakePage::showing(&["#otp"]).link("#send", "https://portal.test/home"),
            )
            .page("https://portal.test/home", FakePage::showing(&["#logout"]))
            .page(
                LIST_URL,
                FakePage::default().with_rows("tr", vec![row("R-1", "/doc/1.pdf")]),
            )
            .file("https://portal.test/doc/1.pdf", invoice_pdf("R-1"))
    }

    #[test]
    fn login_fills_credentials_and_a_current_totp_code() {
        let setup = setup();
        let mut driver = login_portal();

        let report = run(&setup, LOGIN_RECIPE, &mut driver);

        assert_eq!(report.error, None);
        assert_eq!(driver.filled[0], ("#user".into(), "me@portal.test".into()));
        assert_eq!(
            driver.filled[1],
            ("#password".into(), "hunter2-password".into())
        );
        assert_eq!(driver.filled[2].0, "#otp");
        assert_eq!(driver.filled[2].1.len(), 6);
        assert_eq!(driver.clicks, ["#send", "#send"]);
        assert_eq!(files_in(&setup).len(), 1);
    }

    #[test]
    fn an_active_session_skips_the_login() {
        let setup = setup();
        let mut driver =
            login_portal().page("https://portal.test/login", FakePage::showing(&["#logout"]));

        let report = run(&setup, LOGIN_RECIPE, &mut driver);

        assert_eq!(report.error, None);
        assert!(driver.filled.is_empty());
    }

    #[test]
    fn a_trusted_device_gets_no_totp_prompt() {
        let setup = setup();
        let mut driver =
            login_portal().page("https://portal.test/otp", FakePage::showing(&["#logout"]));

        let report = run(&setup, LOGIN_RECIPE, &mut driver);

        assert_eq!(report.error, None);
        assert_eq!(driver.filled.len(), 2);
    }

    #[test]
    fn two_page_logins_and_a_separate_totp_button_work() {
        let setup = setup();
        let recipe = r##"
steps:
  - goto: https://portal.test/login
  - login:
      user: "#user"
      next: "#next"
      password: "#password"
      submit: "#send"
      totp: "#otp"
      totp_submit: "#verify"
"##;
        let mut driver = FakeDriver::default()
            .page(
                "https://portal.test/login",
                FakePage::showing(&["#user"]).link("#next", "https://portal.test/pw"),
            )
            .page(
                "https://portal.test/pw",
                FakePage::showing(&["#password"]).link("#send", "https://portal.test/otp"),
            )
            .page(
                "https://portal.test/otp",
                FakePage::showing(&["#otp"]).link("#verify", "https://portal.test/home"),
            );

        let report = run(&setup, recipe, &mut driver);

        assert_eq!(report.error, None);
        assert_eq!(driver.clicks, ["#next", "#send", "#verify"]);
    }

    #[test]
    fn credentials_are_only_sent_over_encrypted_connections_or_locally() {
        let recipe = |host: &str| {
            format!(
                r##"
steps:
  - goto: {host}/login
  - login: {{ user: "#user", password: "#password", submit: "#send" }}
"##
            )
        };
        let setup = setup();
        let mut driver = FakeDriver::default().page(
            "http://portal.test/login",
            FakePage::showing(&["#user", "#password"]),
        );
        let report = run(&setup, &recipe("http://portal.test"), &mut driver);
        assert!(report.error.unwrap().contains("unencrypted"));
        assert!(driver.filled.is_empty());

        let mut driver = FakeDriver::default().page(
            "http://127.0.0.1:8123/login",
            FakePage::showing(&["#user", "#password"]).link("#send", "http://127.0.0.1:8123/home"),
        );
        let report = run(&setup, &recipe("http://127.0.0.1:8123"), &mut driver);
        assert_eq!(report.error, None);
        assert_eq!(driver.filled.len(), 2);
    }

    #[test]
    fn secrets_never_appear_in_reports_or_errors() {
        let setup = setup();
        let mut driver = login_portal();
        driver.fail_fill = true;
        let report = run(&setup, LOGIN_RECIPE, &mut driver);
        let error = report.error.clone().unwrap();
        assert!(error.contains("#user"), "{error}");

        let mut driver = login_portal();
        let good = run(&setup, LOGIN_RECIPE, &mut driver);
        let code = driver.filled[2].1.clone();

        for text in [
            serde_json::to_string(&report).unwrap(),
            serde_json::to_string(&good).unwrap(),
            format!("{report:?}"),
            format!("{good:?}"),
        ] {
            for secret in ["hunter2-password", "me@portal.test", TOTP_KEY, &code] {
                assert!(!text.contains(secret), "{secret} in {text}");
            }
        }
    }

    #[test]
    fn a_wrong_password_is_never_sent_twice() {
        let setup = setup();
        // After submitting, the form simply stays.
        let mut driver = FakeDriver::default().page(
            "https://portal.test/login",
            FakePage::showing(&["#user", "#password"]),
        );
        let recipe = r##"
step_timeout_secs: 1
steps:
  - goto: https://portal.test/login
  - login: { user: "#user", password: "#password", submit: "#send" }
"##;
        let mut handler = FakeHandler::new(Person::Done);

        let report = run_with(
            &setup,
            recipe,
            Some(&setup.index),
            false,
            &mut driver,
            &mut handler,
        );

        assert!(report.error.unwrap().contains("login"));
        assert_eq!(driver.clicks, ["#send"]);
        assert_eq!(handler.asked.len(), 1);
    }

    #[test]
    fn missing_or_unusable_secrets_are_explained_before_anything_starts() {
        let setup = setup();
        let company = &setup.config.companies[0];
        let recipe = Recipe::from_yaml(LOGIN_RECIPE).unwrap();
        let job = |secrets: &MemoryStore| {
            FetchJob::new(
                &setup.config,
                company,
                &recipe,
                secrets,
                None,
                today(),
                true,
            )
            .err()
            .unwrap()
            .to_string()
        };

        let error = job(&MemoryStore::default());
        assert!(error.contains("docsweep secret set-portal acme"), "{error}");

        let error = job(&MemoryStore::with("acme", "hunter2-plain"));
        assert!(error.contains("`acme`"), "{error}");
        assert!(!error.contains("hunter2"), "{error}");

        let error = job(&MemoryStore::with("acme", LOGIN_JSON));
        assert!(
            error.contains("docsweep secret set-totp acme-totp"),
            "{error}"
        );

        let bad_key = MemoryStore::with("acme", LOGIN_JSON);
        bad_key
            .set("acme-totp", &SecretString::from("!hunter2!"))
            .unwrap();
        let error = job(&bad_key);
        assert!(error.contains("acme-totp"), "{error}");
        assert!(!error.contains("hunter2"), "{error}");
    }

    const CONFIRM_RECIPE: &str = r##"
confirmation: { detect: "#captcha", timeout_secs: 60 }
step_timeout_secs: 5
steps:
  - goto: https://portal.test/invoices
  - extract_list: { rows: tr, fields: { invoice_no: td, url: a@href } }
  - download_new: { key: invoice_no }
"##;

    fn portal_with_captcha_forever() -> FakeDriver {
        FakeDriver::default().page(LIST_URL, FakePage::showing(&["#captcha"]))
    }

    #[test]
    fn a_confirmation_that_times_out_ends_the_run_cleanly() {
        let setup = setup();
        let mut driver = portal_with_captcha_forever();
        let mut handler = FakeHandler::new(Person::Never);

        let report = run_with(
            &setup,
            CONFIRM_RECIPE,
            Some(&setup.index),
            false,
            &mut driver,
            &mut handler,
        );

        assert!(report.error.as_ref().unwrap().contains("in time"));
        assert!(handler.expired);
        assert_eq!(driver.window_shown, 1);
        assert!(files_in(&setup).is_empty());
        assert!(setup.index.invoices_of("acme").unwrap().is_empty());
        assert_eq!(summarize_fetches(&[report]).failed, 1);
    }

    #[test]
    fn a_time_out_later_in_the_run_keeps_what_was_stored() {
        let setup = setup();
        let recipe = r##"
confirmation: { detect: "#captcha", timeout_secs: 60 }
steps:
  - goto: https://portal.test/invoices
  - extract_list: { rows: tr, fields: { invoice_no: td, url: a@href } }
  - download_new: { key: invoice_no }
  - goto: https://portal.test/more
  - extract_list: { rows: li, fields: { invoice_no: td, url: a@href } }
  - download_new: { key: invoice_no }
"##;
        let mut driver = list_portal(vec![row("R-1", "/doc/1.pdf")])
            .file("https://portal.test/doc/1.pdf", invoice_pdf("R-1"))
            .page("https://portal.test/more", FakePage::showing(&["#captcha"]));
        let mut handler = FakeHandler::new(Person::Never);

        let report = run_with(
            &setup,
            recipe,
            Some(&setup.index),
            false,
            &mut driver,
            &mut handler,
        );

        assert!(report.error.is_some());
        assert_eq!(report.entries.len(), 1);
        let rows = setup.index.invoices_of("acme").unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].file_path.as_ref().unwrap().exists());
        assert_eq!(files_in(&setup).len(), 1);
    }

    #[test]
    fn a_person_who_says_done_too_early_is_asked_again_until_the_limit() {
        let setup = setup();
        let mut driver = portal_with_captcha_forever();
        let mut handler = FakeHandler::new(Person::Done);

        let report = run_with(
            &setup,
            CONFIRM_RECIPE,
            Some(&setup.index),
            false,
            &mut driver,
            &mut handler,
        );

        assert!(report.error.unwrap().contains("in time"));
        assert!(handler.asked.len() > 1);
        assert_eq!(handler.asked[0].reason, ConfirmationReason::Detected);
        assert_eq!(handler.asked[0].company_name, "Acme");
        assert_eq!(handler.asked[0].timeout_secs, 60);
    }

    #[test]
    fn cancelling_ends_the_run() {
        let setup = setup();
        let mut driver = portal_with_captcha_forever();
        let mut handler = FakeHandler::new(Person::Cancels);
        let report = run_with(
            &setup,
            CONFIRM_RECIPE,
            Some(&setup.index),
            false,
            &mut driver,
            &mut handler,
        );
        assert!(report.error.unwrap().contains("cancelled"));
    }

    /// A driver whose page changes when a person has been asked.
    struct SolvedByPerson {
        inner: FakeDriver,
        after_person: FakePage,
    }

    impl BrowserDriver for SolvedByPerson {
        async fn goto(&mut self, url: &str) -> Result<(), DriverError> {
            self.inner.goto(url).await
        }
        async fn current_url(&mut self) -> Result<String, DriverError> {
            self.inner.current_url().await
        }
        async fn is_visible(&mut self, selector: &str) -> Result<bool, DriverError> {
            self.inner.is_visible(selector).await
        }
        async fn click(&mut self, selector: &str) -> Result<(), DriverError> {
            self.inner.click(selector).await
        }
        async fn fill(&mut self, selector: &str, value: &SecretString) -> Result<(), DriverError> {
            self.inner.fill(selector, value).await
        }
        async fn extract_rows(
            &mut self,
            rows: &str,
            fields: &BTreeMap<String, FieldSelector>,
        ) -> Result<Vec<RowValues>, DriverError> {
            self.inner.extract_rows(rows, fields).await
        }
        async fn download(&mut self, url: &str) -> Result<Vec<u8>, DriverError> {
            self.inner.download(url).await
        }
        async fn print_to_pdf(&mut self) -> Result<Vec<u8>, DriverError> {
            self.inner.print_to_pdf().await
        }
        async fn show_window(&mut self) -> Result<(), DriverError> {
            self.inner.window_shown += 1;
            let current = self.inner.current.clone();
            self.inner.pages.insert(current, self.after_person.clone());
            Ok(())
        }
    }

    fn run_solved(
        setup: &Setup,
        recipe: &str,
        driver: &mut SolvedByPerson,
    ) -> (FetchReport, FakeHandler) {
        let recipe = Recipe::from_yaml(recipe).unwrap();
        let job = FetchJob::new(
            &setup.config,
            &setup.config.companies[0],
            &recipe,
            &setup.secrets,
            Some(&setup.index),
            today(),
            false,
        )
        .unwrap();
        let mut handler = FakeHandler::new(Person::Solves);
        let report = block_on(run_recipe(&job, driver, &mut handler));
        (report, handler)
    }

    #[test]
    fn after_a_person_solved_a_detected_confirmation_the_run_goes_on() {
        let setup = setup();
        let mut driver = SolvedByPerson {
            inner: FakeDriver::default()
                .page(LIST_URL, FakePage::showing(&["#captcha"]))
                .file("https://portal.test/doc/1.pdf", invoice_pdf("R-1")),
            after_person: FakePage::default().with_rows("tr", vec![row("R-1", "/doc/1.pdf")]),
        };

        let (report, handler) = run_solved(&setup, CONFIRM_RECIPE, &mut driver);

        assert_eq!(report.error, None);
        assert_eq!(handler.asked.len(), 1);
        assert_eq!(driver.inner.window_shown, 1);
        assert_eq!(files_in(&setup).len(), 1);
    }

    #[test]
    fn a_step_that_times_out_asks_a_person_and_continues() {
        let setup = setup();
        let mut driver = SolvedByPerson {
            // The list never shows by itself, for example behind an interstitial.
            inner: FakeDriver::default()
                .page(LIST_URL, FakePage::default())
                .file("https://portal.test/doc/1.pdf", invoice_pdf("R-1")),
            after_person: FakePage::default().with_rows("tr", vec![row("R-1", "/doc/1.pdf")]),
        };

        let (report, handler) = run_solved(&setup, CONFIRM_RECIPE, &mut driver);

        assert_eq!(report.error, None);
        assert!(matches!(
            handler.asked[0].reason,
            ConfirmationReason::StepTimedOut { .. }
        ));
        assert_eq!(files_in(&setup).len(), 1);
    }

    #[test]
    fn a_step_that_still_fails_after_asking_ends_the_run() {
        let setup = setup();
        let mut driver = FakeDriver::default().page(LIST_URL, FakePage::default());
        let mut handler = FakeHandler::new(Person::Done);
        let report = run_with(
            &setup,
            CONFIRM_RECIPE,
            Some(&setup.index),
            false,
            &mut driver,
            &mut handler,
        );
        assert!(report.error.unwrap().contains("waiting for the list"));
    }

    #[test]
    fn a_declared_empty_list_is_not_a_problem() {
        let setup = setup();
        let recipe = r#"
steps:
  - goto: https://portal.test/invoices
  - extract_list: { rows: tr, empty: .none, fields: { invoice_no: td, url: a@href } }
  - download_new: { key: invoice_no }
"#;
        let mut driver = FakeDriver::default().page(LIST_URL, FakePage::showing(&[".none"]));
        let report = run(&setup, recipe, &mut driver);
        assert_eq!(report.error, None);
        assert!(report.entries.is_empty());
    }

    const PAGED_RECIPE: &str = r#"
step_timeout_secs: 2
steps:
  - goto: https://portal.test/invoices
  - extract_list:
      rows: tr
      next_page: a.next
      max_pages: 3
      fields: { invoice_no: td, url: a@href }
  - download_new: { key: invoice_no }
"#;

    fn paged(url: &str, number: &str, next: Option<&str>) -> (String, FakePage) {
        let page = FakePage::default().with_rows("tr", vec![row(number, "/doc/x.pdf")]);
        let page = match next {
            Some(next) => page.link("a.next", next),
            None => page,
        };
        (url.to_owned(), page)
    }

    fn keys(report: &FetchReport) -> Vec<&str> {
        report
            .entries
            .iter()
            .map(|entry| entry.key.as_str())
            .collect()
    }

    #[test]
    fn pagination_collects_all_pages() {
        let setup = setup();
        let mut driver = FakeDriver::default();
        for (url, page) in [
            paged(LIST_URL, "R-1", Some("https://portal.test/p2")),
            paged(
                "https://portal.test/p2",
                "R-2",
                Some("https://portal.test/p3"),
            ),
            paged("https://portal.test/p3", "R-3", None),
        ] {
            driver.pages.insert(url, page);
        }

        let report = run_with(
            &setup,
            PAGED_RECIPE,
            None,
            true,
            &mut driver,
            &mut FakeHandler::new(Person::Never),
        );

        assert_eq!(keys(&report), ["R-1", "R-2", "R-3"]);
        assert_eq!(report.error, None);
    }

    #[test]
    fn a_page_limit_or_a_stuck_next_page_is_reported_and_the_rest_still_processed() {
        let setup = setup();
        // The third page still has a next link, and the limit is three.
        let mut driver = FakeDriver::default();
        for (url, page) in [
            paged(LIST_URL, "R-1", Some("https://portal.test/p2")),
            paged(
                "https://portal.test/p2",
                "R-2",
                Some("https://portal.test/p3"),
            ),
            paged(
                "https://portal.test/p3",
                "R-3",
                Some("https://portal.test/p4"),
            ),
        ] {
            driver.pages.insert(url, page);
        }
        let report = run_with(
            &setup,
            PAGED_RECIPE,
            None,
            true,
            &mut driver,
            &mut FakeHandler::new(Person::Never),
        );
        assert_eq!(report.entries.len(), 4);
        let EntryOutcome::Failed { message } = &report.entries[0].outcome else {
            panic!("expected the page problem first: {report:?}");
        };
        assert!(message.contains('3'), "{message}");
        assert!(matches!(
            report.entries[3].outcome,
            EntryOutcome::WouldDownload { .. }
        ));

        // The next link does nothing.
        let mut driver = FakeDriver::default();
        let (url, page) = paged(LIST_URL, "R-1", Some(LIST_URL));
        driver.pages.insert(url, page);
        let report = run_with(
            &setup,
            PAGED_RECIPE,
            None,
            true,
            &mut driver,
            &mut FakeHandler::new(Person::Never),
        );
        let EntryOutcome::Failed { message } = &report.entries[0].outcome else {
            panic!("expected the page problem first: {report:?}");
        };
        assert!(message.contains("page 2"), "{message}");
        assert_eq!(report.entries.len(), 2);
    }

    const ORDERS_RECIPE: &str = r#"
step_timeout_secs: 2
params: { domain: portal.test }
steps:
  - goto: https://{domain}/orders
  - extract_list:
      rows: .order
      fields: { order: .number, details: a@href }
      open_detail:
        url: details
        rows: li
        missing: .none
        fields: { invoice_no: span, url: a@href }
  - download_new: { key: invoice_no }
"#;

    fn order(number: &str, link: &str) -> RowValues {
        RowValues::from([
            ("order".to_owned(), number.to_owned()),
            ("details".to_owned(), link.to_owned()),
        ])
    }

    fn orders_portal() -> FakeDriver {
        FakeDriver::default()
            .page(
                "https://portal.test/orders",
                FakePage::default().with_rows(
                    ".order",
                    vec![
                        order("O-1", "/orders/1"),
                        order("O-2", "/orders/2"),
                        order("O-3", "/orders/3"),
                    ],
                ),
            )
            .page(
                "https://portal.test/orders/1",
                FakePage::default().with_rows(
                    "li",
                    vec![row("R-1", "/doc/1.pdf"), row("C-1", "/doc/2.pdf")],
                ),
            )
            .page(
                "https://portal.test/orders/2",
                FakePage::showing(&[".none"]),
            )
            .page(
                "https://portal.test/orders/3",
                FakePage::default().with_rows("li", vec![row("R-3", "/doc/3.pdf")]),
            )
            .file("https://portal.test/doc/1.pdf", invoice_pdf("R-1"))
            .file("https://portal.test/doc/2.pdf", invoice_pdf("C-1"))
            .file("https://portal.test/doc/3.pdf", invoice_pdf("R-3"))
    }

    #[test]
    fn a_detail_page_can_hold_several_documents_or_none() {
        let setup = setup();
        let mut driver = orders_portal();

        let report = run(&setup, ORDERS_RECIPE, &mut driver);

        assert_eq!(report.error, None);
        assert_eq!(files_in(&setup).len(), 3);
        let missing: Vec<&str> = report
            .entries
            .iter()
            .filter(|entry| entry.outcome == EntryOutcome::Missing)
            .map(|entry| entry.key.as_str())
            .collect();
        assert_eq!(missing, ["O-2"]);
        let summary = summarize_fetches(&[report]);
        assert_eq!((summary.saved, summary.missing, summary.failed), (3, 1, 0));
    }

    #[test]
    fn a_detail_page_without_documents_counts_as_missing_after_a_wait() {
        let setup = setup();
        let mut driver = orders_portal().page("https://portal.test/orders/2", FakePage::default());
        let report = run(&setup, ORDERS_RECIPE, &mut driver);
        assert!(
            report
                .entries
                .iter()
                .any(|e| e.outcome == EntryOutcome::Missing)
        );
        assert_eq!(files_in(&setup).len(), 3);
    }

    #[test]
    fn a_document_row_without_a_link_is_missing_not_a_failure() {
        let setup = setup();
        let mut no_link = row("R-9", "");
        no_link.remove("url");
        let mut driver = list_portal(vec![no_link]);
        let report = run(&setup, LIST_RECIPE, &mut driver);
        assert_eq!(outcomes(&report), [&EntryOutcome::Missing]);
        assert!(!report.has_failures());
    }

    #[test]
    fn a_row_with_a_link_but_no_key_fails_alone() {
        let setup = setup();
        let mut no_key = row("x", "/doc/1.pdf");
        no_key.remove("invoice_no");
        let mut driver = list_portal(vec![no_key, row("R-2", "/doc/2.pdf")])
            .file("https://portal.test/doc/2.pdf", invoice_pdf("R-2"));
        let report = run(&setup, LIST_RECIPE, &mut driver);
        assert!(matches!(
            report.entries[0].outcome,
            EntryOutcome::Failed { .. }
        ));
        assert!(matches!(
            report.entries[1].outcome,
            EntryOutcome::Stored { .. }
        ));
    }

    #[test]
    fn the_cookie_banner_is_clicked_when_it_shows_and_ignored_otherwise() {
        let setup = setup();
        let recipe = r##"
steps:
  - goto: https://portal.test/a
  - dismiss_cookie_banner: "#ok"
  - goto: https://portal.test/b
  - dismiss_cookie_banner: "#ok"
  - dismiss_cookie_banner: false
  - dismiss_cookie_banner: true
"##;
        let mut driver = FakeDriver::default()
            .page("https://portal.test/a", FakePage::showing(&["#ok"]))
            .page("https://portal.test/b", FakePage::default());
        let report = run(&setup, recipe, &mut driver);
        assert_eq!(report.error, None);
        assert_eq!(driver.clicks, ["#ok"]);
    }

    #[test]
    fn reports_serialize_with_stable_tags_for_the_gui() {
        let report = FetchReport {
            company_id: "acme".into(),
            company_name: "Acme".into(),
            entries: vec![EntryReport {
                key: "K".into(),
                date: None,
                invoice_no: None,
                amount: None,
                outcome: EntryOutcome::StoredForReview {
                    path: PathBuf::from("x.pdf"),
                    reason: ReviewReason::NumberNotFound,
                },
            }],
            error: None,
        };
        let json = serde_json::to_value(&report).unwrap();
        let outcome = &json["entries"][0]["outcome"];
        assert_eq!(outcome["kind"], "stored_for_review");
        assert_eq!(outcome["reason"]["kind"], "number_not_found");
    }
}
