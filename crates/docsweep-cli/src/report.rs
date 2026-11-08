//! Text output of `mail-scan`: which mail, which company, which file, and what
//! happens to the mail.

use std::fmt::Write;

use docsweep_core::index::LinkClosure;
use docsweep_core::mail::execute::{AccountReport, MailReport, Outcome};
use docsweep_core::mail::plan::{
    DisposalAction, Disposition, MailAction, MailDecision, PlannedDocument, ReviewReason,
};
use docsweep_core::pickup::{ImportDecision, ImportOutcome, ImportPlan};
use docsweep_core::portal::engine::{EntryOutcome, FetchReport, RecipeNotice};

/// Renders all accounts.
pub fn render(reports: &[AccountReport], dry_run: bool) -> String {
    let mut out = String::new();
    if dry_run {
        out.push_str("Dry run: nothing was changed.\n");
    }
    for account in reports {
        let _ = writeln!(out, "\nAccount {}", account.account_id);
        if account.mails.is_empty() && account.error.is_none() {
            out.push_str("  No relevant mails.\n");
        }
        for mail in &account.mails {
            render_mail(&mut out, mail, dry_run);
        }
        if let Some(error) = &account.error {
            let _ = writeln!(out, "  ERROR: {error}");
        }
    }
    out
}

/// What `docsweep import --dry-run` found.
pub fn render_import_plan(plan: &ImportPlan) -> String {
    let unknown = "not found".to_owned();
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Number: {}",
        plan.invoice_no.clone().unwrap_or_else(|| unknown.clone())
    );
    let _ = writeln!(
        out,
        "Date: {}",
        plan.date.map_or(unknown, |date| date.to_string())
    );
    let path = plan.target_path.display();
    let _ = match &plan.decision {
        ImportDecision::Store => writeln!(out, "Would save as {path}"),
        ImportDecision::ReviewNoNumber => {
            writeln!(
                out,
                "Would save for review as {path}: no invoice number found"
            )
        }
        ImportDecision::ReviewNoDate => {
            writeln!(
                out,
                "Would save for review as {path}: no invoice date found"
            )
        }
        ImportDecision::ReviewNumberConflict => writeln!(
            out,
            "Would save for review as {path}: this number is known with a different file"
        ),
        ImportDecision::Duplicate { existing, reason } => {
            out.push_str(&describe_duplicate(existing.as_deref(), reason.as_deref()));
            Ok(())
        }
    };
    let _ = writeln!(out, "Nothing was changed.");
    out
}

/// What came of `docsweep import`.
pub fn render_import(outcome: &ImportOutcome) -> String {
    match outcome {
        ImportOutcome::Stored {
            path,
            needs_review,
            original_kept,
        } => {
            let verb = if *needs_review {
                "Saved for review as"
            } else {
                "Saved as"
            };
            let mut out = format!("{verb} {}\n", path.display());
            if let Some(reason) = original_kept {
                let _ = writeln!(out, "The original could not be deleted: {reason}");
            }
            out
        }
        ImportOutcome::Duplicate { existing, reason } => {
            describe_duplicate(existing.as_deref(), reason.as_deref())
        }
    }
}

fn describe_duplicate(existing: Option<&std::path::Path>, reason: Option<&str>) -> String {
    let known = match (existing, reason) {
        (Some(path), _) => format!("this invoice is already saved as {}", path.display()),
        (None, Some(reason)) => reason.to_owned(),
        (None, None) => "this invoice is already known".to_owned(),
    };
    format!("Not saved again: {known}. The file stays where it is.\n")
}

