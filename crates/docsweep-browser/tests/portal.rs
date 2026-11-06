//! Runs real recipes in a real, headless browser against a fictional portal: a
//! small HTTP server in this file that serves the saved pages in
//! `tests/fixtures/portal` and generates its PDFs at test time. No real portal
//! and no real credentials are involved.
//!
//! Run them with `scripts/browser-tests.sh`.

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use docsweep_browser::{
    ChromeDriver, ChromeOptions, FetchOptions, FetchPaths, WindowMode, fetch_company,
};
use docsweep_core::config::Config;
use docsweep_core::document::{looks_like_pdf, test_pdf};
use docsweep_core::index::{Index, Status};
use docsweep_core::portal::driver::{
    BrowserDriver, ConfirmationAnswer, ConfirmationHandler, ConfirmationReason, ConfirmationRequest,
};
use docsweep_core::portal::engine::{EntryOutcome, FetchReport, summarize_fetches};
use docsweep_core::secrets::{MemoryStore, SecretStore, SecretString};

const USER: &str = "fictional-user@portal.test";
const PASSWORD: &str = "fictional-password-do-not-log";
const TOTP_KEY: &str = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";

#[derive(Default)]
struct PortalState {
    /// `METHOD /path`, in order.
    requests: Vec<String>,
    /// Sessions only exist after user, password and TOTP were all correct.
    sessions: u32,
    confirmed: bool,
}

/// The fictional portal. Each test has its own on a free port.
#[derive(Clone)]
struct Portal {
    base: String,
    state: Arc<Mutex<PortalState>>,
}

impl Portal {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let state = Arc::new(Mutex::new(PortalState::default()));
        let shared = Arc::clone(&state);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let state = Arc::clone(&shared);
                std::thread::spawn(move || {
                    let _ = serve(stream, &state);
                });
            }
        });
        Self { base, state }
    }

    fn count(&self, request: &str) -> usize {
        let state = self.state.lock().unwrap();
        state
            .requests
            .iter()
            .filter(|r| r.as_str() == request)
            .count()
    }

    fn document_requests(&self) -> Vec<String> {
        let state = self.state.lock().unwrap();
        state
            .requests
            .iter()
            .filter(|r| r.contains("/doc/"))
            .cloned()
            .collect()
    }

    fn confirm(&self) {
        self.state.lock().unwrap().confirmed = true;
    }
}

fn fixture(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/portal")
        .join(name);
    fs::read(path).unwrap()
}

struct Reply {
    status: &'static str,
    content_type: &'static str,
    headers: Vec<String>,
    body: Vec<u8>,
}

impl Reply {
    fn html(name: &str) -> Self {
        Self {
            status: "200 OK",
            content_type: "text/html; charset=utf-8",
            headers: Vec::new(),
            body: fixture(name),
        }
    }

    fn pdf(lines: &[&str]) -> Self {
        Self {
            status: "200 OK",
            content_type: "application/pdf",
            headers: Vec::new(),
            body: test_pdf::with_text(lines),
        }
    }

    fn status(status: &'static str) -> Self {
        Self {
            status,
            content_type: "text/plain",
            headers: Vec::new(),
            body: status.as_bytes().to_vec(),
        }
    }

    fn redirect(location: &str, cookie: Option<&str>) -> Self {
        let mut headers = vec![format!("Location: {location}")];
        headers.extend(cookie.map(|cookie| format!("Set-Cookie: {cookie}; Path=/; Max-Age=3600")));
        Self {
            status: "303 See Other",
            content_type: "text/plain",
            headers,
            body: Vec::new(),
        }
    }
}

fn serve(mut stream: TcpStream, state: &Mutex<PortalState>) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut parts = request_line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Ok(());
    };
    let (method, target) = (method.to_owned(), target.to_owned());

    let mut length = 0;
    let mut cookies = String::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let line = line.trim();
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':').unwrap_or((line, ""));
        match name.to_ascii_lowercase().as_str() {
            "content-length" => length = value.trim().parse().unwrap_or(0),
            "cookie" => cookies = value.to_owned(),
            _ => {}
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    let body = String::from_utf8_lossy(&body).into_owned();

    let reply = route(&method, &target, &cookies, &body, state);
    write!(
        stream,
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        reply.status,
        reply.content_type,
        reply.body.len()
    )?;
    for header in &reply.headers {
        write!(stream, "{header}\r\n")?;
    }
    stream.write_all(b"\r\n")?;
    stream.write_all(&reply.body)
}

