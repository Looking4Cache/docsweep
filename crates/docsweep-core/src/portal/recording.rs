//! Turns what the recorder saw into the draft of a recipe.
//!
//! Events only ever hold selectors and what kind of field something is. The
//! recorder's listener never reads a typed value, and these events come from a
//! page, so everything here is checked again before it goes into a draft.

use std::collections::BTreeMap;

use serde::Deserialize;

use super::recipe::{DownloadNew, ExtractList, LoginStep, Recipe, Step};
use crate::ai::sanitize::{address_without_details, scrub};

/// What the notes may list at most.
const MAX_NOTES: usize = 15;

/// What kind of field somebody typed into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldRole {
    User,
    Password,
    Totp,
    Other,
}

/// What a column of a list row looks like. The text itself stays in the browser.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CellKind {
    Date,
    Number,
    Amount,
    Other,
}

/// One column of a list row.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cell {
    pub selector: String,
    pub kind: CellKind,
}

/// The list row around a link that someone clicked.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RowHint {
    pub rows: String,
    /// The link, relative to the row.
    pub link: String,
    pub cells: Vec<Cell>,
}

/// One thing that happened while recording.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RecordedEvent {
    /// Somebody typed into a field. There is no value here.
    Field { selector: String, role: FieldRole },
    Click {
        selector: String,
        #[serde(default)]
        row: Option<RowHint>,
    },
    /// A form was sent. The button is missing if it happened with the Enter key.
    Submit {
        #[serde(default)]
        selector: Option<String>,
    },
    /// The main frame opened an address.
    Navigated { address: String },
    /// The page asked for something that looks like a list or a document.
    Request { method: String, address: String },
}

/// A draft, and what a person should know when reading it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedDraft {
    pub recipe: Recipe,
    pub notes: Vec<String>,
}

const ANY_SUBMIT_BUTTON: &str = "button[type=submit], input[type=submit]";

/// Selectors come from a page. One that could hold personal data, or a value at
/// all, does not go into a file.
fn usable(selector: &str) -> bool {
    let compact = selector.split_whitespace().collect::<Vec<_>>().join(" ");
    !selector.is_empty()
        && selector.len() <= 300
        && !selector.contains("value")
        && scrub(selector, &[]) == compact
}

fn clean_address(address: &str) -> Option<String> {
    let lower = address.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return None;
    }
    Some(address_without_details(address))
}

/// What was found out about the login.
#[derive(Default)]
struct Login {
    user: Option<String>,
    password: Option<String>,
    totp: Option<String>,
    next: Option<String>,
    submit: Option<String>,
    totp_submit: Option<String>,
}

impl Login {
    fn field(&mut self, selector: &str, role: FieldRole) {
        let slot = match role {
            FieldRole::User if self.password.is_none() => &mut self.user,
            FieldRole::Password => &mut self.password,
            FieldRole::Totp => &mut self.totp,
            FieldRole::User | FieldRole::Other => return,
        };
        if slot.is_none() && usable(selector) {
            *slot = Some(selector.to_owned());
        }
    }

    /// A button means something different at each point of a login.
    fn button(&mut self, selector: Option<&str>) {
        let selector = selector.filter(|selector| usable(selector));
        let any = || selector.unwrap_or(ANY_SUBMIT_BUTTON).to_owned();
        if self.totp.is_some() {
            self.totp_submit.get_or_insert_with(any);
        } else if self.password.is_some() {
            self.submit.get_or_insert_with(any);
        } else if self.user.is_some() {
            self.next.get_or_insert_with(any);
        }
    }

    fn step(self) -> Option<Step> {
        let totp_submit = self
            .totp_submit
            .filter(|button| Some(button) != self.submit.as_ref());
        Some(Step::Login(LoginStep {
            user: self.user?,
            password: self.password?,
            submit: self.submit?,
            totp: self.totp,
            totp_submit,
            next: self.next,
            logged_in: None,
        }))
    }
}

