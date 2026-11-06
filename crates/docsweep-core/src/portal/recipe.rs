//! Portal recipes: steps and selectors in a YAML file.
//!
//! Loading reports every problem at once with its path, like the configuration.
//! Recipes are meant to be shared, so a recipe holds no secrets of any kind.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use url::Url;

use crate::config::{Issue, Report, format_issues};

const DEFAULT_STEP_TIMEOUT_SECS: u64 = 30;
const DEFAULT_CONFIRMATION_TIMEOUT_SECS: u64 = 300;
const DEFAULT_MAX_PAGES: u32 = 50;

/// A complete recipe.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    /// Values for `{name}` placeholders in `goto`, such as the domain of a
    /// multi-domain portal. A company can override them with `portal.params`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, String>,
    /// When a person has to help, and how long the run waits for them.
    #[serde(default, skip_serializing_if = "Confirmation::is_default")]
    pub confirmation: Confirmation,
    /// How long a step waits for the page before asking a person.
    #[serde(
        default = "default_step_timeout",
        skip_serializing_if = "is_default_step_timeout"
    )]
    pub step_timeout_secs: u64,
    pub steps: Vec<Step>,
    /// Whether a language model may repair a step that no longer matches the
    /// portal. Nothing is sent anywhere unless this says so.
    #[serde(default, skip_serializing_if = "AiFallback::is_off")]
    pub ai_fallback: AiFallback,
}

fn default_step_timeout() -> u64 {
    DEFAULT_STEP_TIMEOUT_SECS
}

fn is_default_step_timeout(secs: &u64) -> bool {
    *secs == DEFAULT_STEP_TIMEOUT_SECS
}

/// The `confirmation` section.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Confirmation {
    /// Selector that shows a captcha or a mail or push confirmation. It is
    /// checked after every step and while waiting for the page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detect: Option<String>,
    #[serde(default = "default_confirmation_timeout")]
    pub timeout_secs: u64,
}

impl Confirmation {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

impl Default for Confirmation {
    fn default() -> Self {
        Self {
            detect: None,
            timeout_secs: DEFAULT_CONFIRMATION_TIMEOUT_SECS,
        }
    }
}

fn default_confirmation_timeout() -> u64 {
    DEFAULT_CONFIRMATION_TIMEOUT_SECS
}

/// The AI modes of a recipe. `full_agent` from the concept can be written down,
/// but validation refuses it: it is not available.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AiFallback {
    #[default]
    Off,
    RepairStep,
    FullAgent,
}

impl AiFallback {
    fn is_off(&self) -> bool {
        *self == Self::Off
    }
}

/// One step of a recipe.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Step {
    Goto(String),
    DismissCookieBanner(CookieBanner),
    Login(LoginStep),
    ExtractList(ExtractList),
    DownloadNew(DownloadNew),
}

impl Step {
    /// The name of this kind of step, as in a recipe file.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Goto(_) => "goto",
            Self::DismissCookieBanner(_) => "dismiss_cookie_banner",
            Self::Login(_) => "login",
            Self::ExtractList(_) => "extract_list",
            Self::DownloadNew(_) => "download_new",
        }
    }
}

/// `true` tries the usual banner buttons; a selector names the button.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged, expecting = "`true`, `false` or a selector")]
pub enum CookieBanner {
    Enabled(bool),
    Selector(String),
}

/// The `login` step. The app fills these fields itself.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LoginStep {
    pub user: String,
    pub password: String,
    pub submit: String,
    /// Field for the TOTP code, on the same or a following page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub totp: Option<String>,
    /// Button for the TOTP code; without it, `submit` is used again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub totp_submit: Option<String>,
    /// Button between user name and password, if they are on separate pages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
    /// Shows an active session, in which case the login is skipped. Profiles
    /// keep sessions, so recipes should name this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logged_in: Option<String>,
}