fn form_value(body: &str, name: &str) -> String {
    body.split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value.replace("%40", "@").replace('+', " "))
        .unwrap_or_default()
}

fn current_code_is_valid(code: &str) -> bool {
    let secret = totp_rs::Secret::try_from_base32(TOTP_KEY).unwrap();
    totp_rs::Builder::new()
        .with_secret(secret)
        .build_noncompliant()
        .check_current(code)
        .is_some()
}

fn route(
    method: &str,
    target: &str,
    cookies: &str,
    body: &str,
    state: &Mutex<PortalState>,
) -> Reply {
    let mut state = state.lock().unwrap();
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if !path.ends_with("favicon.ico") && path != "/confirm/status" {
        state.requests.push(format!("{method} {path}"));
    }
    let session = cookies.contains("session=valid");
    let protected = |page: Reply| {
        if session {
            page
        } else {
            Reply::redirect("/login", None)
        }
    };

    match (method, path) {
        ("GET", "/login") if session => Reply::redirect("/invoices", None),
        ("GET", "/login") => Reply::html("login.html"),
        ("POST", "/login") => {
            if form_value(body, "user") == USER && form_value(body, "password") == PASSWORD {
                Reply::redirect("/otp", Some("first_factor=ok"))
            } else {
                Reply::html("login.html")
            }
        }
        ("GET", "/otp") if cookies.contains("first_factor=ok") => Reply::html("otp.html"),
        ("POST", "/otp") => {
            if cookies.contains("first_factor=ok")
                && current_code_is_valid(&form_value(body, "code"))
            {
                state.sessions += 1;
                Reply::redirect("/invoices", Some("session=valid"))
            } else {
                Reply::html("otp.html")
            }
        }
        ("GET", "/invoices") => protected(Reply::html("invoices.html")),
        ("GET", "/troubled") => Reply::html("troubled.html"),
        ("GET", "/paged") => {
            let page = form_value(query, "page");
            Reply::html(&format!(
                "paged-{}.html",
                if page.is_empty() { "1" } else { &page }
            ))
        }
        ("GET", "/orders") => Reply::html("orders.html"),
        ("GET", "/orders/O-1") => Reply::html("order-O-1.html"),
        ("GET", "/orders/O-2") => Reply::html("order-O-2.html"),
        ("GET", "/orders/O-3") => Reply::html("order-O-3.html"),
        ("GET", "/confirm") if state.confirmed => Reply::html("invoices.html"),
        ("GET", "/confirm") => Reply::html("confirm.html"),
        ("GET", "/confirm/status") if state.confirmed => Reply {
            headers: vec!["Set-Cookie: session=valid; Path=/; Max-Age=3600".to_owned()],
            ..Reply::status("200 OK")
        },
        ("GET", "/confirm/status") => Reply::status("404 Not Found"),
        ("GET", "/doc/broken.pdf") => Reply::status("500 Internal Server Error"),
        ("GET", "/doc/nameless.pdf") => Reply::pdf(&["A document without a number"]),
        ("GET", "/doc/crafted.pdf") => Reply::pdf(&["Rechnungsnummer: ../../R-9/evil"]),
        ("GET", "/doc/R-1003.html") => protected(Reply::html("invoice.html")),
        ("GET", document) if document.starts_with("/doc/R-1") => {
            let number = document_number(document);
            protected(Reply::pdf(&[
                &format!("Rechnungsnummer: {number}"),
                "Rechnungsdatum: 03.11.2026",
            ]))
        }
        ("GET", document) if document.starts_with("/doc/") => {
            let number = document_number(document);
            Reply::pdf(&[
                &format!("Rechnungsnummer: {number}"),
                "Rechnungsdatum: 03.11.2026",
            ])
        }
        _ => Reply::status("404 Not Found"),
    }
}

