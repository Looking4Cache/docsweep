//! The mail pipeline: read mails, match rules, plan, store, clean up.
//!
//! Dry runs and real runs share [`plan::plan_mail`]. Dry runs only ever receive
//! a [`MailReader`], so they cannot change a mailbox by construction.

pub mod execute;
pub mod imap;
pub mod plan;
pub mod rules;

use chrono::NaiveDate;
use mail_parser::MessageParser;

use crate::document::looks_like_pdf;
use crate::storage::sha256_hex;

/// An attachment of a mail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub name: String,
    pub bytes: Vec<u8>,
}

impl Attachment {
    /// Decided by content, because mail clients often send PDFs with generic
    /// content types such as `application/octet-stream`.
    pub fn is_pdf(&self) -> bool {
        looks_like_pdf(&self.bytes)
    }
}

/// A parsed mail. Parsed from headers only, it simply has no attachments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncomingMail {
    pub uid: u32,
    /// `Message-ID` header; stays the same when a mail moves, unlike the UID.
    pub message_id: String,
    pub from: String,
    pub from_domain: Option<String>,
    pub subject: String,
    pub date: Option<NaiveDate>,
    pub attachments: Vec<Attachment>,
}

impl IncomingMail {
    /// Parses a raw mail. A mail too broken to parse yields empty fields, which
    /// match no rule, so it stays untouched.
    pub fn parse(uid: u32, raw: &[u8]) -> Self {
        let fallback_id = || format!("no-message-id:{}", sha256_hex(raw));
        let Some(message) = MessageParser::default().parse(raw) else {
            return Self {
                uid,
                message_id: fallback_id(),
                from: String::new(),
                from_domain: None,
                subject: String::new(),
                date: None,
                attachments: Vec::new(),
            };
        };

        let from = message
            .from()
            .and_then(|list| list.first())
            .and_then(|mailbox| mailbox.address().map(str::to_owned))
            .unwrap_or_default();
        let from_domain = from
            .rsplit_once('@')
            .map(|(_, domain)| domain.trim().to_ascii_lowercase())
            .filter(|domain| !domain.is_empty());
        let date = message.date().and_then(|date| {
            NaiveDate::from_ymd_opt(
                i32::from(date.year),
                u32::from(date.month),
                u32::from(date.day),
            )
        });
        let attachments = message
            .attachments()
            .map(|part| Attachment {
                name: part.attachment_name().unwrap_or("attachment").to_owned(),
                bytes: part.decoded().into_owned(),
            })
            .collect();

        Self {
            uid,
            message_id: message.message_id().map_or_else(fallback_id, str::to_owned),
            from,
            from_domain,
            subject: message.subject().unwrap_or_default().to_owned(),
            date,
            attachments,
        }
    }
}

/// Why a mailbox operation failed. Never contains credentials.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct MailError(pub String);

/// Reading from a mailbox without changing anything, not even `\Seen` flags.
#[allow(
    async_fn_in_trait,
    reason = "only used through generics inside the workspace"
)]
pub trait MailReader {
    /// The raw headers of all mails in the watched folder, with their UIDs.
    async fn list_headers(&mut self) -> Result<Vec<(u32, Vec<u8>)>, MailError>;
    /// One complete raw mail.
    async fn fetch_raw(&mut self, uid: u32) -> Result<Vec<u8>, MailError>;
}

/// Changing a mailbox. Only real runs get a value of this type.
#[allow(
    async_fn_in_trait,
    reason = "only used through generics inside the workspace"
)]
pub trait MailWriter: MailReader {
    /// Moves a mail, creating the folder if needed. `/` separates folder levels.
    async fn move_to(&mut self, uid: u32, folder: &str) -> Result<(), MailError>;
    /// Moves a mail to the server's trash folder. Fails if there is none.
    async fn move_to_trash(&mut self, uid: u32) -> Result<(), MailError>;
    /// Deletes a mail for good, without affecting other mails.
    async fn delete_permanently(&mut self, uid: u32) -> Result<(), MailError>;
    /// Marks a mail for the user's attention with `\Flagged`.
    async fn flag(&mut self, uid: u32) -> Result<(), MailError>;
}

#[cfg(test)]
pub(crate) mod test_mail {
    //! Builds raw test mails with dummy attachments.

    use base64::Engine;

    /// A raw mail with PDF attachments, given as `(file name, content)`.
    pub fn raw(from: &str, subject: &str, id: &str, attachments: &[(&str, &[u8])]) -> Vec<u8> {
        let mut mail = format!(
            "From: Billing <{from}>\r\nTo: me@example.test\r\nSubject: {subject}\r\n\
             Date: Tue, 03 Nov 2026 10:00:00 +0100\r\nMessage-ID: <{id}>\r\nMIME-Version: 1.0\r\n\
             Content-Type: multipart/mixed; boundary=\"b\"\r\n\r\n\
             --b\r\nContent-Type: text/plain\r\n\r\nHello\r\n"
        );
        for (name, content) in attachments {
            let encoded = base64::engine::general_purpose::STANDARD.encode(content);
            mail.push_str(&format!(
                "--b\r\nContent-Type: application/octet-stream\r\n\
                 Content-Disposition: attachment; filename=\"{name}\"\r\n\
                 Content-Transfer-Encoding: base64\r\n\r\n{encoded}\r\n"
            ));
        }
        mail.push_str("--b--\r\n");
        mail.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_fields_used_by_the_pipeline() {
        let raw = test_mail::raw(
            "billing@Mail.Example.COM",
            "Ihre Rechnung",
            "abc@example.com",
            &[("a.pdf", b"%PDF-1.7 dummy")],
        );
        let mail = IncomingMail::parse(7, &raw);
        assert_eq!(mail.uid, 7);
        assert_eq!(mail.message_id, "abc@example.com");
        assert_eq!(mail.from_domain.as_deref(), Some("mail.example.com"));
        assert_eq!(mail.subject, "Ihre Rechnung");
        assert_eq!(mail.date, NaiveDate::from_ymd_opt(2026, 11, 3));
        assert_eq!(mail.attachments.len(), 1);
        assert_eq!(mail.attachments[0].name, "a.pdf");
        assert_eq!(mail.attachments[0].bytes, b"%PDF-1.7 dummy");
        assert!(mail.attachments[0].is_pdf());
    }

    #[test]
    fn missing_message_id_gets_a_stable_substitute() {
        let raw = b"From: a@b.example\r\nSubject: x\r\n\r\nbody\r\n";
        let first = IncomingMail::parse(1, raw);
        assert!(first.message_id.starts_with("no-message-id:"));
        assert_eq!(first.message_id, IncomingMail::parse(2, raw).message_id);
    }

    #[test]
    fn headers_alone_are_enough_for_matching() {
        let mail = IncomingMail::parse(1, b"From: a@b.example\r\nSubject: Hallo\r\n\r\n");
        assert_eq!(mail.from_domain.as_deref(), Some("b.example"));
        assert_eq!(mail.subject, "Hallo");
    }

    #[test]
    fn garbage_matches_nothing_and_does_not_panic() {
        let mail = IncomingMail::parse(1, &[0xFF; 64]);
        assert_eq!(mail.from_domain, None);
        assert!(mail.attachments.is_empty());
    }
}