/// Renders a portal fetch: one line per list entry.
pub fn render_fetch(report: &FetchReport, dry_run: bool) -> String {
    let mut out = String::new();
    if dry_run {
        out.push_str("Dry run: nothing was downloaded.\n");
    }
    let _ = writeln!(out, "\nCompany {}", report.company_name);
    if report.entries.is_empty() && report.error.is_none() {
        out.push_str("  The portal lists nothing.\n");
    }
    for entry in &report.entries {
        let _ = writeln!(out, "  {}: {}", entry.key, describe_entry(&entry.outcome));
    }
    if let Some(error) = &report.error {
        let _ = writeln!(out, "  ERROR: {error}");
    }
    if let Some(check) = &report.bot_check {
        out.push_str(
            "  The run ended on a captcha or bot check, so this portal seems to turn away automated browsers.\n  \
             Consider assisted mode: set `portal.mode: assisted` and `portal.url`",
        );
        if let Some(url) = &check.portal_url {
            let _ = write!(out, " (for instance {url})");
        }
        out.push_str(
            ", and remove `credentials`, `totp` and `schedule`, which assisted mode does not use.\n",
        );
    }
    for notice in &report.notices {
        let _ = writeln!(out, "  {}", describe_notice(notice));
    }
    out
}

fn describe_notice(notice: &RecipeNotice) -> String {
    match notice {
        RecipeNotice::Repaired {
            step,
            provider,
            backup,
        } => format!(
            "NOTICE: {provider} repaired the step `{step}`, and the recipe was saved. The previous recipe is {}. Please check the change.",
            backup.display()
        ),
        RecipeNotice::RepairPossible { step } => format!(
            "A real run could try to repair the step `{step}` with the configured provider. A dry run sends nothing."
        ),
        RecipeNotice::RepairNotDone { step, reason } => {
            format!("The step `{step}` was not repaired: {reason}")
        }
    }
}

fn describe_entry(outcome: &EntryOutcome) -> String {
    match outcome {
        EntryOutcome::WouldDownload {
            file_name: Some(name),
        } => format!("would save as {name}"),
        EntryOutcome::WouldDownload { file_name: None } => "would download".to_owned(),
        EntryOutcome::AlreadyKnown => "already known".to_owned(),
        EntryOutcome::Stored { path } => format!("saved: {}", path.display()),
        EntryOutcome::StoredForReview { path, reason } => {
            let why = match reason {
                ReviewReason::NumberNotFound => "no invoice number found",
                ReviewReason::NumberConflict { .. } => "number known with a different file",
            };
            format!("saved for review ({why}): {}", path.display())
        }
        EntryOutcome::Unresolved { reason } => format!("not saved again: {reason}"),
        EntryOutcome::Missing => "missing: the portal offers no document for this yet".to_owned(),
        EntryOutcome::Failed { message } => format!("FAILED: {message}"),
    }
}

/// How to word planned steps: they did not all happen unless the run succeeded.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tense {
    Planned,
    Done,
    Failed,
}

