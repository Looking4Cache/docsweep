//! SQLite index of all known invoices and the duplicate check.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, NaiveDate, SecondsFormat, Utc};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, params};
use serde::Serialize;

/// Schema migrations; the position plus one is the `user_version` it leads to.
const MIGRATIONS: &[&str] = &[
    r#"
    CREATE TABLE invoices (
        id            INTEGER PRIMARY KEY,
        company_id    TEXT NOT NULL,
        invoice_date  TEXT,
        invoice_no    TEXT,
        amount        TEXT,
        currency      TEXT,
        file_path     TEXT,
        file_hash     TEXT,
        source        TEXT NOT NULL CHECK (source IN ('mail', 'portal')),
        mail_account  TEXT,
        mail_id       TEXT,
        status        TEXT NOT NULL
                      CHECK (status IN ('stored', 'needs_review', 'portal_fetch_required')),
        disposed_at   TEXT,
        created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
    );
    CREATE UNIQUE INDEX invoices_file_hash ON invoices (file_hash)
        WHERE file_hash IS NOT NULL;
    CREATE INDEX invoices_company_number ON invoices (company_id, invoice_no);
    CREATE INDEX invoices_mail ON invoices (mail_account, mail_id);
    CREATE UNIQUE INDEX invoices_pending_portal_fetch
        ON invoices (company_id, mail_account, mail_id)
        WHERE status = 'portal_fetch_required';
"#,
    r#"
    CREATE TABLE scan_runs (
        id           INTEGER PRIMARY KEY,
        finished_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
        summary      TEXT NOT NULL,
        report       TEXT NOT NULL
    );
"#,
    r#"
    ALTER TABLE invoices ADD COLUMN portal_key TEXT;
    CREATE UNIQUE INDEX invoices_portal_key ON invoices (company_id, portal_key)
        WHERE portal_key IS NOT NULL;
"#,
    r#"
    ALTER TABLE invoices ADD COLUMN resolved_at TEXT;
    ALTER TABLE scan_runs ADD COLUMN automatic INTEGER NOT NULL DEFAULT 0;
    CREATE TABLE company_runs (
        company_id          TEXT PRIMARY KEY,
        last_link_fetch_on  TEXT,
        last_scheduled_run  TEXT
    );
"#,
    r#"
    CREATE TABLE ai_consents (
        company_id  TEXT NOT NULL,
        provider    TEXT NOT NULL,
        granted_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
        PRIMARY KEY (company_id, provider)
    );
"#,
    r#"
    ALTER TABLE invoices ADD COLUMN settled_by INTEGER;
"#,
    r#"
    ALTER TABLE invoices ADD COLUMN dismissed_at TEXT;
"#,
    r#"
    ALTER TABLE invoices ADD COLUMN given_up_at TEXT;
"#,
];

/// The first schema version with the column `portal_key`. A dry run reads an
/// index without migrating it, so it can meet older ones.
const PORTAL_KEY_VERSION: usize = 3;
/// The first schema version that knows about resolved rows, automatic runs and
/// the table `company_runs`.
const AUTOMATION_VERSION: usize = 4;
/// The first schema version in which a link mail's row names the invoice that
/// settled it (`settled_by`).
const SETTLED_BY_VERSION: usize = 6;
/// The first schema version in which a person can mark a link mail as done
/// (`dismissed_at`).
const DISMISSED_VERSION: usize = 7;
/// The first schema version in which a link mail can stop waiting because its
/// invoice did not come in time (`given_up_at`).
const GIVEN_UP_VERSION: usize = 8;

/// Where an invoice came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Mail,
    Portal,
}

impl Source {
    fn as_str(self) -> &'static str {
        match self {
            Self::Mail => "mail",
            Self::Portal => "portal",
        }
    }

    fn parse(value: &str) -> rusqlite::Result<Self> {
        match value {
            "mail" => Ok(Self::Mail),
            "portal" => Ok(Self::Portal),
            other => Err(unknown_value(other)),
        }
    }
}

/// Processing state of an invoice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Stored and complete.
    Stored,
    /// Stored, but a person needs to look at it; its mail stays in place.
    NeedsReview,
    /// A link mail was seen; the invoice has to be fetched from the portal.
    PortalFetchRequired,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Self::Stored => "stored",
            Self::NeedsReview => "needs_review",
            Self::PortalFetchRequired => "portal_fetch_required",
        }
    }

    fn parse(value: &str) -> rusqlite::Result<Self> {
        match value {
            "stored" => Ok(Self::Stored),
            "needs_review" => Ok(Self::NeedsReview),
            "portal_fetch_required" => Ok(Self::PortalFetchRequired),
            other => Err(unknown_value(other)),
        }
    }
}

fn unknown_value(value: &str) -> rusqlite::Error {
    rusqlite::Error::InvalidParameterName(format!("unknown value `{value}` in the index"))
}

/// How a noted link mail stopped waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkClosure {
    /// Its invoice is stored and Docsweep cleaned up the mail.
    Settled,
    /// A person marked it as done; Docsweep did nothing to the mail.
    Dismissed,
    /// No invoice came in time; Docsweep flagged the mail and stopped waiting.
    GivenUp,
}

/// A stored document to add to the index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewInvoice {
    pub company_id: String,
    pub invoice_date: Option<NaiveDate>,
    pub invoice_no: Option<String>,
    pub amount: Option<String>,
    pub currency: Option<String>,
    pub file_path: PathBuf,
    pub file_hash: String,
    pub source: Source,
    pub mail_account: Option<String>,
    pub mail_id: Option<String>,
    /// What identifies this document in the portal's list across runs.
    pub portal_key: Option<String>,
    pub status: Status,
}

/// A row of the index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InvoiceRecord {
    pub id: i64,
    pub company_id: String,
    pub invoice_date: Option<NaiveDate>,
    pub invoice_no: Option<String>,
    pub amount: Option<String>,
    pub currency: Option<String>,
    pub file_path: Option<PathBuf>,
    pub file_hash: Option<String>,
    pub source: Source,
    pub mail_account: Option<String>,
    pub mail_id: Option<String>,
    pub status: Status,
    pub disposed_at: Option<String>,
    /// Set on a `portal_fetch_required` row once its invoice is stored and its
    /// mail cleaned up.
    pub resolved_at: Option<String>,
    /// On a resolved `portal_fetch_required` row: the invoice that settled it.
    pub settled_by: Option<i64>,
    /// On a resolved `portal_fetch_required` row: when a person marked it as done
    /// without Docsweep storing an invoice or touching the mail.
    pub dismissed_at: Option<String>,
    /// On a resolved `portal_fetch_required` row: when Docsweep stopped waiting
    /// for its invoice and flagged the mail instead.
    pub given_up_at: Option<String>,
}

impl InvoiceRecord {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        let date: Option<String> = row.get("invoice_date")?;
        let path: Option<String> = row.get("file_path")?;
        Ok(Self {
            id: row.get("id")?,
            company_id: row.get("company_id")?,
            invoice_date: date.and_then(|d| d.parse().ok()),
            invoice_no: row.get("invoice_no")?,
            amount: row.get("amount")?,
            currency: row.get("currency")?,
            file_path: path.map(PathBuf::from),
            file_hash: row.get("file_hash")?,
            source: Source::parse(&row.get::<_, String>("source")?)?,
            mail_account: row.get("mail_account")?,
            mail_id: row.get("mail_id")?,
            status: Status::parse(&row.get::<_, String>("status")?)?,
            disposed_at: row.get("disposed_at")?,
            // A dry run can read an older index, which lacks this column.
            resolved_at: row.get("resolved_at").unwrap_or(None),
            settled_by: row.get("settled_by").unwrap_or(None),
            dismissed_at: row.get("dismissed_at").unwrap_or(None),
            given_up_at: row.get("given_up_at").unwrap_or(None),
        })
    }
}

