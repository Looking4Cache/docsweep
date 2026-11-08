//! Decides what should happen to a mail. Changes nothing itself; dry runs show
//! this plan and real runs execute it.

use std::collections::HashSet;
use std::path::PathBuf;

use chrono::NaiveDate;
use serde::Serialize;

use super::IncomingMail;
use super::rules::RuleSet;
use crate::config::{Company, CompanyDisposal, Config, DisposalMode};
use crate::document::{self, InvoiceFacts};
use crate::index::{DuplicateCheck, Index, IndexError, LinkClosure, Status};
use crate::naming::FilenameTemplate;
use crate::storage::{self, StorageError, sha256_hex};

/// Length of the hash excerpt in the name of a file without an invoice number.
const UNKNOWN_NUMBER_HASH_CHARS: usize = 8;

/// The final step for a mail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum DisposalAction {
    MoveTo(String),
    Trash,
    DeletePermanently,
}

/// Whether a mail is a PDF mail or a link mail, for the global cleanup mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailKind {
    Pdf,
    Link,
}

/// Applies the company override, then the global default.
pub fn disposal_for(config: &Config, company: &Company, kind: MailKind) -> DisposalAction {
    let global = &config.mail.disposal;
    let delete = || {
        if global.delete_to_trash {
            DisposalAction::Trash
        } else {
            DisposalAction::DeletePermanently
        }
    };
    let move_globally = || {
        let folder = global
            .move_to
            .clone()
            .expect("validation requires move_to for the modes that move");
        DisposalAction::MoveTo(folder)
    };
    match &company.disposal {
        Some(CompanyDisposal::Delete(_)) => delete(),
        Some(CompanyDisposal::Move(target)) => DisposalAction::MoveTo(target.move_to.clone()),
        None => match (global.mode, kind) {
            (DisposalMode::AllDelete, _) | (DisposalMode::PdfMoveLinkDelete, MailKind::Link) => {
                delete()
            }
            (DisposalMode::AllMove, _) | (DisposalMode::PdfMoveLinkDelete, MailKind::Pdf) => {
                move_globally()
            }
        },
    }
}

/// Why a stored document needs a person's attention.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ReviewReason {
    NumberNotFound,
    /// The company already has this number with a different file.
    NumberConflict {
        existing: Option<PathBuf>,
    },
}

/// What happens to one document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Disposition {
    /// Store as a complete invoice.
    Store,
    /// Store, but mark for review; the mail stays in place.
    StoreForReview(ReviewReason),
    /// Already stored and complete.
    AlreadyStored { path: Option<PathBuf> },
    /// Known, but not safe to rely on: awaiting review or the file is gone.
    Unresolved { reason: String },
}

/// One PDF of a mail with its planned treatment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlannedDocument {
    pub attachment_index: usize,
    pub attachment_name: String,
    pub target_dir: PathBuf,
    pub file_name: String,
    /// Where the file would land, including a suffix for a taken name.
    pub target_path: PathBuf,
    pub hash: String,
    pub date: NaiveDate,
    pub facts: InvoiceFacts,
    pub disposition: Disposition,
}

/// What happens to the mail once its documents are handled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum MailDecision {
    Dispose(DisposalAction),
    KeepAndFlag { reason: String },
}

/// What should happen to one relevant mail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MailAction {
    /// A PDF mail of a known company.
    Invoices {
        company_id: String,
        company_name: String,
        documents: Vec<PlannedDocument>,
        decision: MailDecision,
    },
    /// A link mail. It stays in place until its invoice is stored.
    PortalFetchRequired {
        company_id: String,
        company_name: String,
        already_recorded: bool,
        /// Whether a real run would start the portal download: a new mail always
        /// does, one that is still waiting at most once per day. Never for a
        /// company in assisted mode, whose invoices a person picks up.
        fetch_due: bool,
        /// The company is in assisted mode.
        assisted: bool,
        /// Assisted mode only: its invoice is stored, so a real run would do this.
        cleanup: Option<DisposalAction>,
        /// The mail no longer waits: its invoice settled it, a person marked it
        /// as done, or Docsweep gave up on it. Nothing more happens to it, neither
        /// a download nor a cleanup.
        closed: Option<LinkClosure>,
        /// Recipe mode only: the mail has waited at least this many days
        /// ([`LINK_MAIL_GIVE_UP_DAYS`]) without a matching invoice, so a
        /// real run stops waiting, starts no download and flags it.
        give_up_after_days: Option<u32>,
    },
    /// Rule matches, but there is no PDF and no portal, so nothing can be done.
    NoDocument {
        company_id: String,
        company_name: String,
    },
    /// No rule matches, but it looks like an invoice.
    UnknownInvoice { attachment_names: Vec<String> },
}

