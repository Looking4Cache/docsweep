//! The providers: Anthropic, OpenAI and OpenRouter over `reqwest`.
//!
//! Only two things can be sent: fixed instructions and a [`SanitizedPrompt`]. A
//! provider's error body never leaves this file, because it could echo the key
//! or the request. Errors are fixed texts chosen by status code.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, USER_AGENT};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};

use super::prompts::Instructions;
use super::sanitize::SanitizedPrompt;
use crate::config::{LlmConfig, LlmProvider};
use crate::names;
use crate::secrets::{SecretError, SecretStore};

/// Anthropic's most capable generally available model, used unless `llm.model`
/// names another one. The other providers have no default on purpose.
pub const ANTHROPIC_DEFAULT_MODEL: &str = "claude-fable-5-1";

const ANTHROPIC_ENDPOINT: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2026-10-21";
/// Lets the API rerun a request on another model if a safety classifier wrongly
/// declines it; page text can trigger that by accident.
const ANTHROPIC_FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
/// The models for which `fallbacks: "default"` is documented.
const ANTHROPIC_MODELS_WITH_FALLBACK: &[&str] = &[
    "claude-fable-5-1",
    "claude-opus-5-5",
    "claude-opus-5",
    "claude-sonnet-5-5",
];
const OPENAI_ENDPOINT: &str = "https://api.openai.com/v1/chat/completions";
const OPENROUTER_ENDPOINT: &str = "https://openrouter.ai/api/v1/chat/completions";

/// How long one request may take. Long enough for a model that thinks first.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_OUTPUT_TOKENS: u32 = 16_000;
/// A recipe is small. Anything much larger is not an answer.
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

/// Why a provider gave no answer. These texts end up in reports, so they are
/// fixed: no provider text, no address, no key.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LlmError {
    #[error("the provider refused the API key; check it under Settings")]
    WrongKey,
    #[error("the provider's rate limit or quota was reached; try again later")]
    RateLimited,
    #[error("the provider could not be reached; check the network connection")]
    Network,
    #[error("the provider did not answer in time")]
    TimedOut,
    #[error(
        "the provider does not know the model or refused the request (HTTP {0}); check the model name"
    )]
    BadRequest(u16),
    #[error("the provider has a problem right now (HTTP {0}); try again later")]
    Unavailable(u16),
    #[error("the provider answered with HTTP {0}")]
    Unexpected(u16),
    #[error("the provider's answer could not be understood")]
    Malformed,
    #[error("the model declined this request")]
    Refused,
    #[error("the model's answer was cut off")]
    CutOff,
}

/// A language model behind an API. It only ever gets text, and only gives text.
pub trait LlmProviderClient {
    /// Sends the fixed instructions and the cleaned text; returns the answer.
    fn complete<'a>(
        &'a self,
        instructions: Instructions,
        prompt: &'a SanitizedPrompt,
    ) -> Pin<Box<dyn Future<Output = Result<String, LlmError>> + 'a>>;
}

fn http_client() -> Result<reqwest::Client, LlmError> {
    // Same TLS setup as for IMAP. The result is an error once it is installed.
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        // The key must never travel to wherever a redirect points.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| LlmError::Network)
}

fn secret_header(name: &'static str, value: &str) -> Result<(HeaderName, HeaderValue), LlmError> {
    // The error would be about the key's characters, so it says nothing more.
    let mut value = HeaderValue::from_str(value).map_err(|_| LlmError::WrongKey)?;
    value.set_sensitive(true);
    Ok((HeaderName::from_static(name), value))
}

/// Posts JSON and returns the parsed answer, or an error from the status code
/// alone. The body of a failed request is not even read.
async fn post_json(
    client: &reqwest::Client,
    endpoint: &str,
    headers: HeaderMap,
    body: &Value,
) -> Result<Value, LlmError> {
    let body = serde_json::to_vec(body).map_err(|_| LlmError::Malformed)?;
    let response = client
        .post(endpoint)
        .headers(headers)
        .header(CONTENT_TYPE, "application/json")
        .header(USER_AGENT, names::user_agent())
        .body(body)
        .send()
        .await
        .map_err(|error| network_error(&error))?;
    let status = response.status().as_u16();
    match status {
        200..=299 => {}
        401 | 403 => return Err(LlmError::WrongKey),
        402 | 429 => return Err(LlmError::RateLimited),
        400 | 404 | 413 | 422 => return Err(LlmError::BadRequest(status)),
        500..=599 => return Err(LlmError::Unavailable(status)),
        other => return Err(LlmError::Unexpected(other)),
    }
    let bytes = read_limited(response).await?;
    serde_json::from_slice(&bytes).map_err(|_| LlmError::Malformed)
}