fn document_number(path: &str) -> &str {
    path.trim_start_matches("/doc/").trim_end_matches(".pdf")
}

/// A person who never answers.
struct NobodyThere;

impl ConfirmationHandler for NobodyThere {
    async fn ask(&mut self, _request: &ConfirmationRequest) -> ConfirmationAnswer {
        std::future::pending().await
    }
}

/// A person who confirms in the fictional portal and then says so.
struct PersonConfirming {
    portal: Portal,
    reasons: Vec<ConfirmationReason>,
}

impl ConfirmationHandler for PersonConfirming {
    async fn ask(&mut self, request: &ConfirmationRequest) -> ConfirmationAnswer {
        self.reasons.push(request.reason.clone());
        self.portal.confirm();
        // The page notices by itself after a moment.
        tokio::time::sleep(Duration::from_millis(600)).await;
        ConfirmationAnswer::Done
    }
}

/// One company in a temporary world.
struct World {
    dir: tempfile::TempDir,
    portal: Portal,
    config: Config,
    secrets: MemoryStore,
}

impl World {
    fn new(recipe: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let portal = Portal::start();
        let recipe_file = dir.path().join("recipe.yaml");
        fs::write(&recipe_file, recipe).unwrap();
        let output = dir.path().join("invoices");
        let config = Config::from_yaml(&format!(
            r#"
mail:
  accounts:
    - {{ id: main, imap: "i.example:993", user: u, password: "keyring:p", watch: INBOX }}
  disposal: {{ mode: all_delete }}
companies:
  - id: fictional
    name: Fictional
    portal:
      credentials: "keyring:fictional"
      totp: "keyring:fictional-totp"
      recipe: '{}'
      params: {{ base: "{}" }}
    output: {{ dir: '{}', filename: "{{date:%Y-%m-%d}} - Fictional - {{invoice_no}}.pdf" }}
"#,
            recipe_file.display(),
            portal.base,
            output.display(),
        ))
        .unwrap();
        let stored = format!(r#"{{"user":"{USER}","password":"{PASSWORD}"}}"#);
        let secrets = MemoryStore::with("fictional", &stored);
        secrets
            .set("fictional-totp", &SecretString::from(TOTP_KEY))
            .unwrap();
        Self {
            dir,
            portal,
            config,
            secrets,
        }
    }

    fn paths(&self) -> FetchPaths {
        FetchPaths {
            index: self.dir.path().join("docsweep.db"),
            recipes_dir: self.dir.path().join("recipes"),
            profile_dir: self.dir.path().join("profile"),
        }
    }

    fn output_dir(&self) -> PathBuf {
        self.dir.path().join("invoices")
    }

    fn files(&self) -> BTreeSet<String> {
        match fs::read_dir(self.output_dir()) {
            Ok(entries) => entries
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect(),
            Err(_) => BTreeSet::new(),
        }
    }

    fn index(&self) -> Index {
        Index::open_read_only(&self.paths().index).unwrap().unwrap()
    }

    fn fetch<H: ConfirmationHandler>(&self, dry_run: bool, handler: &mut H) -> FetchReport {
        let options = FetchOptions {
            dry_run,
            // Local runs must never open a window on the desktop.
            window: WindowMode::Hidden,
            recipe_file: None,
        };
        block_on(fetch_company(
            &self.config,
            &self.config.companies[0],
            &self.secrets,
            &self.paths(),
            &options,
            handler,
        ))
    }
}

fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

fn stored_count(report: &FetchReport) -> usize {
    report
        .entries
        .iter()
        .filter(|entry| matches!(entry.outcome, EntryOutcome::Stored { .. }))
        .count()
}

const LOGIN_RECIPE: &str = r##"
params: { base: "http://placeholder.test" }
step_timeout_secs: 15
steps:
  - goto: "{base}/login"
  - dismiss_cookie_banner: true
  - login:
      user: "#username"
      password: "#password"
      submit: "button[type=submit]"
      totp: "#otp"
      logged_in: "#logout"
  - goto: "{base}/invoices"
  - extract_list:
      rows: "table.invoices tbody tr"
      fields:
        date: "td:nth-child(1)"
        invoice_no: "td:nth-child(2)"
        amount: "td:nth-child(3)"
        url: "a.download@href"
  - download_new:
      key: invoice_no
      html_to_pdf: true
"##;

#[test]
#[ignore = "needs Chrome; run scripts/browser-tests.sh"]
fn logs_in_downloads_prints_stores_and_then_finds_nothing_new() {
    let world = World::new(LOGIN_RECIPE);

    let report = world.fetch(false, &mut NobodyThere);

    assert_eq!(report.error, None, "{report:?}");
    assert_eq!(stored_count(&report), 3, "{report:?}");
    assert_eq!(world.portal.count("POST /login"), 1);
    assert_eq!(world.portal.count("POST /otp"), 1);
    assert_eq!(world.portal.state.lock().unwrap().sessions, 1);
    assert_eq!(
        world.files(),
        BTreeSet::from([
            "2026-11-03 - Fictional - R-1001.pdf".to_owned(),
            "2026-11-03 - Fictional - R-1002.pdf".to_owned(),
            "2026-11-05 - Fictional - R-1003.pdf".to_owned(),
        ])
    );
    let printed = fs::read(
        world
            .output_dir()
            .join("2026-11-05 - Fictional - R-1003.pdf"),
    )
    .unwrap();
    assert!(looks_like_pdf(&printed));
    let rows = world.index().invoices_of("fictional").unwrap();
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|row| row.status == Status::Stored));

    // The second run uses the saved session and downloads nothing new.
    let documents_before = world.portal.document_requests().len();
    let again = world.fetch(false, &mut NobodyThere);

    assert_eq!(again.error, None, "{again:?}");
    let summary = summarize_fetches(std::slice::from_ref(&again));
    assert_eq!((summary.saved, summary.already_known), (0, 3));
    assert_eq!(world.portal.document_requests().len(), documents_before);
    assert_eq!(
        world.portal.count("POST /login"),
        1,
        "the session should persist"
    );
    assert_eq!(world.files().len(), 3);
}

