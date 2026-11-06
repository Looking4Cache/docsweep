//! SQLite index of all known invoices and the duplicate check.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::NaiveDate;
use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, params};

/// Schema migrations; the position plus one is the `user_version` it leads to.
const MIGRATIONS: &[&str] = &[r#"
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
"#];

/// Where an invoice came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    pub status: Status,
}

/// A row of the index.
#[derive(Debug, Clone, PartialEq, Eq)]
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
                                   file_path, file_hash, source, mail_account, mail_id, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
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

    /// Whether a link mail is already noted as requiring a portal fetch.
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

    /// Records that the mail of these rows was cleaned up.
    pub fn mark_mail_disposed(&self, mail_account: &str, mail_id: &str) -> Result<(), IndexError> {
        self.conn.execute(
            "UPDATE invoices SET disposed_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
             WHERE mail_account = ?1 AND mail_id = ?2 AND disposed_at IS NULL",
            params![mail_account, mail_id],
        )?;
        Ok(())
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
