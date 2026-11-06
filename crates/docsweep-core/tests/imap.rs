//! Tests against a local GreenMail server, started by `scripts/imap-tests.sh`.
//! GreenMail only has throw-away test users; no real mail is involved.

use std::collections::HashSet;
use std::fs;

use async_imap::Client;
use base64::Engine;
use chrono::NaiveDate;
use docsweep_core::config::Config;
use docsweep_core::index::{Index, Status};
use docsweep_core::mail::execute::{Outcome, scan_and_execute, scan_read_only};
use docsweep_core::mail::imap::{ImapMailbox, ImapReader, ImapWriter, TransportSecurity};
use docsweep_core::mail::plan::{MailAction, PlanContext};
use docsweep_core::mail::rules::RuleSet;
use docsweep_core::mail::{MailReader, MailWriter};
use docsweep_core::secrets::SecretString;
use futures_util::TryStreamExt;
use lopdf::content::{Content, Operation};
use lopdf::{Dictionary, Document, Object, Stream, dictionary};
use tokio::net::TcpStream;

const USER: &str = "invoices";
const PASSWORD: &str = "test-password";

fn endpoint() -> (String, u16) {
    let value =
        std::env::var("DOCSWEEP_TEST_IMAP").expect("run these tests through scripts/imap-tests.sh");
    let (host, port) = value.rsplit_once(':').unwrap();
    (host.to_owned(), port.parse().unwrap())
}

fn password() -> SecretString {
    SecretString::from(PASSWORD)
}

async fn reader(folder: &str) -> ImapReader {
    let (host, port) = endpoint();
    ImapMailbox::connect_read_only(
        &host,
        port,
        TransportSecurity::PlainForTests,
        USER,
        &password(),
        folder,
    )
    .await
    .unwrap()
}

async fn writer(folder: &str) -> ImapWriter {
    let (host, port) = endpoint();
    ImapMailbox::connect_writable(
        &host,
        port,
        TransportSecurity::PlainForTests,
        USER,
        &password(),
        folder,
    )
    .await
    .unwrap()
}

/// Direct access for setting up and inspecting the server, independent of the
/// code under test.
struct Inspector {
    session: async_imap::Session<TcpStream>,
}

impl Inspector {
    async fn connect() -> Self {
        let (host, port) = endpoint();
        let tcp = TcpStream::connect((host.as_str(), port)).await.unwrap();
        let mut client = Client::new(tcp);
        client.read_response().await.unwrap();
        let session = client.login(USER, PASSWORD).await.map_err(|e| e.0).unwrap();
        Self { session }
    }

    /// Removes everything from the folders used by these tests.
    async fn reset() -> Self {
        let mut inspector = Self::connect().await;
        let folders: Vec<String> = inspector
            .session
            .list(Some(""), Some("*"))
            .await
            .unwrap()
            .map_ok(|name| name.name().to_owned())
            .try_collect()
            .await
            .unwrap();
        for folder in folders {
            if folder.eq_ignore_ascii_case("INBOX") {
                let mailbox = inspector.session.select("INBOX").await.unwrap();
                if mailbox.exists > 0 {
                    let _: Vec<_> = inspector
                        .session
                        .store("1:*", "+FLAGS.SILENT (\\Deleted)")
                        .await
                        .unwrap()
                        .try_collect()
                        .await
                        .unwrap();
                    let _: Vec<_> = inspector
                        .session
                        .expunge()
                        .await
                        .unwrap()
                        .try_collect()
                        .await
                        .unwrap();
                }
            } else {
                inspector.session.delete(&folder).await.unwrap();
            }
        }
        inspector
    }

    async fn add(&mut self, raw: &[u8]) {
        self.session.append("INBOX", None, None, raw).await.unwrap();
    }

    /// UIDs and flags of all mails in a folder, read without changing them.
    async fn contents(&mut self, folder: &str) -> Vec<(u32, Vec<String>)> {
        let mailbox = self.session.examine(folder).await.unwrap();
        if mailbox.exists == 0 {
            return Vec::new();
        }
        let fetches: Vec<_> = self
            .session
            .uid_fetch("1:*", "(UID FLAGS)")
            .await
            .unwrap()
            .try_collect()
            .await
            .unwrap();
        fetches
            .iter()
            .map(|fetch| {
                let flags = fetch.flags().map(|flag| format!("{flag:?}")).collect();
                (fetch.uid.unwrap(), flags)
            })
            .collect()
    }

    async fn count(&mut self, folder: &str) -> usize {
        self.contents(folder).await.len()
    }
}