/// The plan for one mail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MailPlan {
    pub uid: u32,
    pub message_id: String,
    pub from: String,
    pub subject: String,
    pub action: MailAction,
}

/// Why planning failed.
#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error(transparent)]
    Index(#[from] IndexError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("company `{company}` has an invalid file name template: {problem}")]
    Template { company: String, problem: String },
}

/// Everything a plan depends on.
pub struct PlanContext<'a> {
    pub config: &'a Config,
    pub rules: &'a RuleSet<'a>,
    /// `None` when no index exists yet, which only happens in a first dry run.
    pub index: Option<&'a Index>,
    pub account_id: &'a str,
    /// Date for files whose document and mail both have no date.
    pub today: NaiveDate,
}

/// Plans one mail. `seen_hashes` remembers documents within this run, so a dry
/// run also recognises duplicates that a real run would have indexed by then.
/// Returns `None` for mails that are none of Docsweep's business.
pub fn plan_mail(
    context: &PlanContext<'_>,
    mail: &IncomingMail,
    seen_hashes: &mut HashSet<String>,
) -> Result<Option<MailPlan>, PlanError> {
    let action = match context.rules.find(mail) {
        Some(found) => plan_for_company(context, found.company, mail, seen_hashes)?,
        None if context.rules.looks_like_unknown_invoice(mail) => MailAction::UnknownInvoice {
            attachment_names: mail
                .attachments
                .iter()
                .filter(|a| a.is_pdf())
                .map(|a| a.name.clone())
                .collect(),
        },
        None => return Ok(None),
    };
    Ok(Some(MailPlan {
        uid: mail.uid,
        message_id: mail.message_id.clone(),
        from: mail.from.clone(),
        subject: mail.subject.clone(),
        action,
    }))
}

fn plan_for_company(
    context: &PlanContext<'_>,
    company: &Company,
    mail: &IncomingMail,
    seen_hashes: &mut HashSet<String>,
) -> Result<MailAction, PlanError> {
    let positions = context.rules.documents_of(company, mail);
    if positions.is_empty() {
        return if company.portal.is_some() {
            plan_link_mail(context, company, mail)
        } else {
            Ok(MailAction::NoDocument {
                company_id: company.id.clone(),
                company_name: company.name.clone(),
            })
        };
    }

    let mut documents = Vec::with_capacity(positions.len());
    for position in positions {
        documents.push(plan_document(
            context,
            company,
            mail,
            position,
            seen_hashes,
        )?);
    }
    let decision = decide(context.config, company, &documents);
    Ok(MailAction::Invoices {
        company_id: company.id.clone(),
        company_name: company.name.clone(),
        documents,
        decision,
    })
}

fn plan_link_mail(
    context: &PlanContext<'_>,
    company: &Company,
    mail: &IncomingMail,
) -> Result<MailAction, PlanError> {
    let id = (&company.id, context.account_id, &mail.message_id);
    let assisted = company.is_assisted();
    let (already_recorded, fetched_today, cleanup, closed) = match context.index {
        Some(index) => (
            index.is_portal_fetch_recorded(id.0, id.1, id.2)?,
            index.last_link_fetch_on(id.0)? == Some(context.today),
            (assisted && index.has_invoice_for_link_mail(id.0, id.1, id.2)?)
                .then(|| disposal_for(context.config, company, MailKind::Link)),
            index.link_mail_closure(id.0, id.1, id.2)?,
        ),
        None => (false, false, None, None),
    };
    let give_up_after_days = match context.index {
        Some(index) if !assisted && already_recorded && closed.is_none() => {
            give_up_after(context, index, mail, (id.0, id.1, id.2))?
        }
        _ => None,
    };
    Ok(MailAction::PortalFetchRequired {
        company_id: company.id.clone(),
        company_name: company.name.clone(),
        already_recorded,
        fetch_due: !assisted
            && closed.is_none()
            && give_up_after_days.is_none()
            && (!already_recorded || !fetched_today),
        assisted,
        cleanup,
        closed,
        give_up_after_days,
    })
}

