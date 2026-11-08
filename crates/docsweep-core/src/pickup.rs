//! Assisted pick-up: a person fetches an invoice in their ordinary browser, and
//! Docsweep files the PDF.
//!
//! Some portals reject any browser under automation, even with a person clicking.
//! Docsweep does not hide automation, so for these it automates nothing. It opens
//! the configured address in the default browser and never attaches to that
//! browser, reads its cookies or looks at its pages.
//!
//! While a [`PickupSession`] runs, and only then, it looks at the downloads folder
//! for new PDFs. A downloads folder can hold anything, so nothing is imported until
//! a person confirmed the file, and file contents are only opened for files that
//! look like finished PDFs. After the import, the original goes away, and only once
//! the index entry exists. The next scan then finds the invoice for the link mail.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, NaiveDate, TimeDelta, Utc};
use serde::Serialize;

use crate::config::{Company, Config};
use crate::document::{self, looks_like_pdf};
use crate::index::{DuplicateCheck, Index, IndexError, NewInvoice, Source, Status};
use crate::mail::plan::{Disposition, PlanError, ReviewReason, disposition_of, target_of};
use crate::portal::hosts::is_web_address;
use crate::storage::{self, StorageError, sha256_hex};

/// How long a session looks at the downloads folder.
pub const SESSION_LIMIT_MINUTES: i64 = 15;

/// Far beyond any invoice. A larger file is not read into memory just because of
/// its name.
const MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;

/// What browsers name a download that is still going on.
const PARTIAL_SUFFIXES: [&str; 3] = ["crdownload", "part", "download"];

/// Size and time of a file, to tell whether it changed. Times are only ever
/// compared with each other, because a browser or a move can give a new file an
/// old time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStamp {
    pub size: u64,
    pub modified: Option<SystemTime>,
}

/// One thing in the folder. Anything but a regular file, including a symlink, has
/// `is_file` false.
#[derive(Debug, Clone)]
pub struct FolderEntry {
    pub name: String,
    pub is_file: bool,
    pub stamp: FileStamp,
}

/// Access to the folder, so that tests can see what is looked at and opened.
pub trait DownloadsFolder: Send {
    fn location(&self) -> &Path;
    /// What is directly in the folder. Reads no file contents.
    fn list(&self) -> io::Result<Vec<FolderEntry>>;
    /// The content of a regular file.
    fn read(&self, name: &str) -> io::Result<Vec<u8>>;
    fn remove(&self, name: &str) -> io::Result<()>;
}

/// A real folder on this computer.
#[derive(Debug, Clone)]
pub struct LocalFolder {
    dir: PathBuf,
}

impl LocalFolder {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// Only a plain name inside the folder gets a path.
    fn path_of(&self, name: &str) -> io::Result<PathBuf> {
        if Path::new(name)
            .file_name()
            .is_some_and(|plain| plain == name)
        {
            Ok(self.dir.join(name))
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "this is not a file name",
            ))
        }
    }
}

impl DownloadsFolder for LocalFolder {
    fn location(&self) -> &Path {
        &self.dir
    }

    fn list(&self) -> io::Result<Vec<FolderEntry>> {
        let mut entries = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            // A name that is not text could not be shown to anybody.
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            // Neither of these follows a symlink.
            let is_file = entry.file_type()?.is_file();
            let metadata = entry.metadata()?;
            entries.push(FolderEntry {
                name,
                is_file,
                stamp: FileStamp {
                    size: metadata.len(),
                    modified: metadata.modified().ok(),
                },
            });
        }
        Ok(entries)
    }

    fn read(&self, name: &str) -> io::Result<Vec<u8>> {
        let path = self.path_of(name)?;
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "this is not a regular file",
            ));
        }
        if metadata.len() > MAX_FILE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "this file is too large to be an invoice",
            ));
        }
        fs::read(path)
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        fs::remove_file(self.path_of(name)?)
    }
}

/// Why a pick-up or an import did not work. A failed import leaves the original
/// where it is.
#[derive(Debug, thiserror::Error)]
pub enum PickupError {
    #[error("no company `{0}` in the configuration")]
    UnknownCompany(String),
    #[error("the company `{0}` is not in assisted mode")]
    NotAssisted(String),
    #[error("the company `{0}` has no `http` or `https` address in `portal.url`")]
    NoAddress(String),
    #[error("no downloads folder is known; please choose one under Settings")]
    NoDownloadsFolder,
    #[error("cannot look at the downloads folder {path}: {source}")]
    Folder { path: PathBuf, source: io::Error },
    #[error("the browser could not be opened: {0}")]
    OpenBrowser(String),
    #[error("this file is not waiting for your confirmation")]
    NotOffered,
    #[error("this invoice is no longer waiting to be picked up")]
    NotPending,
    #[error("cannot read {name}: {source}")]
    Read { name: String, source: io::Error },
    #[error("{0} is not a PDF")]
    NotPdf(String),
    #[error("{0} has changed since you saw it, so it was left alone")]
    Changed(String),
    #[error(transparent)]
    Plan(#[from] PlanError),
    #[error(transparent)]
    Index(#[from] IndexError),
    #[error(transparent)]
    Storage(#[from] StorageError),
}

/// What an import would do, as decided by the same rules as everywhere.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ImportDecision {
    Store,
    /// Stored for a person to complete, like a mail attachment without a number.
    ReviewNoNumber,
    /// As above. The file carries today's date until a person gives the real one.
    ReviewNoDate,
    /// The company has this number with a different file.
    ReviewNumberConflict,
    /// This file is known already. Nothing is stored and the original stays.
    Duplicate {
        existing: Option<PathBuf>,
        reason: Option<String>,
    },
}

impl ImportDecision {
    fn status(&self) -> Option<Status> {
        match self {
            Self::Store => Some(Status::Stored),
            Self::ReviewNoNumber | Self::ReviewNoDate | Self::ReviewNumberConflict => {
                Some(Status::NeedsReview)
            }
            Self::Duplicate { .. } => None,
        }
    }
}

/// What was read from a file, and where it would go.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImportPlan {
    pub hash: String,
    pub invoice_no: Option<String>,
    /// The date in the file. Without one, the name uses today's.
    pub date: Option<NaiveDate>,
    pub amount: Option<String>,
    pub currency: Option<String>,
    pub target_dir: PathBuf,
    pub file_name: String,
    /// Includes a suffix if the name is taken.
    pub target_path: PathBuf,
    pub decision: ImportDecision,
    #[serde(skip)]
    naming_date: NaiveDate,
}

/// What came of an import.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ImportOutcome {
    Stored {
        path: PathBuf,
        needs_review: bool,
        /// Why the original is still there, although the invoice is stored.
        original_kept: Option<String>,
    },
    /// Known already; the original stays where it is.
    Duplicate {
        existing: Option<PathBuf>,
        reason: Option<String>,
    },
}

