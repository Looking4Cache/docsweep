//! Runs a scan: plans each mail and, in a real run, carries the plan out.
//!
//! The order is the safety rule of the project: store and verify the file, add
//! it to the index, and only then touch the mail. Any failure leaves the mail in
//! place with a flag, and later mails are still processed.

use std::collections::HashSet;
use std::fs;

use chrono::{Local, NaiveDate};

use super::imap::{ImapMailbox, TransportSecurity};
use super::plan::{
    DisposalAction, Disposition, MailAction, MailDecision, MailPlan, PlanContext, PlanError,
    plan_mail,
};
use super::rules::RuleSet;
use super::{IncomingMail, MailError, MailReader, MailWriter};
use crate::config::{Config, MailAccount};
use crate::index::{Index, IndexError, NewInvoice, Source, Status};
use crate::secrets::{SecretError, SecretStore};
use crate::storage::{self, StorageError};

/// What came of a planned mail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Dry run: nothing was changed.
    Planned,
    /// Everything planned has been carried out.
    Completed,
    /// Documents are stored, but the mail stays flagged for a person to look at.
    KeptForReview { reason: String },
    /// Something went wrong. The mail stays, flagged if that was possible.
    Failed { message: String },
}

/// One mail in a report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailReport {
    pub plan: MailPlan,
    pub outcome: Outcome,
}

/// The result for one account.
#[derive(Debug)]
pub struct AccountReport {
    pub account_id: String,
    pub mails: Vec<MailReport>,
    /// Set if the account could not be processed (completely).
    pub error: Option<String>,
}

