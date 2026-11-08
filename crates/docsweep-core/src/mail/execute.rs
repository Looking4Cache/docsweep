//! Runs a scan: plans each mail and, in a real run, carries the plan out.
//!
//! The order is the safety rule of the project: store and verify the file, add
//! it to the index, and only then touch the mail. Any failure leaves the mail in
//! place with a flag, and later mails are still processed.
//!
//! A link mail starts its company's recipe after all accounts have been read,
//! because a fetch can wait minutes for a person and no IMAP connection should
//! stay open that long. Only then is the mail looked at again: it is cleaned up
//! if the fetch had no failures and the index holds its invoice, it is flagged
//! if the fetch failed, and otherwise it stays noted and is checked again later.
//!
//! A company in assisted mode never starts anything here. A person picks up its
//! invoice, and the next scan finds it in the index under the same rule.

use std::collections::HashSet;
use std::fs;

use chrono::NaiveDate;
use serde::Serialize;

use super::imap::{ImapMailbox, ImapWriter, TransportSecurity};
use super::plan::{
    DisposalAction, Disposition, MailAction, MailDecision, MailKind, MailPlan, PlanContext,
    PlanError, disposal_for, plan_mail,
};
use super::rules::RuleSet;
use super::{IncomingMail, MailError, MailReader, MailWriter};
use crate::config::{Config, MailAccount};
use crate::index::{Index, IndexError, NewInvoice, Source, Status};
use crate::portal::driver::PortalFetcher;
use crate::portal::engine::{FetchReport, summarize_fetches};
use crate::secrets::{SecretError, SecretStore};
use crate::storage::{self, StorageError};

/// What came of a planned mail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outcome {
    /// Dry run: nothing was changed.
    Planned,
    /// Everything planned has been carried out.
    Completed,
    /// Documents are stored, but the mail stays flagged for a person to look at.
    KeptForReview { reason: String },
    /// Something went wrong. The mail stays, flagged if that was possible.
    Failed { message: String },
    /// A link mail whose invoice is stored; the mail was cleaned up.
    LinkResolved { disposal: DisposalAction },
    /// A link mail whose invoice is not stored yet. It stays in place, and is
    /// checked again later.
    AwaitingInvoice,
}

/// One mail in a report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MailReport {
    pub plan: MailPlan,
    pub outcome: Outcome,
}

/// The result for one account.
#[derive(Debug, Serialize)]
pub struct AccountReport {
    pub account_id: String,
    pub mails: Vec<MailReport>,
    /// Set if the account could not be processed (completely).
    pub error: Option<String>,
}

/// The result of a real scan.
#[derive(Debug, Default, Serialize)]
pub struct ScanReport {
    pub accounts: Vec<AccountReport>,
    /// The portal downloads started because of link mails.
    pub fetches: Vec<FetchReport>,
}

impl ScanReport {
    /// Whether anything went wrong.
    pub fn has_failures(&self) -> bool {
        self.summary().failed > 0
    }

    /// Counts of mails and portal downloads together.
    pub fn summary(&self) -> ScanSummary {
        let fetched = summarize_fetches(&self.fetches);
        let mut summary = summarize(&self.accounts);
        summary.saved += fetched.saved;
        summary.for_review += fetched.for_review;
        summary.already_stored += fetched.already_known;
        summary.missing += fetched.missing;
        summary.failed += fetched.failed;
        summary.repaired += fetched.repaired;
        summary
    }
}

/// Counts of a scan, for headlines and the run history.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct ScanSummary {
    /// Documents saved as complete invoices (or planned to be).
    pub saved: u32,
    /// Documents saved for review (or planned to be).
    pub for_review: u32,
    pub already_stored: u32,
    /// Mails cleaned up (or planned to be).
    pub disposed: u32,
    pub portal_fetches: u32,
    pub unknown_invoices: u32,
    /// Entries a portal lists without offering a document yet.
    pub missing: u32,
    /// Mails, accounts and portal entries that failed.
    pub failed: u32,
    /// Recipes that a language model repaired during this scan.
    pub repaired: u32,
}

/// Summarises reports. Steps of a failed mail do not count as done.
pub fn summarize(reports: &[AccountReport]) -> ScanSummary {
    let mut summary = ScanSummary::default();
    for account in reports {
        summary.failed += u32::from(account.error.is_some());
        for mail in &account.mails {
            if matches!(mail.outcome, Outcome::Failed { .. }) {
                summary.failed += 1;
                continue;
            }
            match &mail.plan.action {
                MailAction::Invoices {
                    documents,
                    decision,
                    ..
                } => {
                    for document in documents {
                        match document.disposition {
                            Disposition::Store => summary.saved += 1,
                            Disposition::StoreForReview(_) => summary.for_review += 1,
                            Disposition::AlreadyStored { .. } | Disposition::Unresolved { .. } => {
                                summary.already_stored += 1;
                            }
                        }
                    }
                    summary.disposed += u32::from(matches!(decision, MailDecision::Dispose(_)));
                }
                MailAction::PortalFetchRequired { cleanup, .. } => {
                    let planned_cleanup = mail.outcome == Outcome::Planned && cleanup.is_some();
                    if planned_cleanup || matches!(mail.outcome, Outcome::LinkResolved { .. }) {
                        summary.disposed += 1;
                    } else {
                        summary.portal_fetches += 1;
                    }
                }
                MailAction::UnknownInvoice { .. } => summary.unknown_invoices += 1,
                MailAction::NoDocument { .. } => {}
            }
        }
    }
    summary
}