/// Result of the duplicate check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DuplicateCheck {
    /// Neither hash nor number are known.
    New,
    /// The identical file is already known.
    SameFile(InvoiceRecord),
    /// The company already has this number, but with a different file.
    SameNumberDifferentFile(InvoiceRecord),
}

/// A finished real scan in the history. Summary and report are JSON, stored as
/// shown to the user at the time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScanRun {
    pub id: i64,
    pub finished_at: String,
    pub summary: String,
    pub report: String,
    /// Started by the schedule or by polling, not by a person.
    pub automatic: bool,
}

/// Why the index could not be used.
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("cannot create the index directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("index error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error(
        "the index has schema version {found}, but this Docsweep only knows up to {known}; please update Docsweep"
    )]
    TooNew { found: usize, known: usize },
}

/// Connection to the index.
#[derive(Debug)]
pub struct Index {
    conn: Connection,
}

impl Index {
    /// Opens the index for writing, creating and migrating it if needed.
    pub fn open(path: &Path) -> Result<Self, IndexError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| IndexError::CreateDir {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        Self::prepared(Connection::open(path)?)
    }

    /// Opens an empty temporary index, for tests and for a first dry run.
    pub fn open_in_memory() -> Result<Self, IndexError> {
        Self::prepared(Connection::open_in_memory()?)
    }

    /// Opens an existing index without any possibility of changing it, or
    /// returns `None` if it does not exist yet. Used by dry runs.
    pub fn open_read_only(path: &Path) -> Result<Option<Self>, IndexError> {
        if !path.exists() {
            return Ok(None);
        }
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        check_version(schema_version(&conn)?)?;
        Ok(Some(Self { conn }))
    }

    fn prepared(mut conn: Connection) -> Result<Self, IndexError> {
        let current = schema_version(&conn)?;
        check_version(current)?;
        for (position, migration) in MIGRATIONS.iter().enumerate().skip(current) {
            let transaction = conn.transaction()?;
            transaction.execute_batch(migration)?;
            transaction.pragma_update(None, "user_version", (position + 1) as i64)?;
            transaction.commit()?;
        }
        Ok(Self { conn })
    }

    /// Checks whether a document is a duplicate. An identical hash wins over a
    /// matching number, and hashes are compared across all companies.
    pub fn check_duplicate(
        &self,
        company_id: &str,
        invoice_no: Option<&str>,
        file_hash: &str,
    ) -> Result<DuplicateCheck, IndexError> {
        let same_file = self
            .conn
            .query_row(
                "SELECT * FROM invoices WHERE file_hash = ?1",
                [file_hash],
                InvoiceRecord::from_row,
            )
            .optional()?;
        if let Some(record) = same_file {
            return Ok(DuplicateCheck::SameFile(record));
        }

        let Some(invoice_no) = invoice_no else {
            return Ok(DuplicateCheck::New);
        };
        let same_number = self
            .conn
            .query_row(
                "SELECT * FROM invoices
                 WHERE company_id = ?1 AND invoice_no = ?2 AND file_hash IS NOT NULL
                 ORDER BY id LIMIT 1",
                params![company_id, invoice_no],
                InvoiceRecord::from_row,
            )
            .optional()?;
        Ok(same_number.map_or(DuplicateCheck::New, DuplicateCheck::SameNumberDifferentFile))
    }

