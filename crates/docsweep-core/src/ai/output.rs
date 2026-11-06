//! What a model answers is untrusted, as a page can have told it what to say. It
//! is only ever parsed into the typed recipe model and validated. Nothing in it
//! is executed, and a recipe has no way to run a script.

use scraper::Selector;

use crate::portal::hosts::HostBinding;
use crate::portal::recipe::{AiFallback, ExtractList, FieldSelector, LoginStep, Recipe, Step};

/// A recipe is small. A longer answer is not one.
const MAX_ANSWER_CHARS: usize = 100_000;

/// The placeholders of the sanitiser, which are valid CSS but match nothing.
const PLACEHOLDERS: &[&str] = &["[secret]", "[mail]", "[iban]", "[number]", "[token]"];

/// Why an answer was refused. The recipe stays as it is.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OutputError {
    #[error("the answer is too long to be a recipe")]
    TooLong,
    #[error("the answer is not a valid recipe: {0}")]
    Invalid(String),
    #[error("the answer starts no page: a recipe must begin with `goto`")]
    NoStart,
    #[error("the selector `{0}` is not valid")]
    BadSelector(String),
    #[error("the answer repeats a placeholder instead of a selector or an address")]
    Placeholder,
    #[error(
        "a `{0}` step cannot be repaired; only `login` and `extract_list` have selectors that can stop matching"
    )]
    NotRepairable(String),
    #[error("the answer is a `{new}` step, but `{old}` was to be repaired")]
    OtherKind { old: String, new: String },
    #[error("the answer changes the fields of the list, not only its selectors")]
    OtherFields,
    #[error("the answer adds or removes the TOTP field, which is not a repair")]
    OtherTotp,
    #[error("the answer is the same as the step that failed")]
    Unchanged,
    #[error("the answer would leave the portal's host for {0}")]
    OtherHost(String),
}

/// Models like to wrap their answer in a code fence.
fn without_fence(answer: &str) -> &str {
    let trimmed = answer.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    let body = rest.split_once('\n').map_or("", |(_, body)| body);
    body.strip_suffix("```").unwrap_or(body).trim()
}

fn check_selector(selector: &str) -> Result<(), OutputError> {
    if PLACEHOLDERS.iter().any(|mark| selector.contains(mark)) {
        return Err(OutputError::Placeholder);
    }
    Selector::parse(selector)
        .map(|_| ())
        .map_err(|_| OutputError::BadSelector(selector.to_owned()))
}

fn check_fields<'a>(fields: impl Iterator<Item = &'a String>) -> Result<(), OutputError> {
    for field in fields {
        let parsed = FieldSelector::parse(field);
        if !parsed.selector.is_empty() {
            check_selector(&parsed.selector)?;
        }
    }
    Ok(())
}

fn check_login(login: &LoginStep) -> Result<(), OutputError> {
    let required = [&login.user, &login.password, &login.submit];
    for selector in required {
        check_selector(selector)?;
    }
    for selector in [
        &login.totp,
        &login.totp_submit,
        &login.next,
        &login.logged_in,
    ]
    .into_iter()
    .flatten()
    {
        check_selector(selector)?;
    }
    Ok(())
}

fn check_list(list: &ExtractList) -> Result<(), OutputError> {
    check_selector(&list.rows)?;
    for selector in [&list.empty, &list.next_page].into_iter().flatten() {
        check_selector(selector)?;
    }
    check_fields(list.fields.values())?;
    if let Some(detail) = &list.open_detail {
        check_selector(&detail.rows)?;
        if let Some(missing) = &detail.missing {
            check_selector(missing)?;
        }
        check_fields(detail.fields.values())?;
    }
    Ok(())
}

fn check_step(step: &Step) -> Result<(), OutputError> {
    match step {
        Step::Goto(address) => {
            if PLACEHOLDERS.iter().any(|mark| address.contains(mark)) {
                return Err(OutputError::Placeholder);
            }
            Ok(())
        }
        Step::Login(login) => check_login(login),
        Step::ExtractList(list) => check_list(list),
        Step::DismissCookieBanner(crate::portal::recipe::CookieBanner::Selector(selector)) => {
            check_selector(selector)
        }
        Step::DismissCookieBanner(_) | Step::DownloadNew(_) => Ok(()),
    }
}

/// Parses a whole recipe. It comes without any AI permission: a model cannot
/// grant itself `ai_fallback`, only a person can. Whether it stays on the known
/// hosts is a separate question for [`HostBinding::changes_in`], because a person
/// can agree to that.
pub fn parse_recipe(answer: &str) -> Result<Recipe, OutputError> {
    if answer.chars().count() > MAX_ANSWER_CHARS {
        return Err(OutputError::TooLong);
    }
    let mut recipe = Recipe::from_yaml(without_fence(answer))
        .map_err(|error| OutputError::Invalid(error.to_string()))?;
    if !matches!(recipe.steps.first(), Some(Step::Goto(_))) {
        return Err(OutputError::NoStart);
    }
    recipe.steps.iter().try_for_each(check_step)?;
    if let Some(detect) = &recipe.confirmation.detect {
        check_selector(detect)?;
    }
    recipe.ai_fallback = AiFallback::Off;
    Ok(recipe)
}