/// Why a scan could not run at all.
#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error(transparent)]
    Index(#[from] IndexError),
    #[error(transparent)]
    Plan(#[from] PlanError),
    #[error(transparent)]
    Mail(#[from] MailError),
    #[error(transparent)]
    Secret(#[from] SecretError),
    #[error("account `{0}` has no valid password reference or endpoint")]
    InvalidAccount(String),
}

/// Scans all accounts. Problems with one account do not stop the others. A dry
/// run opens the index read-only and does not create it.
pub async fn run_mail_scan(
    config: &Config,
    secrets: &dyn SecretStore,
    index_path: &std::path::Path,
    dry_run: bool,
) -> Result<Vec<AccountReport>, ScanError> {
    let rules = RuleSet::new(config);
    let index = if dry_run {
        Index::open_read_only(index_path)?
    } else {
        Some(Index::open(index_path)?)
    };
    let today = Local::now().date_naive();

    let mut reports = Vec::with_capacity(config.mail.accounts.len());
    for account in &config.mail.accounts {
        let mut mails = Vec::new();
        let result = scan_account(
            config,
            &rules,
            index.as_ref(),
            account,
            secrets,
            today,
            dry_run,
            &mut mails,
        )
        .await;
        reports.push(AccountReport {
            account_id: account.id.clone(),
            mails,
            error: result.err().map(|error| error.to_string()),
        });
    }
    Ok(reports)
}

#[allow(
    clippy::too_many_arguments,
    reason = "a private step of `run_mail_scan`"
)]
async fn scan_account(
    config: &Config,
    rules: &RuleSet<'_>,
    index: Option<&Index>,
    account: &MailAccount,
    secrets: &dyn SecretStore,
    today: NaiveDate,
    dry_run: bool,
    mails: &mut Vec<MailReport>,
) -> Result<(), ScanError> {
    let invalid = || ScanError::InvalidAccount(account.id.clone());
    let (host, port) = account.endpoint().ok_or_else(invalid)?;
    let secret_id = account.password.id().ok_or_else(invalid)?;
    let password = secrets.get(secret_id)?;
    let security = TransportSecurity::for_port(port);
    let context = PlanContext {
        config,
        rules,
        index,
        account_id: &account.id,
        today,
    };

    if dry_run {
        let mut reader = ImapMailbox::connect_read_only(
            host,
            port,
            security,
            &account.user,
            &password,
            &account.watch,
        )
        .await?;
        let result = scan_read_only(&context, &mut reader, mails).await;
        reader.logout().await;
        result
    } else {
        let index = index.expect("a real run always has an index");
        let mut writer = ImapMailbox::connect_writable(
            host,
            port,
            security,
            &account.user,
            &password,
            &account.watch,
        )
        .await?;
        let result = scan_and_execute(&context, index, &mut writer, mails).await;
        writer.logout().await;
        result
    }
}

/// Dry run for one mailbox.
pub async fn scan_read_only<R: MailReader>(
    context: &PlanContext<'_>,
    reader: &mut R,
    mails: &mut Vec<MailReport>,
) -> Result<(), ScanError> {
    let mut seen_hashes = HashSet::new();
    for uid in candidate_uids(context.rules, reader).await? {
        let raw = reader.fetch_raw(uid).await?;
        let mail = IncomingMail::parse(uid, &raw);
        if let Some(plan) = plan_mail(context, &mail, &mut seen_hashes)? {
            mails.push(MailReport {
                plan,
                outcome: Outcome::Planned,
            });
        }
    }
    Ok(())
}

/// Real run for one mailbox. Each mail is planned right before it is executed,
/// so it sees everything earlier mails have added to the index.
pub async fn scan_and_execute<W: MailWriter>(
    context: &PlanContext<'_>,
    index: &Index,
    writer: &mut W,
    mails: &mut Vec<MailReport>,
) -> Result<(), ScanError> {
    for uid in candidate_uids(context.rules, writer).await? {
        let raw = writer.fetch_raw(uid).await?;
        let mail = IncomingMail::parse(uid, &raw);
        // Duplicates within a real run are found through the index.
        let Some(plan) = plan_mail(context, &mail, &mut HashSet::new())? else {
            continue;
        };
        let outcome = execute_plan(&plan, &mail, context.account_id, index, writer).await;
        mails.push(MailReport { plan, outcome });
    }
    Ok(())
}

async fn candidate_uids<R: MailReader>(
    rules: &RuleSet<'_>,
    reader: &mut R,
) -> Result<Vec<u32>, MailError> {
    let headers = reader.list_headers().await?;
    Ok(headers
        .into_iter()
        .filter(|(uid, raw)| rules.is_candidate(&IncomingMail::parse(*uid, raw)))
        .map(|(uid, _)| uid)
        .collect())
}

async fn execute_plan<W: MailWriter>(
    plan: &MailPlan,
    mail: &IncomingMail,
    account_id: &str,
    index: &Index,
    writer: &mut W,
) -> Outcome {
    match &plan.action {
        MailAction::Invoices {
            company_id,
            documents,
            decision,
            ..
        } => {
            for document in documents {
                let stored = store_document(document, company_id, mail, account_id, index);
                if let Err(error) = stored {
                    return fail(writer, mail.uid, error.to_string()).await;
                }
            }
            match decision {
                MailDecision::KeepAndFlag { reason } => match writer.flag(mail.uid).await {
                    Ok(()) => Outcome::KeptForReview {
                        reason: reason.clone(),
                    },
                    Err(error) => Outcome::Failed {
                        message: format!("{reason}; flagging failed too: {error}"),
                    },
                },
                MailDecision::Dispose(action) => {
                    if let Err(error) = dispose(writer, mail.uid, action).await {
                        return fail(writer, mail.uid, error.to_string()).await;
                    }
                    match index.mark_mail_disposed(account_id, &mail.message_id) {
                        Ok(()) => Outcome::Completed,
                        // The mail has been handled, so flagging would be wrong.
                        Err(error) => Outcome::Failed {
                            message: format!(
                                "mail cleaned up, but not noted in the index: {error}"
                            ),
                        },
                    }
                }
            }
        }
        MailAction::PortalFetchRequired { company_id, .. } => {
            match index.record_portal_fetch_required(
                company_id,
                account_id,
                &mail.message_id,
                mail.date,
            ) {
                Ok(_) => Outcome::Completed,
                Err(error) => fail(writer, mail.uid, error.to_string()).await,
            }
        }
        MailAction::NoDocument { .. } | MailAction::UnknownInvoice { .. } => Outcome::Completed,
    }
}

/// Why storing a document failed.
#[derive(Debug, thiserror::Error)]
enum StoreError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Index(#[from] IndexError),
}

fn store_document(
    document: &super::plan::PlannedDocument,
    company_id: &str,
    mail: &IncomingMail,
    account_id: &str,
    index: &Index,
) -> Result<(), StoreError> {
    let status = match document.disposition {
        Disposition::Store => Status::Stored,
        Disposition::StoreForReview(_) => Status::NeedsReview,
        Disposition::AlreadyStored { .. } | Disposition::Unresolved { .. } => return Ok(()),
    };
    let bytes = &mail.attachments[document.attachment_index].bytes;
    let path = storage::store_verified(&document.target_dir, &document.file_name, bytes)?;
    let inserted = index.insert(&NewInvoice {
        company_id: company_id.to_owned(),
        invoice_date: Some(document.date),
        invoice_no: document.facts.invoice_no.clone(),
        amount: document.facts.amount.clone(),
        currency: document.facts.currency.clone(),
        file_path: path.clone(),
        file_hash: document.hash.clone(),
        source: Source::Mail,
        mail_account: Some(account_id.to_owned()),
        mail_id: Some(mail.message_id.clone()),
        status,
    });
    if let Err(error) = inserted {
        // Without an index entry a retry could not recognise this file, and
        // would store a second copy of it.
        let _ = fs::remove_file(&path);
        return Err(error.into());
    }
    Ok(())
}

async fn dispose<W: MailWriter>(
    writer: &mut W,
    uid: u32,
    action: &DisposalAction,
) -> Result<(), MailError> {
    match action {
        DisposalAction::MoveTo(folder) => writer.move_to(uid, folder).await,
        DisposalAction::Trash => writer.move_to_trash(uid).await,
        DisposalAction::DeletePermanently => writer.delete_permanently(uid).await,
    }
}

async fn fail<W: MailWriter>(writer: &mut W, uid: u32, message: String) -> Outcome {
    match writer.flag(uid).await {
        Ok(()) => Outcome::Failed { message },
        Err(flag_error) => Outcome::Failed {
            message: format!("{message}; flagging failed too: {flag_error}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::test_pdf;
    use crate::mail::test_mail;
    use std::collections::BTreeMap;

    /// Records all changes; can be told to fail at disposal.
    #[derive(Default)]
    struct FakeMailbox {
        mails: BTreeMap<u32, Vec<u8>>,
        moved: Vec<(u32, String)>,
        trashed: Vec<u32>,
        deleted: Vec<u32>,
        flagged: Vec<u32>,
        fail_disposal: bool,
    }

    impl FakeMailbox {
        fn with(mails: Vec<Vec<u8>>) -> Self {
            Self {
                mails: (1u32..).zip(mails).collect(),
                ..Self::default()
            }
        }

        fn untouched(&self) -> bool {
            self.moved.is_empty() && self.trashed.is_empty() && self.deleted.is_empty()
        }

        fn disposal_result(&self) -> Result<(), MailError> {
            if self.fail_disposal {
                Err(MailError("server said no".into()))
            } else {
                Ok(())
            }
        }
    }

    impl MailReader for FakeMailbox {
        async fn list_headers(&mut self) -> Result<Vec<(u32, Vec<u8>)>, MailError> {
            Ok(self.mails.iter().map(|(k, v)| (*k, v.clone())).collect())
        }
        async fn fetch_raw(&mut self, uid: u32) -> Result<Vec<u8>, MailError> {
            Ok(self.mails[&uid].clone())
        }
    }

    impl MailWriter for FakeMailbox {
        async fn move_to(&mut self, uid: u32, folder: &str) -> Result<(), MailError> {
            self.disposal_result()?;
            self.moved.push((uid, folder.to_owned()));
            Ok(())
        }
        async fn move_to_trash(&mut self, uid: u32) -> Result<(), MailError> {
            self.disposal_result()?;
            self.trashed.push(uid);
            Ok(())
        }
        async fn delete_permanently(&mut self, uid: u32) -> Result<(), MailError> {
            self.disposal_result()?;
            self.deleted.push(uid);
            Ok(())
        }
        async fn flag(&mut self, uid: u32) -> Result<(), MailError> {
            self.flagged.push(uid);
            Ok(())
        }
    }

    struct Setup {
        dir: tempfile::TempDir,
        config: Config,
    }

    fn setup(global: &str, extra: &str) -> Setup {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::from_yaml(&format!(
            r#"
mail:
  accounts:
    - {{ id: main, imap: "i.example:993", user: u, password: "keyring:p", watch: INBOX }}
  disposal: {global}
companies:
  - id: acme
    name: Acme
    mail: {{ from_domain: acme.example }}
    output: {{ dir: "{}", filename: "{{invoice_no}}.pdf" }}
    {extra}
"#,
            dir.path().display()
        ))
        .unwrap();
        Setup { dir, config }
    }

    fn pdf_mail(id: &str, number: &str) -> Vec<u8> {
        let pdf = test_pdf::with_text(&[&format!("Rechnungsnummer: {number}")]);
        test_mail::raw("b@acme.example", "Post", id, &[("a.pdf", &pdf)])
    }

    fn run_real(setup: &Setup, index: &Index, mailbox: &mut FakeMailbox) -> Vec<MailReport> {
        let rules = RuleSet::new(&setup.config);
        let context = PlanContext {
            config: &setup.config,
            rules: &rules,
            index: Some(index),
            account_id: "main",
            today: NaiveDate::from_ymd_opt(2030, 1, 1).unwrap(),
        };
        let mut mails = Vec::new();
        block_on(scan_and_execute(&context, index, mailbox, &mut mails)).unwrap();
        mails
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(future)
    }

    const MOVE: &str = "{ mode: pdf_move_link_delete, move_to: Done }";

    #[test]
    fn stores_indexes_and_then_moves() {
        let setup = setup(MOVE, "");
        let index = Index::open_in_memory().unwrap();
        let mut mailbox = FakeMailbox::with(vec![pdf_mail("1@x", "R-1")]);

        let reports = run_real(&setup, &index, &mut mailbox);

        assert_eq!(reports[0].outcome, Outcome::Completed);
        assert!(setup.dir.path().join("R-1.pdf").exists());
        let row = index.invoices_of("acme").unwrap().remove(0);
        assert_eq!(row.status, Status::Stored);
        assert_eq!(row.mail_id.as_deref(), Some("1@x"));
        assert!(row.disposed_at.is_some());
        assert_eq!(mailbox.moved, [(1, "Done".to_owned())]);
    }

    #[test]
    fn delete_goes_to_the_trash_by_default() {
        let setup = setup(MOVE, "disposal: delete");
        let index = Index::open_in_memory().unwrap();
        let mut mailbox = FakeMailbox::with(vec![pdf_mail("1@x", "R-1")]);
        run_real(&setup, &index, &mut mailbox);
        assert_eq!(mailbox.trashed, [1]);
        assert!(mailbox.deleted.is_empty());
    }

    #[test]
    fn permanent_delete_needs_the_explicit_setting() {
        let setup = setup("{ mode: all_delete, delete_to_trash: false }", "");
        let index = Index::open_in_memory().unwrap();
        let mut mailbox = FakeMailbox::with(vec![pdf_mail("1@x", "R-1")]);
        run_real(&setup, &index, &mut mailbox);
        assert_eq!(mailbox.deleted, [1]);
    }

    #[test]
    fn failed_storage_leaves_the_mail_flagged_and_not_indexed() {
        let setup = setup(MOVE, "");
        // A file where the target directory should be makes storing impossible.
        let blocked = setup.dir.path().join("blocked");
        fs::write(&blocked, b"x").unwrap();
        let mut config = setup.config.clone();
        config.companies[0].output.dir = blocked.display().to_string();
        let setup = Setup { config, ..setup };
        let index = Index::open_in_memory().unwrap();
        let mut mailbox = FakeMailbox::with(vec![pdf_mail("1@x", "R-1")]);

        let rules = RuleSet::new(&setup.config);
        let context = PlanContext {
            config: &setup.config,
            rules: &rules,
            index: Some(&index),
            account_id: "main",
            today: NaiveDate::from_ymd_opt(2030, 1, 1).unwrap(),
        };
        let mut reports = Vec::new();
        block_on(scan_and_execute(
            &context,
            &index,
            &mut mailbox,
            &mut reports,
        ))
        .unwrap();

        assert!(matches!(reports[0].outcome, Outcome::Failed { .. }));
        assert_eq!(mailbox.flagged, [1]);
        assert!(mailbox.untouched());
        assert!(index.invoices_of("acme").unwrap().is_empty());
    }

    #[test]
    fn index_failure_removes_the_file_and_keeps_the_mail() {
        let setup = setup(MOVE, "");
        let index = Index::open_in_memory().unwrap();
        // Dropping the table makes every later index write fail, while the
        // earlier duplicate check is answered from a copy of the plan.
        let raw = pdf_mail("1@x", "R-1");
        let mail = IncomingMail::parse(1, &raw);
        let rules = RuleSet::new(&setup.config);
        let context = PlanContext {
            config: &setup.config,
            rules: &rules,
            index: Some(&index),
            account_id: "main",
            today: NaiveDate::from_ymd_opt(2030, 1, 1).unwrap(),
        };
        let plan = plan_mail(&context, &mail, &mut HashSet::new())
            .unwrap()
            .unwrap();
        let broken = Index::open_in_memory().unwrap();
        broken.break_for_test();

        let mut mailbox = FakeMailbox::with(vec![raw]);
        let outcome = block_on(execute_plan(&plan, &mail, "main", &broken, &mut mailbox));

        assert!(matches!(outcome, Outcome::Failed { .. }));
        assert!(mailbox.untouched());
        assert_eq!(mailbox.flagged, [1]);
        assert!(!setup.dir.path().join("R-1.pdf").exists());
    }

    #[test]
    fn failed_disposal_flags_the_mail_but_keeps_the_invoice() {
        let setup = setup(MOVE, "");
        let index = Index::open_in_memory().unwrap();
        let mut mailbox = FakeMailbox::with(vec![pdf_mail("1@x", "R-1")]);
        mailbox.fail_disposal = true;

        let reports = run_real(&setup, &index, &mut mailbox);

        assert!(matches!(reports[0].outcome, Outcome::Failed { .. }));
        assert_eq!(mailbox.flagged, [1]);
        assert!(setup.dir.path().join("R-1.pdf").exists());
        assert_eq!(index.invoices_of("acme").unwrap()[0].disposed_at, None);
    }

    #[test]
    fn retry_after_failed_disposal_does_not_store_again() {
        let setup = setup(MOVE, "");
        let index = Index::open_in_memory().unwrap();
        let mut mailbox = FakeMailbox::with(vec![pdf_mail("1@x", "R-1")]);
        mailbox.fail_disposal = true;
        run_real(&setup, &index, &mut mailbox);

        mailbox.fail_disposal = false;
        let reports = run_real(&setup, &index, &mut mailbox);

        assert_eq!(reports[0].outcome, Outcome::Completed);
        assert_eq!(mailbox.moved.len(), 1);
        assert_eq!(index.invoices_of("acme").unwrap().len(), 1);
        assert_eq!(fs::read_dir(setup.dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn review_case_is_stored_but_the_mail_stays_flagged() {
        let setup = setup(MOVE, "disposal: delete");
        let index = Index::open_in_memory().unwrap();
        let pdf = test_pdf::with_text(&["Keine Nummer"]);
        let raw = test_mail::raw("b@acme.example", "Post", "1@x", &[("a.pdf", &pdf)]);
        let mut mailbox = FakeMailbox::with(vec![raw]);

        let reports = run_real(&setup, &index, &mut mailbox);

        assert!(matches!(reports[0].outcome, Outcome::KeptForReview { .. }));
        assert!(mailbox.untouched());
        assert_eq!(mailbox.flagged, [1]);
        assert_eq!(
            index.invoices_of("acme").unwrap()[0].status,
            Status::NeedsReview
        );
    }

    #[test]
    fn failing_mail_does_not_stop_the_next_one() {
        let setup = setup(MOVE, "");
        let index = Index::open_in_memory().unwrap();
        let no_number = test_mail::raw(
            "b@acme.example",
            "Post",
            "1@x",
            &[("a.pdf", &test_pdf::with_text(&["Keine Nummer"]))],
        );
        let mut mailbox = FakeMailbox::with(vec![no_number, pdf_mail("2@x", "R-2")]);

        let reports = run_real(&setup, &index, &mut mailbox);

        assert_eq!(reports.len(), 2);
        assert_eq!(reports[1].outcome, Outcome::Completed);
        assert_eq!(mailbox.moved, [(2, "Done".to_owned())]);
    }

    #[test]
    fn link_mail_is_recorded_once_and_stays_untouched() {
        let setup = setup(
            MOVE,
            "disposal: delete\n    portal: { credentials: \"keyring:c\", recipe: r.yaml }",
        );
        let index = Index::open_in_memory().unwrap();
        let raw = test_mail::raw("b@acme.example", "Rechnung online", "1@x", &[]);
        let mut mailbox = FakeMailbox::with(vec![raw]);

        run_real(&setup, &index, &mut mailbox);
        run_real(&setup, &index, &mut mailbox);

        assert!(mailbox.untouched());
        assert!(mailbox.flagged.is_empty());
        let rows = index.invoices_of("acme").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, Status::PortalFetchRequired);
    }

    #[test]
    fn identical_attachments_in_two_mails_are_stored_once() {
        let setup = setup(MOVE, "");
        let index = Index::open_in_memory().unwrap();
        let pdf = test_pdf::with_text(&["Rechnungsnummer: R-1"]);
        let first = test_mail::raw("b@acme.example", "Post", "1@x", &[("a.pdf", &pdf)]);
        let second = test_mail::raw("b@acme.example", "Erneut", "2@x", &[("a.pdf", &pdf)]);
        let mut mailbox = FakeMailbox::with(vec![first, second]);

        run_real(&setup, &index, &mut mailbox);

        assert_eq!(index.invoices_of("acme").unwrap().len(), 1);
        assert_eq!(fs::read_dir(setup.dir.path()).unwrap().count(), 1);
        assert_eq!(mailbox.moved.len(), 2);
    }

    #[test]
    fn dry_run_changes_nothing_and_reports_the_plan() {
        let setup = setup(MOVE, "");
        let rules = RuleSet::new(&setup.config);
        let context = PlanContext {
            config: &setup.config,
            rules: &rules,
            index: None,
            account_id: "main",
            today: NaiveDate::from_ymd_opt(2030, 1, 1).unwrap(),
        };
        let mut mailbox = FakeMailbox::with(vec![pdf_mail("1@x", "R-1")]);
        let mut mails = Vec::new();

        block_on(scan_read_only(&context, &mut mailbox, &mut mails)).unwrap();

        assert_eq!(mails[0].outcome, Outcome::Planned);
        assert!(mailbox.untouched());
        assert!(mailbox.flagged.is_empty());
        assert_eq!(fs::read_dir(setup.dir.path()).unwrap().count(), 0);
    }
}