fn read_pdf(folder: &dyn DownloadsFolder, name: &str) -> Result<Vec<u8>, PickupError> {
    let bytes = folder.read(name).map_err(|source| PickupError::Read {
        name: name.to_owned(),
        source,
    })?;
    if looks_like_pdf(&bytes) {
        Ok(bytes)
    } else {
        Err(PickupError::NotPdf(name.to_owned()))
    }
}

/// Works out what an import would do, changing nothing. `index` is `None` when no
/// index exists yet.
pub fn plan_import(
    config: &Config,
    company: &Company,
    index: Option<&Index>,
    bytes: &[u8],
    today: NaiveDate,
) -> Result<ImportPlan, PickupError> {
    let hash = sha256_hex(bytes);
    let facts = document::extract_facts(bytes);
    let check = match index {
        Some(index) => index.check_duplicate(&company.id, facts.invoice_no.as_deref(), &hash)?,
        None => DuplicateCheck::New,
    };
    let decision = match disposition_of(check, facts.invoice_no.is_some()) {
        Disposition::Store if facts.date.is_none() => ImportDecision::ReviewNoDate,
        Disposition::Store => ImportDecision::Store,
        Disposition::StoreForReview(ReviewReason::NumberNotFound) => ImportDecision::ReviewNoNumber,
        Disposition::StoreForReview(ReviewReason::NumberConflict { .. }) => {
            ImportDecision::ReviewNumberConflict
        }
        Disposition::AlreadyStored { path } => ImportDecision::Duplicate {
            existing: path,
            reason: None,
        },
        Disposition::Unresolved { reason } => ImportDecision::Duplicate {
            existing: None,
            reason: Some(reason),
        },
    };
    let naming_date = facts.date.unwrap_or(today);
    let target = target_of(
        config,
        company,
        naming_date,
        facts.invoice_no.as_deref(),
        &hash,
    )?;
    Ok(ImportPlan {
        hash,
        invoice_no: facts.invoice_no,
        date: facts.date,
        amount: facts.amount,
        currency: facts.currency,
        target_dir: target.dir,
        file_name: target.file_name,
        target_path: target.path,
        decision,
        naming_date,
    })
}

/// Reads a file and works out what an import would do, changing nothing.
pub fn preview_import(
    config: &Config,
    company: &Company,
    index: Option<&Index>,
    folder: &dyn DownloadsFolder,
    name: &str,
    today: NaiveDate,
) -> Result<ImportPlan, PickupError> {
    plan_import(config, company, index, &read_pdf(folder, name)?, today)
}

/// Files a PDF: stored byte for byte and verified, added to the index, and only
/// then is the original deleted. Calling this is the person's confirmation.
/// `expected_hash` is the file they saw, if they saw one.
pub fn import_file(
    config: &Config,
    company: &Company,
    index: &Index,
    folder: &dyn DownloadsFolder,
    name: &str,
    expected_hash: Option<&str>,
    today: NaiveDate,
) -> Result<ImportOutcome, PickupError> {
    let bytes = read_pdf(folder, name)?;
    let plan = plan_import(config, company, Some(index), &bytes, today)?;
    if expected_hash.is_some_and(|expected| expected != plan.hash) {
        return Err(PickupError::Changed(name.to_owned()));
    }
    let Some(status) = plan.decision.status() else {
        let ImportDecision::Duplicate { existing, reason } = plan.decision else {
            unreachable!("only a duplicate has no status");
        };
        return Ok(ImportOutcome::Duplicate { existing, reason });
    };

    let path = storage::store_verified(&plan.target_dir, &plan.file_name, &bytes)?;
    let inserted = index.insert(&NewInvoice {
        company_id: company.id.clone(),
        invoice_date: Some(plan.naming_date),
        invoice_no: plan.invoice_no,
        amount: plan.amount,
        currency: plan.currency,
        file_path: path.clone(),
        file_hash: plan.hash,
        source: Source::Portal,
        mail_account: None,
        mail_id: None,
        portal_key: None,
        status,
    });
    if let Err(error) = inserted {
        // Without an index entry a retry could not recognise this file, and would
        // store a second copy of it.
        let _ = fs::remove_file(&path);
        return Err(error.into());
    }
    Ok(ImportOutcome::Stored {
        path,
        needs_review: status == Status::NeedsReview,
        original_kept: folder.remove(name).err().map(|error| error.to_string()),
    })
}

/// A new PDF waiting for a person's decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Candidate {
    /// As the folder names it. Untrusted, and only ever shown as text.
    pub file_name: String,
    pub plan: ImportPlan,
}

/// What the interface shows of a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionView {
    pub company_id: String,
    pub company_name: String,
    pub folder: PathBuf,
    pub ends_at: DateTime<Utc>,
    pub watching: bool,
    pub candidates: Vec<Candidate>,
}

/// A person is fetching an invoice in their browser.
pub struct PickupSession {
    company_id: String,
    company_name: String,
    folder: Box<dyn DownloadsFolder>,
    ends_at: DateTime<Utc>,
    /// What was there at the start, which is none of this session's business.
    known: HashMap<String, FileStamp>,
    /// Seen at the last look, to tell when a download has come to rest.
    last_look: HashMap<String, FileStamp>,
    candidates: Vec<Candidate>,
}