fn parse_step(answer: &str) -> Result<Step, OutputError> {
    if answer.chars().count() > MAX_ANSWER_CHARS {
        return Err(OutputError::TooLong);
    }
    let text = without_fence(answer);
    let options = serde_saphyr::options! { with_snippet: false };
    if let Ok(step) = serde_saphyr::from_str_with_options::<Step>(text, options) {
        return Ok(step);
    }
    let options = serde_saphyr::options! { with_snippet: false };
    match serde_saphyr::from_str_with_options::<Vec<Step>>(text, options) {
        Ok(mut steps) if steps.len() == 1 => Ok(steps.remove(0)),
        Ok(_) => Err(OutputError::Invalid("expected exactly one step".to_owned())),
        Err(error) => Err(OutputError::Invalid(error.to_string())),
    }
}

/// Parses the replacement for step `index` of a resolved recipe. A repair can
/// only change selectors: it can never change the kind of step, add a list field,
/// add a TOTP field, change what counts as an empty list, or read more pages.
pub fn parse_repaired_step(
    answer: &str,
    recipe: &Recipe,
    index: usize,
    binding: &HostBinding,
) -> Result<Step, OutputError> {
    let old = recipe
        .steps
        .get(index)
        .ok_or_else(|| OutputError::Invalid("no such step".to_owned()))?;
    let new = parse_step(answer)?;
    if new.name() != old.name() {
        return Err(OutputError::OtherKind {
            old: old.name().to_owned(),
            new: new.name().to_owned(),
        });
    }
    let repaired = match (old, new) {
        (Step::Login(old), Step::Login(new)) => {
            if old.totp.is_some() != new.totp.is_some() {
                return Err(OutputError::OtherTotp);
            }
            Step::Login(new)
        }
        (Step::ExtractList(old), Step::ExtractList(new)) => {
            Step::ExtractList(repaired_list(old, new)?)
        }
        (old, _) => return Err(OutputError::NotRepairable(old.name().to_owned())),
    };
    check_step(&repaired)?;
    if repaired == *old {
        return Err(OutputError::Unchanged);
    }

    let mut whole = recipe.clone();
    whole.steps[index] = repaired.clone();
    if let Some(issue) = whole.validate().first() {
        return Err(OutputError::Invalid(format!(
            "{}: {}",
            issue.path, issue.message
        )));
    }
    if let Some(change) = binding.changes_in(&whole).first() {
        return Err(OutputError::OtherHost(change.new.clone()));
    }
    Ok(repaired)
}

