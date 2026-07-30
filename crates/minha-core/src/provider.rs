//! Direct ChatGPT Responses API access for Minha.
//!
//! This module deliberately speaks the HTTP/SSE protocol itself. It does not depend on an
//! installed Codex binary or on Codex's Rust crates.

use crate::usage::{CreditsSnapshot, RateLimitSnapshot, RateLimitWindow, TokenUsage};
use futures_util::StreamExt;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use thiserror::Error;

const ORIGINATOR: &str = "minha";
const USER_AGENT_VALUE: &str = "minha/0.1";
pub const CODEX_COMPAT_CLIENT_VERSION: &str = "0.145.0";
pub const DEEPSEEK_BASE_URL: &str = "https://api.deepseek.com";
const MAX_PROVIDER_ERROR_BYTES: usize = 4 * 1024;
const MAX_GET_ATTEMPTS: usize = 3;
const MODEL_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const RESPONSE_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// A model returned by the entitlement endpoint. `metadata` retains provider additions without
/// making the model-facing schema depend on a particular server revision.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ModelDescriptor {
    pub slug: String,
    #[serde(flatten)]
    pub metadata: Map<String, Value>,
}

impl ModelDescriptor {
    pub fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            context_window: self.metadata.get("context_window").and_then(Value::as_u64),
            maximum_output: self.metadata.get("max_output_tokens").and_then(Value::as_u64),
            minimal_client_version: self
                .metadata
                .get("minimal_client_version")
                .and_then(Value::as_str)
                .map(str::to_owned),
            supports_parallel_tool_calls: self
                .metadata
                .get("supports_parallel_tool_calls")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            supports_verbosity: self
                .metadata
                .get("support_verbosity")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            reasoning_efforts: self
                .metadata
                .get("supported_reasoning_levels")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|level| level.get("effort").and_then(Value::as_str))
                .map(str::to_owned)
                .collect(),
            supports_tools: self
                .metadata
                .get("supports_tool_calls")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            supports_streaming: true,
            supports_cache_telemetry: true,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ModelCapabilities {
    pub context_window: Option<u64>,
    pub maximum_output: Option<u64>,
    pub minimal_client_version: Option<String>,
    pub supports_parallel_tool_calls: bool,
    pub supports_verbosity: bool,
    pub reasoning_efforts: Vec<String>,
    pub supports_tools: bool,
    pub supports_streaming: bool,
    pub supports_cache_telemetry: bool,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderId {
    ChatGptCodex,
    DeepSeek,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ProviderBalanceV1 {
    pub schema_version: u16,
    pub provider: ProviderId,
    pub is_available: bool,
    pub balances: Vec<ProviderBalanceAmountV1>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ProviderBalanceAmountV1 {
    pub currency: String,
    pub total: String,
    pub granted: String,
    pub topped_up: String,
}

impl ProviderBalanceV1 {
    pub fn total(&self, currency: &str) -> Option<f64> {
        self.balances
            .iter()
            .find(|balance| balance.currency.eq_ignore_ascii_case(currency))?
            .total
            .parse()
            .ok()
    }
}

pub trait ProviderClient: Send + Sync {
    fn provider_id(&self) -> ProviderId;
    fn discover_models(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<ModelCatalog, ProviderError>> + Send + '_>>;
    fn complete_turn(
        &self,
        request: TurnRequest,
    ) -> Pin<Box<dyn Future<Output = Result<TurnResult, ProviderError>> + Send + '_>>;
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Deserialize, Serialize)]
pub struct ModelRef {
    pub provider: ProviderId,
    pub slug: String,
}

impl ModelRef {
    pub fn parse(value: &str) -> Self {
        if let Some(slug) = value.strip_prefix("deepseek/") {
            Self {
                provider: ProviderId::DeepSeek,
                slug: slug.to_owned(),
            }
        } else if let Some(slug) = value.strip_prefix("chatgpt/") {
            Self {
                provider: ProviderId::ChatGptCodex,
                slug: slug.to_owned(),
            }
        } else if value.starts_with("deepseek-") {
            Self {
                provider: ProviderId::DeepSeek,
                slug: value.to_owned(),
            }
        } else {
            Self {
                provider: ProviderId::ChatGptCodex,
                slug: value.to_owned(),
            }
        }
    }
}

pub fn fallback_capabilities(model: &ModelRef) -> ModelCapabilities {
    let deepseek = model.provider == ProviderId::DeepSeek;
    ModelCapabilities {
        context_window: Some(if deepseek {
            1_048_576
        } else if model.slug.contains("spark") {
            128_000
        } else {
            272_000
        }),
        maximum_output: deepseek.then_some(393_216),
        supports_parallel_tool_calls: true,
        supports_verbosity: model.provider == ProviderId::ChatGptCodex,
        reasoning_efforts: if deepseek {
            vec!["high".into(), "max".into()]
        } else {
            Vec::new()
        },
        supports_tools: true,
        supports_streaming: true,
        supports_cache_telemetry: true,
        minimal_client_version: None,
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct ModelCatalog {
    pub models: Vec<ModelDescriptor>,
    pub etag: Option<String>,
    pub not_modified: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TurnRequest {
    pub model: String,
    pub instructions: String,
    pub input: Vec<Value>,
    pub tools: Vec<Value>,
    pub parallel_tool_calls: bool,
    pub reasoning_effort: Option<String>,
    pub prompt_cache_key: Option<String>,
    pub subagent_label: Option<String>,
    pub response_format: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCall {
    pub call_id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TurnResult {
    pub output_text: String,
    pub reasoning_text: String,
    pub output_items: Vec<Value>,
    pub tool_calls: Vec<ToolCall>,
    pub usage: TokenUsage,
    pub response_id: Option<String>,
    pub server_model: Option<String>,
    pub finish_reason: Option<String>,
    pub rate_limits: Vec<RateLimitSnapshot>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ProviderStreamEvent {
    TextDelta(String),
    ReasoningDelta(String),
    OutputItem(Value),
    Completed,
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("provider request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("provider returned HTTP status {status}: {detail}")]
    Http {
        status: reqwest::StatusCode,
        detail: String,
    },
    #[error("provider returned invalid JSON: {0}")]
    Json(#[source] serde_json::Error),
    #[error("provider returned an invalid SSE event: {0}")]
    Sse(#[source] serde_json::Error),
    #[error("provider returned an invalid response: {0}")]
    InvalidResponse(&'static str),
    #[error("provider stream ended before a response.completed event")]
    IncompleteStream,
    #[error("invalid provider header value")]
    Header,
    #[error("provider error event: {0}")]
    RemoteError(String),
}

/// A small, secret-safe HTTP client for ChatGPT's Responses API.
#[derive(Clone)]
pub struct ChatGptClient {
    http: reqwest::Client,
    base_url: String,
    access_token: SecretString,
    account_id: String,
    capabilities: Arc<RwLock<HashMap<String, ModelCapabilities>>>,
}

impl fmt::Debug for ChatGptClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChatGptClient")
            .field("base_url", &self.base_url)
            .field("access_token", &"[REDACTED]")
            .field("account_id", &"[REDACTED]")
            .finish()
    }
}

impl ChatGptClient {
    pub fn new<B, T, A>(base_url: B, access_token: T, account_id: A) -> Self
    where
        B: Into<String>,
        T: Into<SecretString>,
        A: Into<String>,
    {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            access_token: access_token.into(),
            account_id: account_id.into(),
            capabilities: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn list_models(&self) -> Result<Vec<ModelDescriptor>, ProviderError> {
        Ok(self.fetch_models(None).await?.models)
    }

    pub fn install_model_catalog(&self, models: &[ModelDescriptor]) {
        if let Ok(mut capabilities) = self.capabilities.write() {
            capabilities.clear();
            capabilities.extend(
                models
                    .iter()
                    .map(|model| (model.slug.clone(), model.capabilities())),
            );
        }
    }

    pub async fn fetch_models(&self, etag: Option<&str>) -> Result<ModelCatalog, ProviderError> {
        let endpoint = format!("models?client_version={CODEX_COMPAT_CLIENT_VERSION}");
        let response = self
            .send_get_with_retry(|| {
                let mut request = self.request(reqwest::Method::GET, &endpoint, None, None)?;
                if let Some(etag) = etag {
                    request = request.header(reqwest::header::IF_NONE_MATCH, etag);
                }
                Ok(request)
            })
            .await?;
        if response.status() == reqwest::StatusCode::NOT_MODIFIED {
            return Ok(ModelCatalog {
                etag: response
                    .headers()
                    .get(reqwest::header::ETAG)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned),
                not_modified: true,
                ..ModelCatalog::default()
            });
        }
        if !response.status().is_success() {
            return Err(http_error(response).await);
        }
        let etag = response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let bytes = response.bytes().await?;
        let raw: Value = serde_json::from_slice(&bytes).map_err(ProviderError::Json)?;
        let models = parse_models(raw)?;
        self.install_model_catalog(&models);
        Ok(ModelCatalog {
            models,
            etag,
            not_modified: false,
        })
    }

    pub async fn turn(&self, request: TurnRequest) -> Result<TurnResult, ProviderError> {
        self.turn_stream(request, |_| {}).await
    }

    pub async fn turn_stream<F>(
        &self,
        request: TurnRequest,
        mut on_event: F,
    ) -> Result<TurnResult, ProviderError>
    where
        F: FnMut(ProviderStreamEvent),
    {
        let capabilities = self
            .capabilities
            .read()
            .ok()
            .and_then(|catalog| catalog.get(&request.model).cloned());
        let body = build_request(&request, capabilities.as_ref());
        // Responses are not retried automatically: after a transport or 5xx
        // failure the provider may already have accepted and billed the turn.
        // A durable run can be resumed explicitly without duplicating a POST.
        let response = self
            .request(
                reqwest::Method::POST,
                "responses",
                Some(&body),
                request.subagent_label.as_deref(),
            )?
            .header(ACCEPT, "text/event-stream")
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(http_error(response).await);
        }

        let mut result = TurnResult {
            server_model: response
                .headers()
                .get("OpenAI-Model")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned),
            rate_limits: parse_rate_limits(response.headers()),
            ..TurnResult::default()
        };
        let mut stream = response.bytes_stream();
        let mut parser = SseParser::default();
        while let Some(chunk) = stream.next().await {
            for event in parser.push(&chunk.map_err(ProviderError::Request)?, &mut result)? {
                on_event(event);
            }
        }
        for event in parser.finish(&mut result)? {
            on_event(event);
        }
        if parser.completed {
            if result.output_text.is_empty() {
                result.output_text = output_text_from_items(&result.output_items);
            }
            Ok(result)
        } else {
            Err(ProviderError::IncompleteStream)
        }
    }

    fn request(
        &self,
        method: reqwest::Method,
        endpoint: &str,
        body: Option<&Value>,
        subagent: Option<&str>,
    ) -> Result<reqwest::RequestBuilder, ProviderError> {
        let url = format!("{}/{}", self.base_url, endpoint);
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, bearer_header(&self.access_token)?);
        headers.insert(
            "ChatGPT-Account-ID",
            HeaderValue::from_str(&self.account_id).map_err(|_| ProviderError::Header)?,
        );
        headers.insert("originator", HeaderValue::from_static(ORIGINATOR));
        headers.insert(USER_AGENT, HeaderValue::from_static(USER_AGENT_VALUE));
        if let Some(label) = subagent {
            headers.insert(
                "x-openai-subagent",
                HeaderValue::from_str(label).map_err(|_| ProviderError::Header)?,
            );
        }
        let timeout = if endpoint.starts_with("models?") {
            MODEL_REQUEST_TIMEOUT
        } else {
            RESPONSE_REQUEST_TIMEOUT
        };
        let request = self.http.request(method, url).headers(headers).timeout(timeout);
        Ok(match body {
            Some(body) => request.header(CONTENT_TYPE, "application/json").json(body),
            None => request,
        })
    }

    async fn send_get_with_retry<F>(&self, mut make_request: F) -> Result<reqwest::Response, ProviderError>
    where
        F: FnMut() -> Result<reqwest::RequestBuilder, ProviderError>,
    {
        for attempt in 0..MAX_GET_ATTEMPTS {
            match make_request()?.send().await {
                Ok(response) => {
                    let status = response.status();
                    if attempt + 1 < MAX_GET_ATTEMPTS
                        && (status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error())
                    {
                        let delay = retry_after_delay(&response).unwrap_or_else(|| retry_delay(attempt));
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    return Ok(response);
                }
                Err(error)
                    if attempt + 1 < MAX_GET_ATTEMPTS
                        && (error.is_timeout() || error.is_connect() || error.is_request()) =>
                {
                    tokio::time::sleep(retry_delay(attempt)).await;
                }
                Err(error) => return Err(ProviderError::Request(error)),
            }
        }
        Err(ProviderError::InvalidResponse("retry loop made no attempts"))
    }
}

impl ProviderClient for ChatGptClient {
    fn provider_id(&self) -> ProviderId {
        ProviderId::ChatGptCodex
    }

    fn discover_models(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<ModelCatalog, ProviderError>> + Send + '_>> {
        Box::pin(self.fetch_models(None))
    }

    fn complete_turn(
        &self,
        request: TurnRequest,
    ) -> Pin<Box<dyn Future<Output = Result<TurnResult, ProviderError>> + Send + '_>> {
        Box::pin(self.turn(request))
    }
}

fn bearer_header(token: &SecretString) -> Result<HeaderValue, ProviderError> {
    HeaderValue::from_str(&format!("Bearer {}", token.expose_secret())).map_err(|_| ProviderError::Header)
}

fn build_request(request: &TurnRequest, capabilities: Option<&ModelCapabilities>) -> Value {
    let parallel_tool_calls = capabilities.map_or(request.parallel_tool_calls, |capabilities| {
        request.parallel_tool_calls && capabilities.supports_parallel_tool_calls
    });
    let mut object = Map::new();
    object.insert("model".into(), Value::String(request.model.clone()));
    object.insert("instructions".into(), Value::String(request.instructions.clone()));
    object.insert("input".into(), Value::Array(request.input.clone()));
    object.insert("tools".into(), Value::Array(request.tools.clone()));
    object.insert("tool_choice".into(), Value::String("auto".into()));
    object.insert("parallel_tool_calls".into(), Value::Bool(parallel_tool_calls));
    object.insert("store".into(), Value::Bool(false));
    object.insert("stream".into(), Value::Bool(true));
    object.insert(
        "include".into(),
        Value::Array(vec![Value::String("reasoning.encrypted_content".into())]),
    );
    if capabilities.is_none_or(|capabilities| capabilities.supports_verbosity) {
        object.insert("text".into(), serde_json::json!({"verbosity": "low"}));
    }
    if let Some(effort) = &request.reasoning_effort
        && capabilities.is_none_or(|capabilities| {
            capabilities.reasoning_efforts.is_empty() || capabilities.reasoning_efforts.contains(effort)
        })
    {
        object.insert("reasoning".into(), serde_json::json!({"effort": effort}));
    }
    if let Some(key) = &request.prompt_cache_key {
        object.insert("prompt_cache_key".into(), Value::String(key.clone()));
    }
    Value::Object(object)
}

async fn http_error(response: reqwest::Response) -> ProviderError {
    let status = response.status();
    let request_id = response
        .headers()
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let bytes = response.bytes().await.unwrap_or_default();
    let bytes = &bytes[..bytes.len().min(MAX_PROVIDER_ERROR_BYTES)];
    let raw = String::from_utf8_lossy(bytes);
    let parsed = serde_json::from_slice::<Value>(bytes).ok();
    let detail = parsed
        .as_ref()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .or_else(|| value.get("detail"))
                .or_else(|| value.get("message"))
                .and_then(Value::as_str)
        })
        .unwrap_or(raw.trim());
    let mut detail = redact_provider_detail(detail);
    if let Some(request_id) = request_id {
        detail.push_str(" [request ");
        detail.push_str(&request_id);
        detail.push(']');
    }
    if detail.is_empty() {
        detail = "provider returned no error details".into();
    }
    ProviderError::Http { status, detail }
}

fn redact_provider_detail(detail: &str) -> String {
    let one_line = detail.split_whitespace().collect::<Vec<_>>().join(" ");
    let lowercase = one_line.to_ascii_lowercase();
    if [
        "access_token",
        "refresh_token",
        "authorization",
        "cookie",
        "account_id",
        "account-id",
        "email",
    ]
    .iter()
    .any(|marker| lowercase.contains(marker))
    {
        return "provider diagnostic contained sensitive fields and was redacted".into();
    }
    let mut redacted = one_line;
    for marker in ["Bearer ", "access_token=", "refresh_token="] {
        while let Some(start) = redacted.find(marker) {
            let value_start = start + marker.len();
            let value_end = redacted[value_start..]
                .find(|character: char| character.is_whitespace() || character == '&')
                .map_or(redacted.len(), |end| value_start + end);
            redacted.replace_range(value_start..value_end, "[REDACTED]");
        }
    }
    if redacted.len() > 1_000 {
        let mut end = 1_000;
        while !redacted.is_char_boundary(end) {
            end -= 1;
        }
        redacted.truncate(end);
        redacted.push('…');
    }
    redacted
}

fn retry_delay(attempt: usize) -> Duration {
    let base = 150_u64.saturating_mul(1_u64 << attempt.min(4));
    let jitter = rand::random_range(0..=100_u64);
    Duration::from_millis(base + jitter)
}

fn retry_after_delay(response: &reqwest::Response) -> Option<Duration> {
    let seconds = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()?;
    Some(Duration::from_secs(seconds.min(30)))
}

fn parse_models(raw: Value) -> Result<Vec<ModelDescriptor>, ProviderError> {
    let models = raw
        .get("models")
        .cloned()
        .or_else(|| raw.get("data").cloned())
        .unwrap_or(raw);
    let entries = models
        .as_array()
        .ok_or(ProviderError::InvalidResponse("models must be an array"))?;
    entries
        .iter()
        .map(|entry| match entry {
            Value::String(slug) => Ok(ModelDescriptor {
                slug: slug.clone(),
                metadata: Map::new(),
            }),
            Value::Object(object) => {
                let slug = object
                    .get("slug")
                    .or_else(|| object.get("id"))
                    .and_then(Value::as_str)
                    .ok_or(ProviderError::InvalidResponse("model is missing slug"))?;
                let mut metadata = object.clone();
                metadata.remove("slug");
                Ok(ModelDescriptor {
                    slug: slug.to_owned(),
                    metadata,
                })
            }
            _ => Err(ProviderError::InvalidResponse(
                "model must be an object or string",
            )),
        })
        .collect()
}

#[derive(Default)]
struct SseParser {
    buffer: Vec<u8>,
    completed: bool,
}

impl SseParser {
    fn push(
        &mut self,
        bytes: &[u8],
        result: &mut TurnResult,
    ) -> Result<Vec<ProviderStreamEvent>, ProviderError> {
        let mut events = Vec::new();
        self.buffer.extend_from_slice(bytes);
        while let Some(end) = frame_end(&self.buffer) {
            let frame = self.buffer.drain(..end).collect::<Vec<_>>();
            let frame = String::from_utf8_lossy(&frame);
            if let Some(data) = sse_data(&frame)
                && let Some(event) = self.event(&data, result)?
            {
                events.push(event);
            }
        }
        Ok(events)
    }

    fn finish(&mut self, result: &mut TurnResult) -> Result<Vec<ProviderStreamEvent>, ProviderError> {
        let mut events = Vec::new();
        if !self.buffer.iter().all(|byte| byte.is_ascii_whitespace()) {
            let frame = String::from_utf8_lossy(&self.buffer).into_owned();
            if let Some(data) = sse_data(&frame)
                && let Some(event) = self.event(&data, result)?
            {
                events.push(event);
            }
        }
        Ok(events)
    }

    fn event(
        &mut self,
        data: &str,
        result: &mut TurnResult,
    ) -> Result<Option<ProviderStreamEvent>, ProviderError> {
        if data.trim() == "[DONE]" {
            return Ok(None);
        }
        let value: Value = serde_json::from_str(data).map_err(ProviderError::Sse)?;
        let kind = value.get("type").and_then(Value::as_str).unwrap_or_default();
        match kind {
            "response.output_text.delta" => {
                if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                    result.output_text.push_str(delta);
                    return Ok(Some(ProviderStreamEvent::TextDelta(delta.to_owned())));
                }
            }
            "response.output_item.done" => {
                if let Some(item) = value.get("item") {
                    result.output_items.push(item.clone());
                    if item.get("type").and_then(Value::as_str) == Some("function_call") {
                        result.tool_calls.push(ToolCall {
                            call_id: item
                                .get("call_id")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                            name: item
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                            arguments: item
                                .get("arguments")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                        });
                    }
                    return Ok(Some(ProviderStreamEvent::OutputItem(item.clone())));
                }
            }
            "response.created" | "response.in_progress" => self.capture_response(&value, result),
            "response.completed" | "response.done" => {
                self.capture_response(&value, result);
                self.completed = true;
                return Ok(Some(ProviderStreamEvent::Completed));
            }
            "error" | "response.error" | "response.failed" => {
                let detail = value
                    .pointer("/error/message")
                    .or_else(|| value.get("message"))
                    .and_then(Value::as_str)
                    .map(redact_provider_detail)
                    .unwrap_or_else(|| "provider returned an unspecified stream error".into());
                return Err(ProviderError::RemoteError(detail));
            }
            _ => {}
        }
        Ok(None)
    }

    fn capture_response(&self, event: &Value, result: &mut TurnResult) {
        let response = event.get("response").unwrap_or(event);
        result.response_id = response
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or(result.response_id.take());
        result.server_model = response
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or(result.server_model.take());
        if let Some(usage) = response.get("usage") {
            result.usage.input = usage
                .get("input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(result.usage.input);
            result.usage.output = usage
                .get("output_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(result.usage.output);
            result.usage.cached_input = usage
                .pointer("/input_tokens_details/cached_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(result.usage.cached_input);
            result.usage.cache_write = usage
                .get("cache_write_tokens")
                .or_else(|| usage.pointer("/input_tokens_details/cache_write_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(result.usage.cache_write);
            result.usage.reasoning_output = usage
                .pointer("/output_tokens_details/reasoning_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(result.usage.reasoning_output);
        }
        if let Some(output) = response.get("output").and_then(Value::as_array) {
            for item in output {
                let duplicate = result.output_items.iter().any(|existing| {
                    let same_id = item.get("id").is_some() && item.get("id") == existing.get("id");
                    let same_call = item.get("call_id").is_some()
                        && item.get("call_id") == existing.get("call_id")
                        && item.get("type") == existing.get("type");
                    same_id || same_call
                });
                if !duplicate {
                    result.output_items.push(item.clone());
                }
                if item.get("type").and_then(Value::as_str) == Some("function_call")
                    && let Some(call_id) = item.get("call_id").and_then(Value::as_str)
                    && !result.tool_calls.iter().any(|call| call.call_id == call_id)
                {
                    result.tool_calls.push(ToolCall {
                        call_id: call_id.to_owned(),
                        name: item
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        arguments: item
                            .get("arguments")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                    });
                }
            }
        }
    }
}

fn frame_end(buffer: &[u8]) -> Option<usize> {
    let crlf = buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4);
    let lf = buffer
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|index| index + 2);
    match (crlf, lf) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(end), None) | (None, Some(end)) => Some(end),
        (None, None) => None,
    }
}

fn output_text_from_items(items: &[Value]) -> String {
    items
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("message"))
        .filter_map(|item| item.get("content").and_then(Value::as_array))
        .flatten()
        .filter(|content| content.get("type").and_then(Value::as_str) == Some("output_text"))
        .filter_map(|content| content.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("")
}

fn parse_rate_limits(headers: &HeaderMap) -> Vec<RateLimitSnapshot> {
    let mut ids = headers
        .keys()
        .filter_map(|name| {
            name.as_str()
                .strip_prefix("x-")?
                .strip_suffix("-primary-used-percent")
                .map(str::to_owned)
        })
        .collect::<Vec<_>>();
    if headers.contains_key("x-codex-primary-used-percent")
        || headers.contains_key("x-codex-credits-has-credits")
    {
        ids.push("codex".into());
    }
    ids.sort();
    ids.dedup();
    ids.into_iter()
        .filter_map(|id| {
            let prefix = format!("x-{id}");
            let primary = rate_window(headers, &prefix, "primary");
            let secondary = rate_window(headers, &prefix, "secondary");
            let credits = if id == "codex" {
                match (
                    header_bool(headers, "x-codex-credits-has-credits"),
                    header_bool(headers, "x-codex-credits-unlimited"),
                ) {
                    (Some(has_credits), Some(unlimited)) => Some(CreditsSnapshot {
                        has_credits,
                        unlimited,
                        balance: header_str(headers, "x-codex-credits-balance").map(str::to_owned),
                    }),
                    _ => None,
                }
            } else {
                None
            };
            if primary.is_none() && secondary.is_none() && credits.is_none() {
                return None;
            }
            Some(RateLimitSnapshot {
                limit_id: id.replace('-', "_"),
                limit_name: header_str(headers, &format!("{prefix}-limit-name")).map(str::to_owned),
                primary,
                secondary,
                credits,
            })
        })
        .collect()
}

fn rate_window(headers: &HeaderMap, prefix: &str, window: &str) -> Option<RateLimitWindow> {
    let used_percent = header_str(headers, &format!("{prefix}-{window}-used-percent"))?
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())?;
    Some(RateLimitWindow {
        used_percent,
        window_minutes: header_str(headers, &format!("{prefix}-{window}-window-minutes"))
            .and_then(|value| value.parse().ok()),
        resets_at: header_str(headers, &format!("{prefix}-{window}-reset-at"))
            .and_then(|value| value.parse().ok()),
    })
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)?
        .to_str()
        .ok()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn header_bool(headers: &HeaderMap, name: &str) -> Option<bool> {
    match header_str(headers, name)? {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
    }
}

fn sse_data(frame: &str) -> Option<String> {
    let mut data = Vec::new();
    for line in frame.lines() {
        if let Some(value) = line.strip_prefix("data:") {
            data.push(value.strip_prefix(' ').unwrap_or(value));
        }
    }
    (!data.is_empty()).then(|| data.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};
    use std::thread;

    struct ResponseSpec {
        status: u16,
        headers: Vec<(&'static str, &'static str)>,
        body: Vec<u8>,
        split_body: bool,
    }

    struct TestServer {
        base_url: String,
        requests: Arc<Mutex<Vec<String>>>,
        join: thread::JoinHandle<io::Result<()>>,
    }

    impl TestServer {
        fn start(responses: Vec<ResponseSpec>) -> io::Result<Self> {
            let listener = TcpListener::bind(("127.0.0.1", 0))?;
            let address = listener.local_addr()?;
            let requests = Arc::new(Mutex::new(Vec::new()));
            let recorded_requests = Arc::clone(&requests);
            let join = thread::spawn(move || {
                for response in responses {
                    let (mut stream, _) = listener.accept()?;
                    let request = read_http_request(&mut stream)?;
                    if let Ok(mut requests) = recorded_requests.lock() {
                        requests.push(request);
                    }
                    write_http_response(&mut stream, response)?;
                }
                Ok(())
            });
            Ok(Self {
                base_url: format!("http://{address}"),
                requests,
                join,
            })
        }

        fn finish(self) -> Result<Vec<String>, String> {
            match self.join.join() {
                Ok(Ok(())) => match Arc::try_unwrap(self.requests) {
                    Ok(requests) => requests
                        .into_inner()
                        .map_err(|_| "request recording mutex was poisoned".to_owned()),
                    Err(_) => Err("request recording still had live references".to_owned()),
                },
                Ok(Err(error)) => Err(error.to_string()),
                Err(_) => Err("fixture thread panicked".to_owned()),
            }
        }
    }

    fn read_http_request(stream: &mut TcpStream) -> io::Result<String> {
        let mut bytes = Vec::new();
        let mut chunk = [0_u8; 1024];
        let mut body_length = None;
        loop {
            let read = stream.read(&mut chunk)?;
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..read]);
            if body_length.is_none()
                && let Some(header_end) = find_header_end(&bytes)
            {
                let headers = String::from_utf8_lossy(&bytes[..header_end]);
                let content_length = headers.lines().find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                });
                body_length = content_length.or(Some(0));
            }
            if let (Some(header_end), Some(body_length)) = (find_header_end(&bytes), body_length)
                && bytes.len() >= header_end + body_length
            {
                break;
            }
        }
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    fn find_header_end(bytes: &[u8]) -> Option<usize> {
        bytes
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|position| position + 4)
    }

    fn write_http_response(stream: &mut TcpStream, response: ResponseSpec) -> io::Result<()> {
        let reason = match response.status {
            200 => "OK",
            400 => "Bad Request",
            429 => "Too Many Requests",
            _ => "Fixture Response",
        };
        let mut header = format!(
            "HTTP/1.1 {} {}\r\nContent-Length: {}\r\n",
            response.status,
            reason,
            response.body.len()
        );
        for (name, value) in response.headers {
            header.push_str(name);
            header.push_str(": ");
            header.push_str(value);
            header.push_str("\r\n");
        }
        header.push_str("\r\n");
        stream.write_all(header.as_bytes())?;
        if response.split_body && response.body.len() > 1 {
            let split = response.body.len() / 2;
            stream.write_all(&response.body[..split])?;
            stream.flush()?;
            stream.write_all(&response.body[split..])?;
        } else {
            stream.write_all(&response.body)?;
        }
        stream.flush()
    }

    fn response(status: u16, headers: Vec<(&'static str, &'static str)>, body: &str) -> ResponseSpec {
        ResponseSpec {
            status,
            headers,
            body: body.as_bytes().to_vec(),
            split_body: false,
        }
    }

    #[test]
    fn request_has_codex_responses_controls() {
        let request = TurnRequest {
            model: "exact-model".into(),
            instructions: "be brief".into(),
            parallel_tool_calls: true,
            reasoning_effort: Some("low".into()),
            prompt_cache_key: Some("cache".into()),
            ..Default::default()
        };
        let body = build_request(&request, None);
        assert_eq!(body["model"], "exact-model");
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert_eq!(body["text"]["verbosity"], "low");
        assert_eq!(body["include"][0], "reasoning.encrypted_content");
        assert_eq!(body["reasoning"]["effort"], "low");
    }

    #[test]
    fn parser_handles_split_crlf_frames_and_function_calls() {
        let mut parser = SseParser::default();
        let mut result = TurnResult::default();
        parser
            .push(
                b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"hel",
                &mut result,
            )
            .expect("test operation should succeed");
        parser.push(b"lo\"}\r\n\r\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"run\",\"arguments\":\"{}\"}}\r\n\r\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\",\"model\":\"m1\",\"usage\":{\"input_tokens\":3,\"output_tokens\":2}}}\r\n\r\n", &mut result).expect("test operation should succeed");
        assert!(parser.completed);
        assert_eq!(result.output_text, "hello");
        assert_eq!(result.tool_calls[0].name, "run");
        assert_eq!(
            result.usage,
            TokenUsage {
                input: 3,
                output: 2,
                ..Default::default()
            }
        );
    }

    #[test]
    fn completed_response_backfills_function_items_once() {
        let mut parser = SseParser::default();
        let mut result = TurnResult::default();
        parser
            .push(
                b"data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"search\",\"arguments\":\"{}\"}]}}\n\n",
                &mut result,
            )
            .expect("test operation should succeed");
        assert_eq!(result.output_items.len(), 1);
        assert_eq!(result.tool_calls.len(), 1);
        parser
            .event(
                r#"{"type":"response.completed","response":{"output":[{"type":"function_call","call_id":"c1","name":"search","arguments":"{}"}]}}"#,
                &mut result,
            )
            .expect("test operation should succeed");
        assert_eq!(result.output_items.len(), 1);
        assert_eq!(result.tool_calls.len(), 1);
    }

    #[test]
    fn debug_redacts_token() {
        let client = ChatGptClient::new(
            "https://example.test",
            "secret-token".to_owned(),
            "secret-account",
        );
        let debug = format!("{client:?}");
        assert!(!debug.contains("secret-token"));
        assert!(!debug.contains("secret-account"));
        assert!(debug.contains("REDACTED"));
    }

    #[test]
    fn provider_diagnostics_drop_sensitive_fields() {
        assert_eq!(
            redact_provider_detail("authorization: Bearer secret"),
            "provider diagnostic contained sensitive fields and was redacted"
        );
        assert_eq!(
            redact_provider_detail("model is unavailable"),
            "model is unavailable"
        );
    }

    #[test]
    fn provider_diagnostics_truncate_unicode_on_character_boundary() {
        let detail = format!("{}étail", "a".repeat(999));
        let redacted = redact_provider_detail(&detail);
        assert!(redacted.ends_with('…'));
        assert!(redacted.is_char_boundary(redacted.len()));
        assert!(redacted.len() <= 1_003);
    }

    #[test]
    fn capabilities_remove_unsupported_request_controls() {
        let request = TurnRequest {
            model: "exact-model".into(),
            reasoning_effort: Some("high".into()),
            parallel_tool_calls: true,
            ..TurnRequest::default()
        };
        let body = build_request(
            &request,
            Some(&ModelCapabilities {
                supports_parallel_tool_calls: false,
                supports_verbosity: false,
                reasoning_efforts: vec!["low".into()],
                ..ModelCapabilities::default()
            }),
        );
        assert_eq!(body["parallel_tool_calls"], false);
        assert!(body.get("text").is_none());
        assert!(body.get("reasoning").is_none());
    }

    #[test]
    fn completed_message_is_text_fallback() {
        let items = vec![serde_json::json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "answer"}]
        })];
        assert_eq!(output_text_from_items(&items), "answer");
    }

    #[test]
    fn parses_read_only_rate_limit_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("x-codex-primary-used-percent", HeaderValue::from_static("12.5"));
        headers.insert("x-codex-primary-window-minutes", HeaderValue::from_static("300"));
        headers.insert("x-codex-credits-has-credits", HeaderValue::from_static("true"));
        headers.insert("x-codex-credits-unlimited", HeaderValue::from_static("false"));
        let snapshots = parse_rate_limits(&headers);
        assert_eq!(
            snapshots[0]
                .primary
                .as_ref()
                .expect("test operation should succeed")
                .used_percent,
            12.5
        );
        assert!(
            snapshots[0]
                .credits
                .as_ref()
                .expect("test operation should succeed")
                .has_credits
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn turn_uses_real_http_sse_transport_and_collects_response_details() {
        let sse = concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-1\",\"model\":\"server-model\"}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hel\"}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"lo\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call-1\",\"name\":\"lookup\",\"arguments\":\"{\\\"q\\\":\\\"rust\\\"}\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-1\",\"model\":\"server-model\",\"usage\":{\"input_tokens\":11,\"output_tokens\":7,\"cache_write_tokens\":2,\"input_tokens_details\":{\"cached_tokens\":3},\"output_tokens_details\":{\"reasoning_tokens\":5}},\"output\":[{\"type\":\"function_call\",\"call_id\":\"call-1\",\"name\":\"lookup\",\"arguments\":\"{\\\"q\\\":\\\"rust\\\"}\"}]}}\n\n"
        );
        let fixture = match TestServer::start(vec![ResponseSpec {
            status: 200,
            headers: vec![
                ("Content-Type", "text/event-stream"),
                ("OpenAI-Model", "header-model"),
                ("x-codex-primary-used-percent", "12.5"),
                ("x-codex-primary-window-minutes", "300"),
            ],
            body: sse.as_bytes().to_vec(),
            split_body: true,
        }]) {
            Ok(fixture) => fixture,
            Err(error) => panic!("could not start fixture: {error}"),
        };
        let client = ChatGptClient::new(
            fixture.base_url.clone(),
            "secret-token".to_owned(),
            "account-1".to_owned(),
        );
        let request = TurnRequest {
            model: "model-1".into(),
            instructions: "answer".into(),
            input: vec![serde_json::json!({"type": "message", "role": "user", "content": "hi"})],
            tools: vec![serde_json::json!({"type": "function", "name": "lookup"})],
            parallel_tool_calls: true,
            subagent_label: Some("worker".into()),
            ..TurnRequest::default()
        };
        let mut events = Vec::new();
        let result = client.turn_stream(request, |event| events.push(event)).await;
        assert!(result.is_ok(), "transport turn failed: {result:?}");
        let result = match result {
            Ok(result) => result,
            Err(error) => panic!("transport turn unexpectedly failed: {error}"),
        };
        assert_eq!(result.output_text, "hello");
        assert_eq!(result.response_id.as_deref(), Some("resp-1"));
        assert_eq!(result.server_model.as_deref(), Some("server-model"));
        assert_eq!(result.usage.input, 11);
        assert_eq!(result.usage.output, 7);
        assert_eq!(result.usage.cached_input, 3);
        assert_eq!(result.usage.cache_write, 2);
        assert_eq!(result.usage.reasoning_output, 5);
        assert_eq!(
            result.tool_calls,
            vec![ToolCall {
                call_id: "call-1".into(),
                name: "lookup".into(),
                arguments: r#"{"q":"rust"}"#.into(),
            }]
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ProviderStreamEvent::TextDelta(delta) if delta == "hel"))
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ProviderStreamEvent::Completed))
        );
        assert_eq!(
            result.rate_limits[0]
                .primary
                .as_ref()
                .map(|window| window.used_percent),
            Some(12.5)
        );

        let requests = fixture.finish();
        assert!(requests.is_ok(), "fixture failed: {requests:?}");
        let requests = match requests {
            Ok(requests) => requests,
            Err(error) => panic!("fixture unexpectedly failed: {error}"),
        };
        assert_eq!(requests.len(), 1);
        let raw_request = &requests[0];
        assert!(raw_request.contains("authorization: Bearer secret-token"));
        assert!(raw_request.contains("chatgpt-account-id: account-1"));
        assert!(raw_request.contains("originator: minha"));
        assert!(raw_request.contains("x-openai-subagent: worker"));
        let (_, body) = match raw_request.split_once("\r\n\r\n") {
            Some(parts) => parts,
            None => panic!("fixture request had no HTTP body"),
        };
        let body: Value = match serde_json::from_str(body) {
            Ok(body) => body,
            Err(error) => panic!("fixture received invalid JSON: {error}"),
        };
        assert_eq!(body["stream"], true);
        assert_eq!(body["store"], false);
        assert_eq!(body["tools"][0]["name"], "lookup");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn retry_after_zero_retries_without_a_long_sleep() {
        let fixture = match TestServer::start(vec![
            response(429, vec![("Retry-After", "0")], "rate limited"),
            response(200, vec![], r#"{"models":[{"slug":"model-1"}]}"#),
        ]) {
            Ok(fixture) => fixture,
            Err(error) => panic!("could not start fixture: {error}"),
        };
        let client = ChatGptClient::new(fixture.base_url.clone(), "token", "account");
        let started = std::time::Instant::now();
        let result = client.fetch_models(None).await;
        assert!(result.is_ok(), "retry request failed: {result:?}");
        assert!(started.elapsed() < Duration::from_millis(500));
        let requests = fixture.finish();
        assert!(requests.is_ok(), "fixture failed: {requests:?}");
        let requests = match requests {
            Ok(requests) => requests,
            Err(error) => panic!("fixture unexpectedly failed: {error}"),
        };
        assert_eq!(requests.len(), 2);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn response_posts_are_never_retried_implicitly() {
        let fixture = match TestServer::start(vec![response(500, vec![], "provider failed")]) {
            Ok(fixture) => fixture,
            Err(error) => panic!("could not start fixture: {error}"),
        };
        let client = ChatGptClient::new(fixture.base_url.clone(), "token", "account");
        let result = client
            .turn(TurnRequest {
                model: "gpt-5.3-codex-spark".into(),
                instructions: "test".into(),
                input: vec![serde_json::json!({"type":"message","role":"user","content":"test"})],
                ..TurnRequest::default()
            })
            .await;
        assert!(matches!(result, Err(ProviderError::Http { status, .. }) if status.as_u16() == 500));
        let requests = fixture.finish();
        assert!(requests.is_ok(), "fixture failed: {requests:?}");
        let requests = match requests {
            Ok(requests) => requests,
            Err(error) => panic!("fixture unexpectedly failed: {error}"),
        };
        assert_eq!(requests.len(), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn http_error_redacts_sensitive_diagnostics_over_transport() {
        let fixture = match TestServer::start(vec![response(
            400,
            vec![("x-request-id", "req-1")],
            r#"{"error":{"message":"authorization Bearer secret-token account_id=account-1"}}"#,
        )]) {
            Ok(fixture) => fixture,
            Err(error) => panic!("could not start fixture: {error}"),
        };
        let client = ChatGptClient::new(
            fixture.base_url.clone(),
            "secret-token".to_owned(),
            "account-1".to_owned(),
        );
        let error = match client.fetch_models(None).await {
            Ok(_) => panic!("sensitive diagnostic fixture unexpectedly succeeded"),
            Err(error) => error,
        };
        let detail = error.to_string();
        assert!(detail.contains("redacted"));
        assert!(!detail.contains("secret-token"));
        assert!(!detail.contains("account-1"));
        assert!(detail.contains("req-1"));
        let requests = fixture.finish();
        assert!(requests.is_ok(), "fixture failed: {requests:?}");
    }
}
