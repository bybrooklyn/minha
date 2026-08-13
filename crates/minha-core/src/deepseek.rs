//! Direct DeepSeek V4 Chat Completions adapter.

use crate::provider::{
    ModelCatalog, ModelDescriptor, ProviderBalanceAmountV1, ProviderBalanceV1, ProviderClient, ProviderError,
    ProviderId, ProviderStreamEvent, ToolCall, TurnRequest, TurnResult, redact_provider_detail,
    retry_after_from_headers,
};
use futures_util::StreamExt;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderValue};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::{collections::BTreeMap, fmt, time::Duration};
use std::{future::Future, pin::Pin};

pub const DEEPSEEK_PRICING_SOURCE: &str = "https://api-docs.deepseek.com/quick_start/pricing";
pub const DEEPSEEK_PRICING_VERSION: &str = "2026-07-29";

/// The small, explicit compatibility differences Minha supports for fixed
/// OpenAI-style Chat Completions providers.  This is deliberately an enum,
/// not a provider plug-in surface: each variant is reviewed in-tree and its
/// request semantics are fixture-tested.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ChatRequestProfile {
    DeepSeek,
    XiaomiMiMo,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DeepSeekPricing {
    pub cache_hit_input_per_million: f64,
    pub cache_miss_input_per_million: f64,
    pub output_per_million: f64,
}

pub fn pricing_for_model(model: &str) -> Option<DeepSeekPricing> {
    match model.strip_prefix("deepseek/").unwrap_or(model) {
        "deepseek-v4-flash" => Some(DeepSeekPricing {
            cache_hit_input_per_million: 0.0028,
            cache_miss_input_per_million: 0.14,
            output_per_million: 0.28,
        }),
        "deepseek-v4-pro" => Some(DeepSeekPricing {
            cache_hit_input_per_million: 0.003_625,
            cache_miss_input_per_million: 0.435,
            output_per_million: 0.87,
        }),
        _ => None,
    }
}

pub fn estimate_cost_usd(model: &str, input: u64, cached_input: u64, output: u64) -> Option<f64> {
    let pricing = pricing_for_model(model)?;
    let cached_input = cached_input.min(input);
    let uncached_input = input.saturating_sub(cached_input);
    Some(
        (cached_input as f64 * pricing.cache_hit_input_per_million
            + uncached_input as f64 * pricing.cache_miss_input_per_million
            + output as f64 * pricing.output_per_million)
            / 1_000_000.0,
    )
}

fn pricing_metadata(model: &str) -> Value {
    let Some(pricing) = pricing_for_model(model) else {
        return Value::Null;
    };
    json!({
        "currency": "USD",
        "unit_tokens": 1_000_000,
        "cache_hit_input": pricing.cache_hit_input_per_million,
        "cache_miss_input": pricing.cache_miss_input_per_million,
        "output": pricing.output_per_million,
        "source": DEEPSEEK_PRICING_SOURCE,
        "version": DEEPSEEK_PRICING_VERSION,
    })
}

#[derive(Clone)]
pub struct DeepSeekClient {
    http: reqwest::Client,
    base_url: String,
    api_key: SecretString,
}

impl fmt::Debug for DeepSeekClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeepSeekClient")
            .field("base_url", &self.base_url)
            .field("api_key", &"[REDACTED]")
            .finish()
    }
}

