//! Configuration model and YAML loader.
//!
//! Loading reports every problem at once, each with the path of the affected
//! value. Error messages never quote values of secret fields, and YAML snippets
//! are disabled because they could quote a plaintext password.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::names;
use crate::naming::FilenameTemplate;
use crate::portal::hosts::is_web_address;
use crate::schedule::Schedule;

/// Fewer minutes between polls would only burden the mail server.
pub const MIN_POLL_MINUTES: u32 = 5;

/// The whole `docsweep.yaml`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Settings that apply unless a company says otherwise.
    #[serde(default, skip_serializing_if = "Defaults::is_empty")]
    pub defaults: Defaults,
    /// Whether Docsweep runs on its own. Off unless someone turned it on.
    #[serde(default, skip_serializing_if = "Automation::is_off")]
    pub automation: Automation,
    pub mail: MailConfig,
    /// Optional AI provider, with the user's own key. Without it, nothing is ever
    /// sent to a language model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llm: Option<LlmConfig>,
    #[serde(default)]
    pub companies: Vec<Company>,
}

/// The `defaults` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    /// Folder for invoices of companies without their own `output.dir`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_dir: Option<String>,
    /// Where the browser saves downloads, for assisted pick-ups; without it, the
    /// system's downloads folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downloads_dir: Option<String>,
}

impl Defaults {
    fn is_empty(&self) -> bool {
        self.output_dir.is_none() && self.downloads_dir.is_none()
    }
}

/// The `automation` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Automation {
    /// The master switch for `portal.schedule` and `mail.poll_minutes`, which do
    /// nothing without it. Automatic runs save and clean up without a preview.
    #[serde(default)]
    pub enabled: bool,
}

impl Automation {
    fn is_off(&self) -> bool {
        !self.enabled
    }
}

/// The `mail` section.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MailConfig {
    pub accounts: Vec<MailAccount>,
    pub disposal: GlobalDisposal,
    /// Minutes between automatic scans; without it, mailboxes are not polled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll_minutes: Option<u32>,
}

/// One IMAP mailbox.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MailAccount {
    pub id: String,
    /// `host:port`.
    pub imap: String,
    pub user: String,
    pub password: SecretRef,
    /// Folder to scan, for example `INBOX`.
    pub watch: String,
}

impl MailAccount {
    /// Splits `imap` into host and port. Valid after [`Config::from_yaml`].
    pub fn endpoint(&self) -> Option<(&str, u16)> {
        let (host, port) = self.imap.rsplit_once(':')?;
        let port = port.parse().ok()?;
        (!host.is_empty()).then_some((host, port))
    }
}

/// Global default for cleaning up mails.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GlobalDisposal {
    pub mode: DisposalMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub move_to: Option<String>,
    #[serde(default = "default_true")]
    pub delete_to_trash: bool,
}

fn default_true() -> bool {
    true
}

/// The three global cleanup modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DisposalMode {
    AllDelete,
    AllMove,
    PdfMoveLinkDelete,
}

/// The `llm` section.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LlmConfig {
    pub provider: LlmProvider,
    pub api_key: SecretRef,
    /// Required for `openai` and `openrouter`, whose model names Docsweep does
    /// not guess. `anthropic` has a default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl LlmConfig {
    fn validate(&self, report: &mut Report) {
        report.secret("llm.api_key", &self.api_key);
        match &self.model {
            Some(model) => report.not_blank("llm.model", model),
            None if self.provider != LlmProvider::Anthropic => report.add(
                "llm.model",
                "is required for this provider; use the name of a model it offers",
            ),
            None => {}
        }
    }
}

/// Supported AI providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmProvider {
    Anthropic,
    Openai,
    Openrouter,
}

/// A company, the top-level object of the configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Company {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mail: Option<MailRule>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub portal: Option<Portal>,
    pub output: Output,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disposal: Option<CompanyDisposal>,
}

/// Which mails belong to a company. All given conditions must match.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MailRule {
    pub from_domain: String,
    /// Regular expression for the subject.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// Regular expression for attachment file names.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachment: Option<String>,
}

/// How invoices get out of a portal.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PortalMode {
    /// Docsweep logs in with a recipe.
    #[default]
    Recipe,
    /// A person logs in and downloads in their ordinary browser, for portals that
    /// reject automated browsers. Docsweep only opens `url` and files the PDF.
    Assisted,
}

impl PortalMode {
    fn is_recipe(&self) -> bool {
        *self == Self::Recipe
    }
}

/// Portal access. `credentials` holds user name and password in the format of
/// `portal::credentials`, `totp` the setup key of the second factor. Both and
/// `recipe` are required in `recipe` mode, and validation sees to that.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Portal {
    #[serde(default, skip_serializing_if = "PortalMode::is_recipe")]
    pub mode: PortalMode,
    /// The address an assisted pick-up opens in the default browser.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials: Option<SecretRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub totp: Option<SecretRef>,
    /// Relative paths start in the recipes directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipe: Option<PathBuf>,
    /// Values for the recipe's parameters, such as a domain.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, String>,
    /// Cron expression with five fields, in local time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schedule: Option<String>,
}

impl Company {
    /// Whether a person fetches this company's invoices by hand.
    pub fn is_assisted(&self) -> bool {
        self.portal
            .as_ref()
            .is_some_and(|portal| portal.mode == PortalMode::Assisted)
    }
}