/// Logs in and opens the watched folder without changing anything, and returns
/// how many mails it holds.
pub async fn test_mailbox(
    account: &MailAccount,
    secrets: &dyn SecretStore,
) -> Result<u32, ScanError> {
    let invalid = || ScanError::InvalidAccount(account.id.clone());
    let (host, port) = account.endpoint().ok_or_else(invalid)?;
    let secret_id = account.password.id().ok_or_else(invalid)?;
    let password = secrets.get(secret_id)?;
    let mut reader = ImapMailbox::connect_read_only(
        host,
        port,
        TransportSecurity::for_port(port),
        &account.user,
        &password,
        &account.watch,
    )
    .await?;
    let count = reader.message_count();
    reader.logout().await;
    Ok(count)
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

/// A dry run over all accounts. It opens the index read-only and does not create
/// it, and it gets no [`PortalFetcher`], so it cannot start a browser.
pub async fn preview_mail_scan(
    config: &Config,
    secrets: &dyn SecretStore,
    index_path: &std::path::Path,
    today: NaiveDate,
) -> Result<Vec<AccountReport>, ScanError> {
    let rules = RuleSet::new(config);
    let index = Index::open_read_only(index_path)?;
    let mut reports = Vec::with_capacity(config.mail.accounts.len());
    for account in &config.mail.accounts {
        let context = PlanContext {
            config,
            rules: &rules,
            index: index.as_ref(),
            account_id: &account.id,
            today,
        };
        let mut mails = Vec::new();
        let result = preview_account(&context, account, secrets, &mut mails).await;
        reports.push(AccountReport {
            account_id: account.id.clone(),
            mails,
            error: result.err().map(|error| error.to_string()),
        });
    }
    Ok(reports)
}

/// A real scan over all accounts, followed by the portal downloads its link mails
/// call for. Problems with one account or company do not stop the others.
pub async fn run_mail_scan<F: PortalFetcher>(
    config: &Config,
    secrets: &dyn SecretStore,
    index_path: &std::path::Path,
    today: NaiveDate,
    fetcher: &mut F,
) -> Result<ScanReport, ScanError> {
    let rules = RuleSet::new(config);
    let index = Index::open(index_path)?;
    let mut accounts = Vec::with_capacity(config.mail.accounts.len());
    for account in &config.mail.accounts {
        let context = PlanContext {
            config,
            rules: &rules,
            index: Some(&index),
            account_id: &account.id,
            today,
        };
        let mut mails = Vec::new();
        let result = execute_account(&context, &index, account, secrets, &mut mails).await;
        accounts.push(AccountReport {
            account_id: account.id.clone(),
            mails,
            error: result.err().map(|error| error.to_string()),
        });
    }

    let seen = accounts.iter().flat_map(|account| &account.mails);
    let fetches = fetch_for_link_mails(config, &index, today, seen, fetcher).await;
    for (report, account) in accounts.iter_mut().zip(&config.mail.accounts) {
        settle_account(config, &index, account, secrets, &fetches, report).await;
    }
    Ok(ScanReport { accounts, fetches })
}

/// How to reach a mailbox.
struct Connection<'a> {
    host: &'a str,
    port: u16,
    security: TransportSecurity,
    password: crate::secrets::SecretString,
}

fn connection_of<'a>(
    account: &'a MailAccount,
    secrets: &dyn SecretStore,
) -> Result<Connection<'a>, ScanError> {
    let invalid = || ScanError::InvalidAccount(account.id.clone());
    let (host, port) = account.endpoint().ok_or_else(invalid)?;
    let secret_id = account.password.id().ok_or_else(invalid)?;
    Ok(Connection {
        host,
        port,
        security: TransportSecurity::for_port(port),
        password: secrets.get(secret_id)?,
    })
}

async fn preview_account(
    context: &PlanContext<'_>,
    account: &MailAccount,
    secrets: &dyn SecretStore,
    mails: &mut Vec<MailReport>,
) -> Result<(), ScanError> {
    let connection = connection_of(account, secrets)?;
    let mut reader = ImapMailbox::connect_read_only(
        connection.host,
        connection.port,
        connection.security,
        &account.user,
        &connection.password,
        &account.watch,
    )
    .await?;
    let result = scan_read_only(context, &mut reader, mails).await;
    reader.logout().await;
    result
}

async fn open_writer(
    account: &MailAccount,
    secrets: &dyn SecretStore,
) -> Result<ImapWriter, ScanError> {
    let connection = connection_of(account, secrets)?;
    Ok(ImapMailbox::connect_writable(
        connection.host,
        connection.port,
        connection.security,
        &account.user,
        &connection.password,
        &account.watch,
    )
    .await?)
}

async fn execute_account(
    context: &PlanContext<'_>,
    index: &Index,
    account: &MailAccount,
    secrets: &dyn SecretStore,
    mails: &mut Vec<MailReport>,
) -> Result<(), ScanError> {
    let mut writer = open_writer(account, secrets).await?;
    let result = scan_and_execute(context, index, &mut writer, mails).await;
    writer.logout().await;
    result
}

async fn settle_account(
    config: &Config,
    index: &Index,
    account: &MailAccount,
    secrets: &dyn SecretStore,
    fetches: &[FetchReport],
    report: &mut AccountReport,
) {
    let steps = decide_link_mails(config, index, &account.id, &mut report.mails, fetches);
    if steps.is_empty() {
        return;
    }
    match open_writer(account, secrets).await {
        Ok(mut writer) => {
            apply_link_steps(index, &account.id, &mut writer, &mut report.mails, steps).await;
            writer.logout().await;
        }
        Err(error) => {
            for step in steps {
                report.mails[step.position].outcome = Outcome::Failed {
                    message: format!("the mailbox could not be opened again: {error}"),
                };
            }
        }
    }
}