impl DeepSeekClient {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<SecretString>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            api_key: api_key.into(),
        }
    }

    pub async fn fetch_models(&self) -> Result<ModelCatalog, ProviderError> {
        Ok(ModelCatalog {
            models: ["deepseek-v4-flash", "deepseek-v4-pro"]
                .into_iter()
                .map(|slug| ModelDescriptor {
                    slug: slug.into(),
                    metadata: Map::from_iter([
                        ("context_window".into(), json!(1_048_576)),
                        ("max_output_tokens".into(), json!(393_216)),
                        ("supports_parallel_tool_calls".into(), json!(true)),
                        ("supports_tool_calls".into(), json!(true)),
                        (
                            "supported_reasoning_levels".into(),
                            json!([{"effort":"high"},{"effort":"max"}]),
                        ),
                        ("capability_source".into(), json!("fallback_table_v1")),
                        ("pricing".into(), pricing_metadata(slug)),
                    ]),
                })
                .collect(),
            etag: Some("deepseek-v4-fallback-v1".into()),
            not_modified: false,
        })
    }

    pub async fn test_connection(&self) -> Result<(), ProviderError> {
        let authorization = HeaderValue::from_str(&format!("Bearer {}", self.api_key.expose_secret()))
            .map_err(|_| ProviderError::Header)?;
        let response = self
            .http
            .get(format!("{}/models", self.base_url))
            .header(AUTHORIZATION, authorization)
            .timeout(Duration::from_secs(30))
            .send()
            .await?;
        if response.status().is_success() {
            Ok(())
        } else {
            let status = response.status();
            let retry_after = retry_after_from_headers(response.headers());
            let bytes = response.bytes().await.unwrap_or_default();
            Err(ProviderError::Http {
                status,
                detail: redact_provider_detail(&String::from_utf8_lossy(&bytes[..bytes.len().min(4 * 1024)])),
                retry_after,
            })
        }
    }

    pub async fn fetch_balance(&self) -> Result<ProviderBalanceV1, ProviderError> {
        #[derive(Deserialize)]
        struct BalanceResponse {
            is_available: bool,
            #[serde(default)]
            balance_infos: Vec<BalanceInfo>,
        }
        #[derive(Deserialize)]
        struct BalanceInfo {
            currency: String,
            total_balance: String,
            granted_balance: String,
            topped_up_balance: String,
        }

        let authorization = HeaderValue::from_str(&format!("Bearer {}", self.api_key.expose_secret()))
            .map_err(|_| ProviderError::Header)?;
        let response = self
            .http
            .get(format!("{}/user/balance", self.base_url))
            .header(AUTHORIZATION, authorization)
            .header(ACCEPT, "application/json")
            .timeout(Duration::from_secs(30))
            .send()
            .await?;
        if !response.status().is_success() {
            let status = response.status();
            let retry_after = retry_after_from_headers(response.headers());
            let bytes = response.bytes().await.unwrap_or_default();
            let detail =
                redact_provider_detail(&String::from_utf8_lossy(&bytes[..bytes.len().min(4 * 1024)]));
            return Err(ProviderError::Http {
                status,
                detail,
                retry_after,
            });
        }
        let payload: BalanceResponse = response.json().await?;
        Ok(ProviderBalanceV1 {
            schema_version: 1,
            provider: ProviderId::DeepSeek,
            is_available: payload.is_available,
            balances: payload
                .balance_infos
                .into_iter()
                .map(|balance| ProviderBalanceAmountV1 {
                    currency: balance.currency,
                    total: balance.total_balance,
                    granted: balance.granted_balance,
                    topped_up: balance.topped_up_balance,
                })
                .collect(),
        })
    }

    pub async fn turn(&self, request: TurnRequest) -> Result<TurnResult, ProviderError> {
        self.turn_stream(request, |_| {}).await
    }

    pub async fn turn_stream<F>(&self, request: TurnRequest, on_event: F) -> Result<TurnResult, ProviderError>
    where
        F: FnMut(ProviderStreamEvent),
    {
        self.turn_stream_with_profile(request, ChatRequestProfile::DeepSeek, on_event)
            .await
    }

    /// Run a fixed, in-tree Chat Completions compatibility profile.  MiMo
    /// shares the wire framing and SSE result shape with DeepSeek but does not
    /// inherit DeepSeek's thinking/reasoning request fields.
    pub(crate) async fn turn_stream_with_profile<F>(
        &self,
        request: TurnRequest,
        profile: ChatRequestProfile,
        mut on_event: F,
    ) -> Result<TurnResult, ProviderError>
    where
        F: FnMut(ProviderStreamEvent),
    {
        let body = chat_request_with_profile(&request, profile);
        let authorization = HeaderValue::from_str(&format!("Bearer {}", self.api_key.expose_secret()))
            .map_err(|_| ProviderError::Header)?;
        // Model POSTs are deliberately never retried: the provider may have
        // accepted and billed a request before a transport failure is visible.
        let response = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .header(AUTHORIZATION, authorization)
            .header(ACCEPT, "text/event-stream")
            .header(CONTENT_TYPE, "application/json")
            .json(&body)
            .send()
            .await?;
        if !response.status().is_success() {
            let status = response.status();
            let retry_after = retry_after_from_headers(response.headers());
            let bytes = response.bytes().await.unwrap_or_default();
            let detail =
                redact_provider_detail(&String::from_utf8_lossy(&bytes[..bytes.len().min(4 * 1024)]));
            return Err(ProviderError::Http {
                status,
                detail,
                retry_after,
            });
        }

        let mut parser = ChatStreamParser::default();
        let mut result = TurnResult::default();
        let mut stream = response.bytes_stream();
        // Bounded by silence between chunks, never by total wall-clock time:
        // long reasoning turns are already billed and must not be cut.
        loop {
            let chunk = tokio::time::timeout(crate::provider::STREAM_IDLE_TIMEOUT, stream.next())
                .await
                .map_err(|_| {
                    ProviderError::InvalidResponse("provider stream exceeded the idle timeout".into())
                })?;
            let Some(chunk) = chunk else { break };
            let chunk = chunk.map_err(ProviderError::Request)?;
            for event in parser.push(&chunk, &mut result)? {
                on_event(event);
            }
        }
        for event in parser.finish(&mut result)? {
            on_event(event);
        }
        if !parser.completed {
            return Err(ProviderError::IncompleteStream);
        }
        Ok(result)
    }
}