fn list_steps(row: &RowHint, notes: &mut Vec<String>) -> Option<(Step, Step)> {
    if !usable(&row.rows) || !usable(&row.link) {
        return None;
    }
    let mut fields = BTreeMap::new();
    fields.insert("url".to_owned(), format!("{}@href", row.link));
    let mut key = None;
    for cell in row.cells.iter().filter(|cell| usable(&cell.selector)) {
        let name = match cell.kind {
            CellKind::Date => "date",
            CellKind::Number => "invoice_no",
            CellKind::Amount => "amount",
            CellKind::Other => continue,
        };
        if !fields.contains_key(name) {
            fields.insert(name.to_owned(), cell.selector.clone());
            if name == "invoice_no" {
                key = Some(name);
            }
        }
    }
    if key.is_none() {
        notes.push(
            "TODO: no column looked like an invoice number, so documents are told apart by their link. Better add a field `invoice_no` and use it as `key`."
                .to_owned(),
        );
    }
    let list = Step::ExtractList(ExtractList {
        rows: row.rows.clone(),
        fields,
        empty: None,
        next_page: None,
        max_pages: 50,
        open_detail: None,
    });
    let download = Step::DownloadNew(DownloadNew {
        key: key.unwrap_or("url").to_owned(),
        html_to_pdf: false,
    });
    Some((list, download))
}