/// How many days a link mail of a recipe company waits for its invoice.
pub const LINK_MAIL_GIVE_UP_DAYS: u32 = 15;

/// A waiting link mail of a recipe company is given up once it is at least
/// [`LINK_MAIL_GIVE_UP_DAYS`] old, counted from the mail's date or, without
/// one, from the day it was noted. A new mail always gets its first download, and
/// a mail whose invoice is in the index is left for the cleanup.
fn give_up_after(
    context: &PlanContext<'_>,
    index: &Index,
    mail: &IncomingMail,
    id: (&str, &str, &str),
) -> Result<Option<u32>, PlanError> {
    let days = LINK_MAIL_GIVE_UP_DAYS;
    let Some(since) = mail
        .date
        .or(index.link_mail_waiting_since(id.0, id.1, id.2)?)
    else {
        return Ok(None);
    };
    let waited = (context.today - since).num_days();
    if waited < i64::from(days) || index.has_invoice_for_link_mail(id.0, id.1, id.2)? {
        return Ok(None);
    }
    Ok(Some(days))
}

fn plan_document(
    context: &PlanContext<'_>,
    company: &Company,
    mail: &IncomingMail,
    position: usize,
    seen_hashes: &mut HashSet<String>,
) -> Result<PlannedDocument, PlanError> {
    let attachment = &mail.attachments[position];
    let hash = sha256_hex(&attachment.bytes);
    let facts = document::extract_facts(&attachment.bytes);
    let date = facts.date.or(mail.date).unwrap_or(context.today);

    let check = match context.index {
        Some(index) => index.check_duplicate(&company.id, facts.invoice_no.as_deref(), &hash)?,
        None => DuplicateCheck::New,
    };
    let disposition = disposition_of(check, facts.invoice_no.is_some());
    let disposition = if seen_hashes.contains(&hash) {
        // A real run would have indexed the earlier copy by now, so show what
        // it would decide: stored files are fine, review cases stay unresolved.
        match disposition {
            Disposition::Store | Disposition::AlreadyStored { .. } => {
                Disposition::AlreadyStored { path: None }
            }
            _ => Disposition::Unresolved {
                reason: "this file is already stored and waiting for review".to_owned(),
            },
        }
    } else {
        disposition
    };
    seen_hashes.insert(hash.clone());

    let target = target_of(
        context.config,
        company,
        date,
        facts.invoice_no.as_deref(),
        &hash,
    )?;

    Ok(PlannedDocument {
        attachment_index: position,
        attachment_name: attachment.name.clone(),
        target_dir: target.dir,
        file_name: target.file_name,
        target_path: target.path,
        hash,
        date,
        facts,
        disposition,
    })
}

/// Where a document goes.
pub(crate) struct Target {
    pub(crate) dir: PathBuf,
    pub(crate) file_name: String,
    /// Includes a suffix if the name is taken.
    pub(crate) path: PathBuf,
}

/// Names a document after the company's template. Without a number, part of the
/// hash keeps the name unique.
pub(crate) fn target_of(
    config: &Config,
    company: &Company,
    date: NaiveDate,
    invoice_no: Option<&str>,
    hash: &str,
) -> Result<Target, PlanError> {
    let template = FilenameTemplate::parse(&company.output.filename).map_err(|problem| {
        PlanError::Template {
            company: company.id.clone(),
            problem,
        }
    })?;
    let number = invoice_no.map_or_else(
        || format!("unbekannt-{}", &hash[..UNKNOWN_NUMBER_HASH_CHARS]),
        str::to_owned,
    );
    let file_name = template.render(date, &number);
    let dir = config.output_dir_of(company);
    let path = storage::free_path(&dir, &file_name)?;
    Ok(Target {
        dir,
        file_name,
        path,
    })
}

pub(crate) fn disposition_of(check: DuplicateCheck, has_number: bool) -> Disposition {
    match check {
        DuplicateCheck::New if has_number => Disposition::Store,
        DuplicateCheck::New => Disposition::StoreForReview(ReviewReason::NumberNotFound),
        DuplicateCheck::SameNumberDifferentFile(existing) => {
            Disposition::StoreForReview(ReviewReason::NumberConflict {
                existing: existing.file_path,
            })
        }
        DuplicateCheck::SameFile(record) if record.status != Status::Stored => {
            Disposition::Unresolved {
                reason: "this file is already stored and waiting for review".to_owned(),
            }
        }
        DuplicateCheck::SameFile(record) => match record.file_path {
            Some(path) if path.exists() => Disposition::AlreadyStored { path: Some(path) },
            Some(path) => Disposition::Unresolved {
                reason: format!(
                    "the index knows this file, but {} no longer exists",
                    path.display()
                ),
            },
            None => Disposition::Unresolved {
                reason: "the index knows this file, but has no path for it".to_owned(),
            },
        },
    }
}