/// Starts the recipes that link mails call for, once per company however many
/// mails it has, and remembers the attempt. A mail that could not even be noted
/// has failed already and starts nothing, and neither does a company in assisted
/// mode, whatever a plan says.
pub async fn fetch_for_link_mails<'m, F: PortalFetcher>(
    config: &Config,
    index: &Index,
    today: NaiveDate,
    mails: impl Iterator<Item = &'m MailReport>,
    fetcher: &mut F,
) -> Vec<FetchReport> {
    let mut due = HashSet::new();
    for mail in mails {
        if let MailAction::PortalFetchRequired {
            company_id,
            fetch_due: true,
            ..
        } = &mail.plan.action
            && mail.outcome == Outcome::Completed
        {
            due.insert(company_id.as_str());
        }
    }
    let mut reports = Vec::with_capacity(due.len());
    for company in &config.companies {
        if due.contains(company.id.as_str()) && !company.is_assisted() {
            reports.push(match index.note_link_fetch(&company.id, today) {
                Ok(()) => fetcher.fetch(company).await,
                Err(error) => FetchReport::failed(company, error.to_string()),
            });
        }
    }
    reports
}

/// What is left to do in the mailbox for one link mail.
#[derive(Debug)]
pub struct LinkStep {
    position: usize,
    company_id: String,
    action: LinkAction,
}

#[derive(Debug)]
enum LinkAction {
    /// Clean up the mail; the invoice with this id settles it.
    Dispose(DisposalAction, i64),
    Flag(String),
}

/// The safety rule for link mails: a mail is only cleaned up if a fetch ran now
/// and had no failures, and the index holds a stored invoice that matches this
/// mail and no other ([`Index::link_assignment`]). A successful fetch alone is not enough, because the invoice may not be
/// in the portal yet. A company in assisted mode has no fetch to wait for, so only
/// the index counts. Mails that simply stay are settled here; the returned steps
/// need the mailbox.
pub fn decide_link_mails(
    config: &Config,
    index: &Index,
    account_id: &str,
    mails: &mut [MailReport],
    fetches: &[FetchReport],
) -> Vec<LinkStep> {
    let mut steps = Vec::new();
    for (position, mail) in mails.iter_mut().enumerate() {
        let MailAction::PortalFetchRequired { company_id, .. } = &mail.plan.action else {
            continue;
        };
        if mail.outcome != Outcome::Completed {
            continue;
        }
        let company = config.companies.iter().find(|c| &c.id == company_id);
        let fetch = fetches.iter().find(|f| &f.company_id == company_id);
        let Some(company) = company.filter(|c| fetch.is_some() || c.is_assisted()) else {
            mail.outcome = Outcome::AwaitingInvoice;
            continue;
        };
        let action = if let Some(fetch) = fetch.filter(|fetch| fetch.has_failures()) {
            let reason = fetch
                .error
                .as_deref()
                .unwrap_or("some documents could not be stored");
            Some(LinkAction::Flag(format!(
                "the portal download failed: {reason}"
            )))
        } else {
            match index.invoice_for_link_mail(company_id, account_id, &mail.plan.message_id) {
                Ok(Some(invoice)) => Some(LinkAction::Dispose(
                    disposal_for(config, company, MailKind::Link),
                    invoice,
                )),
                Ok(None) => None,
                Err(error) => Some(LinkAction::Flag(error.to_string())),
            }
        };
        match action {
            Some(action) => steps.push(LinkStep {
                position,
                company_id: company_id.clone(),
                action,
            }),
            None => mail.outcome = Outcome::AwaitingInvoice,
        }
    }
    steps
}