impl ProviderClient for DeepSeekClient {
    fn provider_id(&self) -> ProviderId {
        ProviderId::DeepSeek
    }

    fn discover_models(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<ModelCatalog, ProviderError>> + Send + '_>> {
        Box::pin(self.fetch_models())
    }

    fn complete_turn(
        &self,
        request: TurnRequest,
    ) -> Pin<Box<dyn Future<Output = Result<TurnResult, ProviderError>> + Send + '_>> {
        Box::pin(self.turn(request))
    }
}

#[cfg(test)]
fn chat_request(request: &TurnRequest) -> Value {
    chat_request_with_profile(request, ChatRequestProfile::DeepSeek)
}

pub(crate) fn chat_request_with_profile(request: &TurnRequest, profile: ChatRequestProfile) -> Value {
    let mut messages = vec![json!({"role":"system", "content":request.instructions})];
    for item in &request.input {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
                let content = item.get("content").and_then(Value::as_array).into_iter().flatten()
                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                    .collect::<String>();
                messages.push(json!({"role":role, "content":content}));
            }
            Some("function_call") => messages.push(json!({
                "role":"assistant", "content":Value::Null,
                "tool_calls":[{"id":item.get("call_id").and_then(Value::as_str).unwrap_or_default(),
                    "type":"function", "function":{"name":item.get("name").and_then(Value::as_str).unwrap_or_default(),
                    "arguments":item.get("arguments").and_then(Value::as_str).unwrap_or("{}")}}]
            })),
            Some("function_call_output") => messages.push(json!({
                "role":"tool",
                "tool_call_id":item.get("call_id").and_then(Value::as_str).unwrap_or_default(),
                "content":item.get("output").and_then(Value::as_str).unwrap_or_default()
            })),
            _ => {}
        }
    }
    let tools = request
        .tools
        .iter()
        .filter_map(|tool| {
            let name = tool.get("name")?.clone();
            Some(json!({"type":"function", "function":{
                "name":name,
                "description":tool.get("description").cloned().unwrap_or(Value::String(String::new())),
                "parameters":tool.get("parameters").cloned().unwrap_or_else(|| json!({"type":"object"}))
            }}))
        })
        .collect::<Vec<_>>();
    let mut body = json!({
        "model":request.model,
        "messages":messages,
        "stream":true,
        "stream_options":{"include_usage":true},
        "tools":tools,
        "parallel_tool_calls":request.parallel_tool_calls,
    });
    if profile == ChatRequestProfile::DeepSeek {
        body["thinking"] = json!({"type":"enabled"});
        let requested_effort = request.reasoning_effort.as_deref().unwrap_or("max");
        let reasoning_effort = if request.model == "deepseek-v4-flash" {
            "max"
        } else if matches!(requested_effort, "high" | "max") {
            requested_effort
        } else {
            "max"
        };
        body["reasoning_effort"] = json!(reasoning_effort);
    } else {
        // MiMo documents that a tool-call continuation must include the
        // provider's reasoning content when thinking is enabled.  Minha's
        // provider-neutral turn history intentionally does not persist that
        // vendor field, so make the first offline-qualified adapter safe by
        // explicitly disabling thinking rather than silently issuing invalid
        // follow-up tool turns.
        body["thinking"] = json!({"type":"disabled"});
    }
    if let Some(response_format) = &request.response_format {
        body["response_format"] = response_format.clone();
    }
    body
}