/// The `extract_list` step. Field values are `selector`, for its text, or
/// `selector@attribute`; `@attribute` alone reads the row itself.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtractList {
    pub rows: String,
    pub fields: BTreeMap<String, String>,
    /// Shows an empty list, so that it is not taken for a page that never loaded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub empty: Option<String>,
    /// Link or button to the next page of the list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_page: Option<String>,
    #[serde(
        default = "default_max_pages",
        skip_serializing_if = "is_default_max_pages"
    )]
    pub max_pages: u32,
    /// Each row leads to a page with its documents, as with orders.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_detail: Option<OpenDetail>,
}

fn default_max_pages() -> u32 {
    DEFAULT_MAX_PAGES
}

fn is_default_max_pages(pages: &u32) -> bool {
    *pages == DEFAULT_MAX_PAGES
}

/// A detail page with any number of documents per list row. A document inherits
/// the fields of its row. No document at all counts as "missing".
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OpenDetail {
    /// The field of the list row that holds the link.
    pub url: String,
    pub rows: String,
    pub fields: BTreeMap<String, String>,
    /// Shows that this page has no documents, which saves waiting for them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missing: Option<String>,
}

/// The `download_new` step.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadNew {
    /// The field that identifies a document across runs.
    pub key: String,
    /// Print documents that are HTML pages rather than PDFs.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub html_to_pdf: bool,
}

/// A parsed field value of `extract_list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldSelector {
    /// Empty means the row itself.
    pub selector: String,
    /// `None` reads the text.
    pub attribute: Option<String>,
}

impl FieldSelector {
    /// Splits off a trailing `@attribute`. An `@` inside a selector, as in
    /// `a[href*="@"]`, is not followed by an attribute name at the very end.
    pub fn parse(field: &str) -> Self {
        if let Some((selector, attribute)) = field.rsplit_once('@')
            && is_attribute_name(attribute)
        {
            return Self {
                selector: selector.trim().to_owned(),
                attribute: Some(attribute.to_owned()),
            };
        }
        Self {
            selector: field.trim().to_owned(),
            attribute: None,
        }
    }

    /// Parses a map of fields.
    pub fn parse_all(fields: &BTreeMap<String, String>) -> BTreeMap<String, Self> {
        fields
            .iter()
            .map(|(name, value)| (name.clone(), Self::parse(value)))
            .collect()
    }
}

fn is_attribute_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':'))
}

/// Why a recipe could not be used.
#[derive(Debug, thiserror::Error)]
pub enum RecipeError {
    #[error("cannot read recipe {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{origin} has a syntax or structure error: {message}")]
    Syntax { origin: String, message: String },
    #[error("{origin} has {} problem(s):{}", issues.len(), format_issues(issues))]
    Invalid { origin: String, issues: Vec<Issue> },
}

impl Recipe {
    /// Loads and validates a recipe file.
    pub fn load(path: &Path) -> Result<Self, RecipeError> {
        let input = fs::read_to_string(path).map_err(|source| RecipeError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&input, &path.display().to_string())
    }

    /// Parses and validates YAML text.
    pub fn from_yaml(input: &str) -> Result<Self, RecipeError> {
        Self::parse(input, "the recipe")
    }

    /// This recipe as YAML. Comments of a loaded file are not kept.
    pub fn to_yaml(&self) -> Result<String, RecipeError> {
        serde_saphyr::to_string(self).map_err(|e| RecipeError::Syntax {
            origin: "the recipe".to_owned(),
            message: e.to_string(),
        })
    }

    fn parse(input: &str, origin: &str) -> Result<Self, RecipeError> {
        let options = serde_saphyr::options! { with_snippet: false };
        let recipe: Recipe = serde_saphyr::from_str_with_options(input, options).map_err(|e| {
            RecipeError::Syntax {
                origin: origin.to_owned(),
                message: e.to_string(),
            }
        })?;
        let issues = recipe.validate();
        if issues.is_empty() {
            Ok(recipe)
        } else {
            Err(RecipeError::Invalid {
                origin: origin.to_owned(),
                issues,
            })
        }
    }

    /// Whether a step needs the company's credentials.
    pub fn login(&self) -> Option<&LoginStep> {
        self.steps.iter().find_map(|step| match step {
            Step::Login(login) => Some(login),
            _ => None,
        })
    }