/// Builds a draft that a person then edits. A valid recipe in any case, so that
/// `docsweep test-recipe` can check it right away.
pub fn build_draft(start: &str, events: &[RecordedEvent]) -> Result<RecordedDraft, String> {
    let start =
        clean_address(start).ok_or_else(|| "the address must be `http` or `https`".to_owned())?;
    let mut login = Login::default();
    let mut notes = Vec::new();
    let mut current = start.clone();
    let mut list: Option<(String, RowHint)> = None;
    let mut other_clicks = Vec::new();
    let mut requests = Vec::new();

    for event in events {
        match event {
            RecordedEvent::Field { selector, role } => login.field(selector, *role),
            RecordedEvent::Click { selector, row } => {
                if let Some(row) = row {
                    list.get_or_insert_with(|| (current.clone(), row.clone()));
                } else if login.user.is_some() && login.submit.is_none() || login.totp.is_some() {
                    login.button(Some(selector));
                } else if usable(selector) && !other_clicks.contains(selector) {
                    other_clicks.push(selector.clone());
                }
            }
            RecordedEvent::Submit { selector } => login.button(selector.as_deref()),
            RecordedEvent::Navigated { address } => {
                if let Some(address) = clean_address(address) {
                    current = address;
                }
            }
            RecordedEvent::Request { method, address } => {
                if let Some(address) = clean_address(address) {
                    let line = format!("{} {}", scrub(method, &[]), scrub(&address, &[]));
                    if !requests.contains(&line) {
                        requests.push(line);
                    }
                }
            }
        }
    }

    let mut steps = vec![Step::Goto(start.clone())];
    match login.step() {
        Some(step) => {
            steps.push(Step::DismissCookieBanner(
                super::recipe::CookieBanner::Enabled(true),
            ));
            steps.push(step);
            notes.push(
                "TODO: add `logged_in` to the login, with a selector that only shows with an active session."
                    .to_owned(),
            );
        }
        None => notes.push(
            "No complete login was recorded. If this portal needs one, add a `login` step."
                .to_owned(),
        ),
    }
    let list_steps = list.and_then(|(page, row)| {
        let steps = list_steps(&row, &mut notes)?;
        Some((page, steps))
    });
    match list_steps {
        Some((page, (list, download))) => {
            if !matches!(steps.last(), Some(Step::Goto(address)) if *address == page) {
                steps.push(Step::Goto(page));
            }
            steps.push(list);
            steps.push(download);
        }
        None => notes.push(
            "No invoice list was recorded. Click a document link in a list row while recording, or add `extract_list` and `download_new` by hand."
                .to_owned(),
        ),
    }
    if !other_clicks.is_empty() {
        other_clicks.truncate(MAX_NOTES);
        notes.push(format!(
            "Other clicks, which recipes have no step for:\n  {}",
            other_clicks.join("\n  ")
        ));
    }
    if !requests.is_empty() {
        requests.truncate(MAX_NOTES);
        notes.push(format!(
            "Requests seen that may help when editing:\n  {}",
            requests.join("\n  ")
        ));
    }

    let recipe = Recipe {
        params: BTreeMap::new(),
        confirmation: super::recipe::Confirmation::default(),
        step_timeout_secs: 30,
        steps,
        ai_fallback: super::recipe::AiFallback::Off,
    };
    let issues = recipe.validate();
    if let Some(issue) = issues.first() {
        return Err(format!("{}: {}", issue.path, issue.message));
    }
    Ok(RecordedDraft { recipe, notes })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::portal::draft::draft_text;

    const START: &str = "https://portal.test/login?session=abc#x";

    fn field(selector: &str, role: FieldRole) -> RecordedEvent {
        RecordedEvent::Field {
            selector: selector.into(),
            role,
        }
    }

    fn click(selector: &str) -> RecordedEvent {
        RecordedEvent::Click {
            selector: selector.into(),
            row: None,
        }
    }

    fn submit(selector: &str) -> RecordedEvent {
        RecordedEvent::Submit {
            selector: Some(selector.into()),
        }
    }

    fn navigated(address: &str) -> RecordedEvent {
        RecordedEvent::Navigated {
            address: address.into(),
        }
    }

    fn row_click() -> RecordedEvent {
        RecordedEvent::Click {
            selector: "a.download".into(),
            row: Some(RowHint {
                rows: "table.invoices tbody tr".into(),
                link: "a.download".into(),
                cells: vec![
                    Cell {
                        selector: "td:nth-child(1)".into(),
                        kind: CellKind::Date,
                    },
                    Cell {
                        selector: "td:nth-child(2)".into(),
                        kind: CellKind::Number,
                    },
                    Cell {
                        selector: "td:nth-child(3)".into(),
                        kind: CellKind::Amount,
                    },
                    Cell {
                        selector: "td:nth-child(4)".into(),
                        kind: CellKind::Other,
                    },
                ],
            }),
        }
    }

    fn login_of(draft: &RecordedDraft) -> &LoginStep {
        draft.recipe.login().expect("a login")
    }

    #[test]
    fn a_whole_session_becomes_a_recipe() {
        let draft = build_draft(
            START,
            &[
                navigated("https://portal.test/login?session=abc"),
                field("#user", FieldRole::User),
                field("#pw", FieldRole::Password),
                click("#go"),
                submit("#go"),
                navigated("https://portal.test/otp"),
                field("#otp", FieldRole::Totp),
                click("#verify"),
                submit("#verify"),
                navigated("https://portal.test/invoices?page=1&token=abc"),
                row_click(),
            ],
        )
        .unwrap();

        let login = login_of(&draft);
        assert_eq!(login.user, "#user");
        assert_eq!(login.password, "#pw");
        assert_eq!(login.submit, "#go");
        assert_eq!(login.totp.as_deref(), Some("#otp"));
        assert_eq!(login.totp_submit.as_deref(), Some("#verify"));

        assert_eq!(
            draft.recipe.steps[0],
            Step::Goto("https://portal.test/login".into()),
            "no session in the address"
        );
        assert_eq!(
            draft.recipe.steps[3],
            Step::Goto("https://portal.test/invoices".into())
        );
        let Step::ExtractList(list) = &draft.recipe.steps[4] else {
            panic!("expected a list: {:?}", draft.recipe.steps);
        };
        assert_eq!(list.rows, "table.invoices tbody tr");
        assert_eq!(list.fields["url"], "a.download@href");
        assert_eq!(list.fields["date"], "td:nth-child(1)");
        assert_eq!(list.fields["invoice_no"], "td:nth-child(2)");
        assert_eq!(list.fields["amount"], "td:nth-child(3)");
        assert_eq!(
            draft.recipe.steps[5],
            Step::DownloadNew(DownloadNew {
                key: "invoice_no".into(),
                html_to_pdf: false
            })
        );
        assert!(
            Recipe::from_yaml(&draft_text("recorded", &draft.notes, &draft.recipe).unwrap())
                .is_ok()
        );
    }

    #[test]
    fn a_password_on_a_second_page_gives_a_next_button() {
        let draft = build_draft(
            START,
            &[
                field("#user", FieldRole::User),
                click("#continue"),
                submit("#continue"),
                field("#pw", FieldRole::Password),
                submit("#go"),
            ],
        )
        .unwrap();
        let login = login_of(&draft);
        assert_eq!(login.next.as_deref(), Some("#continue"));
        assert_eq!(login.submit, "#go");
        assert_eq!(login.totp_submit, None);
    }

    #[test]
    fn pressing_enter_uses_any_submit_button() {
        let draft = build_draft(
            START,
            &[
                field("#user", FieldRole::User),
                field("#pw", FieldRole::Password),
                RecordedEvent::Submit { selector: None },
            ],
        )
        .unwrap();
        assert_eq!(login_of(&draft).submit, ANY_SUBMIT_BUTTON);
    }

    #[test]
    fn a_session_without_a_login_or_list_still_is_a_valid_draft_with_notes() {
        let draft = build_draft(START, &[click("#menu")]).unwrap();
        assert_eq!(draft.recipe.steps.len(), 1);
        let notes = draft.notes.join("\n");
        assert!(notes.contains("No complete login"), "{notes}");
        assert!(notes.contains("No invoice list"), "{notes}");
        assert!(notes.contains("#menu"), "{notes}");
    }

    #[test]
    fn nothing_typed_or_personal_gets_through_from_a_lying_page() {
        // A page can call the binding with whatever it likes.
        let draft = build_draft(
            START,
            &[
                field(r#"input[value="hunter2"]"#, FieldRole::User),
                field("#jane@example.com", FieldRole::User),
                field("#customer-123456789", FieldRole::User),
                field("#user", FieldRole::User),
                field("#pw", FieldRole::Password),
                submit("#go"),
                RecordedEvent::Request {
                    method: "GET".into(),
                    address: "https://portal.test/api/invoices/123456789?token=abc#f".into(),
                },
                RecordedEvent::Request {
                    method: "GET".into(),
                    address: "file:///etc/passwd".into(),
                },
                navigated("javascript:alert(1)"),
            ],
        )
        .unwrap();
        let text = draft_text("recorded", &draft.notes, &draft.recipe).unwrap();
        for bad in [
            "hunter2",
            "jane@",
            "123456789",
            "token=",
            "passwd",
            "javascript",
        ] {
            assert!(!text.contains(bad), "{bad} in {text}");
        }
        assert_eq!(login_of(&draft).user, "#user");
        assert!(
            text.contains("GET https://portal.test/api/invoices/[number]"),
            "{text}"
        );
    }

    #[test]
    fn without_an_invoice_number_the_link_identifies_a_document() {
        let RecordedEvent::Click { selector, row } = row_click() else {
            unreachable!();
        };
        let mut row = row.unwrap();
        row.cells.retain(|cell| cell.kind != CellKind::Number);
        let draft = build_draft(
            START,
            &[RecordedEvent::Click {
                selector,
                row: Some(row),
            }],
        )
        .unwrap();
        assert!(draft.recipe.steps.contains(&Step::DownloadNew(DownloadNew {
            key: "url".into(),
            html_to_pdf: false
        })));
        assert!(draft.notes.join("\n").contains("invoice_no"));
    }

    #[test]
    fn unknown_events_from_a_page_are_refused() {
        let parsed: Result<RecordedEvent, _> =
            serde_json::from_str(r##"{"kind":"field","selector":"#a","role":"user","value":"x"}"##);
        assert!(parsed.is_err());
        let parsed: RecordedEvent =
            serde_json::from_str(r##"{"kind":"field","selector":"#a","role":"totp"}"##).unwrap();
        assert_eq!(parsed, field("#a", FieldRole::Totp));
    }
}