#[test]
#[ignore = "needs Chrome; run scripts/browser-tests.sh"]
fn a_dry_run_lists_what_is_new_and_writes_nothing() {
    let world = World::new(LOGIN_RECIPE);

    let report = world.fetch(true, &mut NobodyThere);

    assert_eq!(report.error, None, "{report:?}");
    assert_eq!(report.entries.len(), 3);
    assert!(
        report
            .entries
            .iter()
            .all(|entry| matches!(entry.outcome, EntryOutcome::WouldDownload { .. }))
    );
    assert_eq!(report.entries[0].key, "R-1001");
    assert_eq!(report.entries[0].amount.as_deref(), Some("119,00 €"));
    assert_eq!(
        report.entries[0].outcome,
        EntryOutcome::WouldDownload {
            file_name: Some("2026-11-03 - Fictional - R-1001.pdf".to_owned())
        }
    );
    assert!(world.portal.document_requests().is_empty());
    assert!(world.files().is_empty());
    assert!(!world.output_dir().exists());
    assert!(!world.paths().index.exists());
}

#[test]
#[ignore = "needs Chrome; run scripts/browser-tests.sh"]
fn failures_stay_with_their_invoice_and_crafted_names_stay_in_the_folder() {
    let world = World::new(
        r#"
params: { base: "http://placeholder.test" }
step_timeout_secs: 15
steps:
  - goto: "{base}/troubled"
  - extract_list:
      rows: "table.invoices tbody tr"
      fields:
        date: "td:nth-child(1)"
        invoice_no: "td:nth-child(2)"
        url: "a.download@href"
  - download_new: { key: invoice_no }
"#,
    );

    let report = world.fetch(false, &mut NobodyThere);

    assert_eq!(report.error, None, "{report:?}");
    let failed: HashSet<&str> = report
        .entries
        .iter()
        .filter(|entry| matches!(entry.outcome, EntryOutcome::Failed { .. }))
        .map(|entry| entry.key.as_str())
        .collect();
    assert_eq!(failed, HashSet::from(["R-2002", "R-2005"]), "{report:?}");
    assert_eq!(stored_count(&report), 4, "{report:?}");
    assert_eq!(world.files().len(), 4, "{:?}", world.files());
    assert!(
        world
            .files()
            .contains("2026-11-03 - Fictional - R-2006.pdf")
    );
    // Everything stored is directly in the target folder.
    let root = world.output_dir().canonicalize().unwrap();
    for row in world.index().invoices_of("fictional").unwrap() {
        let path = row.file_path.unwrap().canonicalize().unwrap();
        assert_eq!(path.parent().unwrap(), root);
    }
    assert!(!world.dir.path().join("escaped.pdf").exists());
    let requested = world.portal.document_requests();
    assert!(
        requested.iter().all(|r| !r.contains("passwd")),
        "{requested:?}"
    );
}