    /// This recipe with the company's parameters filled into its `goto` steps.
    pub fn with_params(&self, overrides: &BTreeMap<String, String>) -> Result<Self, RecipeError> {
        let mut report = Report::default();
        for name in overrides.keys() {
            if !self.params.contains_key(name) {
                report.add(
                    &format!("portal.params.{name}"),
                    "is not a parameter of this recipe",
                );
            }
        }
        let mut params = self.params.clone();
        params.extend(overrides.clone());
        let mut resolved = self.clone();
        for (i, step) in resolved.steps.iter_mut().enumerate() {
            if let Step::Goto(url) = step {
                let filled = fill_placeholders(url, &params);
                check_url(&format!("steps[{i}].goto"), &filled, &mut report);
                *url = filled;
            }
        }
        if report.issues.is_empty() {
            Ok(resolved)
        } else {
            Err(RecipeError::Invalid {
                origin: "the recipe with these parameters".to_owned(),
                issues: report.issues,
            })
        }
    }

    /// One line per step, for `docsweep test-recipe`.
    pub fn outline(&self) -> Vec<String> {
        self.steps
            .iter()
            .map(|step| match step {
                Step::Goto(url) => format!("goto {url}"),
                Step::ExtractList(list) => format!("extract_list {}", list.rows),
                Step::DownloadNew(download) => format!("download_new by {}", download.key),
                other => other.name().to_owned(),
            })
            .collect()
    }

    /// All validation problems of this recipe.
    pub fn validate(&self) -> Vec<Issue> {
        let mut report = Report::default();
        if self.steps.is_empty() {
            report.add("steps", "at least one step is required");
        }
        if self.ai_fallback == AiFallback::FullAgent {
            report.add(
                "ai_fallback",
                "`full_agent` is not available; use `off` or `repair_step`",
            );
        }
        if self.step_timeout_secs == 0 {
            report.add("step_timeout_secs", "must be at least 1");
        }
        if self.confirmation.timeout_secs == 0 {
            report.add("confirmation.timeout_secs", "must be at least 1");
        }
        if let Some(detect) = &self.confirmation.detect {
            report.not_blank("confirmation.detect", detect);
        }
        if self
            .steps
            .iter()
            .filter(|s| matches!(s, Step::Login(_)))
            .count()
            > 1
        {
            report.add("steps", "only one `login` step is possible");
        }

        // The fields of the list that a `download_new` step would work on.
        let mut pending: Option<HashSet<&str>> = None;
        for (i, step) in self.steps.iter().enumerate() {
            let path = format!("steps[{i}].{}", step.name());
            match step {
                Step::Goto(url) => {
                    let filled = fill_placeholders(url, &self.params);
                    check_url(&path, &filled, &mut report);
                }
                Step::DismissCookieBanner(CookieBanner::Selector(selector)) => {
                    report.not_blank(&path, selector);
                }
                Step::DismissCookieBanner(CookieBanner::Enabled(_)) => {}
                Step::Login(login) => login.validate(&path, &mut report),
                Step::ExtractList(list) => {
                    list.validate(&path, &mut report);
                    if pending.is_some() {
                        report.add(&path, "the previous list has no `download_new` step");
                    }
                    pending = Some(list.field_names());
                }
                Step::DownloadNew(download) => match pending.take() {
                    None => report.add(&path, "needs an `extract_list` step before it"),
                    Some(fields) if !fields.contains(download.key.as_str()) => report.add(
                        &format!("{path}.key"),
                        &format!("`{}` is not a field of the list", download.key),
                    ),
                    Some(_) => {}
                },
            }
        }
        if pending.is_some() {
            report.add("steps", "the last list has no `download_new` step");
        }
        report.issues
    }
}