async fn read_limited(mut response: reqwest::Response) -> Result<Vec<u8>, LlmError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| network_error(&error))?
    {
        bytes.extend_from_slice(&chunk);
        if bytes.len() > MAX_RESPONSE_BYTES {
            return Err(LlmError::Malformed);
        }
    }
    Ok(bytes)
}

fn network_error(error: &reqwest::Error) -> LlmError {
    if error.is_timeout() {
        LlmError::TimedOut
    } else {
        LlmError::Network
    }
}

/// The Anthropic Messages API.
pub struct Anthropic {
    client: reqwest::Client,
    endpoint: String,
    model: String,
    key: SecretString,
}

impl Anthropic {
    /// Tests point `endpoint` at a local server. The configuration has no way to
    /// do this, so that a file cannot send the key elsewhere.
    pub fn new(endpoint: &str, model: &str, key: SecretString) -> Result<Self, LlmError> {
        Ok(Self {
            client: http_client()?,
            endpoint: endpoint.to_owned(),
            model: model.to_owned(),
            key,
        })
    }

    async fn request(
        &self,
        instructions: Instructions,
        prompt: &SanitizedPrompt,
    ) -> Result<String, LlmError> {
        let (name, value) = secret_header("x-api-key", self.key.expose_secret())?;
        let mut headers = HeaderMap::new();
        headers.insert(name, value);
        headers.insert(
            "anthropic-version",
            HeaderValue::from_static(ANTHROPIC_VERSION),
        );
        let mut body = json!({
            "model": self.model,
            "max_tokens": MAX_OUTPUT_TOKENS,
            "system": instructions.text(),
            "messages": [{ "role": "user", "content": prompt.text() }],
        });
        if ANTHROPIC_MODELS_WITH_FALLBACK.contains(&self.model.as_str()) {
            headers.insert(
                "anthropic-beta",
                HeaderValue::from_static(ANTHROPIC_FALLBACK_BETA),
            );
            body["fallbacks"] = json!("default");
        }
        let answer = post_json(&self.client, &self.endpoint, headers, &body).await?;
        anthropic_text(&answer)
    }
}

impl LlmProviderClient for Anthropic {
    fn complete<'a>(
        &'a self,
        instructions: Instructions,
        prompt: &'a SanitizedPrompt,
    ) -> Pin<Box<dyn Future<Output = Result<String, LlmError>> + 'a>> {
        Box::pin(self.request(instructions, prompt))
    }
}

/// The stop reason comes first: a declined request has an empty or partial
/// answer, and a cut-off one is not a recipe.
fn anthropic_text(answer: &Value) -> Result<String, LlmError> {
    match answer["stop_reason"].as_str() {
        Some("refusal") => return Err(LlmError::Refused),
        Some("max_tokens") => return Err(LlmError::CutOff),
        Some(_) => {}
        None => return Err(LlmError::Malformed),
    }
    let blocks = answer["content"].as_array().ok_or(LlmError::Malformed)?;
    let text: String = blocks
        .iter()
        .filter(|block| block["type"] == "text")
        .filter_map(|block| block["text"].as_str())
        .collect();
    if text.trim().is_empty() {
        return Err(LlmError::Malformed);
    }
    Ok(text)
}

/// The Chat Completions API, which OpenAI and OpenRouter share.
pub struct OpenAiCompatible {
    client: reqwest::Client,
    endpoint: String,
    model: String,
    key: SecretString,
}

impl OpenAiCompatible {
    /// See [`Anthropic::new`] about `endpoint`.
    pub fn new(endpoint: &str, model: &str, key: SecretString) -> Result<Self, LlmError> {
        Ok(Self {
            client: http_client()?,
            endpoint: endpoint.to_owned(),
            model: model.to_owned(),
            key,
        })
    }

    async fn request(
        &self,
        instructions: Instructions,
        prompt: &SanitizedPrompt,
    ) -> Result<String, LlmError> {
        let bearer = format!("Bearer {}", self.key.expose_secret());
        let (name, value) = secret_header("authorization", &bearer)?;
        let mut headers = HeaderMap::new();
        headers.insert(name, value);
        let body = json!({
            "model": self.model,
            "messages": [
                { "role": "system", "content": instructions.text() },
                { "role": "user", "content": prompt.text() },
            ],
        });
        let answer = post_json(&self.client, &self.endpoint, headers, &body).await?;
        chat_text(&answer)
    }
}

impl LlmProviderClient for OpenAiCompatible {
    fn complete<'a>(
        &'a self,
        instructions: Instructions,
        prompt: &'a SanitizedPrompt,
    ) -> Pin<Box<dyn Future<Output = Result<String, LlmError>> + 'a>> {
        Box::pin(self.request(instructions, prompt))
    }
}