fn render_mail(out: &mut String, report: &MailReport, dry_run: bool) {
    let plan = &report.plan;
    let tense = match report.outcome {
        _ if dry_run => Tense::Planned,
        Outcome::Failed { .. } => Tense::Failed,
        _ => Tense::Done,
    };
    let _ = writeln!(out, "\n  Mail: \"{}\" from {}", plan.subject, plan.from);

    match &plan.action {
        MailAction::Invoices {
            company_name,
            documents,
            decision,
            ..
        } => {
            let _ = writeln!(out, "    Company: {company_name}");
            for document in documents {
                render_document(out, document, tense);
            }
            if tense != Tense::Failed {
                let _ = writeln!(out, "    Mail: {}", describe_decision(decision, tense));
            }
        }
        MailAction::PortalFetchRequired {
            company_name,
            fetch_due,
            assisted,
            cleanup,
            closed,
            give_up_after_days,
            ..
        } => {
            let _ = writeln!(out, "    Company: {company_name}");
            match (assisted, cleanup, &report.outcome) {
                _ if closed.is_some() => {
                    out.push_str(match closed {
                        Some(LinkClosure::Dismissed) => {
                            "    Invoice: marked as done by you\n    Mail: left alone\n"
                        }
                        Some(LinkClosure::GivenUp) => {
                            "    Invoice: none came in time; Docsweep stopped waiting\n    Mail: left alone\n"
                        }
                        _ => "    Invoice: already stored\n    Mail: left alone\n",
                    });
                }
                (_, _, Outcome::Planned) if give_up_after_days.is_some() => {
                    let days = give_up_after_days.unwrap_or_default();
                    let _ = writeln!(
                        out,
                        "    Invoice: no matching invoice was found in the portal within {days} days; would stop waiting for it"
                    );
                    out.push_str("    Mail: would stay in place and be flagged\n");
                }
                (true, Some(disposal), Outcome::Planned) => {
                    out.push_str("    Invoice: stored\n");
                    let _ = writeln!(out, "    Mail: would {}", describe_disposal(disposal));
                }
                (true, _, Outcome::Planned | Outcome::AwaitingInvoice) => {
                    out.push_str("    Invoice: not picked up yet; pick it up in the app, or with `docsweep import`\n");
                    out.push_str("    Mail: stays in place until the invoice is stored\n");
                }
                _ => render_link_mail(out, &report.outcome, *fetch_due),
            }
        }
        MailAction::NoDocument { company_name, .. } => {
            let _ = writeln!(out, "    Company: {company_name}");
            out.push_str("    Mail: stays in place (no PDF, and no portal configured)\n");
        }
        MailAction::UnknownInvoice { attachment_names } => {
            let _ = writeln!(
                out,
                "    Unknown sender, looks like an invoice: {}",
                attachment_names.join(", ")
            );
            out.push_str("    Mail: stays in place; add a company to process it\n");
        }
    }

    match &report.outcome {
        Outcome::Planned
        | Outcome::Completed
        | Outcome::LinkResolved { .. }
        | Outcome::AwaitingInvoice => {}
        Outcome::KeptForReview { reason } => {
            let _ = writeln!(out, "    Review needed: {reason}");
        }
        Outcome::Failed { message } => {
            let _ = writeln!(out, "    FAILED: {message}");
        }
    }
}

fn render_link_mail(out: &mut String, outcome: &Outcome, fetch_due: bool) {
    let (invoice, mail) = match outcome {
        Outcome::LinkResolved { disposal } => (
            "stored from the portal".to_owned(),
            format!("{} (done)", describe_disposal(disposal)),
        ),
        Outcome::Failed { .. } => ("not stored".to_owned(), "stays in place".to_owned()),
        Outcome::AwaitingInvoice => (
            "not in the portal yet; checked again later, at most once a day".to_owned(),
            "stays in place until the invoice is stored".to_owned(),
        ),
        _ if fetch_due => (
            "would start a portal download".to_owned(),
            "stays in place until the invoice is stored".to_owned(),
        ),
        _ => (
            "the portal was already checked today; checked again tomorrow".to_owned(),
            "stays in place until the invoice is stored".to_owned(),
        ),
    };
    let _ = writeln!(out, "    Invoice: {invoice}");
    let _ = writeln!(out, "    Mail: {mail}");
}

fn render_document(out: &mut String, document: &PlannedDocument, tense: Tense) {
    let path = document.target_path.display();
    let verb = match tense {
        Tense::Planned => "Would save",
        Tense::Done => "Saved",
        Tense::Failed => "Tried to save",
    };
    let line = match &document.disposition {
        Disposition::Store => format!("{verb}: {path}"),
        Disposition::StoreForReview(ReviewReason::NumberNotFound) => {
            format!("{verb} for review (no invoice number found): {path}")
        }
        Disposition::StoreForReview(ReviewReason::NumberConflict { existing }) => {
            let existing = existing
                .as_ref()
                .map_or_else(String::new, |p| format!(", known file: {}", p.display()));
            format!("{verb} for review (number known with a different file{existing}): {path}")
        }
        Disposition::AlreadyStored { path: Some(known) } => {
            format!("Already stored: {}", known.display())
        }
        Disposition::AlreadyStored { path: None } => {
            format!("Already stored: {}", document.attachment_name)
        }
        Disposition::Unresolved { reason } => {
            format!("Not saved again ({reason}): {}", document.attachment_name)
        }
    };
    let _ = writeln!(out, "    {line}");
}