/// Where invoices are stored and how they are named.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Output {
    /// Folder of this company; without it, `defaults.output_dir` applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    pub filename: String,
}

/// The downloads folder the system names for this user.
pub fn system_downloads_dir() -> Option<PathBuf> {
    directories::UserDirs::new()?
        .download_dir()
        .map(Path::to_path_buf)
}

fn expand_home(dir: &str, home: Option<&Path>) -> PathBuf {
    match (dir, home) {
        ("~", Some(home)) => home.to_path_buf(),
        (dir, Some(home)) if dir.starts_with("~/") => home.join(&dir[2..]),
        _ => PathBuf::from(dir),
    }
}

/// Per-company override of the global disposal.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged, expecting = "`delete` or `{ move_to: <folder> }`")]
pub enum CompanyDisposal {
    Delete(DeleteKeyword),
    Move(MoveTarget),
}

/// The literal `delete`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeleteKeyword {
    Delete,
}

/// `{ move_to: <folder> }`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MoveTarget {
    pub move_to: String,
}

/// A reference to a keychain entry, `keyring:<id>`.
///
/// Loading rejects everything else, so plaintext secrets cannot pass. `Debug`
/// is redacted because an unvalidated value could be a plaintext secret.
#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(transparent)]
pub struct SecretRef(String);

impl SecretRef {
    /// The reference for a keychain id.
    pub fn from_id(id: &str) -> Self {
        Self(format!("{}{id}", names::CONFIG_SECRET_PREFIX))
    }

    /// The keychain id, or `None` if this is not a valid reference.
    pub fn id(&self) -> Option<&str> {
        self.0
            .strip_prefix(names::CONFIG_SECRET_PREFIX)
            .filter(|id| is_valid_id(id))
    }
}

impl fmt::Debug for SecretRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.id() {
            Some(id) => write!(f, "SecretRef({id})"),
            None => f.write_str("SecretRef(<invalid>)"),
        }
    }
}

/// A single validation problem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Issue {
    /// Path of the value, for example `companies[1].mail.subject`.
    pub path: String,
    pub message: String,
}

/// Why a configuration could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read configuration file {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("cannot write configuration file {path}: {message}")]
    Write { path: PathBuf, message: String },
    #[error("{origin} has a syntax or structure error: {message}")]
    Syntax { origin: String, message: String },
    #[error("{origin} has {} problem(s):{}", issues.len(), format_issues(issues))]
    Invalid { origin: String, issues: Vec<Issue> },
}

/// Adds advice for the mistake parsers describe worst: `"C:\\Users\\me"` is an
/// invalid escape sequence in double-quoted YAML.
fn explain_syntax_error(message: String) -> String {
    if message.contains("quoted scalar") {
        format!(
            "{message}. A backslash starts an escape sequence inside double quotes; \
             write Windows paths in single quotes or with forward slashes"
        )
    } else {
        message
    }
}

pub(crate) fn format_issues(issues: &[Issue]) -> String {
    issues
        .iter()
        .map(|issue| format!("\n  - {}: {}", issue.path, issue.message))
        .collect()
}

impl Config {
    /// Loads and validates a configuration file.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let input = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&input, &path.display().to_string())
    }

    /// Parses and validates YAML text.
    pub fn from_yaml(input: &str) -> Result<Self, ConfigError> {
        Self::parse(input, "the configuration")
    }

    /// The folder for a company's invoices with a leading `~` expanded.
    pub fn output_dir_of(&self, company: &Company) -> PathBuf {
        let dir = company
            .output
            .dir
            .as_deref()
            .or(self.defaults.output_dir.as_deref())
            .expect("validation requires a folder or a default folder");
        let home = directories::BaseDirs::new().map(|base| base.home_dir().to_path_buf());
        expand_home(dir, home.as_deref())
    }

    /// The folder an assisted pick-up looks at, with a leading `~` expanded.
    /// `None` only if nothing is set and the system names no downloads folder.
    pub fn downloads_dir(&self) -> Option<PathBuf> {
        let home = directories::BaseDirs::new().map(|base| base.home_dir().to_path_buf());
        match &self.defaults.downloads_dir {
            Some(dir) => Some(expand_home(dir, home.as_deref())),
            None => system_downloads_dir(),
        }
    }

    /// Validates and writes the configuration atomically.
    ///
    /// Writing does not preserve YAML comments. The first time an existing file
    /// is replaced, it is therefore kept once as `docsweep.yaml.bak`.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        let origin = path.display().to_string();
        let issues = self.validate();
        if !issues.is_empty() {
            return Err(ConfigError::Invalid { origin, issues });
        }
        let write_error = |message: String| ConfigError::Write {
            path: path.to_path_buf(),
            message,
        };
        let yaml = serde_saphyr::to_string(self).map_err(|e| write_error(e.to_string()))?;

        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(dir).map_err(|e| write_error(e.to_string()))?;
        let backup = path.with_extension("yaml.bak");
        if path.exists() && !backup.exists() {
            fs::copy(path, &backup).map_err(|e| write_error(e.to_string()))?;
        }
        let mut temporary =
            tempfile::NamedTempFile::new_in(dir).map_err(|e| write_error(e.to_string()))?;
        temporary
            .write_all(yaml.as_bytes())
            .and_then(|()| temporary.as_file().sync_all())
            .map_err(|e| write_error(e.to_string()))?;
        temporary
            .persist(path)
            .map_err(|e| write_error(e.to_string()))?;
        Ok(())
    }

    fn parse(input: &str, origin: &str) -> Result<Self, ConfigError> {
        let options = serde_saphyr::options! { with_snippet: false };
        let config: Config = serde_saphyr::from_str_with_options(input, options).map_err(|e| {
            ConfigError::Syntax {
                origin: origin.to_owned(),
                message: explain_syntax_error(e.to_string()),
            }
        })?;
        let issues = config.validate();
        if issues.is_empty() {
            Ok(config)
        } else {
            Err(ConfigError::Invalid {
                origin: origin.to_owned(),
                issues,
            })
        }
    }

    /// All validation problems of this configuration.
    pub fn validate(&self) -> Vec<Issue> {
        let mut report = Report::default();
        if let Some(dir) = &self.defaults.output_dir {
            report.not_blank("defaults.output_dir", dir);
        }
        if let Some(dir) = &self.defaults.downloads_dir {
            report.not_blank("defaults.downloads_dir", dir);
        }
        self.mail.validate(&mut report);
        if let Some(llm) = &self.llm {
            llm.validate(&mut report);
        }
        report.unique_ids(
            "companies",
            self.companies.iter().map(|company| company.id.as_str()),
        );
        for (i, company) in self.companies.iter().enumerate() {
            let path = format!("companies[{i}]");
            company.validate(&path, &mut report);
            if company.output.dir.is_none() && self.defaults.output_dir.is_none() {
                report.add(
                    &format!("{path}.output.dir"),
                    "needs a folder, because there is no default folder (`defaults.output_dir`)",
                );
            }
        }
        report.issues
    }
}