impl std::fmt::Debug for PickupSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PickupSession")
            .field("company_id", &self.company_id)
            .field("ends_at", &self.ends_at)
            .finish_non_exhaustive()
    }
}

impl PickupSession {
    /// Notes what the folder holds, and then opens the company's portal address
    /// from the configuration. `open_url` stands for the system's default browser.
    pub fn start(
        config: &Config,
        company_id: &str,
        folder: Box<dyn DownloadsFolder>,
        now: DateTime<Utc>,
        open_url: impl FnOnce(&str) -> Result<(), String>,
    ) -> Result<Self, PickupError> {
        let company = find_company(config, company_id)?;
        if !company.is_assisted() {
            return Err(PickupError::NotAssisted(company_id.to_owned()));
        }
        let url = company
            .portal
            .as_ref()
            .and_then(|portal| portal.url.as_deref())
            .filter(|url| is_web_address(url))
            .ok_or_else(|| PickupError::NoAddress(company_id.to_owned()))?;
        let known = folder
            .list()
            .map_err(|source| PickupError::Folder {
                path: folder.location().to_path_buf(),
                source,
            })?
            .into_iter()
            .map(|entry| (entry.name, entry.stamp))
            .collect();
        open_url(url).map_err(PickupError::OpenBrowser)?;
        Ok(Self {
            company_id: company.id.clone(),
            company_name: company.name.clone(),
            folder,
            ends_at: now + TimeDelta::minutes(SESSION_LIMIT_MINUTES),
            known,
            last_look: HashMap::new(),
            candidates: Vec::new(),
        })
    }

    /// Whether the session still looks at the folder.
    pub fn is_watching(&self, now: DateTime<Utc>) -> bool {
        now < self.ends_at
    }

    /// PDFs waiting for a decision. They can outlast the watching.
    pub fn candidates(&self) -> &[Candidate] {
        &self.candidates
    }

    pub fn view(&self, now: DateTime<Utc>) -> SessionView {
        SessionView {
            company_id: self.company_id.clone(),
            company_name: self.company_name.clone(),
            folder: self.folder.location().to_path_buf(),
            ends_at: self.ends_at,
            watching: self.is_watching(now),
            candidates: self.candidates.clone(),
        }
    }

    /// One look at the folder; returns whether the session is still watching.
    /// Past the time limit it does not touch the folder any more. A file only gets
    /// opened once it stayed the same from one look to the next.
    pub fn poll(&mut self, config: &Config, index_path: &Path, now: DateTime<Utc>) -> bool {
        if !self.is_watching(now) {
            return false;
        }
        // A folder that went away may come back within the session.
        let Ok(entries) = self.folder.list() else {
            return true;
        };
        let mut this_look = HashMap::new();
        for entry in &entries {
            if !self.is_new_pdf(entry) || is_being_written(entry, &entries) {
                continue;
            }
            this_look.insert(entry.name.clone(), entry.stamp);
            if self.last_look.get(&entry.name) == Some(&entry.stamp)
                && self.examine(config, index_path, &entry.name, now.date_naive())
            {
                // Dealt with, unless the file changes after this.
                self.known.insert(entry.name.clone(), entry.stamp);
            }
        }
        self.last_look = this_look;
        true
    }

    fn is_new_pdf(&self, entry: &FolderEntry) -> bool {
        entry.is_file
            && entry.stamp.size > 0
            && is_pdf_name(&entry.name)
            && self.known.get(&entry.name) != Some(&entry.stamp)
    }

    /// Opens one file. Returns whether there is no need to look at it again, which
    /// is false if the index could not say anything about it just now.
    fn examine(
        &mut self,
        config: &Config,
        index_path: &Path,
        name: &str,
        today: NaiveDate,
    ) -> bool {
        let Ok(company) = find_company(config, &self.company_id) else {
            return true;
        };
        let Ok(index) = Index::open_read_only(index_path) else {
            return false;
        };
        let folder = self.folder.as_ref();
        match preview_import(config, company, index.as_ref(), folder, name, today) {
            Ok(plan) => {
                self.candidates.retain(|known| known.file_name != name);
                self.candidates.push(Candidate {
                    file_name: name.to_owned(),
                    plan,
                });
                true
            }
            // Not a PDF after all, or it cannot be read.
            Err(PickupError::NotPdf(_) | PickupError::Read { .. }) => true,
            Err(_) => false,
        }
    }

    /// A person confirmed that this file belongs to the company. Only a file this
    /// session offered can be named, and only unchanged. After a failure it stays
    /// on offer, and the original stays where it is.
    pub fn confirm(
        &mut self,
        config: &Config,
        index: &Index,
        file_name: &str,
        today: NaiveDate,
    ) -> Result<ImportOutcome, PickupError> {
        let position = self.position_of(file_name)?;
        let company = find_company(config, &self.company_id)?;
        let outcome = import_file(
            config,
            company,
            index,
            self.folder.as_ref(),
            file_name,
            Some(&self.candidates[position].plan.hash),
            today,
        )?;
        self.candidates.remove(position);
        Ok(outcome)
    }

    /// A person said this file does not belong here. It stays in the folder and
    /// is not offered again unless it changes.
    pub fn ignore(&mut self, file_name: &str) -> Result<(), PickupError> {
        let position = self.position_of(file_name)?;
        self.candidates.remove(position);
        Ok(())
    }

    fn position_of(&self, file_name: &str) -> Result<usize, PickupError> {
        self.candidates
            .iter()
            .position(|candidate| candidate.file_name == file_name)
            .ok_or(PickupError::NotOffered)
    }
}

fn find_company<'a>(config: &'a Config, id: &str) -> Result<&'a Company, PickupError> {
    config
        .companies
        .iter()
        .find(|company| company.id == id)
        .ok_or_else(|| PickupError::UnknownCompany(id.to_owned()))
}

fn is_pdf_name(name: &str) -> bool {
    !name.starts_with('.')
        && Path::new(name)
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
}