fn dummy_pdf(lines: &[&str]) -> Vec<u8> {
    let mut doc = Document::with_version("1.7");
    let pages_id = doc.new_object_id();
    let font_id = doc.add_object(dictionary! {
        "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
    });
    let resources_id = doc.add_object(dictionary! {
        "Font" => dictionary! { "F1" => font_id },
    });
    let mut operations = vec![
        Operation::new("BT", vec![]),
        Operation::new("Tf", vec!["F1".into(), 12.into()]),
        Operation::new("Td", vec![50.into(), 700.into()]),
        Operation::new("TL", vec![16.into()]),
    ];
    for line in lines {
        operations.push(Operation::new("Tj", vec![Object::string_literal(*line)]));
        operations.push(Operation::new("T*", vec![]));
    }
    operations.push(Operation::new("ET", vec![]));
    let content = Content { operations }.encode().unwrap();
    let content_id = doc.add_object(Stream::new(Dictionary::new(), content));
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page", "Parent" => pages_id, "Contents" => content_id,
    });
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages", "Kids" => vec![page_id.into()], "Count" => 1,
            "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        }),
    );
    let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog_id);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    bytes
}

fn raw_mail(from: &str, subject: &str, id: &str, pdf: Option<&[u8]>) -> Vec<u8> {
    let mut mail = format!(
        "From: Billing <{from}>\r\nTo: invoices@example.test\r\nSubject: {subject}\r\n\
         Date: Tue, 03 Nov 2026 10:00:00 +0100\r\nMessage-ID: <{id}>\r\nMIME-Version: 1.0\r\n\
         Content-Type: multipart/mixed; boundary=\"b\"\r\n\r\n\
         --b\r\nContent-Type: text/plain\r\n\r\nHello\r\n"
    );
    if let Some(pdf) = pdf {
        let encoded = base64::engine::general_purpose::STANDARD.encode(pdf);
        mail.push_str(&format!(
            "--b\r\nContent-Type: application/pdf\r\n\
             Content-Disposition: attachment; filename=\"invoice.pdf\"\r\n\
             Content-Transfer-Encoding: base64\r\n\r\n{encoded}\r\n"
        ));
    }
    mail.push_str("--b--\r\n");
    mail.into_bytes()
}

fn plain_mail(id: &str) -> Vec<u8> {
    raw_mail("a@example.test", "Hello", id, None)
}

#[tokio::test]
#[ignore = "needs GreenMail, see scripts/imap-tests.sh"]
async fn wrong_password_fails_without_revealing_it() {
    let (host, port) = endpoint();
    let wrong = SecretString::from("wrong-secret-value");
    let result = ImapMailbox::connect_read_only(
        &host,
        port,
        TransportSecurity::PlainForTests,
        USER,
        &wrong,
        "INBOX",
    )
    .await;
    let message = result.err().expect("login must fail").to_string();
    assert!(message.contains("login"), "{message}");
    assert!(!message.contains("wrong-secret-value"), "{message}");
}

#[tokio::test]
#[ignore = "needs GreenMail, see scripts/imap-tests.sh"]
async fn untrusted_certificates_are_refused() {
    let value = std::env::var("DOCSWEEP_TEST_IMAPS").unwrap();
    let (host, port) = value.rsplit_once(':').unwrap();
    let result = ImapMailbox::connect_read_only(
        host,
        port.parse().unwrap(),
        TransportSecurity::Tls,
        USER,
        &password(),
        "INBOX",
    )
    .await;
    let message = result.err().expect("must be refused").to_string();
    assert!(message.contains("TLS"), "{message}");
}

#[tokio::test]
#[ignore = "needs GreenMail, see scripts/imap-tests.sh"]
async fn reading_does_not_mark_mails_as_seen() {
    let mut inspector = Inspector::reset().await;
    inspector.add(&plain_mail("1@x")).await;

    let mut read_only = reader("INBOX").await;
    let headers = read_only.list_headers().await.unwrap();
    assert_eq!(headers.len(), 1);
    let raw = read_only.fetch_raw(headers[0].0).await.unwrap();
    assert!(String::from_utf8_lossy(&raw).contains("Hello"));
    read_only.logout().await;

    let mut writable = writer("INBOX").await;
    writable.fetch_raw(headers[0].0).await.unwrap();
    writable.logout().await;

    let contents = inspector.contents("INBOX").await;
    assert!(
        !contents[0].1.iter().any(|flag| flag.contains("Seen")),
        "{contents:?}"
    );
}