#[derive(Default)]
struct ChatStreamParser {
    buffer: Vec<u8>,
    completed: bool,
    calls: BTreeMap<usize, ToolCall>,
    malformed_frames: u8,
}

impl ChatStreamParser {
    fn push(
        &mut self,
        chunk: &[u8],
        result: &mut TurnResult,
    ) -> Result<Vec<ProviderStreamEvent>, ProviderError> {
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some(end) = chat_frame_end(&self.buffer) {
            let frame = self.buffer.drain(..end).collect::<Vec<_>>();
            self.parse_frame(&String::from_utf8_lossy(&frame), result, &mut events)?;
        }
        Ok(events)
    }

    fn finish(&mut self, result: &mut TurnResult) -> Result<Vec<ProviderStreamEvent>, ProviderError> {
        let mut events = Vec::new();
        if !self.buffer.is_empty() {
            let frame = std::mem::take(&mut self.buffer);
            self.parse_frame(&String::from_utf8_lossy(&frame), result, &mut events)?;
        }
        result.tool_calls = self.calls.values().cloned().collect();
        Ok(events)
    }

    fn parse_frame(
        &mut self,
        frame: &str,
        result: &mut TurnResult,
        events: &mut Vec<ProviderStreamEvent>,
    ) -> Result<(), ProviderError> {
        // Join every data: line of the frame; a lone find_map would silently
        // drop the remainder of multi-line payloads.
        let data = frame
            .lines()
            .filter_map(|line| line.strip_prefix("data:").map(str::trim))
            .collect::<Vec<_>>()
            .join("\n");
        if data.is_empty() {
            return Ok(());
        }
        if data == "[DONE]" {
            self.completed = true;
            for (index, call) in &self.calls {
                if serde_json::from_str::<Value>(&call.arguments).is_err() {
                    return Err(ProviderError::InvalidResponse(format!(
                        "tool call {index} carried invalid JSON arguments"
                    )));
                }
            }
            result.tool_calls = self.calls.values().cloned().collect();
            events.push(ProviderStreamEvent::Completed);
            return Ok(());
        }
        let value: Value = match serde_json::from_str(&data) {
            Ok(value) => value,
            Err(error) => {
                // A single corrupted or injected frame must not discard an
                // otherwise-complete (and billed) turn.
                self.malformed_frames = self.malformed_frames.saturating_add(1);
                if self.malformed_frames > crate::provider::MAX_MALFORMED_FRAMES {
                    return Err(ProviderError::Sse(error));
                }
                return Ok(());
            }
        };
        result.response_id = value
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or(result.response_id.take());
        result.server_model = value
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or(result.server_model.take());
        if let Some(usage) = value.get("usage") {
            // Merge field by field so fields absent from a later usage frame
            // keep their earlier values instead of resetting to zero.
            result.usage.input = usage
                .get("prompt_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(result.usage.input);
            result.usage.output = usage
                .get("completion_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(result.usage.output);
            result.usage.cached_input = usage
                .get("prompt_cache_hit_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(result.usage.cached_input);
            // DeepSeek has no cache-write accounting: miss tokens are uncached
            // input, already included in prompt_tokens, so cache_write stays
            // zero instead of being mislabeled.
            result.usage.reasoning_output = usage
                .get("reasoning_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(result.usage.reasoning_output);
        }
        for choice in value
            .get("choices")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                result.finish_reason = Some(reason.to_owned());
            }
            let delta = choice.get("delta").unwrap_or(&Value::Null);
            if let Some(reasoning) = delta
                .get("reasoning_content")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                result.reasoning_text.push_str(reasoning);
                events.push(ProviderStreamEvent::ReasoningDelta(reasoning.to_owned()));
            }
            if let Some(text) = delta
                .get("content")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                result.output_text.push_str(text);
                events.push(ProviderStreamEvent::TextDelta(text.to_owned()));
            }
            for call in delta
                .get("tool_calls")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let index = call.get("index").and_then(Value::as_u64) as Option<u64>;
                let index = match index {
                    Some(index) => index as usize,
                    None => {
                        // Missing index is only unambiguous while a single
                        // call is in flight; otherwise the fragment cannot be
                        // attributed and must not corrupt an existing call.
                        let Some((&only, _)) = self.calls.iter().next() else {
                            continue;
                        };
                        if self.calls.len() != 1 {
                            continue;
                        }
                        only
                    }
                };
                let fresh_id = call.get("id").and_then(Value::as_str);
                let entry = self.calls.entry(index).or_insert_with(|| ToolCall {
                    call_id: String::new(),
                    name: String::new(),
                    arguments: String::new(),
                });
                if let Some(id) = fresh_id {
                    match entry.call_id.as_str() {
                        "" => entry.call_id = id.to_owned(),
                        existing if existing != id => {
                            // A different id on the same index marks a
                            // restarted call: start a fresh accumulator.
                            *entry = ToolCall {
                                call_id: id.to_owned(),
                                name: String::new(),
                                arguments: String::new(),
                            };
                        }
                        _ => {}
                    }
                }
                if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
                    // Continuation fragments never repeat the name; a name on
                    // an already-named entry marks a restarted call.
                    if !entry.name.is_empty() {
                        *entry = ToolCall {
                            call_id: entry.call_id.clone(),
                            name: String::new(),
                            arguments: String::new(),
                        };
                    }
                    entry.name.push_str(name);
                }
                if let Some(arguments) = call.pointer("/function/arguments").and_then(Value::as_str) {
                    entry.arguments.push_str(arguments);
                }
            }
        }
        Ok(())
    }
}