impl MailConfig {
    fn validate(&self, report: &mut Report) {
        if self.accounts.is_empty() {
            report.add("mail.accounts", "at least one account is required");
        }
        report.unique_ids(
            "mail.accounts",
            self.accounts.iter().map(|account| account.id.as_str()),
        );
        for (i, account) in self.accounts.iter().enumerate() {
            let path = format!("mail.accounts[{i}]");
            report.id(&format!("{path}.id"), &account.id);
            if account.endpoint().is_none() {
                report.add(
                    &format!("{path}.imap"),
                    "must have the form `host:port`, for example `imap.example.com:993`",
                );
            }
            report.not_blank(&format!("{path}.user"), &account.user);
            report.secret(&format!("{path}.password"), &account.password);
            report.not_blank(&format!("{path}.watch"), &account.watch);
        }

        if self
            .poll_minutes
            .is_some_and(|minutes| minutes < MIN_POLL_MINUTES)
        {
            report.add(
                "mail.poll_minutes",
                &format!("must be at least {MIN_POLL_MINUTES} minutes"),
            );
        }

        let needs_folder = matches!(
            self.disposal.mode,
            DisposalMode::AllMove | DisposalMode::PdfMoveLinkDelete
        );
        match &self.disposal.move_to {
            Some(folder) => report.not_blank("mail.disposal.move_to", folder),
            None if needs_folder => report.add(
                "mail.disposal.move_to",
                "is required for the modes `all_move` and `pdf_move_link_delete`",
            ),
            None => {}
        }
    }
}

impl Company {
    fn validate(&self, path: &str, report: &mut Report) {
        report.id(&format!("{path}.id"), &self.id);
        report.not_blank(&format!("{path}.name"), &self.name);
        if self.mail.is_none() && self.portal.is_none() {
            report.add(path, "needs at least one of `mail` and `portal`");
        }
        if let Some(rule) = &self.mail {
            rule.validate(&format!("{path}.mail"), report);
        }
        if let Some(portal) = &self.portal {
            portal.validate(&format!("{path}.portal"), report);
        }
        if let Some(dir) = &self.output.dir {
            report.not_blank(&format!("{path}.output.dir"), dir);
        }
        if let Err(problem) = FilenameTemplate::parse(&self.output.filename) {
            report.add(&format!("{path}.output.filename"), &problem);
        }
        if let Some(CompanyDisposal::Move(target)) = &self.disposal {
            report.not_blank(&format!("{path}.disposal.move_to"), &target.move_to);
        }
    }
}

impl MailRule {
    fn validate(&self, path: &str, report: &mut Report) {
        if !is_valid_domain(&self.from_domain) {
            report.add(
                &format!("{path}.from_domain"),
                "must be a domain without `@`, for example `example.com`",
            );
        }
        report.regex(&format!("{path}.subject"), self.subject.as_deref());
        report.regex(&format!("{path}.attachment"), self.attachment.as_deref());
    }
}

impl Portal {
    fn validate(&self, path: &str, report: &mut Report) {
        if let Some(url) = &self.url
            && !is_web_address(url)
        {
            report.add(
                &format!("{path}.url"),
                "must be an `http` or `https` address, for example `https://portal.example.com/`",
            );
        }
        if let Some(totp) = &self.totp {
            report.secret(&format!("{path}.totp"), totp);
        }
        match self.mode {
            PortalMode::Recipe => self.validate_recipe_mode(path, report),
            PortalMode::Assisted => self.validate_assisted_mode(path, report),
        }
        if let Some(schedule) = &self.schedule
            && let Err(problem) = Schedule::parse(schedule)
        {
            report.add(&format!("{path}.schedule"), &problem);
        }
    }