    /// Adds a stored document and returns its row id.
    pub fn insert(&self, invoice: &NewInvoice) -> Result<i64, IndexError> {
        self.conn.execute(
            "INSERT INTO invoices (company_id, invoice_date, invoice_no, amount, currency,
                                   file_path, file_hash, source, mail_account, mail_id,
                                   portal_key, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                invoice.company_id,
                invoice.invoice_date.map(|d| d.to_string()),
                invoice.invoice_no,
                invoice.amount,
                invoice.currency,
                invoice.file_path.to_string_lossy(),
                invoice.file_hash,
                invoice.source.as_str(),
                invoice.mail_account,
                invoice.mail_id,
                invoice.portal_key,
                invoice.status.as_str(),
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Notes that a link mail requires a portal fetch. Repeating the same mail
    /// does not add another row; returns whether a new row was added.
    pub fn record_portal_fetch_required(
        &self,
        company_id: &str,
        mail_account: &str,
        mail_id: &str,
        mail_date: Option<NaiveDate>,
    ) -> Result<bool, IndexError> {
        let added = self.conn.execute(
            "INSERT OR IGNORE INTO invoices (company_id, invoice_date, source, mail_account,
                                             mail_id, status)
             VALUES (?1, ?2, 'portal', ?3, ?4, 'portal_fetch_required')",
            params![
                company_id,
                mail_date.map(|d| d.to_string()),
                mail_account,
                mail_id
            ],
        )?;
        Ok(added > 0)
    }

    /// Whether a link mail is already noted as requiring a portal fetch. A
    /// resolved row counts, so a mail that comes back is not noted twice.
    pub fn is_portal_fetch_recorded(
        &self,
        company_id: &str,
        mail_account: &str,
        mail_id: &str,
    ) -> Result<bool, IndexError> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM invoices
                 WHERE company_id = ?1 AND mail_account = ?2 AND mail_id = ?3
                   AND status = 'portal_fetch_required'",
                params![company_id, mail_account, mail_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// Whether an entry of a portal's list is already known: it was fetched from
    /// the portal before, or its key is the number of a stored invoice.
    pub fn is_portal_entry_known(&self, company_id: &str, key: &str) -> Result<bool, IndexError> {
        let query = if schema_version(&self.conn)? >= PORTAL_KEY_VERSION {
            "SELECT 1 FROM invoices
             WHERE company_id = ?1 AND (portal_key = ?2 OR invoice_no = ?2) LIMIT 1"
        } else {
            "SELECT 1 FROM invoices WHERE company_id = ?1 AND invoice_no = ?2 LIMIT 1"
        };
        Ok(self
            .conn
            .query_row(query, params![company_id, key], |_| Ok(()))
            .optional()?
            .is_some())
    }

    /// Notes a portal key for a row that was first stored another way, so its
    /// list entry is not downloaded again. A row keeps the key it has.
    pub fn note_portal_key(&self, id: i64, key: &str) -> Result<(), IndexError> {
        self.conn.execute(
            "UPDATE OR IGNORE invoices SET portal_key = ?2 WHERE id = ?1 AND portal_key IS NULL",
            params![id, key],
        )?;
        Ok(())
    }

    /// Records that the mail of these rows was cleaned up.
    pub fn mark_mail_disposed(&self, mail_account: &str, mail_id: &str) -> Result<(), IndexError> {
        self.conn.execute(
            "UPDATE invoices SET disposed_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
             WHERE mail_account = ?1 AND mail_id = ?2 AND disposed_at IS NULL",
            params![mail_account, mail_id],
        )?;
        Ok(())
    }

    /// The stored portal invoice that settles a noted link mail that still
    /// waits, if there is one. Each invoice settles at most one link mail; see
    /// [`Self::link_assignment`].
    pub fn invoice_for_link_mail(
        &self,
        company_id: &str,
        mail_account: &str,
        mail_id: &str,
    ) -> Result<Option<i64>, IndexError> {
        let Some(link) = self.waiting_link_row(company_id, mail_account, mail_id)? else {
            return Ok(None);
        };
        Ok(self.link_assignment(company_id, false)?.remove(&link))
    }

    /// Whether [`Self::invoice_for_link_mail`] finds an invoice.
    pub fn has_invoice_for_link_mail(
        &self,
        company_id: &str,
        mail_account: &str,
        mail_id: &str,
    ) -> Result<bool, IndexError> {
        Ok(self
            .invoice_for_link_mail(company_id, mail_account, mail_id)?
            .is_some())
    }

    /// Like [`Self::has_invoice_for_link_mail`], but a document waiting for review
    /// counts too: it is here, even if its mail has to wait.
    pub fn link_mail_has_document(
        &self,
        company_id: &str,
        mail_account: &str,
        mail_id: &str,
    ) -> Result<bool, IndexError> {
        let Some(link) = self.waiting_link_row(company_id, mail_account, mail_id)? else {
            return Ok(false);
        };
        Ok(self.link_assignment(company_id, true)?.contains_key(&link))
    }

    /// The id of a link mail's row while it still waits for its invoice.
    fn waiting_link_row(
        &self,
        company_id: &str,
        mail_account: &str,
        mail_id: &str,
    ) -> Result<Option<i64>, IndexError> {
        let query = format!(
            "SELECT id FROM invoices
             WHERE company_id = ?1 AND mail_account = ?2 AND mail_id = ?3
               AND status = 'portal_fetch_required' {}",
            self.waiting_condition()?
        );
        Ok(self
            .conn
            .query_row(&query, params![company_id, mail_account, mail_id], |row| {
                row.get(0)
            })
            .optional()?)
    }

    /// The SQL condition for a link row that still waits, starting with `AND`.
    /// An index from before `resolved_at` only has waiting rows.
    fn waiting_condition(&self) -> Result<&'static str, IndexError> {
        Ok(if schema_version(&self.conn)? >= AUTOMATION_VERSION {
            "AND resolved_at IS NULL"
        } else {
            ""
        })
    }

    /// Pairs the waiting link mails of a company with its portal invoices, as
    /// link row id to invoice id. Each invoice settles at most one link mail, and
    /// one that already settled a mail (`settled_by`) is no longer available.
    ///
    /// A pair is possible if the invoice's row is newer than the link's, or its
    /// date is on or after the mail's; the invoice came from the portal, is
    /// stored and its file still exists. Among the possible pairs the closest
    /// dates go first, ties by the older link and then the older invoice. The
    /// whole company is paired at once, so two mails never claim one invoice.
    /// `with_review` also counts documents waiting for review.
    pub fn link_assignment(
        &self,
        company_id: &str,
        with_review: bool,
    ) -> Result<HashMap<i64, i64>, IndexError> {
        let query = format!(
            "SELECT id, invoice_date FROM invoices
             WHERE company_id = ?1 AND status = 'portal_fetch_required' {}",
            self.waiting_condition()?
        );
        let links = self.ids_and_dates(&query, company_id, false)?;
        if links.is_empty() {
            return Ok(HashMap::new());
        }

        let statuses = if with_review {
            "'stored', 'needs_review'"
        } else {
            "'stored'"
        };
        let available = if schema_version(&self.conn)? >= SETTLED_BY_VERSION {
            "AND id NOT IN (SELECT settled_by FROM invoices WHERE settled_by IS NOT NULL)"
        } else {
            ""
        };
        let query = format!(
            "SELECT id, invoice_date, file_path FROM invoices
             WHERE company_id = ?1 AND source = 'portal' AND status IN ({statuses})
               AND file_path IS NOT NULL {available}"
        );
        let invoices = self.ids_and_dates(&query, company_id, true)?;

        let mut pairs = Vec::new();
        for &(link, mail_date) in &links {
            for &(invoice, invoice_date) in &invoices {
                let dates = mail_date.zip(invoice_date);
                if invoice > link || dates.is_some_and(|(mail, document)| document >= mail) {
                    let distance = dates.map_or(i64::MAX, |(mail, document)| {
                        (document - mail).num_days().abs()
                    });
                    pairs.push((distance, link, invoice));
                }
            }
        }
        pairs.sort_unstable();

        let mut assignment = HashMap::new();
        let mut taken = HashSet::new();
        for (_, link, invoice) in pairs {
            if !assignment.contains_key(&link) && taken.insert(invoice) {
                assignment.insert(link, invoice);
            }
        }
        Ok(assignment)
    }

    /// Rows as id and date. With `existing_file`, the query's third column is a
    /// path, and rows whose file is gone are left out.
    fn ids_and_dates(
        &self,
        query: &str,
        company_id: &str,
        existing_file: bool,
    ) -> Result<Vec<(i64, Option<NaiveDate>)>, IndexError> {
        let mut statement = self.conn.prepare(query)?;
        let rows = statement.query_map([company_id], |row| {
            let id: i64 = row.get(0)?;
            let date: Option<String> = row.get(1)?;
            let present = !existing_file
                || row
                    .get::<_, Option<String>>(2)?
                    .is_some_and(|path| Path::new(&path).exists());
            Ok(present.then(|| (id, date.and_then(|d| d.parse().ok()))))
        })?;
        let mut found = Vec::new();
        for row in rows {
            found.extend(row?);
        }
        Ok(found)
    }

    /// The noted link mails that are not done yet, newest first. An index from
    /// before `resolved_at` has no pick-ups to show, since a real run migrates it
    /// before noting any for an assisted company.
    pub fn waiting_link_mails(&self) -> Result<Vec<InvoiceRecord>, IndexError> {
        if schema_version(&self.conn)? < AUTOMATION_VERSION {
            return Ok(Vec::new());
        }
        let mut statement = self.conn.prepare(
            "SELECT * FROM invoices
             WHERE status = 'portal_fetch_required' AND resolved_at IS NULL
             ORDER BY id DESC",
        )?;
        let rows = statement.query_map([], InvoiceRecord::from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Whether and how a noted link mail stopped waiting. `None` for a mail that is
    /// not noted or still waits.
    pub fn link_mail_closure(
        &self,
        company_id: &str,
        mail_account: &str,
        mail_id: &str,
    ) -> Result<Option<LinkClosure>, IndexError> {
        let version = schema_version(&self.conn)?;
        if version < AUTOMATION_VERSION {
            return Ok(None);
        }
        let column = |name, since| if version >= since { name } else { "NULL" };
        let query = format!(
            "SELECT resolved_at, {}, {} FROM invoices
             WHERE company_id = ?1 AND mail_account = ?2 AND mail_id = ?3
               AND status = 'portal_fetch_required'",
            column("dismissed_at", DISMISSED_VERSION),
            column("given_up_at", GIVEN_UP_VERSION),
        );
        let found = self
            .conn
            .query_row(&query, params![company_id, mail_account, mail_id], |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?.is_some(),
                    row.get::<_, Option<String>>(1)?.is_some(),
                    row.get::<_, Option<String>>(2)?.is_some(),
                ))
            })
            .optional()?;
        Ok(match found {
            Some((true, true, _)) => Some(LinkClosure::Dismissed),
            Some((true, _, true)) => Some(LinkClosure::GivenUp),
            Some((true, false, false)) => Some(LinkClosure::Settled),
            _ => None,
        })
    }

    /// Since when a noted link mail waits: the mail's date, or without one, the
    /// day it was noted. `None` if it is not noted or no longer waits.
    pub fn link_mail_waiting_since(
        &self,
        company_id: &str,
        mail_account: &str,
        mail_id: &str,
    ) -> Result<Option<NaiveDate>, IndexError> {
        let query = format!(
            "SELECT COALESCE(invoice_date, substr(created_at, 1, 10)) FROM invoices
             WHERE company_id = ?1 AND mail_account = ?2 AND mail_id = ?3
               AND status = 'portal_fetch_required' {}",
            self.waiting_condition()?
        );
        let date: Option<Option<String>> = self
            .conn
            .query_row(&query, params![company_id, mail_account, mail_id], |row| {
                row.get(0)
            })
            .optional()?;
        Ok(date.flatten().and_then(|date| date.parse().ok()))
    }