#[tokio::test]
#[ignore = "needs GreenMail, see scripts/imap-tests.sh"]
async fn empty_folder_lists_nothing() {
    Inspector::reset().await;
    let mut read_only = reader("INBOX").await;
    assert!(read_only.list_headers().await.unwrap().is_empty());
    read_only.logout().await;
}

#[tokio::test]
#[ignore = "needs GreenMail, see scripts/imap-tests.sh"]
async fn move_creates_nested_folders_on_any_separator() {
    let mut inspector = Inspector::reset().await;
    inspector.add(&plain_mail("1@x")).await;
    inspector.add(&plain_mail("2@x")).await;

    let mut writable = writer("INBOX").await;
    let headers = writable.list_headers().await.unwrap();
    writable
        .move_to(headers[0].0, "Invoices/Büro")
        .await
        .unwrap();
    writable.logout().await;

    assert_eq!(inspector.count("INBOX").await, 1);
    let folders: Vec<String> = inspector
        .session
        .list(Some(""), Some("*"))
        .await
        .unwrap()
        .map_ok(|name| name.name().to_owned())
        .try_collect()
        .await
        .unwrap();
    let target = folders
        .iter()
        .find(|name| name.contains("B&APw-ro"))
        .unwrap_or_else(|| panic!("target folder missing in {folders:?}"))
        .clone();
    assert_eq!(inspector.count(&target).await, 1);
}

#[tokio::test]
#[ignore = "needs GreenMail, see scripts/imap-tests.sh"]
async fn flagging_affects_only_the_given_mail() {
    let mut inspector = Inspector::reset().await;
    inspector.add(&plain_mail("1@x")).await;
    inspector.add(&plain_mail("2@x")).await;

    let mut writable = writer("INBOX").await;
    let headers = writable.list_headers().await.unwrap();
    writable.flag(headers[0].0).await.unwrap();
    writable.logout().await;

    let contents = inspector.contents("INBOX").await;
    assert!(contents[0].1.iter().any(|flag| flag.contains("Flagged")));
    assert!(!contents[1].1.iter().any(|flag| flag.contains("Flagged")));
}

#[tokio::test]
#[ignore = "needs GreenMail, see scripts/imap-tests.sh"]
async fn permanent_delete_spares_mails_someone_else_marked_as_deleted() {
    let mut inspector = Inspector::reset().await;
    inspector.add(&plain_mail("1@x")).await;
    inspector.add(&plain_mail("2@x")).await;
    inspector.session.select("INBOX").await.unwrap();
    let _: Vec<_> = inspector
        .session
        .store("2", "+FLAGS.SILENT (\\Deleted)")
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();

    let mut writable = writer("INBOX").await;
    let headers = writable.list_headers().await.unwrap();
    writable.delete_permanently(headers[0].0).await.unwrap();
    writable.logout().await;

    let contents = inspector.contents("INBOX").await;
    assert_eq!(contents.len(), 1, "{contents:?}");
    assert_eq!(contents[0].0, headers[1].0);
}

#[tokio::test]
#[ignore = "needs GreenMail, see scripts/imap-tests.sh"]
async fn without_a_trash_folder_the_mail_stays() {
    let mut inspector = Inspector::reset().await;
    inspector.add(&plain_mail("1@x")).await;

    let mut writable = writer("INBOX").await;
    let headers = writable.list_headers().await.unwrap();
    let error = writable.move_to_trash(headers[0].0).await.unwrap_err();
    writable.logout().await;

    assert!(error.to_string().contains("trash"), "{error}");
    assert_eq!(inspector.count("INBOX").await, 1);
}

// End-to-end runs of the whole pipeline.

fn config_for(dir: &std::path::Path) -> Config {
    Config::from_yaml(&format!(
        r#"
mail:
  accounts:
    - {{ id: main, imap: "unused.example:993", user: u, password: "keyring:p", watch: INBOX }}
  disposal: {{ mode: pdf_move_link_delete, move_to: Invoices/Done }}
companies:
  - id: acme
    name: Acme
    mail: {{ from_domain: acme.example }}
    output: {{ dir: '{}', filename: "{{date:%Y-%m-%d}} - Acme - {{invoice_no}}.pdf" }}
  - id: portal
    name: Portal
    mail: {{ from_domain: portal.example }}
    portal: {{ credentials: "keyring:c", recipe: recipes/portal.yaml }}
    output: {{ dir: '{}', filename: "{{invoice_no}}.pdf" }}
    disposal: delete
"#,
        dir.display(),
        dir.display()
    ))
    .unwrap()
}

fn today() -> NaiveDate {
    NaiveDate::from_ymd_opt(2030, 1, 1).unwrap()
}

