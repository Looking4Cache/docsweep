//! Completes documents that were stored for review.
//!
//! A person supplies what could not be read. The file gets its final name and
//! the row becomes `stored`, so the next scan recognises the mail's file as
//! safely stored and cleans the mail up as usual.

use std::fs;
use std::path::PathBuf;

use chrono::NaiveDate;

use crate::config::Config;
use crate::index::{Index, IndexError, InvoiceRecord, Status};
use crate::naming::{FilenameTemplate, sanitize_invoice_no};
use crate::storage::{self, StorageError};

/// Why a review could not be completed.
#[derive(Debug, thiserror::Error)]
pub enum ReviewError {
    #[error("this document is not waiting for review")]
    NotInReview,
    #[error("the invoice number must not be empty")]
    EmptyNumber,
    #[error("the company `{0}` is no longer in the configuration")]
    UnknownCompany(String),
    #[error("the file {0} no longer exists")]
    FileMissing(PathBuf),
    #[error("company `{company}` has an invalid file name template: {problem}")]
    Template { company: String, problem: String },
    #[error("cannot rename {path}: {source}")]
    Rename {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(transparent)]
    Index(#[from] IndexError),
    #[error(transparent)]
    Storage(#[from] StorageError),
}

/// Gives a document in review its number and date, renames the file according
/// to the company's template, and marks it as stored.
pub fn complete_review(
    config: &Config,
    index: &Index,
    id: i64,
    invoice_no: &str,
    date: NaiveDate,
) -> Result<InvoiceRecord, ReviewError> {
    let record = index.invoice(id)?.ok_or(ReviewError::NotInReview)?;
    if record.status != Status::NeedsReview {
        return Err(ReviewError::NotInReview);
    }
    let number = invoice_no.trim();
    if sanitize_invoice_no(number).is_empty() {
        return Err(ReviewError::EmptyNumber);
    }
    let company = config
        .companies
        .iter()
        .find(|company| company.id == record.company_id)
        .ok_or_else(|| ReviewError::UnknownCompany(record.company_id.clone()))?;
    let current = record.file_path.clone().ok_or(ReviewError::NotInReview)?;
    if !current.exists() {
        return Err(ReviewError::FileMissing(current));
    }
    let template = FilenameTemplate::parse(&company.output.filename).map_err(|problem| {
        ReviewError::Template {
            company: company.id.clone(),
            problem,
        }
    })?;

    let file_name = template.render(date, number);
    let dir = current.parent().map(PathBuf::from).unwrap_or_default();
    let target = if current
        .file_name()
        .is_some_and(|name| name == file_name.as_str())
    {
        current.clone()
    } else {
        storage::free_path(&dir, &file_name)?
    };
    if target != current {
        fs::rename(&current, &target).map_err(|source| ReviewError::Rename {
            path: current.clone(),
            source,
        })?;
    }

    if let Err(error) = index.complete_review(id, number, date, &target) {
        if target != current {
            // Keep file and index consistent: the index still names the old path.
            let _ = fs::rename(&target, &current);
        }
        return Err(error.into());
    }
    index.invoice(id)?.ok_or(ReviewError::NotInReview)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{NewInvoice, Source};

    struct Setup {
        dir: tempfile::TempDir,
        config: Config,
        index: Index,
    }

    fn setup() -> Setup {
        let dir = tempfile::tempdir().unwrap();
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
    output: {{ dir: '{}', filename: "{{date:%Y-%m-%d}} - Acme - {{invoice_no}}.pdf" }}
"#,
            dir.path().display()
        ))
        .unwrap();
        Setup {
            dir,
            config,
            index: Index::open_in_memory().unwrap(),
        }
    }

    fn date() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, 3).unwrap()
    }

    fn stored_for_review(setup: &Setup, status: Status) -> i64 {
        let path = setup.dir.path().join("unbekannt-abc.pdf");
        fs::write(&path, b"pdf").unwrap();
        setup
            .index
            .insert(&NewInvoice {
                company_id: "acme".into(),
                invoice_date: Some(date()),
                invoice_no: None,
                amount: None,
                currency: None,
                file_path: path,
                file_hash: "hash".into(),
                source: Source::Mail,
                mail_account: None,
                mail_id: None,
                portal_key: None,
                status,
            })
            .unwrap()
    }

    #[test]
    fn renames_the_file_and_completes_the_row() {
        let setup = setup();
        let id = stored_for_review(&setup, Status::NeedsReview);

        let record = complete_review(&setup.config, &setup.index, id, " R-7 ", date()).unwrap();

        assert_eq!(record.status, Status::Stored);
        assert_eq!(record.invoice_no.as_deref(), Some("R-7"));
        let expected = setup.dir.path().join("2026-11-03 - Acme - R-7.pdf");
        assert_eq!(record.file_path.as_ref(), Some(&expected));
        assert!(expected.exists());
        assert!(!setup.dir.path().join("unbekannt-abc.pdf").exists());
    }

    #[test]
    fn never_overwrites_another_file() {
        let setup = setup();
        let taken = setup.dir.path().join("2026-11-03 - Acme - R-7.pdf");
        fs::write(&taken, b"other").unwrap();
        let id = stored_for_review(&setup, Status::NeedsReview);

        let record = complete_review(&setup.config, &setup.index, id, "R-7", date()).unwrap();

        assert_eq!(fs::read(&taken).unwrap(), b"other");
        assert_eq!(
            record.file_path,
            Some(setup.dir.path().join("2026-11-03 - Acme - R-7 (2).pdf"))
        );
    }

    #[test]
    fn crafted_numbers_stay_inside_the_folder() {
        let setup = setup();
        let id = stored_for_review(&setup, Status::NeedsReview);

        let record =
            complete_review(&setup.config, &setup.index, id, "../../evil", date()).unwrap();

        let path = record.file_path.unwrap();
        assert_eq!(path.parent(), Some(setup.dir.path()));
    }

    #[test]
    fn rejects_empty_numbers_and_finished_documents() {
        let setup = setup();
        let id = stored_for_review(&setup, Status::NeedsReview);
        assert!(matches!(
            complete_review(&setup.config, &setup.index, id, "  ", date()),
            Err(ReviewError::EmptyNumber)
        ));
        complete_review(&setup.config, &setup.index, id, "R-1", date()).unwrap();
        assert!(matches!(
            complete_review(&setup.config, &setup.index, id, "R-2", date()),
            Err(ReviewError::NotInReview)
        ));
    }

    #[test]
    fn missing_file_is_reported_and_nothing_changes() {
        let setup = setup();
        let id = stored_for_review(&setup, Status::NeedsReview);
        fs::remove_file(setup.dir.path().join("unbekannt-abc.pdf")).unwrap();
        assert!(matches!(
            complete_review(&setup.config, &setup.index, id, "R-1", date()),
            Err(ReviewError::FileMissing(_))
        ));
        let row = setup.index.invoice(id).unwrap().unwrap();
        assert_eq!(row.status, Status::NeedsReview);
    }
}