fn chat_frame_end(buffer: &[u8]) -> Option<usize> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;

    fn read_fixture_request(stream: &mut TcpStream) -> String {
        let mut bytes = Vec::new();
        let mut chunk = [0_u8; 1024];
        loop {
            let read = stream.read(&mut chunk).expect("read fixture request");
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..read]);
            if let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                let body_start = header_end + 4;
                let headers = String::from_utf8_lossy(&bytes[..body_start]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                if bytes.len() >= body_start + content_length {
                    break;
                }
            }
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn deepseek_fixture(response_body: &'static str) -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind fixture");
        let address = listener.local_addr().expect("fixture address");
        let join = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept fixture request");
            let request = read_fixture_request(&mut stream);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response_body.len()
            )
            .expect("write fixture headers");
            for chunk in response_body.as_bytes().chunks(17) {
                stream.write_all(chunk).expect("write split fixture body");
                stream.flush().expect("flush fixture body");
            }
            request
        });
        (format!("http://{address}"), join)
    }

    #[test]
    fn v4_pricing_is_model_specific_and_cache_aware() {
        let flash = pricing_for_model("deepseek/deepseek-v4-flash").expect("Flash pricing");
        let pro = pricing_for_model("deepseek-v4-pro").expect("Pro pricing");
        assert!(pro.output_per_million > flash.output_per_million);
        assert!(
            (estimate_cost_usd("deepseek-v4-flash", 1_000_000, 500_000, 1_000_000).expect("priced model")
                - 0.3514)
                .abs()
                < f64::EPSILON
        );
        assert_eq!(pricing_for_model("deepseek-v3"), None);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fallback_catalog_carries_versioned_capabilities_and_pricing() {
        let catalog = DeepSeekClient::new("https://example.invalid", "secret")
            .fetch_models()
            .await
            .expect("fallback catalog");
        assert_eq!(catalog.models.len(), 2);
        for model in catalog.models {
            let capabilities = model.capabilities();
            assert_eq!(capabilities.context_window, Some(1_048_576));
            assert_eq!(capabilities.maximum_output, Some(393_216));
            assert_eq!(model.metadata["pricing"]["version"], DEEPSEEK_PRICING_VERSION);
            assert_eq!(model.metadata["pricing"]["source"], DEEPSEEK_PRICING_SOURCE);
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn balance_endpoint_reports_exact_remaining_funds_without_leaking_key() {
        let response = r#"{"is_available":true,"balance_infos":[{"currency":"USD","total_balance":"12.34","granted_balance":"2.34","topped_up_balance":"10.00"}]}"#;
        let (base_url, fixture) = deepseek_fixture(response);
        let client = DeepSeekClient::new(base_url, "balance-fixture-secret");
        let balance = client.fetch_balance().await.expect("balance response");
        assert!(balance.is_available);
        assert_eq!(balance.total("USD"), Some(12.34));
        assert_eq!(balance.balances[0].granted, "2.34");
        let request = fixture.join().expect("fixture thread");
        assert!(request.starts_with("GET /user/balance HTTP/1.1"));
        assert!(!format!("{balance:?}").contains("balance-fixture-secret"));
    }

    #[test]
    fn usage_merges_and_tool_calls_survive_resends_and_bad_frames() {
        let mut parser = ChatStreamParser::default();
        let mut result = TurnResult::default();
        // First frame: partial usage; second frame: another partial usage
        // missing reasoning — absent fields must be preserved, not zeroed.
        parser
            .push(
                b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":3,\"prompt_cache_hit_tokens\":8}}\n\n",
                &mut result,
            )
            .expect("first usage frame");
        parser
            .push(
                b"data: {\"choices\":[],\"usage\":{\"completion_tokens\":5,\"reasoning_tokens\":2}}\n\n",
                &mut result,
            )
            .expect("second usage frame");
        assert_eq!(result.usage.input, 10);
        assert_eq!(result.usage.output, 5);
        assert_eq!(result.usage.cached_input, 8);
        assert_eq!(result.usage.reasoning_output, 2);
        // A garbage frame is skipped, not fatal.
        parser
            .push(b"data: not json\n\n", &mut result)
            .expect("skipped frame");
        // Continuation fragments carry only the index; a re-sent id or name
        // marks a restarted call and resets the accumulator.
        parser
            .push(
                b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-1\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"a\\\"\"}}]}}]}\n\n",
                &mut result,
            )
            .expect("first fragment");
        parser
            .push(
                b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\":1}\"}}]}}]}\n\n",
                &mut result,
            )
            .expect("continuation fragment");
        parser
            .push(
                b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-2\",\"function\":{\"name\":\"search\",\"arguments\":\"{\\\"q\\\":\\\"rust\\\"}\"}}]}}]}\n\n",
                &mut result,
            )
            .expect("restarted call");
        parser.push(b"data: [DONE]\n\n", &mut result).expect("done");
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].call_id, "call-2");
        assert_eq!(result.tool_calls[0].name, "search");
        assert_eq!(result.tool_calls[0].arguments, "{\"q\":\"rust\"}");
        assert!(parser.completed);
    }

    #[test]
    fn done_rejects_truncated_tool_arguments() {
        let mut parser = ChatStreamParser::default();
        let mut result = TurnResult::default();
        parser
            .push(
                b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"search\",\"arguments\":\"{\\\"q\\\":\"}}]}}]}\n\n",
                &mut result,
            )
            .expect("fragment");
        let error = parser
            .push(b"data: [DONE]\n\n", &mut result)
            .expect_err("truncated arguments must fail at [DONE]");
        assert!(format!("{error}").contains("invalid JSON"));
    }

    #[test]
    fn multi_line_data_frames_are_joined() {
        let mut parser = ChatStreamParser::default();
        let mut result = TurnResult::default();
        // A JSON payload split across data: lines must be joined, not parsed
        // from the first line only.
        parser
            .push(
                b"data: {\"choices\":[{\"delta\":{\"content\":\ndata: \"line\"}}]}\n\n",
                &mut result,
            )
            .expect("joined multi-line frame");
        assert!(!parser.completed);
        assert_eq!(result.output_text, "line");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn error_paths_redact_the_api_key_from_provider_bodies() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind fixture");
        let address = listener.local_addr().expect("fixture address");
        let join = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept fixture request");
            let _request = read_fixture_request(&mut stream);
            let body =
                r#"{"error":{"message":"Authentication Fails, Your api key: sk-leaky-key is invalid"}}"#;
            write!(
                stream,
                "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .expect("write fixture response");
        });
        let client = DeepSeekClient::new(format!("http://{address}"), "sk-leaky-key");
        let error = client.test_connection().await.expect_err("auth failure expected");
        let debug = format!("{error:?}");
        assert!(
            !debug.contains("sk-leaky-key"),
            "error leaked the API key: {debug}"
        );
        assert!(debug.contains("redacted"), "error was not redacted: {debug}");
        join.join().expect("fixture thread");
    }

    #[test]
    fn fragmented_tool_arguments_and_cache_usage_are_preserved() {
        let mut parser = ChatStreamParser::default();
        let mut result = TurnResult::default();
        parser.push(b"data: {\"id\":\"r1\",\"model\":\"deepseek-v4-flash\",\"choices\":[{\"delta\":{\"content\":\"ok\",\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"read_files\",\"arguments\":\"{\\\"fi\"}}]}}]}\n\n", &mut result).expect("first frame");
        parser.push(b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"les\\\":[]}\"}}]}}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":3,\"prompt_cache_hit_tokens\":8,\"prompt_cache_miss_tokens\":2}}\n\ndata: [DONE]\n\n", &mut result).expect("second frame");
        parser.finish(&mut result).expect("finish");
        assert_eq!(result.output_text, "ok");
        assert_eq!(result.tool_calls[0].arguments, "{\"files\":[]}");
        assert_eq!(result.usage.cached_input, 8);
        assert!(parser.completed);
    }

    #[test]
    fn keep_alives_reasoning_finish_reasons_and_json_mode_are_supported() {
        let request = TurnRequest {
            model: "deepseek-v4-pro".into(),
            instructions: "return json".into(),
            reasoning_effort: Some("max".into()),
            response_format: Some(json!({"type":"json_object"})),
            ..TurnRequest::default()
        };
        let body = chat_request(&request);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["response_format"]["type"], "json_object");
        let requested_non_thinking = chat_request(&TurnRequest {
            model: "deepseek-v4-flash".into(),
            reasoning_effort: Some("none".into()),
            ..TurnRequest::default()
        });
        assert_eq!(requested_non_thinking["thinking"]["type"], "enabled");
        assert_eq!(requested_non_thinking["reasoning_effort"], "max");

        let mut parser = ChatStreamParser::default();
        let mut result = TurnResult::default();
        let events = parser
            .push(
                b": keep-alive\n\ndata: {\"choices\":[{\"delta\":{\"reasoning_content\":\"check\",\"content\":\"{\\\"ok\\\":true}\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
                &mut result,
            )
            .expect("stream frames");
        parser.finish(&mut result).expect("finish");
        assert_eq!(result.reasoning_text, "check");
        assert_eq!(result.output_text, "{\"ok\":true}");
        assert_eq!(result.finish_reason.as_deref(), Some("stop"));
        assert!(events.contains(&ProviderStreamEvent::ReasoningDelta("check".into())));
        assert!(parser.completed);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn real_http_stream_translates_tools_reasoning_json_and_usage() {
        let response = concat!(
            ": keep-alive\n\n",
            "data: {\"id\":\"ds-1\",\"model\":\"deepseek-v4-pro\",\"choices\":[{\"delta\":{\"reasoning_content\":\"inspect\",\"tool_calls\":[{\"index\":0,\"id\":\"call-1\",\"function\":{\"name\":\"search\",\"arguments\":\"{\\\"q\\\":\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"{\\\"ok\\\":true}\",\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"rust\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":20,\"completion_tokens\":5,\"prompt_cache_hit_tokens\":12,\"prompt_cache_miss_tokens\":8,\"reasoning_tokens\":3}}\n\n",
            "data: [DONE]\n\n"
        );
        let (base_url, fixture) = deepseek_fixture(response);
        let client = DeepSeekClient::new(base_url, "fixture-secret");
        let request = TurnRequest {
            model: "deepseek-v4-pro".into(),
            instructions: "inspect safely".into(),
            input: vec![
                json!({"type":"message","role":"user","content":[{"type":"input_text","text":"find Rust"}]}),
            ],
            tools: vec![json!({"name":"search","description":"search text","parameters":{"type":"object"}})],
            parallel_tool_calls: true,
            reasoning_effort: Some("max".into()),
            response_format: Some(json!({"type":"json_object"})),
            ..TurnRequest::default()
        };
        let mut events = Vec::new();
        let result = client
            .turn_stream(request, |event| events.push(event))
            .await
            .expect("DeepSeek fixture turn");
        assert_eq!(result.output_text, "{\"ok\":true}");
        assert_eq!(result.reasoning_text, "inspect");
        assert_eq!(result.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(result.tool_calls[0].arguments, "{\"q\":\"rust\"}");
        assert_eq!(result.usage.input, 20);
        assert_eq!(result.usage.cached_input, 12);
        // Miss tokens are uncached input, not a cache write; DeepSeek has no
        // cache-write accounting so the counter must not be mislabeled.
        assert_eq!(result.usage.cache_write, 0);
        assert_eq!(result.usage.reasoning_output, 3);
        assert!(events.contains(&ProviderStreamEvent::Completed));

        let raw_request = fixture.join().expect("fixture thread");
        assert!(raw_request.starts_with("POST /chat/completions HTTP/1.1"));
        assert!(
            raw_request
                .to_ascii_lowercase()
                .contains("authorization: bearer fixture-secret")
        );
        let body = raw_request.split_once("\r\n\r\n").expect("fixture body").1;
        let body: Value = serde_json::from_str(body).expect("request JSON");
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["response_format"]["type"], "json_object");
        assert_eq!(body["tools"][0]["function"]["name"], "search");
        assert_eq!(body["messages"][1]["content"], "find Rust");
    }

    #[test]
    fn debug_never_contains_the_api_key() {
        let client = DeepSeekClient::new("https://example.invalid", "secret-key");
        let debug = format!("{client:?}");
        assert!(!debug.contains("secret-key"));
        assert!(debug.contains("[REDACTED]"));
    }
}