/// Keeps what is not a selector. `empty` stays as it was, because a selector that
/// always matches would make every list look empty, and nobody would notice that
/// invoices stopped coming.
fn repaired_list(old: &ExtractList, mut new: ExtractList) -> Result<ExtractList, OutputError> {
    let names = |list: &ExtractList| list.fields.keys().cloned().collect::<Vec<_>>();
    if names(old) != names(&new) {
        return Err(OutputError::OtherFields);
    }
    match (&old.open_detail, &new.open_detail) {
        (None, None) => {}
        (Some(old_detail), Some(new_detail))
            if old_detail.url == new_detail.url
                && old_detail.fields.keys().eq(new_detail.fields.keys()) => {}
        _ => return Err(OutputError::OtherFields),
    }
    new.empty.clone_from(&old.empty);
    new.max_pages = old.max_pages;
    Ok(new)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RECIPE: &str = r##"
ai_fallback: repair_step
steps:
  - goto: https://portal.test/login
  - login: { user: "#u", password: "#p", submit: "#s" }
  - goto: https://portal.test/invoices
  - extract_list:
      rows: "table tr"
      empty: ".none"
      max_pages: 3
      fields: { invoice_no: "td", url: "a@href" }
  - download_new: { key: invoice_no }
"##;

    fn recipe() -> Recipe {
        Recipe::from_yaml(RECIPE).unwrap()
    }

    fn binding() -> HostBinding {
        HostBinding::of_recipe(&recipe())
    }

    fn repair(answer: &str, index: usize) -> Result<Step, OutputError> {
        parse_repaired_step(answer, &recipe(), index, &binding())
    }

    #[test]
    fn a_good_recipe_passes_fenced_or_not_without_ai_permission() {
        let parsed = parse_recipe(RECIPE).unwrap();
        assert_eq!(
            parsed.ai_fallback,
            AiFallback::Off,
            "only a person grants it"
        );
        let fenced = format!("```yaml\n{}\n```", RECIPE.trim());
        assert_eq!(parse_recipe(&fenced).unwrap(), parsed);
    }

    #[test]
    fn nonsense_is_refused() {
        for answer in [
            "Sorry, I cannot help with that.",
            "steps: []",
            "steps: [ { run_script: 'alert(1)' } ]",
            "steps: [ { goto: 'javascript:alert(1)' } ]",
            "steps: [ { goto: 'file:///etc/passwd' } ]",
            "steps: [ { goto: https://portal.test/ } ]\nexecute: rm -rf /",
            "ai_fallback: full_agent\nsteps: [ { goto: https://portal.test/ } ]",
        ] {
            assert!(parse_recipe(answer).is_err(), "{answer}");
        }
        let start =
            parse_recipe(r##"steps: [ { login: { user: "#u", password: "#p", submit: "#s" } } ]"##);
        assert_eq!(start.unwrap_err(), OutputError::NoStart);
        let long = format!("# {}", "x".repeat(MAX_ANSWER_CHARS));
        assert_eq!(parse_recipe(&long).unwrap_err(), OutputError::TooLong);
    }

    #[test]
    fn bad_selectors_and_placeholders_are_refused() {
        let bad = r###"steps:
  - goto: https://portal.test/
  - login: { user: "##bad(", password: "#p", submit: "#s" }
"###;
        assert!(matches!(
            parse_recipe(bad).unwrap_err(),
            OutputError::BadSelector(_)
        ));
        let placeholder = bad.replace("##bad(", "input[secret]");
        assert_eq!(
            parse_recipe(&placeholder).unwrap_err(),
            OutputError::Placeholder
        );
        assert_eq!(
            parse_recipe("steps: [ { goto: 'https://portal.test/[number]' } ]").unwrap_err(),
            OutputError::Placeholder
        );
    }

    #[test]
    fn a_login_can_get_new_selectors() {
        let step = repair(
            r##"login: { user: "#new", password: "#p", submit: "#s" }"##,
            1,
        )
        .unwrap();
        let Step::Login(login) = step else {
            panic!("expected a login");
        };
        assert_eq!(login.user, "#new");
    }

    #[test]
    fn a_step_in_a_list_or_a_fence_is_understood() {
        assert!(
            repair(
                r##"- login: { user: "#new", password: "#p", submit: "#s" }"##,
                1
            )
            .is_ok()
        );
        let fenced = "```yaml\nlogin: { user: '#new', password: '#p', submit: '#s' }\n```";
        assert!(repair(fenced, 1).is_ok());
    }

    #[test]
    fn a_repair_cannot_become_another_kind_of_step() {
        assert_eq!(
            repair("goto: https://evil.example/login", 1).unwrap_err(),
            OutputError::OtherKind {
                old: "login".into(),
                new: "goto".into()
            }
        );
        assert!(matches!(
            repair("goto: https://evil.example/", 0).unwrap_err(),
            OutputError::NotRepairable(_)
        ));
    }

    #[test]
    fn a_repair_cannot_widen_a_list() {
        let keeps = repair(
            r##"extract_list: { rows: ".new", empty: "body", max_pages: 9000, fields: { invoice_no: ".n", url: "a@href" } }"##,
            3,
        )
        .unwrap();
        let Step::ExtractList(list) = keeps else {
            panic!("expected a list");
        };
        assert_eq!(list.rows, ".new");
        assert_eq!(list.empty.as_deref(), Some(".none"));
        assert_eq!(list.max_pages, 3);

        assert_eq!(
            repair(
                r##"extract_list: { rows: ".new", fields: { invoice_no: ".n", url: "a@href", extra: "b" } }"##,
                3
            )
            .unwrap_err(),
            OutputError::OtherFields
        );
    }

    #[test]
    fn a_repair_cannot_add_a_totp_field_or_change_nothing() {
        assert_eq!(
            repair(
                r##"login: { user: "#u", password: "#p", submit: "#s", totp: "#o" }"##,
                1
            )
            .unwrap_err(),
            OutputError::OtherTotp
        );
        assert_eq!(
            repair(
                r##"login: { user: "#u", password: "#p", submit: "#s" }"##,
                1
            )
            .unwrap_err(),
            OutputError::Unchanged
        );
    }

    #[test]
    fn invalid_answers_for_a_repair_are_refused() {
        for answer in [
            "I think the selector is #user",
            "login: { user: '#u' }",
            "login: { user: '#u', password: '#p', submit: '#s', script: 'x' }",
            "- { goto: 'https://portal.test/' }\n- { goto: 'https://portal.test/' }",
        ] {
            assert!(repair(answer, 1).is_err(), "{answer}");
        }
    }
}