    fn validate_recipe_mode(&self, path: &str, report: &mut Report) {
        match &self.credentials {
            Some(credentials) => report.secret(&format!("{path}.credentials"), credentials),
            None => report.add(
                &format!("{path}.credentials"),
                "is required, unless `mode` is `assisted`",
            ),
        }
        match &self.recipe {
            Some(recipe) if recipe.as_os_str().is_empty() => {
                report.add(&format!("{path}.recipe"), "must not be empty");
            }
            Some(_) => {}
            None => report.add(
                &format!("{path}.recipe"),
                "is required, unless `mode` is `assisted`",
            ),
        }
    }

    fn validate_assisted_mode(&self, path: &str, report: &mut Report) {
        if self.url.is_none() {
            report.add(
                &format!("{path}.url"),
                "is required when `mode` is `assisted`",
            );
        }
        const LOGS_IN_BY_HAND: &str = "must be removed when `mode` is `assisted`: you log in yourself, so Docsweep keeps no login";
        if self.credentials.is_some() {
            report.add(&format!("{path}.credentials"), LOGS_IN_BY_HAND);
        }
        if self.totp.is_some() {
            report.add(&format!("{path}.totp"), LOGS_IN_BY_HAND);
        }
        if self.schedule.is_some() {
            report.add(
                &format!("{path}.schedule"),
                "must be removed when `mode` is `assisted`: nothing runs on its own, a link mail shows the invoice to pick up",
            );
        }
    }
}

#[derive(Default)]
pub(crate) struct Report {
    pub(crate) issues: Vec<Issue>,
}

impl Report {
    pub(crate) fn add(&mut self, path: &str, message: &str) {
        self.issues.push(Issue {
            path: path.to_owned(),
            message: message.to_owned(),
        });
    }

    pub(crate) fn not_blank(&mut self, path: &str, value: &str) {
        if value.trim().is_empty() {
            self.add(path, "must not be empty");
        }
    }

    fn id(&mut self, path: &str, id: &str) {
        if !is_valid_id(id) {
            self.add(
                path,
                "must start with a lowercase letter or digit and contain only lowercase letters, digits, `-` and `_`",
            );
        }
    }

    fn unique_ids<'a>(&mut self, path: &str, ids: impl Iterator<Item = &'a str>) {
        let mut seen = HashSet::new();
        for id in ids {
            if !seen.insert(id) {
                self.add(path, &format!("the id `{id}` is used more than once"));
            }
        }
    }

    /// Never quotes the value: a rejected value may be a plaintext secret.
    fn secret(&mut self, path: &str, secret: &SecretRef) {
        if secret.id().is_none() {
            self.add(
                path,
                "must be a reference of the form `keyring:<id>`; secrets must not be written into the configuration",
            );
        }
    }

    fn regex(&mut self, path: &str, pattern: Option<&str>) {
        if let Some(pattern) = pattern
            && let Err(error) = Regex::new(pattern)
        {
            self.add(path, &format!("is not a valid regular expression: {error}"));
        }
    }
}