fn chat_text(answer: &Value) -> Result<String, LlmError> {
    let choice = &answer["choices"][0];
    match choice["finish_reason"].as_str() {
        Some("length") => return Err(LlmError::CutOff),
        Some("content_filter") => return Err(LlmError::Refused),
        _ => {}
    }
    if choice["message"]["refusal"].is_string() {
        return Err(LlmError::Refused);
    }
    choice["message"]["content"]
        .as_str()
        .filter(|text| !text.trim().is_empty())
        .map(str::to_owned)
        .ok_or(LlmError::Malformed)
}

/// A provider that is ready to use, as the configuration says.
pub struct Ai {
    pub client: Box<dyn LlmProviderClient>,
    /// Shown to people, and the key under which their consent is remembered.
    pub label: String,
    /// Values that must never be part of a prompt.
    pub redact: Vec<SecretString>,
}

impl std::fmt::Debug for Ai {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ai").field("label", &self.label).finish()
    }
}

/// Why the configured provider cannot be used.
#[derive(Debug, thiserror::Error)]
pub enum AiSetupError {
    #[error("`llm.api_key` is not a valid `keyring:<id>` reference")]
    InvalidReference,
    #[error(transparent)]
    Secret(#[from] SecretError),
    #[error(transparent)]
    Provider(#[from] LlmError),
    #[error("`llm.model` is required for this provider")]
    NoModel,
}

/// Reads the key and builds the configured provider.
pub fn from_config(llm: &LlmConfig, secrets: &dyn SecretStore) -> Result<Ai, AiSetupError> {
    let id = llm.api_key.id().ok_or(AiSetupError::InvalidReference)?;
    let key = secrets.get(id)?;
    let model = llm.model.as_deref();
    let redact = vec![key.clone()];
    let (client, label): (Box<dyn LlmProviderClient>, &str) = match llm.provider {
        LlmProvider::Anthropic => (
            Box::new(Anthropic::new(
                ANTHROPIC_ENDPOINT,
                model.unwrap_or(ANTHROPIC_DEFAULT_MODEL),
                key,
            )?),
            "Anthropic",
        ),
        LlmProvider::Openai => (
            Box::new(OpenAiCompatible::new(
                OPENAI_ENDPOINT,
                model.ok_or(AiSetupError::NoModel)?,
                key,
            )?),
            "OpenAI",
        ),
        LlmProvider::Openrouter => (
            Box::new(OpenAiCompatible::new(
                OPENROUTER_ENDPOINT,
                model.ok_or(AiSetupError::NoModel)?,
                key,
            )?),
            "OpenRouter",
        ),
    };
    Ok(Ai {
        client,
        label: label.to_owned(),
        redact,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::sanitize::{RawPrompt, sanitize};
    use crate::ai::test_support::FakeProviderServer;
    use crate::config::SecretRef;
    use crate::secrets::MemoryStore;

    const KEY: &str = "sk-fake-provider-key";

    fn prompt() -> SanitizedPrompt {
        sanitize(&RawPrompt {
            company: "Acme",
            ..RawPrompt::default()
        })
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    fn anthropic(server: &FakeProviderServer, model: &str) -> Anthropic {
        Anthropic::new(&server.url(), model, SecretString::from(KEY)).unwrap()
    }

    fn chat(server: &FakeProviderServer) -> OpenAiCompatible {
        OpenAiCompatible::new(&server.url(), "some-model", SecretString::from(KEY)).unwrap()
    }

    #[test]
    fn anthropic_requests_have_the_documented_form() {
        let server = FakeProviderServer::answering_anthropic("steps: []");
        let answer = block_on(
            anthropic(&server, ANTHROPIC_DEFAULT_MODEL).complete(Instructions::Generate, &prompt()),
        );
        assert_eq!(answer.unwrap(), "steps: []");

        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(request.header("x-api-key").as_deref(), Some(KEY));
        assert_eq!(
            request.header("anthropic-version").as_deref(),
            Some(ANTHROPIC_VERSION)
        );
        assert_eq!(request.header("user-agent"), Some(names::user_agent()));
        let body: Value = serde_json::from_str(&request.body).unwrap();
        assert_eq!(body["model"], ANTHROPIC_DEFAULT_MODEL);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], prompt().text());
        assert_eq!(body["system"], Instructions::Generate.text());
        assert_eq!(body["fallbacks"], "default");
        assert!(!request.body.contains(KEY), "the key stays in its header");
    }

    #[test]
    fn a_model_without_documented_fallbacks_is_asked_without_them() {
        let server = FakeProviderServer::answering_anthropic("x");
        block_on(anthropic(&server, "claude-haiku-5-5").complete(Instructions::Repair, &prompt()))
            .unwrap();
        let request = &server.requests()[0];
        assert_eq!(request.header("anthropic-beta"), None);
        let body: Value = serde_json::from_str(&request.body).unwrap();
        assert!(body.get("fallbacks").is_none());
    }

    #[test]
    fn chat_requests_have_the_documented_form() {
        let server = FakeProviderServer::answering_chat("steps: []");
        let answer = block_on(chat(&server).complete(Instructions::Repair, &prompt()));
        assert_eq!(answer.unwrap(), "steps: []");

        let requests = server.requests();
        let request = &requests[0];
        assert_eq!(
            request.header("authorization"),
            Some(format!("Bearer {KEY}"))
        );
        let body: Value = serde_json::from_str(&request.body).unwrap();
        assert_eq!(body["model"], "some-model");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["content"], prompt().text());
    }

    #[test]
    fn status_codes_become_understandable_errors_without_the_body() {
        for (status, expected) in [
            (401, LlmError::WrongKey),
            (403, LlmError::WrongKey),
            (429, LlmError::RateLimited),
            (404, LlmError::BadRequest(404)),
            (529, LlmError::Unavailable(529)),
            (302, LlmError::Unexpected(302)),
        ] {
            let body = format!(r#"{{"error":"bad key {KEY}"}}"#);
            let server = FakeProviderServer::with_status(status, &body);
            let error =
                block_on(chat(&server).complete(Instructions::Repair, &prompt())).unwrap_err();
            assert_eq!(error, expected);
            assert!(!error.to_string().contains(KEY));
            assert!(!format!("{error:?}").contains(KEY));
        }
    }

    #[test]
    fn a_redirect_does_not_take_the_key_along() {
        let target = FakeProviderServer::answering_chat("x");
        let server = FakeProviderServer::redirecting_to(&target.url());
        let error = block_on(chat(&server).complete(Instructions::Repair, &prompt())).unwrap_err();
        assert_eq!(error, LlmError::Unexpected(307));
        assert!(target.requests().is_empty());
    }

    #[test]
    fn a_network_failure_is_named_without_the_address() {
        let client = OpenAiCompatible::new(
            "http://127.0.0.1:9/nothing-here",
            "m",
            SecretString::from(KEY),
        )
        .unwrap();
        let error = block_on(client.complete(Instructions::Repair, &prompt())).unwrap_err();
        assert_eq!(error, LlmError::Network);
        assert!(!error.to_string().contains("127.0.0.1"));
    }

    #[test]
    fn refusals_cut_offs_and_nonsense_are_not_answers() {
        for (answer, expected) in [
            (
                r#"{"stop_reason":"refusal","content":[]}"#,
                LlmError::Refused,
            ),
            (
                r#"{"stop_reason":"max_tokens","content":[{"type":"text","text":"steps:"}]}"#,
                LlmError::CutOff,
            ),
            (
                r#"{"stop_reason":"end_turn","content":[{"type":"thinking","thinking":""}]}"#,
                LlmError::Malformed,
            ),
            (r#"{"unexpected":true}"#, LlmError::Malformed),
            ("not json", LlmError::Malformed),
        ] {
            let server = FakeProviderServer::with_status(200, answer);
            let result = block_on(
                anthropic(&server, ANTHROPIC_DEFAULT_MODEL)
                    .complete(Instructions::Repair, &prompt()),
            );
            assert_eq!(result.unwrap_err(), expected, "{answer}");
        }
        for (answer, expected) in [
            (
                r#"{"choices":[{"finish_reason":"length","message":{"content":"x"}}]}"#,
                LlmError::CutOff,
            ),
            (
                r#"{"choices":[{"finish_reason":"content_filter","message":{"content":""}}]}"#,
                LlmError::Refused,
            ),
            (
                r#"{"choices":[{"message":{"refusal":"no","content":null}}]}"#,
                LlmError::Refused,
            ),
            (r#"{"choices":[]}"#, LlmError::Malformed),
        ] {
            let server = FakeProviderServer::with_status(200, answer);
            let result = block_on(chat(&server).complete(Instructions::Repair, &prompt()));
            assert_eq!(result.unwrap_err(), expected, "{answer}");
        }
    }

    #[test]
    fn openai_and_openrouter_need_a_model_and_a_stored_key() {
        let llm = |provider, model: Option<&str>| LlmConfig {
            provider,
            api_key: SecretRef::from_id("llm-key"),
            model: model.map(str::to_owned),
        };
        let store = MemoryStore::with("llm-key", KEY);

        assert_eq!(
            from_config(&llm(LlmProvider::Anthropic, None), &store)
                .unwrap()
                .label,
            "Anthropic"
        );
        for provider in [LlmProvider::Openai, LlmProvider::Openrouter] {
            let missing = from_config(&llm(provider, None), &store).unwrap_err();
            assert!(matches!(missing, AiSetupError::NoModel));
            assert!(from_config(&llm(provider, Some("m")), &store).is_ok());
        }

        let empty =
            from_config(&llm(LlmProvider::Anthropic, None), &MemoryStore::default()).unwrap_err();
        assert!(empty.to_string().contains("llm-key"), "{empty}");
    }
}