/// Carries out the steps on a fresh connection. A mail is only touched if it is
/// still the one that was read, since a person may have dealt with it meanwhile.
pub async fn apply_link_steps<W: MailWriter>(
    index: &Index,
    account_id: &str,
    writer: &mut W,
    mails: &mut [MailReport],
    steps: Vec<LinkStep>,
) {
    for step in steps {
        let mail = &mut mails[step.position];
        let uid = mail.plan.uid;
        let message_id = &mail.plan.message_id;
        let unchanged = writer
            .fetch_raw(uid)
            .await
            .is_ok_and(|raw| &IncomingMail::parse(uid, &raw).message_id == message_id);
        mail.outcome = if !unchanged {
            Outcome::Failed {
                message: "this mail changed or is gone, so it was left alone".to_owned(),
            }
        } else {
            match step.action {
                LinkAction::Flag(message) => fail(writer, uid, message).await,
                LinkAction::Dispose(disposal, invoice) => {
                    match dispose(writer, uid, &disposal).await {
                        Err(error) => fail(writer, uid, error.to_string()).await,
                        Ok(()) => {
                            let noted = index.resolve_link_mail(
                                &step.company_id,
                                account_id,
                                message_id,
                                invoice,
                            );
                            match noted {
                                Ok(()) => Outcome::LinkResolved { disposal },
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
            }
        };
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
        portal_key: None,
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
    let message = match writer.flag(uid).await {
        Ok(()) => format!("{message}; the mail stays in place and is flagged"),
        Err(flag_error) => {
            format!("{message}; the mail stays in place, but flagging failed: {flag_error}")
        }
    };
    Outcome::Failed { message }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Company;
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
    output: {{ dir: '{}', filename: "{{invoice_no}}.pdf" }}
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
        config.companies[0].output.dir = Some(blocked.display().to_string());
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

    const WITH_PORTAL: &str = "portal: { credentials: \"keyring:c\", recipe: r.yaml }";

    fn link_mail(id: &str) -> Vec<u8> {
        test_mail::raw("b@acme.example", "Rechnung online", id, &[])
    }

    fn day(number: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2030, 1, number).unwrap()
    }

    /// Stands in for the browser and says what each fetch comes to.
    struct FakeFetcher<A> {
        started: Vec<String>,
        act: A,
    }

    fn fetcher<A: FnMut(&Company) -> FetchReport>(act: A) -> FakeFetcher<A> {
        FakeFetcher {
            started: Vec::new(),
            act,
        }
    }

    impl<A: FnMut(&Company) -> FetchReport> PortalFetcher for FakeFetcher<A> {
        async fn fetch(&mut self, company: &Company) -> FetchReport {
            self.started.push(company.id.clone());
            (self.act)(company)
        }
    }

    fn fine(company: &Company) -> FetchReport {
        FetchReport {
            company_id: company.id.clone(),
            company_name: company.name.clone(),
            entries: Vec::new(),
            error: None,
            notices: Vec::new(),
        }
    }

    /// As if a fetch stored a new invoice.
    fn store_portal_invoice(setup: &Setup, index: &Index, number: &str) {
        let file = setup.dir.path().join(format!("{number}.pdf"));
        fs::write(&file, b"x").unwrap();
        index
            .insert(&NewInvoice {
                company_id: "acme".into(),
                invoice_date: Some(day(1)),
                invoice_no: Some(number.into()),
                amount: None,
                currency: None,
                file_path: file,
                file_hash: number.into(),
                source: Source::Portal,
                mail_account: None,
                mail_id: None,
                portal_key: Some(number.into()),
                status: Status::Stored,
            })
            .unwrap();
    }

    /// A real run for one mailbox on the given day, with all three stages.
    fn scan_with<A: FnMut(&Company) -> FetchReport>(
        setup: &Setup,
        index: &Index,
        mailbox: &mut FakeMailbox,
        fetcher: &mut FakeFetcher<A>,
        today: NaiveDate,
    ) -> Vec<MailReport> {
        let rules = RuleSet::new(&setup.config);
        let context = PlanContext {
            config: &setup.config,
            rules: &rules,
            index: Some(index),
            account_id: "main",
            today,
        };
        let mut mails = Vec::new();
        block_on(async {
            scan_and_execute(&context, index, mailbox, &mut mails)
                .await
                .unwrap();
            let fetches =
                fetch_for_link_mails(&setup.config, index, today, mails.iter(), fetcher).await;
            let steps = decide_link_mails(&setup.config, index, "main", &mut mails, &fetches);
            apply_link_steps(index, "main", mailbox, &mut mails, steps).await;
        });
        mails
    }

    #[test]
    fn a_link_mail_stays_unflagged_when_the_fetch_succeeded_but_stored_nothing_matching() {
        let setup = setup(MOVE, WITH_PORTAL);
        let index = Index::open_in_memory().unwrap();
        let mut mailbox = FakeMailbox::with(vec![link_mail("1@x")]);
        let mut portal = fetcher(fine);

        let mails = scan_with(&setup, &index, &mut mailbox, &mut portal, day(1));

        assert_eq!(portal.started, ["acme"]);
        assert_eq!(mails[0].outcome, Outcome::AwaitingInvoice);
        assert!(mailbox.untouched());
        assert!(mailbox.flagged.is_empty());
        let row = index.invoices_of("acme").unwrap().remove(0);
        assert_eq!(row.status, Status::PortalFetchRequired);
        assert_eq!(row.resolved_at, None);
    }

    #[test]
    fn a_link_mail_is_disposed_when_a_matching_invoice_was_stored() {
        let setup = setup(MOVE, WITH_PORTAL);
        let index = Index::open_in_memory().unwrap();
        let mut mailbox = FakeMailbox::with(vec![link_mail("1@x")]);
        let mut portal = fetcher(|company| {
            store_portal_invoice(&setup, &index, "R-1");
            fine(company)
        });

        let mails = scan_with(&setup, &index, &mut mailbox, &mut portal, day(1));

        assert_eq!(mailbox.trashed, [1], "the default for link mails");
        assert!(mailbox.flagged.is_empty());
        assert_eq!(
            mails[0].outcome,
            Outcome::LinkResolved {
                disposal: DisposalAction::Trash
            }
        );
        let row = index.invoices_of("acme").unwrap().remove(0);
        assert!(row.resolved_at.is_some());
        assert!(row.disposed_at.is_some());
    }

    #[test]
    fn a_link_mail_follows_the_normal_disposal_rules() {
        let setup = setup(
            "{ mode: all_move, move_to: Done }",
            &format!("{WITH_PORTAL}\n    disposal: {{ move_to: Acme }}"),
        );
        let index = Index::open_in_memory().unwrap();
        let mut mailbox = FakeMailbox::with(vec![link_mail("1@x")]);
        let mut portal = fetcher(|company| {
            store_portal_invoice(&setup, &index, "R-1");
            fine(company)
        });

        scan_with(&setup, &index, &mut mailbox, &mut portal, day(1));

        assert_eq!(mailbox.moved, [(1, "Acme".to_owned())]);
    }

    #[test]
    fn a_failed_fetch_flags_and_keeps_the_mail_even_with_a_matching_invoice() {
        let setup = setup(MOVE, WITH_PORTAL);
        let index = Index::open_in_memory().unwrap();
        let mut mailbox = FakeMailbox::with(vec![link_mail("1@x")]);
        let mut portal = fetcher(|company| {
            store_portal_invoice(&setup, &index, "R-1");
            FetchReport::failed(company, "the login page did not load".into())
        });

        let mails = scan_with(&setup, &index, &mut mailbox, &mut portal, day(1));

        assert!(mailbox.untouched());
        assert_eq!(mailbox.flagged, [1]);
        let Outcome::Failed { message } = &mails[0].outcome else {
            panic!("expected a failure: {:?}", mails[0].outcome);
        };
        assert!(message.contains("the login page did not load"), "{message}");
        assert_eq!(index.invoices_of("acme").unwrap()[0].resolved_at, None);
    }

    #[test]
    fn one_failed_document_counts_as_a_failed_fetch() {
        let setup = setup(MOVE, WITH_PORTAL);
        let index = Index::open_in_memory().unwrap();
        let mut mailbox = FakeMailbox::with(vec![link_mail("1@x")]);
        let mut portal = fetcher(|company| {
            store_portal_invoice(&setup, &index, "R-1");
            let mut report = fine(company);
            report.entries.push(crate::portal::engine::EntryReport {
                key: "R-2".into(),
                date: None,
                invoice_no: None,
                amount: None,
                outcome: crate::portal::engine::EntryOutcome::Failed {
                    message: "no PDF".into(),
                },
            });
            report
        });

        scan_with(&setup, &index, &mut mailbox, &mut portal, day(1));

        assert!(mailbox.untouched());
        assert_eq!(mailbox.flagged, [1]);
    }

    #[test]
    fn several_link_mails_of_a_company_give_one_fetch() {
        let setup = setup(MOVE, WITH_PORTAL);
        let index = Index::open_in_memory().unwrap();
        let mut mailbox =
            FakeMailbox::with(vec![link_mail("1@x"), link_mail("2@x"), link_mail("3@x")]);
        let mut portal = fetcher(|company| {
            for number in ["R-1", "R-2", "R-3"] {
                store_portal_invoice(&setup, &index, number);
            }
            fine(company)
        });

        let mails = scan_with(&setup, &index, &mut mailbox, &mut portal, day(1));

        assert_eq!(portal.started, ["acme"]);
        assert_eq!(mailbox.trashed, [1, 2, 3]);
        assert_eq!(mails.len(), 3);
    }

    #[test]
    fn one_stored_invoice_settles_only_one_of_several_link_mails() {
        let setup = setup(MOVE, WITH_PORTAL);
        let index = Index::open_in_memory().unwrap();
        let mut mailbox = FakeMailbox::with(vec![link_mail("1@x"), link_mail("2@x")]);
        let mut portal = fetcher(|company| {
            store_portal_invoice(&setup, &index, "R-1");
            fine(company)
        });

        let mails = scan_with(&setup, &index, &mut mailbox, &mut portal, day(1));

        assert_eq!(mailbox.trashed, [1]);
        assert!(mailbox.flagged.is_empty());
        assert!(matches!(mails[0].outcome, Outcome::LinkResolved { .. }));
        assert_eq!(mails[1].outcome, Outcome::AwaitingInvoice);
        let rows = index.invoices_of("acme").unwrap();
        let invoice = rows.iter().find(|r| r.status == Status::Stored).unwrap();
        assert_eq!(rows[0].settled_by, Some(invoice.id));
        assert_eq!(rows[1].resolved_at, None);
    }

    /// A link mail sent on another day than the usual 3 November 2026.
    fn link_mail_of(id: &str, date: &str) -> Vec<u8> {
        String::from_utf8(link_mail(id))
            .unwrap()
            .replace("Tue, 03 Nov 2026", date)
            .into_bytes()
    }

    fn import(setup: &Setup, index: &Index, text: &[&str]) {
        use crate::pickup::{LocalFolder, import_file};
        let downloads = tempfile::tempdir().unwrap();
        fs::write(
            downloads.path().join("invoice.pdf"),
            test_pdf::with_text(text),
        )
        .unwrap();
        let folder = LocalFolder::new(downloads.path().to_path_buf());
        import_file(
            &setup.config,
            &setup.config.companies[0],
            index,
            &folder,
            "invoice.pdf",
            None,
            day(1),
        )
        .unwrap();
    }

    #[test]
    fn importing_the_october_invoice_cleans_up_only_the_october_link_mail() {
        let setup = setup(MOVE, ASSISTED);
        let index = Index::open_in_memory().unwrap();
        let mut mailbox = FakeMailbox::with(vec![
            link_mail_of("oct@x", "Sat, 03 Oct 2026"),
            link_mail("nov@x"),
        ]);
        let mut portal = fetcher(fine);
        scan_with(&setup, &index, &mut mailbox, &mut portal, day(1));
        assert!(mailbox.untouched());

        import(
            &setup,
            &index,
            &["Rechnungsnummer: R-10", "Rechnungsdatum: 01.10.2026"],
        );
        let mails = scan_with(&setup, &index, &mut mailbox, &mut portal, day(1));

        assert_eq!(mailbox.trashed, [1], "only October");
        assert!(matches!(mails[0].outcome, Outcome::LinkResolved { .. }));
        assert_eq!(mails[1].outcome, Outcome::AwaitingInvoice);
        assert_eq!(
            crate::pickup::pending_pickups(&setup.config, &index)
                .unwrap()
                .len(),
            1,
            "November is still to pick up"
        );

        import(
            &setup,
            &index,
            &["Rechnungsnummer: R-11", "Rechnungsdatum: 01.11.2026"],
        );
        mailbox.mails.remove(&1);
        let mails = scan_with(&setup, &index, &mut mailbox, &mut portal, day(1));

        assert_eq!(mailbox.trashed, [1, 2]);
        assert!(matches!(mails[0].outcome, Outcome::LinkResolved { .. }));
        assert!(mailbox.flagged.is_empty());
    }

    #[test]
    fn link_mails_in_two_mailboxes_still_give_one_fetch() {
        let setup = setup(MOVE, WITH_PORTAL);
        let index = Index::open_in_memory().unwrap();
        let rules = RuleSet::new(&setup.config);
        let mut reports = Vec::new();
        for (account_id, id) in [("first", "1@x"), ("second", "2@x")] {
            let context = PlanContext {
                config: &setup.config,
                rules: &rules,
                index: Some(&index),
                account_id,
                today: day(1),
            };
            let mut mailbox = FakeMailbox::with(vec![link_mail(id)]);
            block_on(scan_and_execute(
                &context,
                &index,
                &mut mailbox,
                &mut reports,
            ))
            .unwrap();
        }
        let mut portal = fetcher(fine);

        block_on(fetch_for_link_mails(
            &setup.config,
            &index,
            day(1),
            reports.iter(),
            &mut portal,
        ));

        assert_eq!(portal.started, ["acme"]);
    }

    #[test]
    fn a_waiting_company_is_retried_at_most_once_per_day() {
        let setup = setup(MOVE, WITH_PORTAL);
        let index = Index::open_in_memory().unwrap();
        let mut mailbox = FakeMailbox::with(vec![link_mail("1@x")]);
        let mut portal = fetcher(fine);

        scan_with(&setup, &index, &mut mailbox, &mut portal, day(1));
        let later = scan_with(&setup, &index, &mut mailbox, &mut portal, day(1));
        assert_eq!(portal.started.len(), 1, "same day");
        assert_eq!(later[0].outcome, Outcome::AwaitingInvoice);

        scan_with(&setup, &index, &mut mailbox, &mut portal, day(2));
        assert_eq!(portal.started.len(), 2, "next day");
        scan_with(&setup, &index, &mut mailbox, &mut portal, day(2));
        assert_eq!(portal.started.len(), 2);

        assert!(mailbox.untouched());
        assert!(mailbox.flagged.is_empty());
        assert_eq!(index.invoices_of("acme").unwrap().len(), 1);
    }

    #[test]
    fn a_new_link_mail_does_not_wait_for_the_next_day() {
        let setup = setup(MOVE, WITH_PORTAL);
        let index = Index::open_in_memory().unwrap();
        let mut mailbox = FakeMailbox::with(vec![link_mail("1@x")]);
        let mut portal = fetcher(fine);
        scan_with(&setup, &index, &mut mailbox, &mut portal, day(1));

        mailbox.mails.insert(2, link_mail("2@x"));
        scan_with(&setup, &index, &mut mailbox, &mut portal, day(1));

        assert_eq!(portal.started.len(), 2);
    }

    #[test]
    fn a_later_retry_cleans_up_once_the_invoice_is_there() {
        let setup = setup(MOVE, WITH_PORTAL);
        let index = Index::open_in_memory().unwrap();
        let mut mailbox = FakeMailbox::with(vec![link_mail("1@x")]);
        let mut portal = fetcher(fine);
        scan_with(&setup, &index, &mut mailbox, &mut portal, day(1));
        assert!(mailbox.untouched());

        let mut portal = fetcher(|company| {
            store_portal_invoice(&setup, &index, "R-1");
            fine(company)
        });
        scan_with(&setup, &index, &mut mailbox, &mut portal, day(2));

        assert_eq!(mailbox.trashed, [1]);
    }

    #[test]
    fn a_failed_disposal_flags_the_link_mail_and_leaves_its_row_open() {
        let setup = setup(MOVE, WITH_PORTAL);
        let index = Index::open_in_memory().unwrap();
        let mut mailbox = FakeMailbox::with(vec![link_mail("1@x")]);
        mailbox.fail_disposal = true;
        let mut portal = fetcher(|company| {
            store_portal_invoice(&setup, &index, "R-1");
            fine(company)
        });

        let mails = scan_with(&setup, &index, &mut mailbox, &mut portal, day(1));

        assert!(matches!(mails[0].outcome, Outcome::Failed { .. }));
        assert_eq!(mailbox.flagged, [1]);
        assert_eq!(index.invoices_of("acme").unwrap()[0].resolved_at, None);
    }

    #[test]
    fn a_mail_that_changed_during_the_fetch_is_left_alone() {
        let setup = setup(MOVE, WITH_PORTAL);
        let index = Index::open_in_memory().unwrap();
        let mut mailbox = FakeMailbox::with(vec![link_mail("1@x")]);
        let rules = RuleSet::new(&setup.config);
        let context = PlanContext {
            config: &setup.config,
            rules: &rules,
            index: Some(&index),
            account_id: "main",
            today: day(1),
        };
        let mut mails = Vec::new();
        block_on(scan_and_execute(&context, &index, &mut mailbox, &mut mails)).unwrap();
        store_portal_invoice(&setup, &index, "R-1");
        let fetches = [fine(&setup.config.companies[0])];

        // The server now has another mail under this number.
        mailbox.mails.insert(1, link_mail("other@x"));
        let steps = decide_link_mails(&setup.config, &index, "main", &mut mails, &fetches);
        block_on(apply_link_steps(
            &index,
            "main",
            &mut mailbox,
            &mut mails,
            steps,
        ));

        assert!(matches!(mails[0].outcome, Outcome::Failed { .. }));
        assert!(mailbox.untouched());
        assert!(mailbox.flagged.is_empty());
    }

    #[test]
    fn a_link_mail_that_could_not_be_noted_starts_no_fetch() {
        let setup = setup(MOVE, WITH_PORTAL);
        let index = Index::open_in_memory().unwrap();
        let mut mails = vec![MailReport {
            plan: MailPlan {
                uid: 1,
                message_id: "1@x".into(),
                from: "b@acme.example".into(),
                subject: "Rechnung".into(),
                action: MailAction::PortalFetchRequired {
                    company_id: "acme".into(),
                    company_name: "Acme".into(),
                    already_recorded: false,
                    fetch_due: true,
                    assisted: false,
                    cleanup: None,
                },
            },
            outcome: Outcome::Failed {
                message: "index broken".into(),
            },
        }];
        let mut portal = fetcher(fine);

        let fetches = block_on(fetch_for_link_mails(
            &setup.config,
            &index,
            day(1),
            mails.iter(),
            &mut portal,
        ));
        let steps = decide_link_mails(&setup.config, &index, "main", &mut mails, &fetches);

        assert!(portal.started.is_empty());
        assert!(steps.is_empty());
        assert!(matches!(mails[0].outcome, Outcome::Failed { .. }));
    }

    const ASSISTED: &str = "portal: { mode: assisted, url: \"https://portal.example/\" }";

    #[test]
    fn a_scan_never_starts_a_fetcher_for_an_assisted_company() {
        let setup = setup(MOVE, ASSISTED);
        let index = Index::open_in_memory().unwrap();
        let mut mailbox = FakeMailbox::with(vec![link_mail("1@x"), link_mail("2@x")]);
        let mut portal = fetcher(fine);

        for today in [day(1), day(1), day(2)] {
            let mails = scan_with(&setup, &index, &mut mailbox, &mut portal, today);
            assert_eq!(mails[0].outcome, Outcome::AwaitingInvoice);
        }

        assert!(portal.started.is_empty());
        assert!(mailbox.untouched());
        assert!(mailbox.flagged.is_empty());
        let rows = index.invoices_of("acme").unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.status == Status::PortalFetchRequired));
        assert_eq!(index.last_link_fetch_on("acme").unwrap(), None);
    }

    #[test]
    fn a_plan_that_calls_for_a_download_still_starts_nothing_for_an_assisted_company() {
        let setup = setup(MOVE, ASSISTED);
        let index = Index::open_in_memory().unwrap();
        let mails = [MailReport {
            plan: MailPlan {
                uid: 1,
                message_id: "1@x".into(),
                from: "b@acme.example".into(),
                subject: "Rechnung".into(),
                action: MailAction::PortalFetchRequired {
                    company_id: "acme".into(),
                    company_name: "Acme".into(),
                    already_recorded: false,
                    fetch_due: true,
                    assisted: true,
                    cleanup: None,
                },
            },
            outcome: Outcome::Completed,
        }];
        let mut portal = fetcher(fine);

        let fetches = block_on(fetch_for_link_mails(
            &setup.config,
            &index,
            day(1),
            mails.iter(),
            &mut portal,
        ));

        assert!(portal.started.is_empty());
        assert!(fetches.is_empty());
    }

    #[test]
    fn a_stored_invoice_lets_a_scan_clean_up_an_assisted_link_mail_without_a_fetcher() {
        let setup = setup(MOVE, ASSISTED);
        let index = Index::open_in_memory().unwrap();
        let mut mailbox = FakeMailbox::with(vec![link_mail("1@x")]);
        let mut portal = fetcher(fine);
        scan_with(&setup, &index, &mut mailbox, &mut portal, day(1));
        assert!(mailbox.untouched());

        store_portal_invoice(&setup, &index, "R-1");
        let mails = scan_with(&setup, &index, &mut mailbox, &mut portal, day(2));

        assert!(portal.started.is_empty());
        assert_eq!(mailbox.trashed, [1], "the default for link mails");
        assert_eq!(
            mails[0].outcome,
            Outcome::LinkResolved {
                disposal: DisposalAction::Trash
            }
        );
        assert!(index.invoices_of("acme").unwrap()[0].resolved_at.is_some());
    }

    #[test]
    fn an_imported_invoice_leads_the_next_scan_to_clean_up_the_link_mail() {
        use crate::pickup::{LocalFolder, import_file};

        let setup = setup(MOVE, ASSISTED);
        let index = Index::open_in_memory().unwrap();
        let mut mailbox = FakeMailbox::with(vec![link_mail("1@x")]);
        let mut portal = fetcher(fine);
        scan_with(&setup, &index, &mut mailbox, &mut portal, day(1));
        assert!(mailbox.untouched());

        let downloads = tempfile::tempdir().unwrap();
        fs::write(
            downloads.path().join("invoice.pdf"),
            test_pdf::with_text(&["Rechnungsnummer: R-1", "Rechnungsdatum: 01.01.2030"]),
        )
        .unwrap();
        let folder = LocalFolder::new(downloads.path().to_path_buf());
        import_file(
            &setup.config,
            &setup.config.companies[0],
            &index,
            &folder,
            "invoice.pdf",
            None,
            day(1),
        )
        .unwrap();
        // Not before a scan: a pick-up alone does not touch the mailbox.
        assert!(mailbox.untouched());

        let mails = scan_with(&setup, &index, &mut mailbox, &mut portal, day(1));

        assert_eq!(mailbox.trashed, [1]);
        assert!(mailbox.flagged.is_empty());
        assert!(portal.started.is_empty());
        assert!(matches!(mails[0].outcome, Outcome::LinkResolved { .. }));
        assert!(setup.dir.path().join("R-1.pdf").exists());
    }

    #[test]
    fn a_dry_run_only_shows_that_a_download_would_start_and_changes_nothing() {
        let setup = setup(MOVE, WITH_PORTAL);
        let rules = RuleSet::new(&setup.config);
        let context = PlanContext {
            config: &setup.config,
            rules: &rules,
            index: None,
            account_id: "main",
            today: day(1),
        };
        let mut mailbox = FakeMailbox::with(vec![link_mail("1@x")]);
        let mut mails = Vec::new();

        // `scan_read_only` takes no fetcher, so it cannot start a browser.
        block_on(scan_read_only(&context, &mut mailbox, &mut mails)).unwrap();

        assert_eq!(mails[0].outcome, Outcome::Planned);
        assert!(matches!(
            mails[0].plan.action,
            MailAction::PortalFetchRequired {
                fetch_due: true,
                already_recorded: false,
                ..
            }
        ));
        assert!(mailbox.untouched());
        assert!(mailbox.flagged.is_empty());
    }

    #[test]
    fn a_dry_run_of_a_mail_checked_today_shows_that_no_download_would_start() {
        let setup = setup(MOVE, WITH_PORTAL);
        let index = Index::open_in_memory().unwrap();
        let mut mailbox = FakeMailbox::with(vec![link_mail("1@x")]);
        scan_with(&setup, &index, &mut mailbox, &mut fetcher(fine), day(1));

        let rules = RuleSet::new(&setup.config);
        let context = PlanContext {
            config: &setup.config,
            rules: &rules,
            index: Some(&index),
            account_id: "main",
            today: day(1),
        };
        let mut mails = Vec::new();
        block_on(scan_read_only(&context, &mut mailbox, &mut mails)).unwrap();

        assert!(matches!(
            mails[0].plan.action,
            MailAction::PortalFetchRequired {
                fetch_due: false,
                ..
            }
        ));
    }

    #[test]
    fn the_summary_counts_mails_and_portal_downloads_together() {
        let setup = setup(MOVE, WITH_PORTAL);
        let index = Index::open_in_memory().unwrap();
        let mut mailbox = FakeMailbox::with(vec![link_mail("1@x")]);
        let mut portal = fetcher(|company| {
            store_portal_invoice(&setup, &index, "R-1");
            let mut report = fine(company);
            report.entries.push(crate::portal::engine::EntryReport {
                key: "R-1".into(),
                date: None,
                invoice_no: None,
                amount: None,
                outcome: crate::portal::engine::EntryOutcome::Stored {
                    path: "/x/R-1.pdf".into(),
                },
            });
            report
        });
        let mails = scan_with(&setup, &index, &mut mailbox, &mut portal, day(1));
        let report = ScanReport {
            accounts: vec![AccountReport {
                account_id: "main".into(),
                mails,
                error: None,
            }],
            fetches: vec![{
                let mut report = fine(&setup.config.companies[0]);
                report.entries.push(crate::portal::engine::EntryReport {
                    key: "R-1".into(),
                    date: None,
                    invoice_no: None,
                    amount: None,
                    outcome: crate::portal::engine::EntryOutcome::Stored {
                        path: "/x/R-1.pdf".into(),
                    },
                });
                report
            }],
        };

        let summary = report.summary();

        assert_eq!(summary.saved, 1);
        assert_eq!(summary.disposed, 1);
        assert_eq!(summary.portal_fetches, 0);
        assert!(!report.has_failures());
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
    fn summary_counts_what_actually_happened() {
        let setup = setup(MOVE, "");
        let index = Index::open_in_memory().unwrap();
        let no_number = test_mail::raw(
            "b@acme.example",
            "Post",
            "1@x",
            &[("a.pdf", &test_pdf::with_text(&["Keine Nummer"]))],
        );
        let mut mailbox = FakeMailbox::with(vec![no_number, pdf_mail("2@x", "R-2")]);
        let mails = run_real(&setup, &index, &mut mailbox);
        let reports = [AccountReport {
            account_id: "main".into(),
            mails,
            error: Some("connection lost".into()),
        }];

        let summary = summarize(&reports);

        assert_eq!(summary.saved, 1);
        assert_eq!(summary.for_review, 1);
        assert_eq!(summary.disposed, 1);
        assert_eq!(summary.failed, 1);
    }

    #[test]
    fn failed_mails_do_not_count_as_saved() {
        let setup = setup(MOVE, "");
        let index = Index::open_in_memory().unwrap();
        let mut mailbox = FakeMailbox::with(vec![pdf_mail("1@x", "R-1")]);
        mailbox.fail_disposal = true;
        let mails = run_real(&setup, &index, &mut mailbox);
        let reports = [AccountReport {
            account_id: "main".into(),
            mails,
            error: None,
        }];

        let summary = summarize(&reports);

        assert_eq!(summary.failed, 1);
        assert_eq!(summary.disposed, 0);
    }

    #[test]
    fn reports_serialize_with_stable_tags_for_the_gui() {
        let setup = setup(MOVE, "");
        let index = Index::open_in_memory().unwrap();
        let mut mailbox = FakeMailbox::with(vec![pdf_mail("1@x", "R-1")]);
        let mails = run_real(&setup, &index, &mut mailbox);
        let json = serde_json::to_value(&mails[0]).unwrap();

        assert_eq!(json["outcome"]["kind"], "completed");
        assert_eq!(json["plan"]["action"]["kind"], "invoices");
        let action = &json["plan"]["action"];
        assert_eq!(action["decision"]["kind"], "dispose");
        assert_eq!(action["decision"]["value"]["kind"], "move_to");
        assert_eq!(action["decision"]["value"]["value"], "Done");
        assert_eq!(action["documents"][0]["disposition"]["kind"], "store");
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
