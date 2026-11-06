//! The recorder: a browser with the company's profile, in which a person logs in
//! and clicks a document, and which notes how they did it.
//!
//! A listener in the page reports selectors through a binding (see `recorder.js`).
//! It never reads typed values. Neither page content nor cookies are kept: what
//! comes out is a list of events for `portal::recording::build_draft`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use chromiumoxide::cdp::browser_protocol::network;
use chromiumoxide::cdp::browser_protocol::page::EventFrameNavigated;
use chromiumoxide::cdp::js_protocol::runtime::{AddBindingParams, EventBindingCalled};
use docsweep_core::portal::driver::{BrowserDriver, DriverError};
use docsweep_core::portal::recording::RecordedEvent;
use futures_util::StreamExt;
use tokio::task::JoinHandle;

use crate::chrome::{ChromeDriver, ChromeOptions};

const BINDING: &str = "docsweepRecord";
const LISTENER: &str = include_str!("recorder.js");
/// A page may call the binding with whatever it likes. This is plenty for events
/// made of selectors, and bounds what it can make this process keep.
const MAX_EVENT_BYTES: usize = 8 * 1024;
const MAX_EVENTS: usize = 5_000;
const CHECK_INTERVAL: Duration = Duration::from_millis(500);

type Events = Arc<Mutex<Vec<RecordedEvent>>>;

fn keep(events: &Events, event: RecordedEvent) {
    let mut events = events.lock().expect("lock is never poisoned");
    if events.len() < MAX_EVENTS {
        events.push(event);
    }
}

/// A running recording.
pub struct Recorder {
    driver: ChromeDriver,
    events: Events,
    tasks: Vec<JoinHandle<()>>,
}

impl Recorder {
    /// Starts a browser with the listener in place and opens the address.
    pub async fn start(options: ChromeOptions, address: &str) -> Result<Self, DriverError> {
        let mut driver = ChromeDriver::launch(options).await?;
        let events: Events = Arc::default();
        let failed = |_| DriverError::Failed("the recorder could not be set up".to_owned());
        let page = driver.page().clone();

        page.execute(AddBindingParams::new(BINDING))
            .await
            .map_err(failed)?;
        page.execute(network::EnableParams::default())
            .await
            .map_err(failed)?;
        page.evaluate_on_new_document(LISTENER)
            .await
            .map_err(failed)?;

        let mut tasks = Vec::new();
        let mut calls = page
            .event_listener::<EventBindingCalled>()
            .await
            .map_err(failed)?;
        let sink = Arc::clone(&events);
        tasks.push(tokio::spawn(async move {
            while let Some(call) = calls.next().await {
                if call.name != BINDING || call.payload.len() > MAX_EVENT_BYTES {
                    continue;
                }
                // Anything that is not exactly an event is dropped.
                if let Ok(event) = serde_json::from_str::<RecordedEvent>(&call.payload) {
                    // These two only ever come from the browser itself.
                    if !matches!(
                        event,
                        RecordedEvent::Navigated { .. } | RecordedEvent::Request { .. }
                    ) {
                        keep(&sink, event);
                    }
                }
            }
        }));

        let mut navigations = page
            .event_listener::<EventFrameNavigated>()
            .await
            .map_err(failed)?;
        let sink = Arc::clone(&events);
        tasks.push(tokio::spawn(async move {
            while let Some(navigation) = navigations.next().await {
                if navigation.frame.parent_id.is_none() {
                    keep(
                        &sink,
                        RecordedEvent::Navigated {
                            address: navigation.frame.url.clone(),
                        },
                    );
                }
            }
        }));

        let mut requests = page
            .event_listener::<network::EventRequestWillBeSent>()
            .await
            .map_err(failed)?;
        let sink = Arc::clone(&events);
        tasks.push(tokio::spawn(async move {
            while let Some(request) = requests.next().await {
                if is_interesting(&request.request.url, request.r#type.as_ref()) {
                    keep(
                        &sink,
                        RecordedEvent::Request {
                            method: request.request.method.clone(),
                            address: request.request.url.clone(),
                        },
                    );
                }
            }
        }));

        driver.goto(address).await?;
        Ok(Self {
            driver,
            events,
            tasks,
        })
    }

    /// The browser, for tests that play the person.
    pub fn driver(&mut self) -> &mut ChromeDriver {
        &mut self.driver
    }

    /// Waits until `stop` finishes or the person closes the browser.
    pub async fn wait_for_end<F: Future<Output = ()>>(&mut self, stop: F) {
        tokio::pin!(stop);
        loop {
            tokio::select! {
                () = &mut stop => return,
                () = tokio::time::sleep(CHECK_INTERVAL) => {
                    if self.driver.current_url().await.is_err() {
                        return;
                    }
                }
            }
        }
    }

    /// Ends the recording and closes the browser, so that the profile is free.
    pub async fn finish(self) -> Vec<RecordedEvent> {
        for task in &self.tasks {
            task.abort();
        }
        self.driver.close().await;
        let events = self.events.lock().expect("lock is never poisoned");
        events.clone()
    }
}

/// Requests that look like a list or a document. Their address is only a hint for
/// a person, and loses its query string on the way into a draft.
fn is_interesting(address: &str, kind: Option<&network::ResourceType>) -> bool {
    let relevant_kind = matches!(
        kind,
        Some(
            network::ResourceType::Xhr
                | network::ResourceType::Fetch
                | network::ResourceType::Document
        )
    );
    let lower = address.to_lowercase();
    relevant_kind
        && [
            "invoice", "rechnung", "bill", "beleg", "document", "dokument", "download", ".pdf",
            "order", "bestell",
        ]
        .iter()
        .any(|word| lower.contains(word))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_requests_that_look_like_lists_or_documents_are_noted() {
        let fetch = Some(&network::ResourceType::Fetch);
        assert!(is_interesting("https://portal.test/api/invoices", fetch));
        assert!(is_interesting(
            "https://portal.test/x/a.PDF",
            Some(&network::ResourceType::Document)
        ));
        assert!(!is_interesting("https://portal.test/api/user", fetch));
        assert!(!is_interesting(
            "https://portal.test/invoices.png",
            Some(&network::ResourceType::Image)
        ));
    }

    #[test]
    fn the_listener_never_reads_a_value() {
        assert!(!LISTENER.contains(".value"));
        assert!(!LISTENER.contains(".checked"));
        assert!(!LISTENER.contains("keydown"));
        assert!(!LISTENER.contains("cookie"));
        assert!(!LISTENER.contains("innerHTML"));
    }
}