/// Some browsers put an empty file under the final name and write next to it.
fn is_being_written(entry: &FolderEntry, entries: &[FolderEntry]) -> bool {
    PARTIAL_SUFFIXES.iter().any(|suffix| {
        let partial = format!("{}.{suffix}", entry.name);
        entries.iter().any(|other| other.name == partial)
    })
}

/// An invoice waiting to be picked up by a person.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PickupItem {
    /// The row of the link mail in the index.
    pub id: i64,
    pub company_id: String,
    pub company_name: String,
    pub mail_date: Option<NaiveDate>,
}

/// The link mails of assisted companies whose invoice is not here yet, newest
/// first. An invoice that waits for review is here, and needs no second pick-up.
pub fn pending_pickups(config: &Config, index: &Index) -> Result<Vec<PickupItem>, IndexError> {
    let mut items = Vec::new();
    for row in index.waiting_link_mails()? {
        let Some(company) = config
            .companies
            .iter()
            .find(|company| company.id == row.company_id && company.is_assisted())
        else {
            continue;
        };
        let (Some(account), Some(mail)) = (&row.mail_account, &row.mail_id) else {
            continue;
        };
        if !index.link_mail_has_document(&company.id, account, mail)? {
            items.push(PickupItem {
                id: row.id,
                company_id: company.id.clone(),
                company_name: company.name.clone(),
                mail_date: row.invoice_date,
            });
        }
    }
    Ok(items)
}

