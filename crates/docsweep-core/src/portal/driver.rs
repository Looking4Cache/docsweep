//! What the recipe engine needs from a browser and from a person.
//!
//! All are traits so that the logic can be tested without a browser. The real
//! browser is in `docsweep-browser`; the command line and the desktop app each
//! bring their own way of asking a person.

use std::collections::BTreeMap;

use serde::Serialize;

use super::engine::FetchReport;
use super::recipe::FieldSelector;
use crate::config::Company;
use crate::secrets::SecretString;

/// Why the browser could not do something. Implementations must never put a
/// filled value into an error, and should not put addresses there either,
/// because document links can carry tokens.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DriverError {
    #[error("the page did not respond in time")]
    Timeout,
    #[error("{0}")]
    Failed(String),
}

/// One extracted row: field name to text or attribute value.
pub type RowValues = BTreeMap<String, String>;

/// A browser with one open page.
#[allow(
    async_fn_in_trait,
    reason = "only used through generics inside this workspace"
)]
pub trait BrowserDriver {
    /// Opens an address and waits until it has loaded.
    async fn goto(&mut self, url: &str) -> Result<(), DriverError>;
    /// The address of the open page.
    async fn current_url(&mut self) -> Result<String, DriverError>;
    /// Whether an element matching the selector is rendered right now. A page
    /// that is just changing counts as not showing it; an invalid selector is
    /// an error.
    async fn is_visible(&mut self, selector: &str) -> Result<bool, DriverError>;
    /// Clicks the first element matching the selector.
    async fn click(&mut self, selector: &str) -> Result<(), DriverError>;
    /// Replaces the content of an input field. The value is a secret.
    async fn fill(&mut self, selector: &str, value: &SecretString) -> Result<(), DriverError>;
    /// Reads all rows with their fields. Fields that are absent stay out.
    async fn extract_rows(
        &mut self,
        rows: &str,
        fields: &BTreeMap<String, FieldSelector>,
    ) -> Result<Vec<RowValues>, DriverError>;
    /// Fetches a file within the session of the open page.
    async fn download(&mut self, url: &str) -> Result<Vec<u8>, DriverError>;
    /// Prints the open page to a PDF.
    async fn print_to_pdf(&mut self) -> Result<Vec<u8>, DriverError>;
    /// Makes the browser visible to a person, keeping the current address. Does
    /// nothing if it already is, or if this driver never shows a window.
    async fn show_window(&mut self) -> Result<(), DriverError>;
}

/// Why a run needs a person.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConfirmationReason {
    /// The recipe's `confirmation.detect` selector shows a captcha or a request
    /// to confirm by mail or push.
    Detected,
    /// A step waited too long for the page. `step` names it.
    StepTimedOut { step: String },
}

/// What a person is asked to do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConfirmationRequest {
    pub company_name: String,
    pub reason: ConfirmationReason,
    pub timeout_secs: u64,
}

/// What the person answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmationAnswer {
    /// The person is done in the browser; the run goes on.
    Done,
    /// The person gave up; the run ends as failed.
    Cancelled,
}

/// Asks a person for help. The engine enforces the time limit by dropping the
/// future of [`ask`](Self::ask), and then calls [`expired`](Self::expired).
#[allow(
    async_fn_in_trait,
    reason = "only used through generics inside this workspace"
)]
pub trait ConfirmationHandler {
    async fn ask(&mut self, request: &ConfirmationRequest) -> ConfirmationAnswer;
    /// The time limit has passed, so a visible question can be taken back.
    fn expired(&mut self) {}
}

/// Starts the recipe of a company for real. Mail scans and the schedule use this,
/// and only real runs get one, so a preview cannot start a browser.
#[allow(
    async_fn_in_trait,
    reason = "only used through generics inside this workspace"
)]
pub trait PortalFetcher {
    async fn fetch(&mut self, company: &Company) -> FetchReport;
}