#[test]
#[ignore = "needs Chrome; run scripts/browser-tests.sh"]
fn pagination_reads_all_pages() {
    let world = World::new(
        r#"
params: { base: "http://placeholder.test" }
step_timeout_secs: 15
steps:
  - goto: "{base}/paged"
  - extract_list:
      rows: "table.invoices tbody tr"
      next_page: a.next
      fields:
        date: "td:nth-child(1)"
        invoice_no: "td:nth-child(2)"
        url: "a.download@href"
  - download_new: { key: invoice_no }
"#,
    );

    let report = world.fetch(true, &mut NobodyThere);

    assert_eq!(report.error, None, "{report:?}");
    let keys: Vec<&str> = report
        .entries
        .iter()
        .map(|entry| entry.key.as_str())
        .collect();
    assert_eq!(keys, ["P-11", "P-12", "P-21", "P-22", "P-31", "P-32"]);
}

#[test]
#[ignore = "needs Chrome; run scripts/browser-tests.sh"]
fn orders_can_have_several_documents_or_none() {
    let world = World::new(
        r#"
params: { base: "http://placeholder.test" }
step_timeout_secs: 15
steps:
  - goto: "{base}/orders"
  - extract_list:
      rows: li.order
      fields: { order: .number, details: a.details@href }
      open_detail:
        url: details
        rows: li.document
        missing: .no-documents
        fields: { invoice_no: .number, url: a@href }
  - download_new: { key: invoice_no }
"#,
    );

    let report = world.fetch(false, &mut NobodyThere);

    assert_eq!(report.error, None, "{report:?}");
    assert_eq!(stored_count(&report), 3, "{report:?}");
    let missing: Vec<&str> = report
        .entries
        .iter()
        .filter(|entry| entry.outcome == EntryOutcome::Missing)
        .map(|entry| entry.key.as_str())
        .collect();
    assert_eq!(missing, ["O-2"]);
    assert_eq!(world.files().len(), 3);
    assert!(!report.has_failures());
}

const CONFIRM_RECIPE: &str = r##"
params: { base: "http://placeholder.test" }
step_timeout_secs: 15
confirmation: { detect: "#confirm-required", timeout_secs: 3 }
steps:
  - goto: "{base}/confirm"
  - extract_list:
      rows: "table.invoices tbody tr"
      fields: { invoice_no: "td:nth-child(2)", url: "a.download@href" }
  - download_new: { key: invoice_no, html_to_pdf: true }
"##;

#[test]
#[ignore = "needs Chrome; run scripts/browser-tests.sh"]
fn a_detected_confirmation_waits_for_a_person_and_the_run_goes_on() {
    let world = World::new(&CONFIRM_RECIPE.replace("timeout_secs: 3", "timeout_secs: 30"));
    let mut person = PersonConfirming {
        portal: world.portal.clone(),
        reasons: Vec::new(),
    };

    let report = world.fetch(true, &mut person);

    assert_eq!(person.reasons, [ConfirmationReason::Detected]);
    assert_eq!(report.error, None, "{report:?}");
    assert_eq!(report.entries.len(), 3);
}