    /// Records that Docsweep stopped waiting for a link mail's invoice. Nothing
    /// claims an invoice for it, and `disposed_at` stays empty, since the mail
    /// stays where it is. Returns whether a waiting row was found.
    pub fn give_up_link_mail(
        &self,
        company_id: &str,
        mail_account: &str,
        mail_id: &str,
    ) -> Result<bool, IndexError> {
        let changed = self.conn.execute(
            "UPDATE invoices
             SET resolved_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
                 given_up_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
             WHERE company_id = ?1 AND mail_account = ?2 AND mail_id = ?3
               AND status = 'portal_fetch_required' AND resolved_at IS NULL",
            params![company_id, mail_account, mail_id],
        )?;
        Ok(changed > 0)
    }

    /// Records that a person marked a waiting link mail as done, for instance
    /// because they deleted it by hand. Nothing claims an invoice for it, and
    /// `disposed_at` stays empty, since Docsweep did not touch the mail. Returns
    /// whether a waiting row was found.
    pub fn dismiss_link_mail(&self, id: i64) -> Result<bool, IndexError> {
        let changed = self.conn.execute(
            "UPDATE invoices
             SET resolved_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
                 dismissed_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
             WHERE id = ?1 AND status = 'portal_fetch_required' AND resolved_at IS NULL",
            [id],
        )?;
        Ok(changed > 0)
    }

    /// Records that a link mail's invoice is stored and the mail was cleaned up,
    /// and which invoice settled it, so that this invoice settles no other mail.
    pub fn resolve_link_mail(
        &self,
        company_id: &str,
        mail_account: &str,
        mail_id: &str,
        invoice_id: i64,
    ) -> Result<(), IndexError> {
        self.conn.execute(
            "UPDATE invoices
             SET resolved_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
                 disposed_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
                 settled_by = ?4
             WHERE company_id = ?1 AND mail_account = ?2 AND mail_id = ?3
               AND status = 'portal_fetch_required'",
            params![company_id, mail_account, mail_id, invoice_id],
        )?;
        Ok(())
    }

    /// Whether a person agreed to send this company's pages to this provider. Only
    /// real runs ask, and those always have a migrated index.
    pub fn has_ai_consent(&self, company_id: &str, provider: &str) -> Result<bool, IndexError> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM ai_consents WHERE company_id = ?1 AND provider = ?2",
                params![company_id, provider],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// Notes that a person saw what would be sent, and agreed.
    pub fn note_ai_consent(&self, company_id: &str, provider: &str) -> Result<(), IndexError> {
        self.conn.execute(
            "INSERT OR IGNORE INTO ai_consents (company_id, provider) VALUES (?1, ?2)",
            params![company_id, provider],
        )?;
        Ok(())
    }

    /// The local date of the last fetch started because of link mails.
    pub fn last_link_fetch_on(&self, company_id: &str) -> Result<Option<NaiveDate>, IndexError> {
        let value = self.company_run_value("last_link_fetch_on", company_id)?;
        Ok(value.and_then(|date| date.parse().ok()))
    }

    /// Notes that link mails started a fetch on this local date.
    pub fn note_link_fetch(&self, company_id: &str, on: NaiveDate) -> Result<(), IndexError> {
        self.conn.execute(
            "INSERT INTO company_runs (company_id, last_link_fetch_on) VALUES (?1, ?2)
             ON CONFLICT (company_id) DO UPDATE SET last_link_fetch_on = ?2",
            params![company_id, on.to_string()],
        )?;
        Ok(())
    }

    /// When the schedule of a company last started a run.
    pub fn last_scheduled_run(
        &self,
        company_id: &str,
    ) -> Result<Option<DateTime<Utc>>, IndexError> {
        let value = self.company_run_value("last_scheduled_run", company_id)?;
        Ok(value.and_then(|time| {
            DateTime::parse_from_rfc3339(&time)
                .ok()
                .map(|time| time.to_utc())
        }))
    }

    /// Notes when the schedule of a company started a run.
    pub fn note_scheduled_run(
        &self,
        company_id: &str,
        at: DateTime<Utc>,
    ) -> Result<(), IndexError> {
        self.conn.execute(
            "INSERT INTO company_runs (company_id, last_scheduled_run) VALUES (?1, ?2)
             ON CONFLICT (company_id) DO UPDATE SET last_scheduled_run = ?2",
            params![company_id, at.to_rfc3339_opts(SecondsFormat::Secs, true)],
        )?;
        Ok(())
    }

    /// `column` is one of the fixed names above, never input.
    fn company_run_value(
        &self,
        column: &str,
        company_id: &str,
    ) -> Result<Option<String>, IndexError> {
        if schema_version(&self.conn)? < AUTOMATION_VERSION {
            return Ok(None);
        }
        let query = format!("SELECT {column} FROM company_runs WHERE company_id = ?1");
        let value = self
            .conn
            .query_row(&query, [company_id], |row| row.get(0))
            .optional()?;
        Ok(value.flatten())
    }