fn is_valid_id(id: &str) -> bool {
    let mut chars = id.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

fn is_valid_domain(domain: &str) -> bool {
    domain.contains('.')
        && domain.split('.').all(|label| {
            !label.is_empty() && label.chars().all(|c| c.is_alphanumeric() || c == '-')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A complete example of the documented format.
    const CONCEPT_EXAMPLE: &str = r#"
mail:
  accounts:
    - id: rechnungen
      imap: imap.example.de:993
      user: rechnungen@example.de
      password: keyring:mail-rechnungen
      watch: INBOX
  disposal:                          # global default
    mode: pdf_move_link_delete       # all_delete | all_move | pdf_move_link_delete
    move_to: Rechnungen/Erledigt
    delete_to_trash: true

llm:
  provider: anthropic                # anthropic | openai | openrouter
  api_key: keyring:llm-key

companies:
  - id: telekom                      # link mails + portal
    name: Telekom
    mail: { from_domain: telekom.de, subject: "(?i)rechnung" }
    portal:
      credentials: keyring:telekom
      totp: keyring:telekom-totp     # optional
      recipe: recipes/telekom.yaml
      schedule: "0 9 10 * *"
    output:
      dir: ~/Rechnungen/Eingang
      filename: "{date:%Y-%m-%d} - Telekom - {invoice_no}.pdf"
    disposal: delete                 # overrides the default

  - id: hetzner                      # PDF mails only, no portal
    name: Hetzner
    mail: { from_domain: hetzner.com, attachment: "(?i).*\\.pdf" }
    output:
      dir: ~/Rechnungen/Eingang
      filename: "{date:%Y-%m-%d} - Hetzner - {invoice_no}.pdf"
    disposal: { move_to: Rechnungen/Hetzner }
"#;

    const MINIMAL_MAIL: &str = r#"
mail:
  accounts:
    - { id: a, imap: "imap.example.com:993", user: u, password: "keyring:p", watch: INBOX }
  disposal: { mode: all_delete }
"#;

    fn issues_of(yaml: &str) -> Vec<Issue> {
        match Config::from_yaml(yaml) {
            Err(ConfigError::Invalid { issues, .. }) => issues,
            other => panic!("expected validation issues, got {other:?}"),
        }
    }

    fn company_yaml(body: &str) -> String {
        format!("{MINIMAL_MAIL}\ncompanies:\n  - {body}\n")
    }

    const VALID_OUTPUT: &str =
        r#"output: { dir: /tmp/x, filename: "{date:%Y-%m-%d} - {invoice_no}.pdf" }"#;

    #[test]
    fn parses_the_concept_example_exactly() {
        let config = Config::from_yaml(CONCEPT_EXAMPLE).unwrap();

        let account = &config.mail.accounts[0];
        assert_eq!(account.endpoint(), Some(("imap.example.de", 993)));
        assert_eq!(account.password.id(), Some("mail-rechnungen"));
        assert_eq!(config.mail.disposal.mode, DisposalMode::PdfMoveLinkDelete);
        assert!(config.mail.disposal.delete_to_trash);
        assert_eq!(config.llm.unwrap().provider, LlmProvider::Anthropic);

        let telekom = &config.companies[0];
        assert_eq!(
            telekom.disposal,
            Some(CompanyDisposal::Delete(DeleteKeyword::Delete))
        );
        let portal = telekom.portal.as_ref().unwrap();
        assert_eq!(portal.totp.as_ref().unwrap().id(), Some("telekom-totp"));
        assert_eq!(portal.schedule.as_deref(), Some("0 9 10 * *"));

        let hetzner = &config.companies[1];
        assert!(hetzner.portal.is_none());
        assert_eq!(
            hetzner.disposal,
            Some(CompanyDisposal::Move(MoveTarget {
                move_to: "Rechnungen/Hetzner".into()
            }))
        );
    }

    #[test]
    fn a_model_is_optional_for_anthropic_and_required_otherwise() {
        let with = |llm: &str| Config::from_yaml(&format!("{MINIMAL_MAIL}\nllm: {llm}\n"));
        let parsed = with(r#"{ provider: anthropic, api_key: "keyring:k" }"#).unwrap();
        assert_eq!(parsed.llm.unwrap().model, None);
        let parsed =
            with(r#"{ provider: openai, api_key: "keyring:k", model: my-model }"#).unwrap();
        assert_eq!(parsed.llm.unwrap().model.as_deref(), Some("my-model"));
        for provider in ["openai", "openrouter"] {
            let error = with(&format!(
                r#"{{ provider: {provider}, api_key: "keyring:k" }}"#
            ))
            .unwrap_err()
            .to_string();
            assert!(error.contains("llm.model"), "{error}");
        }
        let blank = with(r#"{ provider: anthropic, api_key: "keyring:k", model: " " }"#);
        assert!(blank.unwrap_err().to_string().contains("llm.model"));
    }

    #[test]
    fn llm_and_companies_are_optional_and_trash_is_the_default() {
        let config = Config::from_yaml(MINIMAL_MAIL).unwrap();
        assert!(config.llm.is_none());
        assert!(config.companies.is_empty());
        assert!(config.mail.disposal.delete_to_trash);
    }

    #[test]
    fn plaintext_secrets_are_rejected_without_being_quoted() {
        let yaml = MINIMAL_MAIL.replace("keyring:p", "hunter2-secret");
        let error = Config::from_yaml(&yaml).unwrap_err().to_string();
        assert!(error.contains("mail.accounts[0].password"), "{error}");
        assert!(error.contains("keyring:<id>"), "{error}");
        assert!(!error.contains("hunter2-secret"), "{error}");
    }

    #[test]
    fn syntax_errors_do_not_quote_neighbouring_secrets() {
        let yaml = "mail:\n  accounts:\n    - password: hunter2-secret\n      imap: [\n";
        let error = Config::from_yaml(yaml).unwrap_err().to_string();
        assert!(!error.contains("hunter2-secret"), "{error}");
    }

    #[test]
    fn invalid_secret_ref_debug_output_is_redacted() {
        let secret = SecretRef("hunter2-secret".into());
        assert!(!format!("{secret:?}").contains("hunter2"));
    }

    #[test]
    fn unknown_fields_are_reported_with_their_name() {
        let yaml = MINIMAL_MAIL.replace("mode: all_delete", "mode: all_delete, moveto: x");
        let error = Config::from_yaml(&yaml).unwrap_err().to_string();
        assert!(error.contains("moveto"), "{error}");
    }

    #[test]
    fn unknown_disposal_mode_names_the_problem() {
        let yaml = MINIMAL_MAIL.replace("all_delete", "everything");
        let error = Config::from_yaml(&yaml).unwrap_err().to_string();
        assert!(error.contains("everything"), "{error}");
    }

    #[test]
    fn move_modes_require_a_folder() {
        for mode in ["all_move", "pdf_move_link_delete"] {
            let issues = issues_of(&MINIMAL_MAIL.replace("all_delete", mode));
            assert_eq!(issues[0].path, "mail.disposal.move_to");
        }
    }

    #[test]
    fn all_problems_are_reported_together() {
        let yaml = company_yaml(
            r#"{ id: "Bad Id", name: "", mail: { from_domain: "a@b", subject: "(" },
                 output: { dir: "", filename: "{nope}.pdf" } }"#,
        );
        let paths: Vec<String> = issues_of(&yaml).into_iter().map(|i| i.path).collect();
        assert_eq!(
            paths,
            [
                "companies[0].id",
                "companies[0].name",
                "companies[0].mail.from_domain",
                "companies[0].mail.subject",
                "companies[0].output.dir",
                "companies[0].output.filename",
            ]
        );
    }

    #[test]
    fn company_needs_mail_or_portal() {
        let issues = issues_of(&company_yaml(&format!(
            "{{ id: x, name: X, {VALID_OUTPUT} }}"
        )));
        assert!(issues[0].message.contains("`mail` and `portal`"));
    }

    #[test]
    fn duplicate_ids_are_rejected() {
        let company =
            format!("{{ id: x, name: X, mail: {{ from_domain: x.de }}, {VALID_OUTPUT} }}");
        let yaml = format!("{MINIMAL_MAIL}\ncompanies:\n  - {company}\n  - {company}\n");
        assert!(issues_of(&yaml)[0].message.contains("`x`"));
    }

    #[test]
    fn bad_endpoint_and_schedule_are_rejected() {
        let issues = issues_of(&MINIMAL_MAIL.replace("imap.example.com:993", "imap.example.com"));
        assert_eq!(issues[0].path, "mail.accounts[0].imap");

        let yaml = company_yaml(&format!(
            r#"{{ id: x, name: X, {VALID_OUTPUT}, portal: {{ credentials: "keyring:c", recipe: r.yaml, schedule: "nope" }} }}"#
        ));
        assert_eq!(issues_of(&yaml)[0].path, "companies[0].portal.schedule");
    }

    #[test]
    fn a_schedule_with_a_wrong_value_is_rejected_with_a_clear_message() {
        let yaml = company_yaml(&format!(
            r#"{{ id: x, name: X, {VALID_OUTPUT}, portal: {{ credentials: "keyring:c", recipe: r.yaml, schedule: "99 9 10 * *" }} }}"#
        ));
        let issue = issues_of(&yaml).remove(0);
        assert_eq!(issue.path, "companies[0].portal.schedule");
        assert!(issue.message.contains("five fields"), "{}", issue.message);
    }

    #[test]
    fn all_automation_stays_off_by_default() {
        let yaml = company_yaml(&format!(
            r#"{{ id: x, name: X, {VALID_OUTPUT}, portal: {{ credentials: "keyring:c", recipe: r.yaml, schedule: "0 9 10 * *" }} }}"#
        ));
        let config = Config::from_yaml(&yaml).unwrap();
        assert!(!config.automation.enabled);
        assert_eq!(config.mail.poll_minutes, None);

        let written = serde_saphyr::to_string(&config).unwrap();
        assert!(!written.contains("automation"), "{written}");
        assert!(!written.contains("poll_minutes"), "{written}");
    }

    #[test]
    fn automation_settings_survive_saving() {
        let yaml = format!("automation: {{ enabled: true }}\n{MINIMAL_MAIL}").replace(
            "mode: all_delete }",
            "mode: all_delete }\n  poll_minutes: 30",
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("docsweep.yaml");
        Config::from_yaml(&yaml).unwrap().save(&path).unwrap();

        let reloaded = Config::load(&path).unwrap();
        assert!(reloaded.automation.enabled);
        assert_eq!(reloaded.mail.poll_minutes, Some(30));
    }

    #[test]
    fn polling_too_often_is_rejected() {
        let yaml = MINIMAL_MAIL.replace(
            "mode: all_delete }",
            "mode: all_delete }\n  poll_minutes: 1",
        );
        assert_eq!(issues_of(&yaml)[0].path, "mail.poll_minutes");
    }

    #[test]
    fn bare_move_keyword_explains_the_expected_form() {
        let yaml = company_yaml(&format!(
            "{{ id: x, name: X, mail: {{ from_domain: x.de }}, {VALID_OUTPUT}, disposal: move }}"
        ));
        let error = Config::from_yaml(&yaml).unwrap_err().to_string();
        assert!(error.contains("move_to"), "{error}");
    }

    #[test]
    fn windows_paths_work_in_single_quotes_and_get_advice_in_double_quotes() {
        let company = |dir: &str| {
            company_yaml(&format!(
                r#"{{ id: x, name: X, mail: {{ from_domain: x.de }},
                     output: {{ dir: {dir}, filename: "{{invoice_no}}.pdf" }} }}"#
            ))
        };
        let config = Config::from_yaml(&company(r"'C:\Users\me\Invoices'")).unwrap();
        assert_eq!(
            config.companies[0].output.dir.as_deref(),
            Some(r"C:\Users\me\Invoices")
        );

        let error = Config::from_yaml(&company(r#""C:\Users\me""#))
            .unwrap_err()
            .to_string();
        assert!(error.contains("single quotes"), "{error}");
    }

    #[test]
    fn saving_round_trips_every_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("docsweep.yaml");
        let mut config = Config::from_yaml(CONCEPT_EXAMPLE).unwrap();
        config.companies[1].output.dir = Some(r"C:\Users\me\Rechnungen".to_owned());

        config.save(&path).unwrap();
        let reloaded = Config::load(&path).unwrap();

        assert_eq!(
            serde_saphyr::to_string(&reloaded).unwrap(),
            serde_saphyr::to_string(&config).unwrap()
        );
        let hetzner = &reloaded.companies[1];
        assert_eq!(
            hetzner.output.dir.as_deref(),
            Some(r"C:\Users\me\Rechnungen")
        );
        assert_eq!(
            hetzner.mail.as_ref().unwrap().attachment.as_deref(),
            Some("(?i).*\\.pdf")
        );
        assert_eq!(
            reloaded.companies[0].disposal,
            Some(CompanyDisposal::Delete(DeleteKeyword::Delete))
        );
        let written = fs::read_to_string(&path).unwrap();
        assert!(written.contains("keyring:mail-rechnungen"), "{written}");
    }

    #[test]
    fn saving_refuses_invalid_configurations_and_keeps_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("docsweep.yaml");
        let mut config = Config::from_yaml(MINIMAL_MAIL).unwrap();
        config.save(&path).unwrap();
        let before = fs::read_to_string(&path).unwrap();

        config.mail.accounts[0].password = SecretRef("plaintext".into());
        assert!(matches!(
            config.save(&path),
            Err(ConfigError::Invalid { .. })
        ));
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn saving_keeps_the_original_file_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("docsweep.yaml");
        fs::write(&path, format!("# my comment\n{MINIMAL_MAIL}")).unwrap();
        let config = Config::load(&path).unwrap();

        config.save(&path).unwrap();
        config.save(&path).unwrap();

        let backup = fs::read_to_string(dir.path().join("docsweep.yaml.bak")).unwrap();
        assert!(backup.contains("# my comment"));
    }

    #[test]
    fn secret_refs_can_be_built_from_an_id() {
        assert_eq!(SecretRef::from_id("mail").id(), Some("mail"));
    }

    fn company_without_folder() -> String {
        company_yaml(
            r#"{ id: x, name: X, mail: { from_domain: x.de }, output: { filename: "{invoice_no}.pdf" } }"#,
        )
    }

    #[test]
    fn a_company_without_folder_uses_the_default_folder() {
        let yaml = format!(
            "defaults: {{ output_dir: /all/invoices }}\n{}",
            company_without_folder()
        );
        let config = Config::from_yaml(&yaml).unwrap();
        assert_eq!(
            config.output_dir_of(&config.companies[0]),
            Path::new("/all/invoices")
        );
    }

    #[test]
    fn a_company_folder_wins_over_the_default_folder() {
        let yaml = format!(
            "defaults: {{ output_dir: /all/invoices }}\n{}",
            company_yaml(&format!(
                "{{ id: x, name: X, mail: {{ from_domain: x.de }}, {VALID_OUTPUT} }}"
            ))
        );
        let config = Config::from_yaml(&yaml).unwrap();
        assert_eq!(
            config.output_dir_of(&config.companies[0]),
            Path::new("/tmp/x")
        );
    }

    #[test]
    fn without_any_folder_the_company_is_invalid() {
        let issues = issues_of(&company_without_folder());
        assert_eq!(issues[0].path, "companies[0].output.dir");
        assert!(issues[0].message.contains("defaults.output_dir"));
    }

    #[test]
    fn the_default_folder_survives_saving_and_is_left_out_when_unset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("docsweep.yaml");
        let plain = Config::from_yaml(MINIMAL_MAIL).unwrap();
        plain.save(&path).unwrap();
        assert!(!fs::read_to_string(&path).unwrap().contains("defaults"));

        let yaml = format!(
            "defaults: {{ output_dir: \"~/Invoices\" }}\n{}",
            company_without_folder()
        );
        Config::from_yaml(&yaml).unwrap().save(&path).unwrap();
        let reloaded = Config::load(&path).unwrap();
        assert_eq!(reloaded.defaults.output_dir.as_deref(), Some("~/Invoices"));
        assert_eq!(reloaded.companies[0].output.dir, None);
    }

    const ASSISTED: &str = r#"portal: { mode: assisted, url: "https://portal.example.com/login" }"#;

    #[test]
    fn recipe_is_the_default_mode_and_existing_configurations_load_unchanged() {
        let config = Config::from_yaml(CONCEPT_EXAMPLE).unwrap();
        let telekom = &config.companies[0];
        let portal = telekom.portal.as_ref().unwrap();
        assert_eq!(portal.mode, PortalMode::Recipe);
        assert!(!telekom.is_assisted());
        assert_eq!(portal.credentials.as_ref().unwrap().id(), Some("telekom"));
        assert_eq!(
            portal.recipe.as_deref(),
            Some(Path::new("recipes/telekom.yaml"))
        );
        assert_eq!(config.defaults, Defaults::default());

        let written = serde_saphyr::to_string(&config).unwrap();
        for new_field in ["mode: recipe", "url:", "downloads_dir"] {
            assert!(!written.contains(new_field), "{written}");
        }
    }

    #[test]
    fn recipe_mode_still_requires_credentials_and_a_recipe() {
        let yaml = company_yaml(&format!(
            "{{ id: x, name: X, {VALID_OUTPUT}, portal: {{ schedule: \"0 9 10 * *\" }} }}"
        ));
        let paths: Vec<String> = issues_of(&yaml).into_iter().map(|i| i.path).collect();
        assert_eq!(
            paths,
            [
                "companies[0].portal.credentials",
                "companies[0].portal.recipe"
            ]
        );
    }

    #[test]
    fn an_assisted_company_needs_only_a_web_address_and_keeps_it_through_saving() {
        let yaml = company_yaml(&format!("{{ id: x, name: X, {VALID_OUTPUT}, {ASSISTED} }}"));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("docsweep.yaml");
        Config::from_yaml(&yaml).unwrap().save(&path).unwrap();

        let reloaded = Config::load(&path).unwrap();
        let company = &reloaded.companies[0];
        assert!(company.is_assisted());
        let portal = company.portal.as_ref().unwrap();
        assert_eq!(
            portal.url.as_deref(),
            Some("https://portal.example.com/login")
        );
        assert!(portal.credentials.is_none() && portal.recipe.is_none());
        let written = fs::read_to_string(&path).unwrap();
        assert!(!written.contains("keyring:x"), "{written}");
    }

    #[test]
    fn an_assisted_company_without_an_address_is_rejected() {
        let yaml = company_yaml(&format!(
            "{{ id: x, name: X, {VALID_OUTPUT}, portal: {{ mode: assisted }} }}"
        ));
        let issue = issues_of(&yaml).remove(0);
        assert_eq!(issue.path, "companies[0].portal.url");
        assert!(issue.message.contains("required"), "{}", issue.message);
    }

    #[test]
    fn only_web_addresses_are_accepted_and_a_bad_one_is_not_quoted() {
        for url in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "ftp://files.example.net/",
            "mailto:me@example.net",
            "files.example.net",
            "",
        ] {
            let yaml = company_yaml(&format!(
                r#"{{ id: x, name: X, {VALID_OUTPUT}, portal: {{ mode: assisted, url: "{url}" }} }}"#
            ));
            let issue = issues_of(&yaml).remove(0);
            assert_eq!(issue.path, "companies[0].portal.url", "{url}");
            assert!(issue.message.contains("`http`"), "{}", issue.message);
            assert!(url.is_empty() || !issue.message.contains(url));
        }
        let plain_http = company_yaml(&format!(
            r#"{{ id: x, name: X, {VALID_OUTPUT}, portal: {{ mode: assisted, url: "http://127.0.0.1:8000/" }} }}"#
        ));
        assert!(Config::from_yaml(&plain_http).is_ok());
    }

    #[test]
    fn an_assisted_company_refuses_a_schedule_with_a_clear_message() {
        let yaml = company_yaml(&format!(
            r#"{{ id: x, name: X, {VALID_OUTPUT}, portal: {{ mode: assisted, url: "https://p.example/", schedule: "0 9 10 * *" }} }}"#
        ));
        let issue = issues_of(&yaml).remove(0);
        assert_eq!(issue.path, "companies[0].portal.schedule");
        assert!(issue.message.contains("assisted"), "{}", issue.message);
    }

    #[test]
    fn an_assisted_company_keeps_no_login() {
        let yaml = company_yaml(&format!(
            r#"{{ id: x, name: X, {VALID_OUTPUT}, portal: {{ mode: assisted, url: "https://p.example/", credentials: "keyring:c", totp: "keyring:t" }} }}"#
        ));
        let paths: Vec<String> = issues_of(&yaml).into_iter().map(|i| i.path).collect();
        assert_eq!(
            paths,
            [
                "companies[0].portal.credentials",
                "companies[0].portal.totp"
            ]
        );
    }

    #[test]
    fn an_unknown_mode_names_the_problem() {
        let yaml = company_yaml(&format!(
            "{{ id: x, name: X, {VALID_OUTPUT}, portal: {{ mode: stealth }} }}"
        ));
        let error = Config::from_yaml(&yaml).unwrap_err().to_string();
        assert!(error.contains("stealth"), "{error}");
    }

    #[test]
    fn the_downloads_folder_can_be_set_and_expands_home() {
        let yaml = format!("defaults: {{ downloads_dir: /my/downloads }}\n{MINIMAL_MAIL}");
        let config = Config::from_yaml(&yaml).unwrap();
        assert_eq!(config.downloads_dir().unwrap(), Path::new("/my/downloads"));
        let blank = format!("defaults: {{ downloads_dir: \" \" }}\n{MINIMAL_MAIL}");
        assert_eq!(issues_of(&blank)[0].path, "defaults.downloads_dir");
        // Without the setting, whatever the system says; a test machine may say nothing.
        let unset = Config::from_yaml(MINIMAL_MAIL).unwrap();
        assert_eq!(unset.downloads_dir(), system_downloads_dir());
    }

    #[test]
    fn missing_file_names_the_path() {
        let error = Config::load(Path::new("/does/not/exist.yaml"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("/does/not/exist.yaml"), "{error}");
    }

    #[test]
    fn home_is_expanded_only_at_the_start() {
        let home = Path::new("/h");
        assert_eq!(expand_home("~/a", Some(home)), Path::new("/h/a"));
        assert_eq!(expand_home("~", Some(home)), Path::new("/h"));
        assert_eq!(expand_home("/x/~/a", Some(home)), Path::new("/x/~/a"));
        assert_eq!(expand_home("~other", Some(home)), Path::new("~other"));
    }
}