impl LoginStep {
    fn validate(&self, path: &str, report: &mut Report) {
        report.not_blank(&format!("{path}.user"), &self.user);
        report.not_blank(&format!("{path}.password"), &self.password);
        report.not_blank(&format!("{path}.submit"), &self.submit);
        for (name, value) in [
            ("totp", &self.totp),
            ("totp_submit", &self.totp_submit),
            ("next", &self.next),
            ("logged_in", &self.logged_in),
        ] {
            if let Some(value) = value {
                report.not_blank(&format!("{path}.{name}"), value);
            }
        }
        if self.totp_submit.is_some() && self.totp.is_none() {
            report.add(&format!("{path}.totp_submit"), "needs `totp` as well");
        }
    }
}

impl ExtractList {
    fn validate(&self, path: &str, report: &mut Report) {
        report.not_blank(&format!("{path}.rows"), &self.rows);
        validate_fields(&format!("{path}.fields"), &self.fields, report);
        for (name, value) in [("empty", &self.empty), ("next_page", &self.next_page)] {
            if let Some(value) = value {
                report.not_blank(&format!("{path}.{name}"), value);
            }
        }
        if self.max_pages == 0 {
            report.add(&format!("{path}.max_pages"), "must be at least 1");
        }
        if let Some(detail) = &self.open_detail {
            let detail_path = format!("{path}.open_detail");
            if !self.fields.contains_key(&detail.url) {
                report.add(
                    &format!("{detail_path}.url"),
                    &format!("`{}` is not a field of the list", detail.url),
                );
            }
            report.not_blank(&format!("{detail_path}.rows"), &detail.rows);
            validate_fields(&format!("{detail_path}.fields"), &detail.fields, report);
            if let Some(missing) = &detail.missing {
                report.not_blank(&format!("{detail_path}.missing"), missing);
            }
            if !detail.fields.contains_key("url") {
                report.add(&format!("{detail_path}.fields"), "needs a field `url`");
            }
        } else if !self.fields.contains_key("url") {
            report.add(&format!("{path}.fields"), "needs a field `url`");
        }
    }

    fn field_names(&self) -> HashSet<&str> {
        let detail = self
            .open_detail
            .iter()
            .flat_map(|detail| detail.fields.keys());
        self.fields
            .keys()
            .chain(detail)
            .map(String::as_str)
            .collect()
    }
}

fn validate_fields(path: &str, fields: &BTreeMap<String, String>, report: &mut Report) {
    if fields.is_empty() {
        report.add(path, "at least one field is required");
    }
    for (name, value) in fields {
        let parsed = FieldSelector::parse(value);
        if parsed.selector.is_empty() && parsed.attribute.is_none() {
            report.add(&format!("{path}.{name}"), "must not be empty");
        }
    }
}

/// Replaces every `{name}`. An unknown placeholder stays and fails the URL check.
fn fill_placeholders(url: &str, params: &BTreeMap<String, String>) -> String {
    let mut filled = url.to_owned();
    for (name, value) in params {
        filled = filled.replace(&format!("{{{name}}}"), value);
    }
    filled
}