/// A person says that a pick-up is done, for instance because they deleted the
/// mail by hand or the invoice is stored already. Only a listed pick-up can be
/// dismissed. Its row is resolved without an invoice and without anything done
/// to the mail: the mail stays where it is, and later scans leave it alone.
pub fn dismiss_pickup(config: &Config, index: &Index, id: i64) -> Result<(), PickupError> {
    let listed = pending_pickups(config, index)?
        .iter()
        .any(|item| item.id == id);
    if !listed || !index.dismiss_link_mail(id)? {
        return Err(PickupError::NotPending);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::test_pdf;
    use std::sync::{Arc, Mutex};

    const URL: &str = "https://portal.example/login";

    struct World {
        downloads: tempfile::TempDir,
        output: tempfile::TempDir,
        data: tempfile::TempDir,
        config: Config,
    }

    fn world() -> World {
        let downloads = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let config = Config::from_yaml(&format!(
            r#"
mail:
  accounts:
    - {{ id: main, imap: "i.example:993", user: u, password: "keyring:p", watch: INBOX }}
  disposal: {{ mode: all_delete }}
companies:
  - id: acme
    name: Acme
    mail: {{ from_domain: acme.example }}
    portal: {{ mode: assisted, url: "{URL}" }}
    output: {{ dir: '{}', filename: "{{date:%Y-%m-%d}} - Acme - {{invoice_no}}.pdf" }}
  - id: robot
    name: Robot
    portal: {{ credentials: "keyring:c", recipe: r.yaml }}
    output: {{ dir: '{}', filename: "{{invoice_no}}.pdf" }}
"#,
            output.path().display(),
            output.path().display(),
        ))
        .unwrap();
        World {
            downloads,
            output,
            data,
            config,
        }
    }

    /// Writes down what is listed, opened and removed.
    #[derive(Clone, Default)]
    struct Log {
        lists: Arc<Mutex<u32>>,
        reads: Arc<Mutex<Vec<String>>>,
        removes: Arc<Mutex<Vec<String>>>,
        /// Stands in for the state of the index when a file is removed.
        rows_at_remove: Arc<Mutex<Vec<usize>>>,
    }

    struct SpyFolder {
        inner: LocalFolder,
        log: Log,
        index: Option<PathBuf>,
        refuse_remove: bool,
    }

    impl DownloadsFolder for SpyFolder {
        fn location(&self) -> &Path {
            self.inner.location()
        }
        fn list(&self) -> io::Result<Vec<FolderEntry>> {
            *self.log.lists.lock().unwrap() += 1;
            self.inner.list()
        }
        fn read(&self, name: &str) -> io::Result<Vec<u8>> {
            self.log.reads.lock().unwrap().push(name.to_owned());
            self.inner.read(name)
        }
        fn remove(&self, name: &str) -> io::Result<()> {
            self.log.removes.lock().unwrap().push(name.to_owned());
            if let Some(path) = &self.index {
                let index = Index::open_read_only(path).unwrap().unwrap();
                let rows = index.invoices_of("acme").unwrap().len();
                self.log.rows_at_remove.lock().unwrap().push(rows);
            }
            if self.refuse_remove {
                return Err(io::Error::other("the file is in use"));
            }
            self.inner.remove(name)
        }
    }

    impl World {
        fn file(&self, name: &str) -> PathBuf {
            self.downloads.path().join(name)
        }

        fn write(&self, name: &str, bytes: &[u8]) {
            fs::write(self.file(name), bytes).unwrap();
        }

        fn index_path(&self) -> PathBuf {
            self.data.path().join("docsweep.db")
        }

        fn index(&self) -> Index {
            Index::open(&self.index_path()).unwrap()
        }

        fn stored_files(&self) -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(self.output.path())
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect();
            names.sort();
            names
        }

        fn spy(&self, log: &Log) -> Box<SpyFolder> {
            Box::new(SpyFolder {
                inner: LocalFolder::new(self.downloads.path().to_path_buf()),
                log: log.clone(),
                index: None,
                refuse_remove: false,
            })
        }

        fn session(&self, log: &Log) -> PickupSession {
            self.index();
            PickupSession::start(
                &self.config,
                "acme",
                self.spy(log),
                start_time(),
                |_| Ok(()),
            )
            .unwrap()
        }

        /// Two looks, so that a file at rest gets its turn.
        fn look_twice(&self, session: &mut PickupSession) {
            let now = start_time() + TimeDelta::seconds(5);
            assert!(session.poll(&self.config, &self.index_path(), now));
            assert!(session.poll(&self.config, &self.index_path(), now));
        }
    }

    fn start_time() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-06T12:00:00Z")
            .unwrap()
            .to_utc()
    }

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, 6).unwrap()
    }

    fn invoice(number: &str) -> Vec<u8> {
        test_pdf::with_text(&[
            &format!("Rechnungsnummer: {number}"),
            "Rechnungsdatum: 03.11.2026",
        ])
    }

    fn offered(session: &PickupSession) -> Vec<&str> {
        session
            .candidates()
            .iter()
            .map(|candidate| candidate.file_name.as_str())
            .collect()
    }

    #[test]
    fn only_files_that_appear_after_the_start_are_considered() {
        let world = world();
        world.write("old.pdf", &invoice("OLD-1"));
        let log = Log::default();
        let mut session = world.session(&log);

        world.write("new.pdf", &invoice("NEW-1"));
        world.look_twice(&mut session);

        assert_eq!(offered(&session), ["new.pdf"]);
        assert_eq!(
            *log.reads.lock().unwrap(),
            ["new.pdf"],
            "old.pdf is not opened"
        );
    }

    #[test]
    fn a_file_with_an_old_time_that_arrives_during_the_session_counts_as_new() {
        let world = world();
        let log = Log::default();
        let mut session = world.session(&log);

        world.write("moved.pdf", &invoice("R-1"));
        let long_ago = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        fs::File::options()
            .write(true)
            .open(world.file("moved.pdf"))
            .unwrap()
            .set_modified(long_ago)
            .unwrap();
        world.look_twice(&mut session);

        assert_eq!(offered(&session), ["moved.pdf"]);
    }

    #[test]
    fn a_known_file_that_gets_overwritten_counts_as_new() {
        let world = world();
        world.write("invoice.pdf", &invoice("R-1"));
        let log = Log::default();
        let mut session = world.session(&log);
        world.look_twice(&mut session);
        assert!(session.candidates().is_empty());

        world.write("invoice.pdf", &invoice("R-22222"));
        world.look_twice(&mut session);

        assert_eq!(offered(&session), ["invoice.pdf"]);
    }

    #[test]
    fn other_and_partial_files_are_ignored_and_not_opened() {
        let world = world();
        let log = Log::default();
        let mut session = world.session(&log);

        let pdf = invoice("R-1");
        for name in [
            "notes.txt",
            "photo.jpg",
            "no-extension",
            "invoice.pdf.crdownload",
            "invoice.pdf.part",
            "invoice.pdf.download",
            "Unconfirmed 123.crdownload",
            ".hidden.pdf",
        ] {
            world.write(name, &pdf);
        }
        // As some browsers do: an empty file under the final name, or one next to
        // the partial download.
        world.write("empty.pdf", b"");
        world.write("busy.pdf", &pdf);
        world.write("busy.pdf.part", b"more to come");
        world.write("invoice.pdf", b"");
        // Subfolders are not looked into.
        fs::create_dir(world.file("sub")).unwrap();
        world.write("sub/deep.pdf", &pdf);
        fs::create_dir(world.file("folder.pdf")).unwrap();

        world.look_twice(&mut session);

        assert!(session.candidates().is_empty());
        let reads = log.reads.lock().unwrap();
        assert!(reads.is_empty(), "opened: {reads:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_is_ignored_and_not_followed() {
        let world = world();
        let log = Log::default();
        let mut session = world.session(&log);
        let outside = world.data.path().join("outside.pdf");
        fs::write(&outside, invoice("R-1")).unwrap();
        std::os::unix::fs::symlink(&outside, world.file("link.pdf")).unwrap();

        world.look_twice(&mut session);

        assert!(session.candidates().is_empty());
        assert!(log.reads.lock().unwrap().is_empty());
        let folder = LocalFolder::new(world.downloads.path().to_path_buf());
        assert!(folder.read("link.pdf").is_err(), "not even when named");
    }

    #[test]
    fn a_file_named_pdf_with_other_content_is_not_offered_and_opened_once() {
        let world = world();
        let log = Log::default();
        let mut session = world.session(&log);
        world.write("fake.pdf", b"MZ this is a program");

        world.look_twice(&mut session);
        world.look_twice(&mut session);

        assert!(session.candidates().is_empty());
        assert_eq!(*log.reads.lock().unwrap(), ["fake.pdf"]);
    }

    #[test]
    fn a_file_is_only_opened_once_its_size_stays_the_same() {
        let world = world();
        let log = Log::default();
        let mut session = world.session(&log);
        let now = start_time() + TimeDelta::seconds(5);
        let pdf = invoice("R-1");

        for written in [10, 20, pdf.len()] {
            world.write("growing.pdf", &pdf[..written]);
            assert!(session.poll(&world.config, &world.index_path(), now));
            assert!(log.reads.lock().unwrap().is_empty());
        }
        assert!(session.poll(&world.config, &world.index_path(), now));

        assert_eq!(offered(&session), ["growing.pdf"]);
    }

    #[test]
    fn nothing_is_imported_without_a_confirmation() {
        let world = world();
        let log = Log::default();
        let mut session = world.session(&log);
        world.write("invoice.pdf", &invoice("R-1"));

        for _ in 0..5 {
            world.look_twice(&mut session);
        }

        assert_eq!(offered(&session), ["invoice.pdf"]);
        let plan = &session.candidates()[0].plan;
        assert_eq!(plan.invoice_no.as_deref(), Some("R-1"));
        assert_eq!(plan.date, NaiveDate::from_ymd_opt(2026, 11, 3));
        assert_eq!(plan.decision, ImportDecision::Store);
        assert!(world.stored_files().is_empty());
        assert!(world.index().invoices_of("acme").unwrap().is_empty());
        assert!(world.file("invoice.pdf").exists());
        assert!(log.removes.lock().unwrap().is_empty());
    }

    #[test]
    fn saying_no_leaves_the_file_alone_and_does_not_ask_again() {
        let world = world();
        let log = Log::default();
        let mut session = world.session(&log);
        world.write("private.pdf", &invoice("R-1"));
        world.look_twice(&mut session);

        session.ignore("private.pdf").unwrap();
        world.look_twice(&mut session);

        assert!(session.candidates().is_empty());
        assert!(world.file("private.pdf").exists());
        assert!(world.stored_files().is_empty());
    }

    #[test]
    fn a_confirmed_file_is_stored_byte_for_byte_and_the_original_goes() {
        let world = world();
        let log = Log::default();
        let mut session = world.session(&log);
        let pdf = invoice("R-1");
        world.write("invoice.pdf", &pdf);
        world.look_twice(&mut session);

        let outcome = session
            .confirm(&world.config, &world.index(), "invoice.pdf", today())
            .unwrap();

        let expected = world.output.path().join("2026-11-03 - Acme - R-1.pdf");
        assert_eq!(
            outcome,
            ImportOutcome::Stored {
                path: expected.clone(),
                needs_review: false,
                original_kept: None,
            }
        );
        assert_eq!(fs::read(&expected).unwrap(), pdf);
        assert!(!world.file("invoice.pdf").exists());
        assert!(session.candidates().is_empty());
        let row = world.index().invoices_of("acme").unwrap().remove(0);
        assert_eq!(row.source, Source::Portal);
        assert_eq!(row.status, Status::Stored);
        assert_eq!(row.invoice_no.as_deref(), Some("R-1"));
        assert_eq!(row.file_hash.as_deref(), Some(sha256_hex(&pdf).as_str()));
    }

    #[test]
    fn the_original_is_deleted_only_after_the_index_entry_exists() {
        let world = world();
        let index = world.index();
        world.write("invoice.pdf", &invoice("R-1"));
        let log = Log::default();
        let folder = SpyFolder {
            inner: LocalFolder::new(world.downloads.path().to_path_buf()),
            log: log.clone(),
            index: Some(world.index_path()),
            refuse_remove: false,
        };
        let company = &world.config.companies[0];

        import_file(
            &world.config,
            company,
            &index,
            &folder,
            "invoice.pdf",
            None,
            today(),
        )
        .unwrap();

        assert_eq!(*log.rows_at_remove.lock().unwrap(), [1]);
    }

    #[test]
    fn a_failing_index_keeps_the_original_and_stores_nothing() {
        let world = world();
        let log = Log::default();
        let mut session = world.session(&log);
        world.write("invoice.pdf", &invoice("R-1"));
        world.look_twice(&mut session);
        let index = world.index();
        index.refuse_inserts_for_test();

        let error = session.confirm(&world.config, &index, "invoice.pdf", today());

        assert!(matches!(error, Err(PickupError::Index(_))), "{error:?}");
        assert!(world.file("invoice.pdf").exists());
        assert!(world.stored_files().is_empty());
        assert!(log.removes.lock().unwrap().is_empty());
        assert_eq!(offered(&session), ["invoice.pdf"], "it can be tried again");
    }

    #[test]
    fn a_failing_target_folder_keeps_the_original() {
        let world = world();
        let log = Log::default();
        let mut session = world.session(&log);
        world.write("invoice.pdf", &invoice("R-1"));
        world.look_twice(&mut session);
        let blocked = world.data.path().join("blocked");
        fs::write(&blocked, b"x").unwrap();
        let mut config = world.config.clone();
        config.companies[0].output.dir = Some(blocked.display().to_string());

        let error = session.confirm(&config, &world.index(), "invoice.pdf", today());

        assert!(matches!(error, Err(PickupError::Storage(_))), "{error:?}");
        assert!(world.file("invoice.pdf").exists());
        assert!(world.index().invoices_of("acme").unwrap().is_empty());
    }

    #[test]
    fn a_duplicate_is_reported_and_its_original_stays() {
        let world = world();
        let index = world.index();
        let company = &world.config.companies[0];
        let pdf = invoice("R-1");
        let folder = LocalFolder::new(world.downloads.path().to_path_buf());
        world.write("first.pdf", &pdf);
        import_file(
            &world.config,
            company,
            &index,
            &folder,
            "first.pdf",
            None,
            today(),
        )
        .unwrap();

        let log = Log::default();
        let mut session = world.session(&log);
        world.write("again.pdf", &pdf);
        world.look_twice(&mut session);
        let shown = &session.candidates()[0].plan.decision;
        assert!(
            matches!(shown, ImportDecision::Duplicate { .. }),
            "{shown:?}"
        );

        let outcome = session
            .confirm(&world.config, &index, "again.pdf", today())
            .unwrap();

        let expected = world.output.path().join("2026-11-03 - Acme - R-1.pdf");
        assert_eq!(
            outcome,
            ImportOutcome::Duplicate {
                existing: Some(expected),
                reason: None,
            }
        );
        assert!(world.file("again.pdf").exists());
        assert_eq!(world.stored_files().len(), 1);
        assert_eq!(index.invoices_of("acme").unwrap().len(), 1);
        world.look_twice(&mut session);
        assert!(session.candidates().is_empty(), "not asked again");
    }

    /// Another PDF file with the same number: the same text and a different date.
    fn same_number_other_file() -> Vec<u8> {
        test_pdf::with_text(&["Rechnungsnummer: R-1", "Rechnungsdatum: 04.11.2026"])
    }

    #[test]
    fn the_same_number_with_another_file_goes_to_review_as_everywhere() {
        let world = world();
        let index = world.index();
        let company = &world.config.companies[0];
        let folder = LocalFolder::new(world.downloads.path().to_path_buf());
        world.write("first.pdf", &invoice("R-1"));
        import_file(
            &world.config,
            company,
            &index,
            &folder,
            "first.pdf",
            None,
            today(),
        )
        .unwrap();

        world.write("second.pdf", &same_number_other_file());
        let plan = preview_import(
            &world.config,
            company,
            Some(&index),
            &folder,
            "second.pdf",
            today(),
        )
        .unwrap();
        assert_eq!(plan.decision, ImportDecision::ReviewNumberConflict);

        let outcome = import_file(
            &world.config,
            company,
            &index,
            &folder,
            "second.pdf",
            None,
            today(),
        )
        .unwrap();

        assert!(matches!(
            outcome,
            ImportOutcome::Stored {
                needs_review: true,
                ..
            }
        ));
        assert!(!world.file("second.pdf").exists());
    }

    #[test]
    fn a_missing_number_or_date_means_review() {
        let world = world();
        let index = world.index();
        let company = &world.config.companies[0];
        let folder = LocalFolder::new(world.downloads.path().to_path_buf());
        let cases = [
            (
                "no-number.pdf",
                test_pdf::with_text(&["Rechnungsdatum: 03.11.2026"]),
                ImportDecision::ReviewNoNumber,
            ),
            (
                "no-date.pdf",
                test_pdf::with_text(&["Rechnungsnummer: R-5"]),
                ImportDecision::ReviewNoDate,
            ),
        ];
        for (name, pdf, decision) in cases {
            world.write(name, &pdf);
            let plan = preview_import(&world.config, company, Some(&index), &folder, name, today())
                .unwrap();
            assert_eq!(plan.decision, decision);
            let outcome =
                import_file(&world.config, company, &index, &folder, name, None, today()).unwrap();
            assert!(matches!(
                outcome,
                ImportOutcome::Stored {
                    needs_review: true,
                    ..
                }
            ));
        }
        let rows = index.invoices_of("acme").unwrap();
        assert!(rows.iter().all(|row| row.status == Status::NeedsReview));
        assert_eq!(rows[1].invoice_date, Some(today()), "a date to start from");
        assert!(
            world
                .stored_files()
                .iter()
                .any(|name| name.contains("unbekannt-"))
        );
    }

    #[test]
    fn a_failed_removal_is_reported_but_the_invoice_stays_stored() {
        let world = world();
        let index = world.index();
        world.write("invoice.pdf", &invoice("R-1"));
        let folder = SpyFolder {
            inner: LocalFolder::new(world.downloads.path().to_path_buf()),
            log: Log::default(),
            index: None,
            refuse_remove: true,
        };
        let company = &world.config.companies[0];

        let outcome = import_file(
            &world.config,
            company,
            &index,
            &folder,
            "invoice.pdf",
            None,
            today(),
        )
        .unwrap();

        let ImportOutcome::Stored { original_kept, .. } = outcome else {
            panic!("expected it to be stored: {outcome:?}");
        };
        assert!(original_kept.unwrap().contains("in use"));
        assert!(world.file("invoice.pdf").exists());
        assert_eq!(index.invoices_of("acme").unwrap().len(), 1);
    }

    #[test]
    fn a_file_that_changed_after_it_was_shown_is_left_alone() {
        let world = world();
        let log = Log::default();
        let mut session = world.session(&log);
        world.write("invoice.pdf", &invoice("R-1"));
        world.look_twice(&mut session);

        let other = invoice("EVIL-1");
        world.write("invoice.pdf", &other);
        let result = session.confirm(&world.config, &world.index(), "invoice.pdf", today());

        assert!(matches!(result, Err(PickupError::Changed(_))), "{result:?}");
        assert_eq!(fs::read(world.file("invoice.pdf")).unwrap(), other);
        assert!(world.stored_files().is_empty());
    }

    #[test]
    fn crafted_names_and_numbers_stay_in_their_folders() {
        let world = world();
        let log = Log::default();
        let mut session = world.session(&log);
        let name = "evil ..&img src=x onerror=alert(1)& ..pdf";
        world.write(name, &invoice("R1/../../evil"));
        world.look_twice(&mut session);
        assert_eq!(offered(&session), [name]);

        // Only what the session offered can be named, whatever comes in.
        for crafted in ["../../secret.pdf", "/etc/passwd", "sub/deep.pdf", ".."] {
            let result = session.confirm(&world.config, &world.index(), crafted, today());
            assert!(matches!(result, Err(PickupError::NotOffered)), "{crafted}");
            assert!(matches!(
                session.ignore(crafted),
                Err(PickupError::NotOffered)
            ));
        }

        let outcome = session
            .confirm(&world.config, &world.index(), name, today())
            .unwrap();

        let ImportOutcome::Stored { path, .. } = outcome else {
            panic!("expected it to be stored: {outcome:?}");
        };
        assert_eq!(path.parent(), Some(world.output.path()));
        let stored = path.file_name().unwrap().to_str().unwrap();
        assert!(!stored.contains("img"), "the name comes from the template");
        assert!(!stored.contains('/') && !stored.contains(".."), "{stored}");
        let outside = world.output.path().parent().unwrap().join("evil.pdf");
        assert!(!outside.exists());

        let folder = LocalFolder::new(world.downloads.path().to_path_buf());
        for crafted in ["../secret.pdf", "..", "/etc/passwd", ""] {
            assert!(folder.read(crafted).is_err(), "{crafted}");
            assert!(folder.remove(crafted).is_err(), "{crafted}");
        }
    }

    #[test]
    fn the_session_stops_looking_at_the_time_limit() {
        let world = world();
        let log = Log::default();
        let mut session = world.session(&log);
        let limit = start_time() + TimeDelta::minutes(SESSION_LIMIT_MINUTES);
        let index = world.index_path();

        let just_before = limit - TimeDelta::seconds(1);
        assert!(session.poll(&world.config, &index, just_before));
        let looks = *log.lists.lock().unwrap();

        world.write("late.pdf", &invoice("R-1"));
        assert!(!session.poll(&world.config, &index, limit));
        assert!(!session.poll(&world.config, &index, limit + TimeDelta::hours(5)));

        assert_eq!(
            *log.lists.lock().unwrap(),
            looks,
            "the folder was left alone"
        );
        assert!(log.reads.lock().unwrap().is_empty());
        assert!(session.candidates().is_empty());
        assert!(!session.view(limit).watching);
    }

    #[test]
    fn a_file_found_in_time_can_still_be_confirmed_after_the_limit() {
        let world = world();
        let log = Log::default();
        let mut session = world.session(&log);
        world.write("invoice.pdf", &invoice("R-1"));
        world.look_twice(&mut session);

        let late = start_time() + TimeDelta::minutes(SESSION_LIMIT_MINUTES + 5);
        assert!(!session.poll(&world.config, &world.index_path(), late));
        session
            .confirm(&world.config, &world.index(), "invoice.pdf", today())
            .unwrap();

        assert_eq!(world.stored_files().len(), 1);
    }

    #[test]
    fn starting_opens_the_configured_address_and_only_that() {
        let world = world();
        let mut opened = Vec::new();
        let folder = LocalFolder::new(world.downloads.path().to_path_buf());

        let session = PickupSession::start(
            &world.config,
            "acme",
            Box::new(folder),
            start_time(),
            |url| {
                opened.push(url.to_owned());
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(opened, [URL]);
        let view = session.view(start_time());
        assert!(view.watching);
        assert_eq!(view.company_name, "Acme");
    }

    #[test]
    fn non_web_addresses_are_refused_even_if_they_got_past_validation() {
        let world = world();
        for url in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "ftp://x.example/",
        ] {
            let mut config = world.config.clone();
            config.companies[0].portal.as_mut().unwrap().url = Some(url.to_owned());
            let folder = LocalFolder::new(world.downloads.path().to_path_buf());

            let result =
                PickupSession::start(&config, "acme", Box::new(folder), start_time(), |_| {
                    panic!("nothing may be opened");
                });

            assert!(matches!(result, Err(PickupError::NoAddress(_))), "{url}");
        }
    }

    #[test]
    fn only_an_assisted_company_can_start_a_session() {
        let world = world();
        let start = |id: &str| {
            let folder = LocalFolder::new(world.downloads.path().to_path_buf());
            PickupSession::start(&world.config, id, Box::new(folder), start_time(), |_| {
                panic!("nothing may be opened");
            })
        };
        assert!(matches!(start("robot"), Err(PickupError::NotAssisted(_))));
        assert!(matches!(start("nope"), Err(PickupError::UnknownCompany(_))));
    }

    #[test]
    fn a_missing_folder_stops_the_start_before_a_browser_opens() {
        let world = world();
        let folder = LocalFolder::new(world.downloads.path().join("missing"));
        let result = PickupSession::start(
            &world.config,
            "acme",
            Box::new(folder),
            start_time(),
            |_| {
                panic!("nothing may be opened");
            },
        );
        assert!(matches!(result, Err(PickupError::Folder { .. })));
    }

    #[test]
    fn a_failing_browser_is_reported() {
        let world = world();
        let folder = LocalFolder::new(world.downloads.path().to_path_buf());
        let result = PickupSession::start(
            &world.config,
            "acme",
            Box::new(folder),
            start_time(),
            |_| Err("no browser".to_owned()),
        );
        assert!(matches!(result, Err(PickupError::OpenBrowser(_))));
    }

    #[test]
    fn pending_pickups_lists_waiting_mails_of_assisted_companies_only() {
        let world = world();
        let index = world.index();
        let date = NaiveDate::from_ymd_opt(2026, 11, 2);
        for (company, mail) in [("acme", "a@x"), ("robot", "r@x"), ("gone", "g@x")] {
            index
                .record_portal_fetch_required(company, "main", mail, date)
                .unwrap();
        }

        let items = pending_pickups(&world.config, &index).unwrap();

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].company_name, "Acme");
        assert_eq!(items[0].mail_date, date);
    }

    #[test]
    fn a_dismissed_pickup_is_no_longer_listed_and_only_listed_ones_can_be_dismissed() {
        let world = world();
        let index = world.index();
        for (company, mail) in [("acme", "a@x"), ("robot", "r@x")] {
            index
                .record_portal_fetch_required(company, "main", mail, None)
                .unwrap();
        }
        let rows = |company| index.invoices_of(company).unwrap().remove(0);
        let robot = rows("robot").id;
        assert!(matches!(
            dismiss_pickup(&world.config, &index, robot),
            Err(PickupError::NotPending)
        ));
        assert!(matches!(
            dismiss_pickup(&world.config, &index, 999),
            Err(PickupError::NotPending)
        ));
        assert_eq!(rows("robot").resolved_at, None);

        let item = pending_pickups(&world.config, &index).unwrap().remove(0);
        dismiss_pickup(&world.config, &index, item.id).unwrap();

        assert!(pending_pickups(&world.config, &index).unwrap().is_empty());
        let row = rows("acme");
        assert!(row.dismissed_at.is_some());
        assert_eq!(row.disposed_at, None);
        assert!(matches!(
            dismiss_pickup(&world.config, &index, item.id),
            Err(PickupError::NotPending)
        ));
    }

    #[test]
    fn a_stored_invoice_or_one_in_review_ends_the_pending_pickup() {
        for pdf in [invoice("R-1"), test_pdf::with_text(&["Keine Nummer"])] {
            let world = world();
            let index = world.index();
            index
                .record_portal_fetch_required("acme", "main", "a@x", None)
                .unwrap();
            assert_eq!(pending_pickups(&world.config, &index).unwrap().len(), 1);

            let folder = LocalFolder::new(world.downloads.path().to_path_buf());
            world.write("invoice.pdf", &pdf);
            let company = &world.config.companies[0];
            import_file(
                &world.config,
                company,
                &index,
                &folder,
                "invoice.pdf",
                None,
                today(),
            )
            .unwrap();

            assert!(pending_pickups(&world.config, &index).unwrap().is_empty());
        }
    }

    #[test]
    fn an_invoice_in_review_does_not_clean_up_the_mail_until_it_is_completed() {
        let world = world();
        let index = world.index();
        index
            .record_portal_fetch_required("acme", "main", "a@x", None)
            .unwrap();
        let folder = LocalFolder::new(world.downloads.path().to_path_buf());
        world.write("invoice.pdf", &test_pdf::with_text(&["Keine Nummer"]));
        let company = &world.config.companies[0];
        import_file(
            &world.config,
            company,
            &index,
            &folder,
            "invoice.pdf",
            None,
            today(),
        )
        .unwrap();
        let matches = || {
            index
                .has_invoice_for_link_mail("acme", "main", "a@x")
                .unwrap()
        };
        assert!(!matches());

        let row = index.invoices_of("acme").unwrap().remove(1);
        crate::review::complete_review(&world.config, &index, row.id, "R-9", today()).unwrap();

        assert!(matches());
    }
}