async fn add_example_mails(inspector: &mut Inspector) {
    let invoice = dummy_pdf(&["Rechnungsnummer: E2E-1", "Rechnungsdatum: 03.11.2026"]);
    let no_number = dummy_pdf(&["Allgemeine Informationen"]);
    let mails = [
        raw_mail("b@acme.example", "Ihre Post", "invoice@x", Some(&invoice)),
        raw_mail("b@acme.example", "Ihre Post", "review@x", Some(&no_number)),
        raw_mail("b@portal.example", "Rechnung online", "link@x", None),
        raw_mail("a@elsewhere.example", "Urlaub", "other@x", None),
    ];
    for mail in mails {
        inspector.add(&mail).await;
    }
}

#[tokio::test]
#[ignore = "needs GreenMail, see scripts/imap-tests.sh"]
async fn dry_run_changes_nothing_anywhere() {
    let mut inspector = Inspector::reset().await;
    add_example_mails(&mut inspector).await;
    let before = inspector.contents("INBOX").await;

    let dir = tempfile::tempdir().unwrap();
    let config = config_for(dir.path());
    let rules = RuleSet::new(&config);
    let context = PlanContext {
        config: &config,
        rules: &rules,
        index: None,
        account_id: "main",
        today: today(),
    };
    let mut reports = Vec::new();
    let mut read_only = reader("INBOX").await;
    scan_read_only(&context, &mut read_only, &mut reports)
        .await
        .unwrap();
    read_only.logout().await;

    assert_eq!(reports.len(), 3, "the unrelated mail is not reported");
    assert!(reports.iter().all(|r| r.outcome == Outcome::Planned));
    let MailAction::Invoices { documents, .. } = &reports[0].plan.action else {
        panic!("expected invoices: {:?}", reports[0].plan.action);
    };
    assert_eq!(documents[0].file_name, "2026-11-03 - Acme - E2E-1.pdf");

    assert_eq!(inspector.contents("INBOX").await, before);
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[tokio::test]
#[ignore = "needs GreenMail, see scripts/imap-tests.sh"]
async fn real_run_stores_indexes_and_cleans_up_only_what_is_safe() {
    let mut inspector = Inspector::reset().await;
    add_example_mails(&mut inspector).await;

    let dir = tempfile::tempdir().unwrap();
    let config = config_for(dir.path());
    let rules = RuleSet::new(&config);
    let index = Index::open(&dir.path().join("index").join("docsweep.db")).unwrap();
    let context = PlanContext {
        config: &config,
        rules: &rules,
        index: Some(&index),
        account_id: "main",
        today: today(),
    };

    for _ in 0..2 {
        // The second run proves that repeating a scan is harmless.
        let mut reports = Vec::new();
        let mut writable = writer("INBOX").await;
        scan_and_execute(&context, &index, &mut writable, &mut reports)
            .await
            .unwrap();
        writable.logout().await;
        let failures: Vec<_> = reports
            .iter()
            .filter(|report| matches!(report.outcome, Outcome::Failed { .. }))
            .collect();
        assert!(failures.is_empty(), "{failures:?}");
    }

    let stored = dir.path().join("2026-11-03 - Acme - E2E-1.pdf");
    assert!(stored.exists());
    let acme = index.invoices_of("acme").unwrap();
    assert_eq!(acme.len(), 2);
    assert_eq!(acme[0].status, Status::Stored);
    assert!(acme[0].disposed_at.is_some());
    assert_eq!(acme[1].status, Status::NeedsReview);
    assert_eq!(acme[1].disposed_at, None);
    let portal = index.invoices_of("portal").unwrap();
    assert_eq!(portal.len(), 1);
    assert_eq!(portal[0].status, Status::PortalFetchRequired);

    // Moved: the invoice. Staying: review case (flagged), link mail, other mail.
    let inbox = inspector.contents("INBOX").await;
    assert_eq!(inbox.len(), 3, "{inbox:?}");
    let flagged = inbox
        .iter()
        .filter(|(_, flags)| flags.iter().any(|flag| flag.contains("Flagged")))
        .count();
    assert_eq!(flagged, 1, "{inbox:?}");
    let separator: HashSet<String> = inspector
        .session
        .list(Some(""), Some("*"))
        .await
        .unwrap()
        .map_ok(|name| name.name().to_owned())
        .try_collect()
        .await
        .unwrap();
    let done = separator
        .iter()
        .find(|name| name.ends_with("Done"))
        .unwrap_or_else(|| panic!("no Done folder in {separator:?}"))
        .clone();
    assert_eq!(inspector.count(&done).await, 1);
}