fn describe_decision(decision: &MailDecision, tense: Tense) -> String {
    let dry_run = tense == Tense::Planned;
    match decision {
        MailDecision::KeepAndFlag { .. } if dry_run => "would stay in place and be flagged".into(),
        MailDecision::KeepAndFlag { .. } => "stays in place, flagged".into(),
        MailDecision::Dispose(action) => {
            let description = describe_disposal(action);
            if dry_run {
                format!("would {description}")
            } else {
                format!("{description} (done)")
            }
        }
    }
}

fn describe_disposal(action: &DisposalAction) -> String {
    match action {
        DisposalAction::MoveTo(folder) => format!("move to {folder}"),
        DisposalAction::Trash => "move to trash".to_owned(),
        DisposalAction::DeletePermanently => "delete permanently".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use docsweep_core::document::InvoiceFacts;
    use docsweep_core::mail::plan::MailPlan;
    use docsweep_core::portal::engine::EntryReport;
    use std::path::PathBuf;

    fn invoice_report(decision: MailDecision, disposition: Disposition) -> AccountReport {
        AccountReport {
            account_id: "main".into(),
            error: None,
            mails: vec![MailReport {
                outcome: Outcome::Planned,
                plan: MailPlan {
                    uid: 1,
                    message_id: "<1>".into(),
                    from: "billing@acme.example".into(),
                    subject: "Ihre Rechnung".into(),
                    action: MailAction::Invoices {
                        company_id: "acme".into(),
                        company_name: "Acme".into(),
                        decision,
                        documents: vec![PlannedDocument {
                            attachment_index: 0,
                            attachment_name: "a.pdf".into(),
                            target_dir: PathBuf::from("/invoices"),
                            file_name: "2026-11-03 - Acme - R-1.pdf".into(),
                            target_path: PathBuf::from("/invoices/2026-11-03 - Acme - R-1.pdf"),
                            hash: "h".into(),
                            date: NaiveDate::from_ymd_opt(2026, 11, 3).unwrap(),
                            facts: InvoiceFacts::default(),
                            disposition,
                        }],
                    },
                },
            }],
        }
    }

    #[test]
    fn dry_run_names_mail_company_file_and_action() {
        let report = invoice_report(
            MailDecision::Dispose(DisposalAction::MoveTo("Done".into())),
            Disposition::Store,
        );
        let text = render(&[report], true);
        assert!(text.contains("nothing was changed"), "{text}");
        assert!(
            text.contains("\"Ihre Rechnung\" from billing@acme.example"),
            "{text}"
        );
        assert!(text.contains("Company: Acme"), "{text}");
        assert!(
            text.contains("Would save: /invoices/2026-11-03 - Acme - R-1.pdf"),
            "{text}"
        );
        assert!(text.contains("would move to Done"), "{text}");
    }

    #[test]
    fn deleting_is_described_precisely() {
        let trash = invoice_report(
            MailDecision::Dispose(DisposalAction::Trash),
            Disposition::Store,
        );
        assert!(render(&[trash], true).contains("would move to trash"));
        let permanent = invoice_report(
            MailDecision::Dispose(DisposalAction::DeletePermanently),
            Disposition::Store,
        );
        assert!(render(&[permanent], true).contains("would delete permanently"));
    }

    #[test]
    fn review_cases_explain_themselves() {
        let report = invoice_report(
            MailDecision::KeepAndFlag { reason: "x".into() },
            Disposition::StoreForReview(ReviewReason::NumberNotFound),
        );
        let text = render(&[report], true);
        assert!(text.contains("no invoice number found"), "{text}");
        assert!(
            text.contains("would stay in place and be flagged"),
            "{text}"
        );
    }

    #[test]
    fn account_errors_and_failures_are_visible() {
        let mut report = invoice_report(
            MailDecision::Dispose(DisposalAction::Trash),
            Disposition::Store,
        );
        report.error = Some("login failed".into());
        report.mails[0].outcome = Outcome::Failed {
            message: "disk full".into(),
        };
        let text = render(&[report], false);
        assert!(text.contains("ERROR: login failed"), "{text}");
        assert!(text.contains("FAILED: disk full"), "{text}");
    }

    #[test]
    fn failed_mails_never_claim_success() {
        let mut report = invoice_report(
            MailDecision::Dispose(DisposalAction::MoveTo("Done".into())),
            Disposition::Store,
        );
        report.mails[0].outcome = Outcome::Failed {
            message: "disk full".into(),
        };
        let text = render(&[report], false);
        assert!(text.contains("Tried to save"), "{text}");
        assert!(!text.contains("Saved:"), "{text}");
        assert!(!text.contains("(done)"), "{text}");
    }

    #[test]
    fn successful_real_runs_report_what_happened() {
        let mut report = invoice_report(
            MailDecision::Dispose(DisposalAction::MoveTo("Done".into())),
            Disposition::Store,
        );
        report.mails[0].outcome = Outcome::Completed;
        let text = render(&[report], false);
        assert!(text.contains("Saved: /invoices"), "{text}");
        assert!(text.contains("move to Done (done)"), "{text}");
    }

    fn link_report(outcome: Outcome, fetch_due: bool) -> AccountReport {
        AccountReport {
            account_id: "main".into(),
            error: None,
            mails: vec![MailReport {
                outcome,
                plan: MailPlan {
                    uid: 1,
                    message_id: "<1>".into(),
                    from: "billing@acme.example".into(),
                    subject: "Ihre Rechnung ist online".into(),
                    action: MailAction::PortalFetchRequired {
                        company_id: "acme".into(),
                        company_name: "Acme".into(),
                        already_recorded: false,
                        fetch_due,
                        assisted: false,
                        cleanup: None,
                        closed: None,
                        give_up_after_days: None,
                    },
                },
            }],
        }
    }

    #[test]
    fn assisted_link_mails_say_that_a_person_picks_them_up() {
        let assisted = |outcome, cleanup| {
            let mut report = link_report(outcome, false);
            let MailAction::PortalFetchRequired {
                assisted,
                cleanup: slot,
                ..
            } = &mut report.mails[0].plan.action
            else {
                unreachable!();
            };
            *assisted = true;
            *slot = cleanup;
            report
        };
        let waiting = render(&[assisted(Outcome::AwaitingInvoice, None)], false);
        assert!(waiting.contains("docsweep import"), "{waiting}");
        let stored = render(
            &[assisted(Outcome::Planned, Some(DisposalAction::Trash))],
            true,
        );
        assert!(stored.contains("Invoice: stored"), "{stored}");
    }

    #[test]
    fn a_link_mail_given_up_on_says_so_in_a_dry_run() {
        let mut report = link_report(Outcome::Planned, false);
        let MailAction::PortalFetchRequired {
            give_up_after_days, ..
        } = &mut report.mails[0].plan.action
        else {
            unreachable!();
        };
        *give_up_after_days = Some(45);
        let text = render(&[report], true);
        assert!(text.contains("within 45 days"), "{text}");
        assert!(
            text.contains("would stay in place and be flagged"),
            "{text}"
        );
    }

    #[test]
    fn a_link_mail_marked_as_done_says_so() {
        let mut report = link_report(Outcome::Completed, false);
        let MailAction::PortalFetchRequired { closed, .. } = &mut report.mails[0].plan.action
        else {
            unreachable!();
        };
        *closed = Some(LinkClosure::Dismissed);
        let text = render(&[report], false);
        assert!(text.contains("marked as done by you"), "{text}");
        assert!(text.contains("left alone"), "{text}");
    }

    #[test]
    fn link_mails_say_what_became_of_them() {
        let planned = render(&[link_report(Outcome::Planned, true)], true);
        assert!(
            planned.contains("would start a portal download"),
            "{planned}"
        );

        let checked = render(&[link_report(Outcome::Planned, false)], true);
        assert!(checked.contains("already checked today"), "{checked}");

        let waiting = render(&[link_report(Outcome::AwaitingInvoice, true)], false);
        assert!(waiting.contains("not in the portal yet"), "{waiting}");
        assert!(waiting.contains("stays in place"), "{waiting}");

        let resolved = Outcome::LinkResolved {
            disposal: DisposalAction::Trash,
        };
        let resolved = render(&[link_report(resolved, true)], false);
        assert!(resolved.contains("move to trash (done)"), "{resolved}");

        let failed = Outcome::Failed {
            message: "the portal download failed: boom".into(),
        };
        let failed = render(&[link_report(failed, true)], false);
        assert!(
            failed.contains("FAILED: the portal download failed"),
            "{failed}"
        );
        assert!(!failed.contains("(done)"), "{failed}");
    }

    #[test]
    fn empty_accounts_say_so() {
        let report = AccountReport {
            account_id: "main".into(),
            error: None,
            mails: Vec::new(),
        };
        assert!(render(&[report], true).contains("No relevant mails"));
    }

    #[test]
    fn fetch_reports_say_what_happened_to_each_entry() {
        let entry = |key: &str, outcome| EntryReport {
            key: key.to_owned(),
            date: None,
            invoice_no: None,
            amount: None,
            outcome,
        };
        let report = FetchReport {
            company_id: "acme".into(),
            company_name: "Acme".into(),
            entries: vec![
                entry(
                    "R-1",
                    EntryOutcome::WouldDownload {
                        file_name: Some("a.pdf".into()),
                    },
                ),
                entry("R-2", EntryOutcome::AlreadyKnown),
                entry("O-3", EntryOutcome::Missing),
                entry(
                    "R-4",
                    EntryOutcome::Failed {
                        message: "boom".into(),
                    },
                ),
            ],
            error: Some("gave up".into()),
            notices: Vec::new(),
            bot_check: None,
        };

        let text = render_fetch(&report, true);

        assert!(text.contains("Dry run: nothing was downloaded."), "{text}");
        assert!(text.contains("R-1: would save as a.pdf"), "{text}");
        assert!(text.contains("R-2: already known"), "{text}");
        assert!(text.contains("O-3: missing"), "{text}");
        assert!(text.contains("R-4: FAILED: boom"), "{text}");
        assert!(text.contains("ERROR: gave up"), "{text}");
    }

    #[test]
    fn a_run_that_ended_on_a_bot_check_suggests_assisted_mode() {
        let mut report = FetchReport {
            company_id: "acme".into(),
            company_name: "Acme".into(),
            entries: Vec::new(),
            error: Some("nobody answered the confirmation in time; the run was ended".into()),
            notices: Vec::new(),
            bot_check: None,
        };
        assert!(!render_fetch(&report, false).contains("assisted"));

        report.bot_check = Some(docsweep_core::portal::engine::BotCheck {
            portal_url: Some("https://portal.example/login".into()),
        });
        let text = render_fetch(&report, false);

        assert!(text.contains("portal.mode: assisted"), "{text}");
        assert!(text.contains("https://portal.example/login"), "{text}");
    }

    #[test]
    fn fetch_reports_name_a_repaired_recipe_and_its_backup() {
        let report = FetchReport {
            company_id: "acme".into(),
            company_name: "Acme".into(),
            entries: Vec::new(),
            error: None,
            notices: vec![
                RecipeNotice::Repaired {
                    step: "login".into(),
                    provider: "Anthropic".into(),
                    backup: "acme.yaml.bak".into(),
                },
                RecipeNotice::RepairPossible {
                    step: "extract_list".into(),
                },
                RecipeNotice::RepairNotDone {
                    step: "login".into(),
                    reason: "no answer".into(),
                },
            ],
            bot_check: None,
        };

        let text = render_fetch(&report, false);

        assert!(
            text.contains("NOTICE: Anthropic repaired the step `login`"),
            "{text}"
        );
        assert!(text.contains("acme.yaml.bak"), "{text}");
        assert!(
            text.contains("`extract_list` with the configured provider"),
            "{text}"
        );
        assert!(text.contains("was not repaired: no answer"), "{text}");
    }
}