fn check_url(path: &str, url: &str, report: &mut Report) {
    if url.contains(['{', '}']) {
        report.add(
            path,
            "has a placeholder without a value; add it to `params` in the recipe",
        );
        return;
    }
    match Url::parse(url) {
        Ok(parsed) if matches!(parsed.scheme(), "http" | "https") => {}
        _ => report.add(path, "must be an `http` or `https` address"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A complete example of the documented format.
    const CONCEPT_EXAMPLE: &str = r##"
steps:
  - goto: https://portal.example.de/login
  - dismiss_cookie_banner: true
  - login:
      user: "#username"
      password: "#password"
      submit: "button[type=submit]"
      totp: "#otp"
  - goto: https://portal.example.de/rechnungen
  - extract_list:
      rows: "table.invoices tbody tr"
      fields:
        date: "td:nth-child(1)"
        invoice_no: "td:nth-child(2)"
        url: "a.download@href"
  - download_new:
      key: invoice_no
      html_to_pdf: true
ai_fallback: repair_step             # off | repair_step
"##;

    fn issues_of(yaml: &str) -> Vec<Issue> {
        match Recipe::from_yaml(yaml) {
            Err(RecipeError::Invalid { issues, .. }) => issues,
            other => panic!("expected validation issues, got {other:?}"),
        }
    }

    fn paths_of(yaml: &str) -> Vec<String> {
        issues_of(yaml)
            .into_iter()
            .map(|issue| issue.path)
            .collect()
    }

    #[test]
    fn parses_the_concept_example_exactly() {
        let recipe = Recipe::from_yaml(CONCEPT_EXAMPLE).unwrap();
        assert_eq!(recipe.steps.len(), 6);
        assert_eq!(recipe.ai_fallback, AiFallback::RepairStep);
        assert_eq!(recipe.confirmation, Confirmation::default());
        assert_eq!(
            recipe.steps[1],
            Step::DismissCookieBanner(CookieBanner::Enabled(true))
        );
        assert_eq!(recipe.login().unwrap().totp.as_deref(), Some("#otp"));
        let Step::ExtractList(list) = &recipe.steps[4] else {
            panic!("expected a list");
        };
        assert_eq!(list.fields["url"], "a.download@href");
        assert_eq!(list.max_pages, DEFAULT_MAX_PAGES);
        assert_eq!(
            recipe.steps[5],
            Step::DownloadNew(DownloadNew {
                key: "invoice_no".into(),
                html_to_pdf: true
            })
        );
    }

    #[test]
    fn parses_multi_step_portals() {
        let recipe = Recipe::from_yaml(
            r##"
params: { domain: shop.example }
confirmation: { detect: "#captcha", timeout_secs: 60 }
step_timeout_secs: 5
steps:
  - goto: https://{domain}/orders
  - dismiss_cookie_banner: "#accept"
  - extract_list:
      rows: .order
      empty: .no-orders
      next_page: a.next
      max_pages: 3
      fields:
        order_no: .number
        detail: a.details@href
      open_detail:
        url: detail
        rows: li.document
        missing: .no-documents
        fields:
          invoice_no: .number
          url: a@href
  - download_new: { key: invoice_no }
"##,
        )
        .unwrap();
        assert_eq!(recipe.step_timeout_secs, 5);
        assert_eq!(recipe.confirmation.detect.as_deref(), Some("#captcha"));
        assert_eq!(recipe.ai_fallback, AiFallback::Off);
        assert!(recipe.login().is_none());
    }

    #[test]
    fn field_selectors_split_off_the_attribute() {
        let parse = FieldSelector::parse;
        assert_eq!(parse("td").attribute, None);
        let link = parse("a.download@href");
        assert_eq!(link.selector, "a.download");
        assert_eq!(link.attribute.as_deref(), Some("href"));
        let own = parse("@data-id");
        assert_eq!(own.selector, "");
        assert_eq!(own.attribute.as_deref(), Some("data-id"));
        assert_eq!(parse(r#"a[href*="@"]"#).attribute, None);
    }

    #[test]
    fn domain_parameter_can_be_overridden_by_the_company() {
        let yaml = r#"
params: { domain: shop.example }
steps: [ { goto: "https://{domain}/x" } ]
"#;
        let recipe = Recipe::from_yaml(yaml).unwrap();
        let with_default = recipe.with_params(&BTreeMap::new()).unwrap();
        assert_eq!(
            with_default.steps[0],
            Step::Goto("https://shop.example/x".into())
        );

        let overrides = BTreeMap::from([("domain".to_owned(), "shop.test".to_owned())]);
        let replaced = recipe.with_params(&overrides).unwrap();
        assert_eq!(replaced.steps[0], Step::Goto("https://shop.test/x".into()));

        let unknown = BTreeMap::from([("dmain".to_owned(), "x".to_owned())]);
        let error = recipe.with_params(&unknown).unwrap_err().to_string();
        assert!(error.contains("portal.params.dmain"), "{error}");
    }

    #[test]
    fn placeholder_without_a_value_is_reported() {
        let paths = paths_of(r#"steps: [ { goto: "https://{domain}/x" } ]"#);
        assert_eq!(paths, ["steps[0].goto"]);
    }

    #[test]
    fn only_web_addresses_can_be_opened() {
        for url in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "portal.example",
        ] {
            let paths = paths_of(&format!("steps: [ {{ goto: \"{url}\" }} ]"));
            assert_eq!(paths, ["steps[0].goto"], "{url}");
        }
    }

    #[test]
    fn all_problems_are_reported_together_with_their_paths() {
        let paths = paths_of(
            r#"
step_timeout_secs: 0
confirmation: { timeout_secs: 0 }
steps:
  - login: { user: "", password: " ", submit: "" }
  - download_new: { key: x }
  - extract_list: { rows: "", fields: { date: td }, max_pages: 0 }
  - extract_list: { rows: tr, fields: { date: td, url: a@href } }
  - download_new: { key: nope }
"#,
        );
        assert_eq!(
            paths,
            [
                "step_timeout_secs",
                "confirmation.timeout_secs",
                "steps[0].login.user",
                "steps[0].login.password",
                "steps[0].login.submit",
                "steps[1].download_new",
                "steps[2].extract_list.rows",
                "steps[2].extract_list.max_pages",
                "steps[2].extract_list.fields",
                "steps[3].extract_list",
                "steps[4].download_new.key",
            ]
        );
    }

    #[test]
    fn a_list_must_be_followed_by_its_download() {
        let paths =
            paths_of(r#"steps: [ { extract_list: { rows: tr, fields: { url: a@href } } } ]"#);
        assert_eq!(paths, ["steps"]);
    }

    #[test]
    fn detail_pages_are_checked_too() {
        let paths = paths_of(
            r#"
steps:
  - extract_list:
      rows: tr
      fields: { no: td }
      open_detail: { url: link, rows: "", fields: { no: td } }
  - download_new: { key: no }
"#,
        );
        assert_eq!(
            paths,
            [
                "steps[0].extract_list.open_detail.url",
                "steps[0].extract_list.open_detail.rows",
                "steps[0].extract_list.open_detail.fields",
            ]
        );
    }

    #[test]
    fn the_full_agent_mode_is_refused_and_off_is_the_default() {
        let steps = r#"steps: [ { goto: "https://portal.test/" } ]"#;
        let error = Recipe::from_yaml(&format!("ai_fallback: full_agent\n{steps}"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("`full_agent` is not available"), "{error}");
        assert_eq!(
            Recipe::from_yaml(steps).unwrap().ai_fallback,
            AiFallback::Off
        );
    }

    #[test]
    fn a_recipe_survives_being_written_and_read_again() {
        for yaml in [
            CONCEPT_EXAMPLE,
            r##"
params: { domain: shop.example }
confirmation: { detect: "#captcha", timeout_secs: 60 }
step_timeout_secs: 5
steps:
  - goto: https://{domain}/orders
  - dismiss_cookie_banner: "#accept"
  - extract_list:
      rows: .order
      empty: .no-orders
      next_page: a.next
      max_pages: 3
      fields: { order_no: .number, detail: a.details@href }
      open_detail:
        url: detail
        rows: li.document
        missing: .no-documents
        fields: { invoice_no: .number, url: a@href }
  - download_new: { key: invoice_no }
"##,
        ] {
            let recipe = Recipe::from_yaml(yaml).unwrap();
            let written = recipe.to_yaml().unwrap();
            assert_eq!(Recipe::from_yaml(&written).unwrap(), recipe, "{written}");
        }
    }

    #[test]
    fn unknown_fields_and_steps_are_named() {
        let error = Recipe::from_yaml("steps: [ { click: a } ]")
            .unwrap_err()
            .to_string();
        assert!(error.contains("click"), "{error}");
        let error =
            Recipe::from_yaml("steps: [ { login: { user: a, password: b, submit: c, otp: d } } ]")
                .unwrap_err()
                .to_string();
        assert!(error.contains("otp"), "{error}");
    }

    #[test]
    fn missing_file_names_the_path() {
        let error = Recipe::load(Path::new("/does/not/exist.yaml"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("/does/not/exist.yaml"), "{error}");
    }
}
