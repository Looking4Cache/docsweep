//! [`BrowserDriver`] on top of `chromiumoxide`.
//!
//! Errors are built from fixed texts and selectors. Filled values and document
//! addresses stay out of them, as does the text of the underlying errors, which
//! is not under this code's control.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use chromiumoxide::Page;
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::input::InsertTextParams;
use chromiumoxide::cdp::browser_protocol::page::PrintToPdfParams;
use chromiumoxide::cdp::js_protocol::runtime::EvaluateParams;
use chromiumoxide::error::CdpError;
use docsweep_core::portal::driver::{BrowserDriver, DriverError, RowValues};
use docsweep_core::portal::recipe::FieldSelector;
use docsweep_core::secrets::{ExposeSecret, SecretString};
use futures_util::StreamExt;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tokio::task::JoinHandle;

/// How long one command to the browser may take, including loading a page.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a closing browser gets before it is ended by force.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(10);
/// Larger documents are not invoices. Base64 must also fit one protocol message.
const MAX_DOWNLOAD_BYTES: u64 = 32 * 1024 * 1024;

/// When the browser has a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowMode {
    /// Never a window, not even for a confirmation. For machines without a
    /// display; a person can then only confirm by mail or push.
    Hidden,
    /// No window until a person is needed. The browser then restarts visibly
    /// with the same profile, because a running Chrome cannot change this.
    OnDemand,
    /// A window from the start.
    Visible,
}

/// How to start the browser.
#[derive(Debug, Clone)]
pub struct ChromeOptions {
    pub profile_dir: PathBuf,
    pub window: WindowMode,
}

/// A running browser with one page.
#[derive(Debug)]
pub struct ChromeDriver {
    browser: Browser,
    page: Page,
    handler: JoinHandle<()>,
    options: ChromeOptions,
    has_window: bool,
}

impl ChromeDriver {
    /// Starts installed Chrome or Edge. The `CHROME` environment variable can
    /// point to a specific one.
    pub async fn launch(options: ChromeOptions) -> Result<Self, DriverError> {
        let has_window = options.window == WindowMode::Visible;
        Self::start(options, has_window).await
    }

    async fn start(options: ChromeOptions, has_window: bool) -> Result<Self, DriverError> {
        std::fs::create_dir_all(&options.profile_dir).map_err(|source| {
            DriverError::Failed(format!(
                "cannot create the browser profile {}: {source}",
                options.profile_dir.display()
            ))
        })?;
        let builder = BrowserConfig::builder()
            .user_data_dir(&options.profile_dir)
            // The library ignores certificate errors unless told otherwise, and
            // this browser types passwords.
            .respect_https_errors()
            .request_timeout(REQUEST_TIMEOUT);
        let builder = if has_window {
            builder.with_head().viewport(None)
        } else {
            builder.new_headless_mode()
        };
        let config = builder.build().map_err(|_| {
            DriverError::Failed(
                "no Chrome or Edge found; install one of them to fetch from portals".to_owned(),
            )
        })?;
        let (browser, mut handler) = Browser::launch(config)
            .await
            .map_err(|error| DriverError::Failed(format!("cannot start the browser: {error}")))?;
        let handler = tokio::spawn(async move { while handler.next().await.is_some() {} });
        let page = browser
            .new_page("about:blank")
            .await
            .map_err(|_| DriverError::Failed("cannot open a page in the browser".to_owned()))?;
        Ok(Self {
            browser,
            page,
            handler,
            options,
            has_window,
        })
    }

    /// Ends the browser so that its profile is saved and free again.
    pub async fn close(mut self) {
        self.shut_down().await;
    }

    async fn shut_down(&mut self) {
        let closed = self.browser.close().await.is_ok()
            && tokio::time::timeout(CLOSE_TIMEOUT, self.browser.wait())
                .await
                .is_ok();
        if !closed {
            let _ = self.browser.kill().await;
        }
        self.handler.abort();
    }

    async fn evaluate<T: DeserializeOwned>(&self, expression: String) -> Result<T, CdpError> {
        let params = EvaluateParams::builder()
            .expression(expression)
            .await_promise(true)
            .return_by_value(true)
            .build()
            .map_err(CdpError::msg)?;
        self.page
            .evaluate(params)
            .await?
            .into_value()
            .map_err(CdpError::from)
    }
}

/// A selector or address as a JavaScript string. Page code never sees secrets:
/// these are the only values placed into scripts.
fn js_string(text: &str) -> String {
    serde_json::Value::from(text).to_string()
}

fn is_browser_gone(error: &CdpError) -> bool {
    matches!(
        error,
        CdpError::Ws(_) | CdpError::NoResponse | CdpError::ChannelSendError(_) | CdpError::Io(_)
    )
}

/// Replies of the download script.
#[derive(Deserialize)]
struct DownloadReply {
    status: Option<u16>,
    too_large: Option<bool>,
    data: Option<String>,
}

impl BrowserDriver for ChromeDriver {
    async fn goto(&mut self, url: &str) -> Result<(), DriverError> {
        match self.page.goto(url).await {
            Ok(_) => Ok(()),
            Err(CdpError::Timeout) => Err(DriverError::Timeout),
            Err(CdpError::ChromeMessage(message)) => Err(DriverError::Failed(format!(
                "the page did not open: {message}"
            ))),
            Err(_) => Err(DriverError::Failed("the page did not open".to_owned())),
        }
    }

    async fn current_url(&mut self) -> Result<String, DriverError> {
        self.page
            .url()
            .await
            .map_err(|_| DriverError::Failed("the browser did not respond".to_owned()))?
            .ok_or_else(|| DriverError::Failed("the page has no address yet".to_owned()))
    }