    /// The newest rows of all companies.
    pub fn recent_invoices(&self, limit: u32) -> Result<Vec<InvoiceRecord>, IndexError> {
        let mut statement = self
            .conn
            .prepare("SELECT * FROM invoices ORDER BY id DESC LIMIT ?1")?;
        let rows = statement.query_map([limit], InvoiceRecord::from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// One row by id.
    pub fn invoice(&self, id: i64) -> Result<Option<InvoiceRecord>, IndexError> {
        Ok(self
            .conn
            .query_row(
                "SELECT * FROM invoices WHERE id = ?1",
                [id],
                InvoiceRecord::from_row,
            )
            .optional()?)
    }

    /// Completes a reviewed document with its number, date and final path.
    pub fn complete_review(
        &self,
        id: i64,
        invoice_no: &str,
        invoice_date: NaiveDate,
        file_path: &Path,
    ) -> Result<(), IndexError> {
        self.conn.execute(
            "UPDATE invoices SET invoice_no = ?2, invoice_date = ?3, file_path = ?4,
                                 status = 'stored'
             WHERE id = ?1 AND status = 'needs_review'",
            params![
                id,
                invoice_no,
                invoice_date.to_string(),
                file_path.to_string_lossy()
            ],
        )?;
        Ok(())
    }

    /// Adds a finished real run to the history.
    pub fn record_run(
        &self,
        summary: &str,
        report: &str,
        automatic: bool,
    ) -> Result<(), IndexError> {
        self.conn.execute(
            "INSERT INTO scan_runs (summary, report, automatic) VALUES (?1, ?2, ?3)",
            params![summary, report, automatic],
        )?;
        Ok(())
    }

    /// The newest scans in the history.
    pub fn recent_runs(&self, limit: u32) -> Result<Vec<ScanRun>, IndexError> {
        let automatic = if schema_version(&self.conn)? >= AUTOMATION_VERSION {
            "automatic"
        } else {
            "0"
        };
        let mut statement = self.conn.prepare(&format!(
            "SELECT id, finished_at, summary, report, {automatic} FROM scan_runs
             ORDER BY id DESC LIMIT ?1"
        ))?;
        let rows = statement.query_map([limit], |row| {
            Ok(ScanRun {
                id: row.get(0)?,
                finished_at: row.get(1)?,
                summary: row.get(2)?,
                report: row.get(3)?,
                automatic: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// All rows of a company, oldest first.
    pub fn invoices_of(&self, company_id: &str) -> Result<Vec<InvoiceRecord>, IndexError> {
        let mut statement = self
            .conn
            .prepare("SELECT * FROM invoices WHERE company_id = ?1 ORDER BY id")?;
        let rows = statement.query_map([company_id], InvoiceRecord::from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

#[cfg(test)]
impl Index {
    /// Makes every later write fail, to test how callers react.
    pub(crate) fn break_for_test(&self) {
        self.conn.execute_batch("DROP TABLE invoices").unwrap();
    }

    /// Makes every later insert fail, while reads go on working.
    pub(crate) fn refuse_inserts_for_test(&self) {
        self.conn
            .execute_batch(
                "CREATE TEMP TRIGGER refuse BEFORE INSERT ON main.invoices
                 BEGIN SELECT RAISE(ABORT, 'refused'); END",
            )
            .unwrap();
    }
}

fn schema_version(conn: &Connection) -> rusqlite::Result<usize> {
    conn.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
        .map(|version| version as usize)
}

fn check_version(found: usize) -> Result<(), IndexError> {
    if found > MIGRATIONS.len() {
        return Err(IndexError::TooNew {
            found,
            known: MIGRATIONS.len(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invoice(number: Option<&str>, hash: &str) -> NewInvoice {
        NewInvoice {
            company_id: "acme".into(),
            invoice_date: NaiveDate::from_ymd_opt(2026, 11, 3),
            invoice_no: number.map(Into::into),
            amount: Some("119.00".into()),
            currency: Some("EUR".into()),
            file_path: PathBuf::from("/invoices/a.pdf"),
            file_hash: hash.into(),
            source: Source::Mail,
            mail_account: Some("main".into()),
            mail_id: Some("<1@example.test>".into()),
            portal_key: None,
            status: Status::Stored,
        }
    }

    #[test]
    fn stores_all_fields() {
        let index = Index::open_in_memory().unwrap();
        index.insert(&invoice(Some("R-1"), "h1")).unwrap();

        let row = index.invoices_of("acme").unwrap().remove(0);
        assert_eq!(row.invoice_no.as_deref(), Some("R-1"));
        assert_eq!(row.invoice_date, NaiveDate::from_ymd_opt(2026, 11, 3));
        assert_eq!(row.amount.as_deref(), Some("119.00"));
        assert_eq!(row.currency.as_deref(), Some("EUR"));
        assert_eq!(row.file_hash.as_deref(), Some("h1"));
        assert_eq!(row.source, Source::Mail);
        assert_eq!(row.mail_id.as_deref(), Some("<1@example.test>"));
        assert_eq!(row.status, Status::Stored);
        assert_eq!(row.disposed_at, None);
    }

    #[test]
    fn amount_is_optional() {
        let index = Index::open_in_memory().unwrap();
        let mut new = invoice(Some("R-1"), "h1");
        new.amount = None;
        new.currency = None;
        index.insert(&new).unwrap();
        assert_eq!(index.invoices_of("acme").unwrap()[0].amount, None);
    }

    #[test]
    fn unknown_document_is_new() {
        let index = Index::open_in_memory().unwrap();
        index.insert(&invoice(Some("R-1"), "h1")).unwrap();
        assert_eq!(
            index.check_duplicate("acme", Some("R-2"), "h2").unwrap(),
            DuplicateCheck::New
        );
    }

    #[test]
    fn same_hash_is_a_duplicate_even_with_another_number_or_company() {
        let index = Index::open_in_memory().unwrap();
        index.insert(&invoice(Some("R-1"), "h1")).unwrap();
        for (company, number) in [("acme", Some("R-1")), ("acme", None), ("other", Some("X"))] {
            assert!(matches!(
                index.check_duplicate(company, number, "h1").unwrap(),
                DuplicateCheck::SameFile(_)
            ));
        }
    }

    #[test]
    fn same_number_with_another_hash_is_a_conflict() {
        let index = Index::open_in_memory().unwrap();
        index.insert(&invoice(Some("R-1"), "h1")).unwrap();
        assert!(matches!(
            index.check_duplicate("acme", Some("R-1"), "h2").unwrap(),
            DuplicateCheck::SameNumberDifferentFile(_)
        ));
    }

    #[test]
    fn same_number_at_another_company_is_new() {
        let index = Index::open_in_memory().unwrap();
        index.insert(&invoice(Some("R-1"), "h1")).unwrap();
        assert_eq!(
            index.check_duplicate("other", Some("R-1"), "h2").unwrap(),
            DuplicateCheck::New
        );
    }

    #[test]
    fn documents_without_number_only_compare_by_hash() {
        let index = Index::open_in_memory().unwrap();
        index.insert(&invoice(None, "h1")).unwrap();
        assert_eq!(
            index.check_duplicate("acme", None, "h2").unwrap(),
            DuplicateCheck::New
        );
    }

    #[test]
    fn the_same_hash_cannot_be_inserted_twice() {
        let index = Index::open_in_memory().unwrap();
        index.insert(&invoice(Some("R-1"), "h1")).unwrap();
        assert!(index.insert(&invoice(Some("R-2"), "h1")).is_err());
    }

    #[test]
    fn portal_fetch_is_recorded_once_per_mail() {
        let index = Index::open_in_memory().unwrap();
        let date = NaiveDate::from_ymd_opt(2026, 11, 3);
        assert!(
            !index
                .is_portal_fetch_recorded("acme", "main", "<1>")
                .unwrap()
        );
        assert!(
            index
                .record_portal_fetch_required("acme", "main", "<1>", date)
                .unwrap()
        );
        assert!(
            !index
                .record_portal_fetch_required("acme", "main", "<1>", date)
                .unwrap()
        );
        assert!(
            index
                .is_portal_fetch_recorded("acme", "main", "<1>")
                .unwrap()
        );

        let rows = index.invoices_of("acme").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, Status::PortalFetchRequired);
        assert_eq!(rows[0].source, Source::Portal);
        assert_eq!(rows[0].file_hash, None);
    }

    #[test]
    fn disposal_is_recorded_per_mail() {
        let index = Index::open_in_memory().unwrap();
        index.insert(&invoice(Some("R-1"), "h1")).unwrap();
        index
            .mark_mail_disposed("main", "<1@example.test>")
            .unwrap();
        assert!(index.invoices_of("acme").unwrap()[0].disposed_at.is_some());
    }

    #[test]
    fn index_persists_and_read_only_cannot_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("docsweep.db");
        assert!(Index::open_read_only(&path).unwrap().is_none());
        assert!(!path.exists(), "a dry run must not create the index");

        Index::open(&path)
            .unwrap()
            .insert(&invoice(Some("R-1"), "h1"))
            .unwrap();
        let reopened = Index::open(&path).unwrap();
        assert_eq!(reopened.invoices_of("acme").unwrap().len(), 1);
        drop(reopened);

        let read_only = Index::open_read_only(&path).unwrap().unwrap();
        assert_eq!(read_only.invoices_of("acme").unwrap().len(), 1);
        assert!(read_only.insert(&invoice(Some("R-2"), "h2")).is_err());
    }

    #[test]
    fn recent_invoices_are_newest_first_and_limited() {
        let index = Index::open_in_memory().unwrap();
        index.insert(&invoice(Some("R-1"), "h1")).unwrap();
        let newest = index.insert(&invoice(Some("R-2"), "h2")).unwrap();

        let rows = index.recent_invoices(1).unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, newest);
        assert_eq!(index.invoice(newest).unwrap().unwrap().id, newest);
        assert_eq!(index.invoice(999).unwrap(), None);
    }

    #[test]
    fn completing_a_review_only_affects_review_rows() {
        let index = Index::open_in_memory().unwrap();
        let mut new = invoice(None, "h1");
        new.status = Status::NeedsReview;
        let in_review = index.insert(&new).unwrap();
        let stored = index.insert(&invoice(Some("R-9"), "h2")).unwrap();
        let date = NaiveDate::from_ymd_opt(2026, 12, 1).unwrap();

        index
            .complete_review(in_review, "R-1", date, Path::new("/new.pdf"))
            .unwrap();
        index
            .complete_review(stored, "CHANGED", date, Path::new("/x.pdf"))
            .unwrap();

        let row = index.invoice(in_review).unwrap().unwrap();
        assert_eq!(row.status, Status::Stored);
        assert_eq!(row.invoice_no.as_deref(), Some("R-1"));
        assert_eq!(row.invoice_date, Some(date));
        assert_eq!(row.file_path, Some(PathBuf::from("/new.pdf")));
        let untouched = index.invoice(stored).unwrap().unwrap();
        assert_eq!(untouched.invoice_no.as_deref(), Some("R-9"));
    }

    #[test]
    fn run_history_is_newest_first() {
        let index = Index::open_in_memory().unwrap();
        index.record_run("{\"saved\":1}", "[]", false).unwrap();
        index.record_run("{\"saved\":2}", "[]", true).unwrap();
        let runs = index.recent_runs(10).unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].summary, "{\"saved\":2}");
        assert!(runs[0].automatic);
        assert!(!runs[1].automatic);
    }

    #[test]
    fn an_existing_first_version_index_is_migrated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("docsweep.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(MIGRATIONS[0]).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        drop(conn);

        let index = Index::open(&path).unwrap();
        index.record_run("{}", "[]", false).unwrap();
        assert_eq!(index.recent_runs(1).unwrap().len(), 1);
    }

    #[test]
    fn portal_entries_are_known_by_key_or_number() {
        let index = Index::open_in_memory().unwrap();
        let mut new = invoice(Some("R-1"), "h1");
        new.portal_key = Some("K-1".into());
        let id = index.insert(&new).unwrap();
        assert!(index.is_portal_entry_known("acme", "K-1").unwrap());
        assert!(index.is_portal_entry_known("acme", "R-1").unwrap());
        assert!(!index.is_portal_entry_known("acme", "K-2").unwrap());
        assert!(!index.is_portal_entry_known("other", "K-1").unwrap());

        index.note_portal_key(id, "K-2").unwrap();
        assert!(
            !index.is_portal_entry_known("acme", "K-2").unwrap(),
            "a row keeps its key"
        );
        let mail = index.insert(&invoice(Some("R-2"), "h2")).unwrap();
        index.note_portal_key(mail, "K-2").unwrap();
        assert!(index.is_portal_entry_known("acme", "K-2").unwrap());
    }

    #[test]
    fn a_taken_portal_key_does_not_fail_the_note() {
        let index = Index::open_in_memory().unwrap();
        let mut new = invoice(Some("R-1"), "h1");
        new.portal_key = Some("K-1".into());
        index.insert(&new).unwrap();
        let other = index.insert(&invoice(Some("R-2"), "h2")).unwrap();
        index.note_portal_key(other, "K-1").unwrap();
    }

    #[test]
    fn a_dry_run_can_read_an_index_from_before_portal_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("docsweep.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(MIGRATIONS[0]).unwrap();
        conn.execute_batch(MIGRATIONS[1]).unwrap();
        conn.pragma_update(None, "user_version", 2).unwrap();
        conn.execute(
            "INSERT INTO invoices (company_id, invoice_no, source, status)
             VALUES ('acme', 'R-1', 'mail', 'stored')",
            [],
        )
        .unwrap();
        drop(conn);

        let index = Index::open_read_only(&path).unwrap().unwrap();
        assert!(index.is_portal_entry_known("acme", "R-1").unwrap());
        assert!(!index.is_portal_entry_known("acme", "R-2").unwrap());
    }

    /// An index as the previous version left it, with one row and one run.
    fn index_before_automation(path: &Path) {
        let conn = Connection::open(path).unwrap();
        for migration in &MIGRATIONS[..AUTOMATION_VERSION - 1] {
            conn.execute_batch(migration).unwrap();
        }
        conn.pragma_update(None, "user_version", (AUTOMATION_VERSION - 1) as i64)
            .unwrap();
        conn.execute_batch(
            "INSERT INTO invoices (company_id, invoice_no, source, status)
             VALUES ('acme', 'R-1', 'mail', 'stored');
             INSERT INTO scan_runs (summary, report) VALUES ('{}', '[]');",
        )
        .unwrap();
    }

    #[test]
    fn a_dry_run_can_read_an_index_from_before_automation_without_migrating() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("docsweep.db");
        index_before_automation(&path);

        let index = Index::open_read_only(&path).unwrap().unwrap();

        assert_eq!(index.invoices_of("acme").unwrap()[0].resolved_at, None);
        assert_eq!(index.last_link_fetch_on("acme").unwrap(), None);
        assert_eq!(index.last_scheduled_run("acme").unwrap(), None);
        assert!(!index.recent_runs(1).unwrap()[0].automatic);
        assert_eq!(
            schema_version(&index.conn).unwrap(),
            AUTOMATION_VERSION - 1,
            "a dry run never migrates"
        );
    }

    #[test]
    fn an_index_from_before_automation_is_migrated_by_a_real_run() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("docsweep.db");
        index_before_automation(&path);

        let index = Index::open(&path).unwrap();

        assert_eq!(index.invoices_of("acme").unwrap().len(), 1);
        assert!(!index.recent_runs(1).unwrap()[0].automatic);
        index
            .note_link_fetch("acme", NaiveDate::from_ymd_opt(2026, 11, 6).unwrap())
            .unwrap();
    }

    /// A noted link mail from 3 November, and a stored file on disk.
    fn with_link_mail(dir: &Path) -> (Index, PathBuf) {
        let index = Index::open_in_memory().unwrap();
        index
            .record_portal_fetch_required(
                "acme",
                "main",
                "<link>",
                NaiveDate::from_ymd_opt(2026, 11, 3),
            )
            .unwrap();
        let file = dir.join("a.pdf");
        fs::write(&file, b"x").unwrap();
        (index, file)
    }

    fn portal_invoice(file: &Path, date: Option<NaiveDate>, status: Status) -> NewInvoice {
        NewInvoice {
            invoice_date: date,
            file_path: file.to_path_buf(),
            source: Source::Portal,
            mail_account: None,
            mail_id: None,
            portal_key: Some("K-1".into()),
            status,
            ..invoice(Some("R-1"), "h1")
        }
    }

    fn matches_link_mail(index: &Index) -> bool {
        index
            .has_invoice_for_link_mail("acme", "main", "<link>")
            .unwrap()
    }

    #[test]
    fn a_link_mail_matches_a_portal_invoice_stored_after_it() {
        let dir = tempfile::tempdir().unwrap();
        let (index, file) = with_link_mail(dir.path());
        assert!(!matches_link_mail(&index), "the mail's own row is no match");

        let older = NaiveDate::from_ymd_opt(2026, 10, 1);
        index
            .insert(&portal_invoice(&file, older, Status::Stored))
            .unwrap();

        assert!(matches_link_mail(&index));
    }

    #[test]
    fn a_link_mail_matches_an_earlier_portal_invoice_dated_on_or_after_it() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.pdf");
        fs::write(&file, b"x").unwrap();
        let date = NaiveDate::from_ymd_opt(2026, 11, 3).unwrap();
        for (invoice_date, expected) in [(date, true), (date.pred_opt().unwrap(), false)] {
            let index = Index::open_in_memory().unwrap();
            index
                .insert(&portal_invoice(&file, Some(invoice_date), Status::Stored))
                .unwrap();
            index
                .record_portal_fetch_required("acme", "main", "<link>", Some(date))
                .unwrap();
            assert_eq!(matches_link_mail(&index), expected, "{invoice_date}");
        }
    }

    #[test]
    fn a_link_mail_does_not_match_what_is_not_a_stored_portal_invoice() {
        let dir = tempfile::tempdir().unwrap();
        let date = NaiveDate::from_ymd_opt(2026, 11, 4);

        let (index, file) = with_link_mail(dir.path());
        index
            .insert(&portal_invoice(&file, date, Status::NeedsReview))
            .unwrap();
        assert!(!matches_link_mail(&index), "still in review");

        let (index, file) = with_link_mail(dir.path());
        let mut from_mail = portal_invoice(&file, date, Status::Stored);
        from_mail.source = Source::Mail;
        index.insert(&from_mail).unwrap();
        assert!(!matches_link_mail(&index), "came from a mail");

        let (index, file) = with_link_mail(dir.path());
        let mut elsewhere = portal_invoice(&file, date, Status::Stored);
        elsewhere.company_id = "other".into();
        index.insert(&elsewhere).unwrap();
        assert!(!matches_link_mail(&index), "another company's");

        let (index, _) = with_link_mail(dir.path());
        let gone = portal_invoice(&dir.path().join("gone.pdf"), date, Status::Stored);
        index.insert(&gone).unwrap();
        assert!(!matches_link_mail(&index), "its file is gone");
    }

    #[test]
    fn resolving_a_link_mail_keeps_the_row_and_marks_only_that_mail() {
        let dir = tempfile::tempdir().unwrap();
        let (index, _) = with_link_mail(dir.path());
        index
            .record_portal_fetch_required("acme", "main", "<other>", None)
            .unwrap();

        index
            .resolve_link_mail("acme", "main", "<link>", 42)
            .unwrap();

        let rows = index.invoices_of("acme").unwrap();
        assert!(rows[0].resolved_at.is_some());
        assert!(rows[0].disposed_at.is_some());
        assert_eq!(rows[0].settled_by, Some(42));
        assert_eq!(rows[1].resolved_at, None);
        assert!(
            !index
                .record_portal_fetch_required("acme", "main", "<link>", None)
                .unwrap(),
            "a mail that comes back is not noted again"
        );
    }

    #[test]
    fn a_person_can_dismiss_a_waiting_link_mail_once() {
        let dir = tempfile::tempdir().unwrap();
        let (index, _) = with_link_mail(dir.path());
        let link = index.invoices_of("acme").unwrap()[0].id;
        let stored = index.insert(&invoice(Some("R-1"), "h1")).unwrap();
        let closure = |index: &Index| index.link_mail_closure("acme", "main", "<link>").unwrap();
        assert_eq!(closure(&index), None);

        assert!(!index.dismiss_link_mail(stored).unwrap(), "not a link mail");
        assert!(index.dismiss_link_mail(link).unwrap());
        assert!(!index.dismiss_link_mail(link).unwrap(), "already done");

        let row = index.invoice(link).unwrap().unwrap();
        assert!(row.resolved_at.is_some());
        assert!(row.dismissed_at.is_some());
        assert_eq!(row.disposed_at, None, "Docsweep did not touch the mail");
        assert_eq!(row.settled_by, None, "no invoice was claimed");
        assert_eq!(closure(&index), Some(LinkClosure::Dismissed));
        assert!(index.waiting_link_mails().unwrap().is_empty());
        assert!(
            !index
                .record_portal_fetch_required("acme", "main", "<link>", None)
                .unwrap(),
            "a dismissed mail is not noted again"
        );
    }

    #[test]
    fn giving_up_on_a_link_mail_resolves_it_without_an_invoice() {
        let dir = tempfile::tempdir().unwrap();
        let (index, _) = with_link_mail(dir.path());
        assert_eq!(
            index
                .link_mail_waiting_since("acme", "main", "<link>")
                .unwrap(),
            NaiveDate::from_ymd_opt(2026, 11, 3)
        );

        assert!(index.give_up_link_mail("acme", "main", "<link>").unwrap());
        assert!(!index.give_up_link_mail("acme", "main", "<link>").unwrap());

        let row = index.invoices_of("acme").unwrap().remove(0);
        assert!(row.given_up_at.is_some());
        assert!(row.resolved_at.is_some());
        assert_eq!(
            (row.disposed_at, row.settled_by, row.dismissed_at),
            (None, None, None)
        );
        assert_eq!(
            index.link_mail_closure("acme", "main", "<link>").unwrap(),
            Some(LinkClosure::GivenUp)
        );
        assert_eq!(
            index
                .link_mail_waiting_since("acme", "main", "<link>")
                .unwrap(),
            None,
            "no longer waiting"
        );
    }

    #[test]
    fn a_link_mail_without_a_date_waits_since_it_was_noted() {
        let index = Index::open_in_memory().unwrap();
        index
            .record_portal_fetch_required("acme", "main", "<undated>", None)
            .unwrap();
        let since = index
            .link_mail_waiting_since("acme", "main", "<undated>")
            .unwrap()
            .unwrap();
        assert!((Utc::now().date_naive() - since).num_days().abs() <= 1);
    }

    #[test]
    fn a_dry_run_reads_closures_on_an_index_from_before_giving_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("docsweep.db");
        let conn = Connection::open(&path).unwrap();
        for migration in &MIGRATIONS[..GIVEN_UP_VERSION - 1] {
            conn.execute_batch(migration).unwrap();
        }
        conn.pragma_update(None, "user_version", (GIVEN_UP_VERSION - 1) as i64)
            .unwrap();
        drop(conn);
        let writer = Index {
            conn: Connection::open(&path).unwrap(),
        };
        writer
            .record_portal_fetch_required("acme", "main", "<link>", None)
            .unwrap();
        let link = writer.invoices_of("acme").unwrap()[0].id;
        writer.dismiss_link_mail(link).unwrap();
        drop(writer);

        let index = Index::open_read_only(&path).unwrap().unwrap();

        assert_eq!(
            index.link_mail_closure("acme", "main", "<link>").unwrap(),
            Some(LinkClosure::Dismissed)
        );
        assert_eq!(index.invoices_of("acme").unwrap()[0].given_up_at, None);
    }

    #[test]
    fn a_settled_link_mail_is_closed_as_settled() {
        let dir = tempfile::tempdir().unwrap();
        let (index, _) = with_link_mail(dir.path());
        index
            .resolve_link_mail("acme", "main", "<link>", 7)
            .unwrap();
        assert_eq!(
            index.link_mail_closure("acme", "main", "<link>").unwrap(),
            Some(LinkClosure::Settled)
        );
        assert_eq!(
            index.link_mail_closure("acme", "main", "<other>").unwrap(),
            None
        );
    }

    #[test]
    fn a_dry_run_reads_closures_on_an_index_from_before_dismissals() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("docsweep.db");
        let conn = Connection::open(&path).unwrap();
        for migration in &MIGRATIONS[..DISMISSED_VERSION - 1] {
            conn.execute_batch(migration).unwrap();
        }
        conn.pragma_update(None, "user_version", (DISMISSED_VERSION - 1) as i64)
            .unwrap();
        drop(conn);
        let writer = Index {
            conn: Connection::open(&path).unwrap(),
        };
        writer
            .record_portal_fetch_required("acme", "main", "<link>", None)
            .unwrap();
        writer
            .record_portal_fetch_required("acme", "main", "<open>", None)
            .unwrap();
        writer
            .resolve_link_mail("acme", "main", "<link>", 7)
            .unwrap();
        drop(writer);

        let index = Index::open_read_only(&path).unwrap().unwrap();

        assert_eq!(
            index.link_mail_closure("acme", "main", "<link>").unwrap(),
            Some(LinkClosure::Settled)
        );
        assert_eq!(
            index.link_mail_closure("acme", "main", "<open>").unwrap(),
            None
        );
        assert_eq!(index.invoices_of("acme").unwrap()[0].dismissed_at, None);
    }

    fn ymd(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).unwrap()
    }

    /// Stores a portal invoice of this date with its own file and hash.
    fn add_portal_invoice(index: &Index, dir: &Path, number: &str, date: NaiveDate) -> i64 {
        let file = dir.join(format!("{number}.pdf"));
        fs::write(&file, b"x").unwrap();
        index
            .insert(&NewInvoice {
                invoice_no: Some(number.into()),
                file_hash: number.into(),
                portal_key: Some(number.into()),
                ..portal_invoice(&file, Some(date), Status::Stored)
            })
            .unwrap()
    }

    /// Link mails of 3 October and 3 November, both waiting.
    fn october_and_november() -> Index {
        let index = Index::open_in_memory().unwrap();
        for (mail, date) in [("<oct>", ymd(2026, 10, 3)), ("<nov>", ymd(2026, 11, 3))] {
            index
                .record_portal_fetch_required("acme", "main", mail, Some(date))
                .unwrap();
        }
        index
    }

    fn invoice_of(index: &Index, mail: &str) -> Option<i64> {
        index.invoice_for_link_mail("acme", "main", mail).unwrap()
    }

    #[test]
    fn one_invoice_settles_only_the_link_mail_closest_to_its_date() {
        let dir = tempfile::tempdir().unwrap();
        let index = october_and_november();

        let october = add_portal_invoice(&index, dir.path(), "R-10", ymd(2026, 10, 1));

        assert_eq!(invoice_of(&index, "<oct>"), Some(october));
        assert_eq!(invoice_of(&index, "<nov>"), None, "November still waits");
        assert!(
            !index
                .link_mail_has_document("acme", "main", "<nov>")
                .unwrap()
        );

        // Once the October mail is settled, its invoice is gone for good.
        index
            .resolve_link_mail("acme", "main", "<oct>", october)
            .unwrap();
        assert_eq!(invoice_of(&index, "<oct>"), None, "no longer waiting");
        assert_eq!(invoice_of(&index, "<nov>"), None);
    }

    #[test]
    fn the_november_invoice_alone_settles_the_november_mail() {
        let dir = tempfile::tempdir().unwrap();
        let index = october_and_november();

        let november = add_portal_invoice(&index, dir.path(), "R-11", ymd(2026, 11, 1));

        assert_eq!(invoice_of(&index, "<oct>"), None);
        assert_eq!(invoice_of(&index, "<nov>"), Some(november));
    }

    #[test]
    fn two_invoices_settle_both_link_mails_each_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let index = october_and_november();
        // Stored in the other order, so the row order does not decide.
        let november = add_portal_invoice(&index, dir.path(), "R-11", ymd(2026, 11, 1));
        let october = add_portal_invoice(&index, dir.path(), "R-10", ymd(2026, 10, 1));

        assert_eq!(invoice_of(&index, "<oct>"), Some(october));
        assert_eq!(invoice_of(&index, "<nov>"), Some(november));
        index
            .resolve_link_mail("acme", "main", "<nov>", november)
            .unwrap();
        assert_eq!(invoice_of(&index, "<oct>"), Some(october));
    }

    #[test]
    fn an_old_invoice_from_before_the_link_mails_settles_none_of_them() {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::open_in_memory().unwrap();
        add_portal_invoice(&index, dir.path(), "R-9", ymd(2026, 9, 1));
        for (mail, date) in [("<oct>", ymd(2026, 10, 3)), ("<nov>", ymd(2026, 11, 3))] {
            index
                .record_portal_fetch_required("acme", "main", mail, Some(date))
                .unwrap();
        }

        assert_eq!(invoice_of(&index, "<oct>"), None);
        assert_eq!(invoice_of(&index, "<nov>"), None);
    }

    #[test]
    fn an_invoice_that_settled_a_mail_does_not_settle_a_later_one() {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::open_in_memory().unwrap();
        index
            .record_portal_fetch_required("acme", "main", "<oct>", Some(ymd(2026, 10, 3)))
            .unwrap();
        let october = add_portal_invoice(&index, dir.path(), "R-10", ymd(2026, 10, 1));
        index
            .resolve_link_mail("acme", "main", "<oct>", october)
            .unwrap();

        index
            .record_portal_fetch_required("acme", "main", "<again>", None)
            .unwrap();

        assert_eq!(invoice_of(&index, "<again>"), None);
    }

    #[test]
    fn a_dry_run_pairs_link_mails_on_an_index_from_before_settled_by() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("docsweep.db");
        let conn = Connection::open(&path).unwrap();
        for migration in &MIGRATIONS[..SETTLED_BY_VERSION - 1] {
            conn.execute_batch(migration).unwrap();
        }
        conn.pragma_update(None, "user_version", (SETTLED_BY_VERSION - 1) as i64)
            .unwrap();
        drop(conn);
        let writer = Index {
            conn: Connection::open(&path).unwrap(),
        };
        for (mail, date) in [("<oct>", ymd(2026, 10, 3)), ("<nov>", ymd(2026, 11, 3))] {
            writer
                .record_portal_fetch_required("acme", "main", mail, Some(date))
                .unwrap();
        }
        let october = add_portal_invoice(&writer, dir.path(), "R-10", ymd(2026, 10, 1));
        drop(writer);

        let index = Index::open_read_only(&path).unwrap().unwrap();

        assert_eq!(invoice_of(&index, "<oct>"), Some(october));
        assert_eq!(invoice_of(&index, "<nov>"), None);
        assert_eq!(index.invoices_of("acme").unwrap()[0].settled_by, None);
        assert_eq!(
            schema_version(&index.conn).unwrap(),
            SETTLED_BY_VERSION - 1,
            "a dry run never migrates"
        );
    }

    #[test]
    fn a_dry_run_pairs_link_mails_on_an_index_from_before_automation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("docsweep.db");
        index_before_automation(&path);
        let writer = Index {
            conn: Connection::open(&path).unwrap(),
        };
        writer
            .record_portal_fetch_required("acme", "main", "<oct>", Some(ymd(2026, 10, 3)))
            .unwrap();
        let october = add_portal_invoice(&writer, dir.path(), "R-10", ymd(2026, 10, 1));
        drop(writer);

        let index = Index::open_read_only(&path).unwrap().unwrap();

        assert_eq!(invoice_of(&index, "<oct>"), Some(october));
    }

    #[test]
    fn company_runs_are_remembered_per_company_and_kind() {
        let index = Index::open_in_memory().unwrap();
        let day = NaiveDate::from_ymd_opt(2026, 11, 6).unwrap();
        let time = DateTime::parse_from_rfc3339("2026-11-06T08:00:00Z")
            .unwrap()
            .to_utc();

        index.note_link_fetch("acme", day).unwrap();
        assert_eq!(index.last_link_fetch_on("acme").unwrap(), Some(day));
        assert_eq!(index.last_scheduled_run("acme").unwrap(), None);

        index.note_scheduled_run("acme", time).unwrap();
        assert_eq!(index.last_link_fetch_on("acme").unwrap(), Some(day));
        assert_eq!(index.last_scheduled_run("acme").unwrap(), Some(time));
        assert_eq!(index.last_link_fetch_on("other").unwrap(), None);
    }

    #[test]
    fn a_newer_schema_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("docsweep.db");
        Connection::open(&path)
            .unwrap()
            .pragma_update(None, "user_version", 99)
            .unwrap();
        assert!(matches!(
            Index::open(&path),
            Err(IndexError::TooNew { found: 99, .. })
        ));
    }
}