#[test]
#[ignore = "needs Chrome; run scripts/browser-tests.sh"]
fn a_confirmation_nobody_answers_ends_the_run_consistently() {
    let world = World::new(CONFIRM_RECIPE);

    let report = world.fetch(false, &mut NobodyThere);

    assert!(
        report.error.as_ref().unwrap().contains("in time"),
        "{report:?}"
    );
    assert!(report.entries.is_empty());
    assert!(world.files().is_empty());
    assert!(world.index().invoices_of("fictional").unwrap().is_empty());
    assert!(report.has_failures());
}

#[test]
#[ignore = "needs Chrome; run scripts/browser-tests.sh"]
fn wrong_credentials_are_sent_once_and_never_appear_in_the_report() {
    let world = World::new(
        LOGIN_RECIPE
            .replace(
                "step_timeout_secs: 15",
                "step_timeout_secs: 2\nconfirmation: { timeout_secs: 1 }",
            )
            .as_str(),
    );
    let wrong = r#"{"user":"fictional-wrong-user","password":"fictional-wrong-password"}"#;
    world
        .secrets
        .set("fictional", &SecretString::from(wrong))
        .unwrap();

    let report = world.fetch(false, &mut NobodyThere);

    assert!(report.error.is_some(), "{report:?}");
    assert_eq!(world.portal.count("POST /login"), 1);
    let text = format!("{report:?} {}", serde_json::to_string(&report).unwrap());
    for secret in ["fictional-wrong-user", "fictional-wrong-password", TOTP_KEY] {
        assert!(!text.contains(secret), "{text}");
    }
}

#[test]
#[ignore = "needs Chrome; run scripts/browser-tests.sh"]
fn an_invalid_selector_is_named_and_a_failed_fill_hides_the_value() {
    let world = World::new(&LOGIN_RECIPE.replace("#username", "#username[["));

    let report = world.fetch(false, &mut NobodyThere);

    let error = report.error.unwrap();
    assert!(error.contains("#username[["), "{error}");
    assert!(!error.contains(PASSWORD), "{error}");
    assert_eq!(world.portal.count("POST /login"), 0);
}

#[test]
#[ignore = "needs Chrome; run scripts/browser-tests.sh"]
fn the_driver_reports_address_and_visibility() {
    let portal = Portal::start();
    let dir = tempfile::tempdir().unwrap();
    block_on(async {
        let mut driver = ChromeDriver::launch(ChromeOptions {
            profile_dir: dir.path().join("profile"),
            window: WindowMode::Hidden,
        })
        .await
        .unwrap();
        driver
            .goto(&format!("{}/login", portal.base))
            .await
            .unwrap();
        let address = driver.current_url().await.unwrap();
        assert_eq!(address, format!("{}/login", portal.base));
        assert!(driver.is_visible("#username").await.unwrap());
        assert!(!driver.is_visible("#nope").await.unwrap());
        driver.close().await;
    });
}

/// Opens a window, so it only runs with `scripts/browser-tests.sh --visible`.
#[test]
#[ignore = "opens a window; run scripts/browser-tests.sh --visible"]
fn visible_mode_opens_a_window_and_on_demand_switches_with_the_same_profile() {
    let world = World::new(
        CONFIRM_RECIPE
            .replace("timeout_secs: 3", "timeout_secs: 30")
            .as_str(),
    );
    let mut person = PersonConfirming {
        portal: world.portal.clone(),
        reasons: Vec::new(),
    };
    let options = FetchOptions {
        dry_run: false,
        window: WindowMode::OnDemand,
        recipe_file: None,
    };

    let report = block_on(fetch_company(
        &world.config,
        &world.config.companies[0],
        &world.secrets,
        &world.paths(),
        &options,
        &mut person,
    ));

    assert_eq!(report.error, None, "{report:?}");
    assert_eq!(person.reasons, [ConfirmationReason::Detected]);
}