    async fn is_visible(&mut self, selector: &str) -> Result<bool, DriverError> {
        let script = format!(
            "[...document.querySelectorAll({})].some((e) => e.getClientRects().length > 0)",
            js_string(selector)
        );
        match self.evaluate(script).await {
            Ok(visible) => Ok(visible),
            Err(CdpError::JavascriptException(_)) => Err(DriverError::Failed(format!(
                "invalid selector `{selector}`"
            ))),
            Err(error) if is_browser_gone(&error) => {
                Err(DriverError::Failed("the browser has closed".to_owned()))
            }
            // The page is just changing.
            Err(_) => Ok(false),
        }
    }

    async fn click(&mut self, selector: &str) -> Result<(), DriverError> {
        let failed = |_| DriverError::Failed(format!("could not click `{selector}`"));
        let element = self.page.find_element(selector).await.map_err(failed)?;
        element.click().await.map_err(failed)?;
        Ok(())
    }

    async fn fill(&mut self, selector: &str, value: &SecretString) -> Result<(), DriverError> {
        let failed = || DriverError::Failed(format!("could not fill `{selector}`"));
        // Selecting what is there makes the typed text replace it. The value
        // takes the same path as typing and never becomes part of a script.
        let focus = format!(
            "(() => {{ const e = [...document.querySelectorAll({})]
                .find((c) => c.getClientRects().length > 0);
              if (!e) return false;
              e.focus();
              e.select?.();
              return true; }})()",
            js_string(selector)
        );
        let focused: bool = self.evaluate(focus).await.map_err(|_| failed())?;
        if !focused {
            return Err(failed());
        }
        self.page
            .execute(InsertTextParams::new(value.expose_secret()))
            .await
            .map_err(|_| failed())?;
        Ok(())
    }

    async fn extract_rows(
        &mut self,
        rows: &str,
        fields: &BTreeMap<String, FieldSelector>,
    ) -> Result<Vec<RowValues>, DriverError> {
        let fields: serde_json::Map<String, serde_json::Value> = fields
            .iter()
            .map(|(name, field)| {
                let value = serde_json::json!({
                    "selector": field.selector,
                    "attribute": field.attribute,
                });
                (name.clone(), value)
            })
            .collect();
        let script = format!(
            "(() => {{ const fields = Object.entries({});
              return [...document.querySelectorAll({})].map((row) => {{
                const values = {{}};
                for (const [name, field] of fields) {{
                  const element = field.selector ? row.querySelector(field.selector) : row;
                  if (!element) continue;
                  const value = field.attribute
                    ? element.getAttribute(field.attribute)
                    : element.textContent;
                  if (value !== null) values[name] = value;
                }}
                return values;
              }}); }})()",
            serde_json::Value::Object(fields),
            js_string(rows),
        );
        self.evaluate(script)
            .await
            .map_err(|_| DriverError::Failed(format!("could not read the rows `{rows}`")))
    }

    /// Fetches inside the page, so that the session's cookies apply and no
    /// cookie ever leaves the browser. The browser's rules for other origins
    /// apply as well: a document on a different host that does not allow this
    /// fails with its own message.
    async fn download(&mut self, url: &str) -> Result<Vec<u8>, DriverError> {
        let script = format!(
            "(async () => {{
              const response = await fetch({}, {{ credentials: 'include' }});
              if (!response.ok) return {{ status: response.status }};
              const blob = await response.blob();
              if (blob.size > {MAX_DOWNLOAD_BYTES}) return {{ too_large: true }};
              const data = await new Promise((resolve, reject) => {{
                const reader = new FileReader();
                reader.onload = () => resolve(reader.result);
                reader.onerror = () => reject(reader.error);
                reader.readAsDataURL(blob);
              }});
              return {{ data }};
            }})()",
            js_string(url)
        );
        let reply: DownloadReply = self.evaluate(script).await.map_err(|error| match error {
            CdpError::JavascriptException(_) => DriverError::Failed(
                "the document could not be fetched from this page; it may be on another host"
                    .to_owned(),
            ),
            CdpError::Timeout => DriverError::Timeout,
            _ => DriverError::Failed("the document could not be fetched".to_owned()),
        })?;
        if let Some(status) = reply.status {
            return Err(DriverError::Failed(format!(
                "the portal answered with status {status}"
            )));
        }
        if reply.too_large == Some(true) {
            return Err(DriverError::Failed(format!(
                "the document is larger than {} MiB",
                MAX_DOWNLOAD_BYTES / 1024 / 1024
            )));
        }
        let data = reply.data.unwrap_or_default();
        let (_, encoded) = data.split_once(',').unwrap_or(("", ""));
        BASE64
            .decode(encoded)
            .map_err(|_| DriverError::Failed("the document could not be decoded".to_owned()))
    }

    async fn print_to_pdf(&mut self) -> Result<Vec<u8>, DriverError> {
        let params = PrintToPdfParams {
            print_background: Some(true),
            ..PrintToPdfParams::default()
        };
        self.page
            .pdf(params)
            .await
            .map_err(|_| DriverError::Failed("the page could not be printed".to_owned()))
    }

    async fn show_window(&mut self) -> Result<(), DriverError> {
        if self.options.window != WindowMode::OnDemand || self.has_window {
            return Ok(());
        }
        let address = self.current_url().await.ok();
        self.shut_down().await;
        *self = Self::start(self.options.clone(), true).await?;
        if let Some(address) = address {
            self.goto(&address).await?;
        }
        Ok(())
    }
}