/// A mail may only be cleaned up if every document is safely stored.
fn decide(config: &Config, company: &Company, documents: &[PlannedDocument]) -> MailDecision {
    let blocker = documents
        .iter()
        .find_map(|document| match &document.disposition {
            Disposition::Store | Disposition::AlreadyStored { .. } => None,
            Disposition::StoreForReview(ReviewReason::NumberNotFound) => {
                Some("no invoice number found; please review".to_owned())
            }
            Disposition::StoreForReview(ReviewReason::NumberConflict { .. }) => {
                Some("invoice number already known with a different file; please review".to_owned())
            }
            Disposition::Unresolved { reason } => Some(reason.clone()),
        });
    match blocker {
        Some(reason) => MailDecision::KeepAndFlag { reason },
        None => MailDecision::Dispose(disposal_for(config, company, MailKind::Pdf)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::test_pdf;
    use crate::index::{NewInvoice, Source};
    use crate::mail::test_mail;

    fn config_with(disposal: &str, company_extra: &str, dir: &str) -> Config {
        Config::from_yaml(&format!(
            r#"
mail:
  accounts:
    - {{ id: main, imap: "i.example:993", user: u, password: "keyring:p", watch: INBOX }}
  disposal: {disposal}
companies:
  - id: acme
    name: Acme
    mail: {{ from_domain: acme.example }}
    output: {{ dir: '{dir}', filename: "{{date:%Y-%m-%d}} - Acme - {{invoice_no}}.pdf" }}
    {company_extra}
"#
        ))
        .unwrap()
    }

    const MOVE_DEFAULT: &str =
        "{ mode: pdf_move_link_delete, move_to: Done, delete_to_trash: true }";

    fn plan(config: &Config, index: Option<&Index>, raw: &[u8]) -> Option<MailPlan> {
        let rules = RuleSet::new(config);
        let context = PlanContext {
            config,
            rules: &rules,
            index,
            account_id: "main",
            today: NaiveDate::from_ymd_opt(2026, 11, 6).unwrap(),
        };
        plan_mail(&context, &IncomingMail::parse(1, raw), &mut HashSet::new()).unwrap()
    }

    fn invoice_mail(number_line: &str) -> Vec<u8> {
        let pdf = test_pdf::with_text(&[number_line, "Rechnungsdatum: 03.11.2026"]);
        test_mail::raw("b@acme.example", "Post", "m1@x", &[("a.pdf", &pdf)])
    }

    fn only_document(plan: &MailPlan) -> (&PlannedDocument, &MailDecision) {
        match &plan.action {
            MailAction::Invoices {
                documents,
                decision,
                ..
            } => (&documents[0], decision),
            other => panic!("unexpected action {other:?}"),
        }
    }

    #[test]
    fn disposal_table_covers_modes_and_overrides() {
        use DisposalAction::*;
        let cases = [
            ("{ mode: all_delete }", "", MailKind::Pdf, Trash),
            ("{ mode: all_delete }", "", MailKind::Link, Trash),
            (
                "{ mode: all_delete, delete_to_trash: false }",
                "",
                MailKind::Pdf,
                DeletePermanently,
            ),
            (
                "{ mode: all_move, move_to: Done }",
                "",
                MailKind::Link,
                MoveTo("Done".into()),
            ),
            (MOVE_DEFAULT, "", MailKind::Pdf, MoveTo("Done".into())),
            (MOVE_DEFAULT, "", MailKind::Link, Trash),
            (MOVE_DEFAULT, "disposal: delete", MailKind::Pdf, Trash),
            (
                "{ mode: all_delete }",
                "disposal: { move_to: Acme }",
                MailKind::Link,
                MoveTo("Acme".into()),
            ),
        ];
        for (global, extra, kind, expected) in cases {
            let config = config_with(global, extra, "/x");
            let actual = disposal_for(&config, &config.companies[0], kind);
            assert_eq!(actual, expected, "{global} / {extra} / {kind:?}");
        }
    }

    #[test]
    fn irrelevant_mails_are_not_planned() {
        let config = config_with(MOVE_DEFAULT, "", "/x");
        let raw = test_mail::raw("a@other.example", "Urlaub", "m@x", &[]);
        assert_eq!(plan(&config, None, &raw), None);
    }

    #[test]
    fn complete_invoice_is_stored_and_disposed() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with(MOVE_DEFAULT, "", &dir.path().display().to_string());
        let plan = plan(&config, None, &invoice_mail("Rechnungsnummer: R-1")).unwrap();
        let (document, decision) = only_document(&plan);
        assert_eq!(document.disposition, Disposition::Store);
        assert_eq!(document.file_name, "2026-11-03 - Acme - R-1.pdf");
        assert_eq!(
            decision,
            &MailDecision::Dispose(DisposalAction::MoveTo("Done".into()))
        );
    }

    #[test]
    fn mail_date_is_the_fallback_for_the_document_date() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with(MOVE_DEFAULT, "", &dir.path().display().to_string());
        let pdf = test_pdf::with_text(&["Rechnungsnummer: R-1"]);
        let raw = test_mail::raw("b@acme.example", "Post", "m1@x", &[("a.pdf", &pdf)]);
        let plan = plan(&config, None, &raw).unwrap();
        assert_eq!(
            only_document(&plan).0.file_name,
            "2026-11-03 - Acme - R-1.pdf"
        );
    }

    #[test]
    fn missing_number_keeps_the_mail_and_uses_the_hash_in_the_name() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with(
            MOVE_DEFAULT,
            "disposal: delete",
            &dir.path().display().to_string(),
        );
        let plan = plan(&config, None, &invoice_mail("Keine Nummer hier")).unwrap();
        let (document, decision) = only_document(&plan);
        assert_eq!(
            document.disposition,
            Disposition::StoreForReview(ReviewReason::NumberNotFound)
        );
        let expected = format!("2026-11-03 - Acme - unbekannt-{}.pdf", &document.hash[..8]);
        assert_eq!(document.file_name, expected);
        assert!(matches!(decision, MailDecision::KeepAndFlag { .. }));
    }

    fn index_with(path: &std::path::Path, number: &str, hash: &str, status: Status) -> Index {
        let index = Index::open_in_memory().unwrap();
        index
            .insert(&NewInvoice {
                company_id: "acme".into(),
                invoice_date: None,
                invoice_no: Some(number.into()),
                amount: None,
                currency: None,
                file_path: path.to_path_buf(),
                file_hash: hash.into(),
                source: Source::Mail,
                mail_account: None,
                mail_id: None,
                portal_key: None,
                status,
            })
            .unwrap();
        index
    }

    fn pdf_mail_and_hash() -> (Vec<u8>, String) {
        let pdf = test_pdf::with_text(&["Rechnungsnummer: R-1"]);
        let hash = sha256_hex(&pdf);
        (
            test_mail::raw("b@acme.example", "Post", "m1@x", &[("a.pdf", &pdf)]),
            hash,
        )
    }

    #[test]
    fn identical_file_with_existing_copy_may_be_disposed() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with(MOVE_DEFAULT, "", &dir.path().display().to_string());
        let (raw, hash) = pdf_mail_and_hash();
        let existing = dir.path().join("old.pdf");
        std::fs::write(&existing, b"x").unwrap();
        let index = index_with(&existing, "R-1", &hash, Status::Stored);

        let plan = plan(&config, Some(&index), &raw).unwrap();
        let (document, decision) = only_document(&plan);
        assert!(matches!(
            document.disposition,
            Disposition::AlreadyStored { .. }
        ));
        assert!(matches!(decision, MailDecision::Dispose(_)));
    }

    #[test]
    fn identical_file_whose_copy_is_gone_keeps_the_mail() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with(MOVE_DEFAULT, "", &dir.path().display().to_string());
        let (raw, hash) = pdf_mail_and_hash();
        let index = index_with(&dir.path().join("gone.pdf"), "R-1", &hash, Status::Stored);

        let plan = plan(&config, Some(&index), &raw).unwrap();
        assert!(matches!(
            only_document(&plan).1,
            MailDecision::KeepAndFlag { .. }
        ));
    }

    #[test]
    fn identical_file_still_in_review_keeps_the_mail() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with(MOVE_DEFAULT, "", &dir.path().display().to_string());
        let (raw, hash) = pdf_mail_and_hash();
        let existing = dir.path().join("old.pdf");
        std::fs::write(&existing, b"x").unwrap();
        let index = index_with(&existing, "R-1", &hash, Status::NeedsReview);

        let plan = plan(&config, Some(&index), &raw).unwrap();
        assert!(matches!(
            only_document(&plan).1,
            MailDecision::KeepAndFlag { .. }
        ));
    }

    #[test]
    fn same_number_with_different_file_is_stored_with_suffix_for_review() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with(MOVE_DEFAULT, "", &dir.path().display().to_string());
        let (raw, _) = pdf_mail_and_hash();
        let existing = dir.path().join("2026-11-03 - Acme - R-1.pdf");
        std::fs::write(&existing, b"x").unwrap();
        let index = index_with(&existing, "R-1", "another-hash", Status::Stored);

        let plan = plan(&config, Some(&index), &raw).unwrap();
        let (document, decision) = only_document(&plan);
        assert!(matches!(
            document.disposition,
            Disposition::StoreForReview(ReviewReason::NumberConflict { .. })
        ));
        assert_eq!(
            document.target_path,
            dir.path().join("2026-11-03 - Acme - R-1 (2).pdf")
        );
        assert!(matches!(decision, MailDecision::KeepAndFlag { .. }));
    }

    #[test]
    fn one_unresolved_document_blocks_the_whole_mail() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with(MOVE_DEFAULT, "", &dir.path().display().to_string());
        let good = test_pdf::with_text(&["Rechnungsnummer: R-1"]);
        let terms = test_pdf::with_text(&["Allgemeine Geschäftsbedingungen"]);
        let raw = test_mail::raw(
            "b@acme.example",
            "Post",
            "m@x",
            &[("a.pdf", &good), ("agb.pdf", &terms)],
        );
        let plan = plan(&config, None, &raw).unwrap();
        let MailAction::Invoices {
            documents,
            decision,
            ..
        } = &plan.action
        else {
            panic!("expected invoices");
        };
        assert_eq!(documents.len(), 2);
        assert!(matches!(decision, MailDecision::KeepAndFlag { .. }));
    }

    #[test]
    fn duplicate_within_one_run_is_recognised_without_an_index() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with(MOVE_DEFAULT, "", &dir.path().display().to_string());
        let rules = RuleSet::new(&config);
        let context = PlanContext {
            config: &config,
            rules: &rules,
            index: None,
            account_id: "main",
            today: NaiveDate::from_ymd_opt(2030, 1, 1).unwrap(),
        };
        let (raw, _) = pdf_mail_and_hash();
        let mail = IncomingMail::parse(1, &raw);
        let mut seen = HashSet::new();
        let first = plan_mail(&context, &mail, &mut seen).unwrap().unwrap();
        let second = plan_mail(&context, &mail, &mut seen).unwrap().unwrap();
        assert_eq!(only_document(&first).0.disposition, Disposition::Store);
        assert!(matches!(
            only_document(&second).0.disposition,
            Disposition::AlreadyStored { .. }
        ));
    }

    #[test]
    fn dry_run_duplicate_of_a_review_case_stays_unresolved() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with(MOVE_DEFAULT, "", &dir.path().display().to_string());
        let rules = RuleSet::new(&config);
        let context = PlanContext {
            config: &config,
            rules: &rules,
            index: None,
            account_id: "main",
            today: NaiveDate::from_ymd_opt(2030, 1, 1).unwrap(),
        };
        let mail = IncomingMail::parse(1, &invoice_mail("Keine Nummer"));
        let mut seen = HashSet::new();
        plan_mail(&context, &mail, &mut seen).unwrap();
        let second = plan_mail(&context, &mail, &mut seen).unwrap().unwrap();
        assert!(matches!(
            only_document(&second).1,
            MailDecision::KeepAndFlag { .. }
        ));
    }

    #[test]
    fn link_mail_of_a_portal_company_stays_even_if_it_should_be_deleted() {
        let config = config_with(
            MOVE_DEFAULT,
            "disposal: delete\n    portal: { credentials: \"keyring:c\", recipe: r.yaml }",
            "/x",
        );
        let raw = test_mail::raw("b@acme.example", "Rechnung online", "m@x", &[]);
        let plan = plan(&config, None, &raw).unwrap();
        assert!(matches!(
            plan.action,
            MailAction::PortalFetchRequired {
                already_recorded: false,
                fetch_due: true,
                ..
            }
        ));
    }

    #[test]
    fn a_waiting_link_mail_is_due_once_per_day_and_a_new_one_always() {
        let config = config_with(
            MOVE_DEFAULT,
            "portal: { credentials: \"keyring:c\", recipe: r.yaml }",
            "/x",
        );
        let index = Index::open_in_memory().unwrap();
        // The date `plan` uses as today.
        let today = NaiveDate::from_ymd_opt(2026, 11, 6).unwrap();
        index
            .record_portal_fetch_required("acme", "main", "old@x", None)
            .unwrap();
        let plan_for = |id: &str| {
            let raw = test_mail::raw("b@acme.example", "Rechnung online", id, &[]);
            plan(&config, Some(&index), &raw).unwrap().action
        };
        let due = |action: MailAction| match action {
            MailAction::PortalFetchRequired { fetch_due, .. } => fetch_due,
            other => panic!("unexpected action {other:?}"),
        };

        assert!(due(plan_for("old@x")), "not tried yet");
        index
            .note_link_fetch("acme", today.pred_opt().unwrap())
            .unwrap();
        assert!(due(plan_for("old@x")), "tried yesterday");
        index.note_link_fetch("acme", today).unwrap();
        assert!(!due(plan_for("old@x")), "tried today");
        assert!(due(plan_for("new@x")), "a new mail does not wait");
    }

    const ASSISTED: &str = "portal: { mode: assisted, url: \"https://portal.example/\" }";

    #[test]
    fn a_link_mail_of_an_assisted_company_is_noted_without_any_download_due() {
        let config = config_with(MOVE_DEFAULT, ASSISTED, "/x");
        let index = Index::open_in_memory().unwrap();
        let raw = test_mail::raw("b@acme.example", "Rechnung online", "m@x", &[]);

        let action = plan(&config, Some(&index), &raw).unwrap().action;

        assert!(matches!(
            action,
            MailAction::PortalFetchRequired {
                assisted: true,
                fetch_due: false,
                already_recorded: false,
                cleanup: None,
                ..
            }
        ));
    }

    #[test]
    fn a_stored_invoice_shows_up_as_the_cleanup_of_an_assisted_link_mail() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with(MOVE_DEFAULT, ASSISTED, "/x");
        let index = Index::open_in_memory().unwrap();
        index
            .record_portal_fetch_required("acme", "main", "m@x", None)
            .unwrap();
        let file = dir.path().join("R-1.pdf");
        std::fs::write(&file, b"x").unwrap();
        index
            .insert(&NewInvoice {
                company_id: "acme".into(),
                invoice_date: None,
                invoice_no: Some("R-1".into()),
                amount: None,
                currency: None,
                file_path: file,
                file_hash: "h".into(),
                source: Source::Portal,
                mail_account: None,
                mail_id: None,
                portal_key: None,
                status: Status::Stored,
            })
            .unwrap();
        let raw = test_mail::raw("b@acme.example", "Rechnung online", "m@x", &[]);

        let action = plan(&config, Some(&index), &raw).unwrap().action;

        let MailAction::PortalFetchRequired { cleanup, .. } = action else {
            panic!("unexpected action {action:?}");
        };
        assert_eq!(cleanup, Some(DisposalAction::Trash));
    }

    #[test]
    fn mail_without_pdf_and_portal_needs_no_action() {
        let config = config_with(MOVE_DEFAULT, "", "/x");
        let raw = test_mail::raw("b@acme.example", "Newsletter", "m@x", &[]);
        assert!(matches!(
            plan(&config, None, &raw).unwrap().action,
            MailAction::NoDocument { .. }
        ));
    }

    #[test]
    fn unknown_sender_with_invoice_pdf_is_only_a_hint() {
        let config = config_with(MOVE_DEFAULT, "", "/x");
        let raw = test_mail::raw(
            "a@new.example",
            "Ihre Rechnung",
            "m@x",
            &[("r.pdf", b"%PDF-1.7")],
        );
        assert_eq!(
            plan(&config, None, &raw).unwrap().action,
            MailAction::UnknownInvoice {
                attachment_names: vec!["r.pdf".into()]
            }
        );
    }
}
