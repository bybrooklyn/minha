//! Fixed Xiaomi MiMo Chat Completions adapter.
//!
//! MiMo documents an OpenAI-compatible `/v1/chat/completions` endpoint.  The
//! adapter deliberately uses that documented surface rather than guessing at
//! an undocumented Responses endpoint.  Its first supported profile disables
//! thinking: MiMo requires a caller to replay `reasoning_content` in a
//! tool-call continuation, while Minha's portable turn history does not store
//! vendor reasoning payloads yet.  That keeps tool continuations valid and
//! makes the limitation explicit instead of silently corrupting history.

use crate::{
    deepseek::{ChatRequestProfile, DeepSeekClient},
    provider::{
        ModelCatalog, ModelDescriptor, ProviderClient, ProviderError, ProviderId, ProviderStreamEvent,
        TurnRequest, TurnResult,
    },
};
use futures_util::Future;
use secrecy::SecretString;
use serde_json::{Map, Value, json};
use std::{fmt, pin::Pin};

pub const XIAOMI_MIMO_BASE_URL: &str = "https://api.xiaomimimo.com/v1";
pub const MIMO_PRICING_SOURCE: &str = "https://mimo.mi.com/docs/en-US/price/pay-as-you-go";
pub const MIMO_PRICING_VERSION: &str = "2026-07-15";

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MiMoPricing {
    pub cache_hit_input_per_million: f64,
    pub cache_miss_input_per_million: f64,
    pub output_per_million: f64,
}

pub fn pricing_for_model(model: &str) -> Option<MiMoPricing> {
    match model.strip_prefix("xiaomi/").unwrap_or(model) {
        "mimo-v2.5-pro" => Some(MiMoPricing {
            cache_hit_input_per_million: 0.0036,
            cache_miss_input_per_million: 0.435,
            output_per_million: 0.87,
        }),
        "mimo-v2.5" => Some(MiMoPricing {
            cache_hit_input_per_million: 0.0028,
            cache_miss_input_per_million: 0.14,
            output_per_million: 0.28,
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
        "source": MIMO_PRICING_SOURCE,
        "version": MIMO_PRICING_VERSION,
        "status": "reference_price_not_account_quota",
    })
}

/// Xiaomi MiMo's fixed, offline-qualified provider client.
#[derive(Clone)]
pub struct MiMoClient {
    transport: DeepSeekClient,
}

impl fmt::Debug for MiMoClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MiMoClient")
            .field("transport", &self.transport)
            .finish()
    }
}

impl MiMoClient {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<SecretString>) -> Self {
        Self {
            transport: DeepSeekClient::new(base_url, api_key),
        }
    }

    /// Static, timestamped entries are intentional.  A configured MiMo key
    /// is not needed to understand safe current routing/capability defaults;
    /// live account qualification remains an explicit `provider test` action.
    pub async fn fetch_models(&self) -> Result<ModelCatalog, ProviderError> {
        Ok(ModelCatalog {
            models: ["mimo-v2.5-pro", "mimo-v2.5"]
                .into_iter()
                .map(|slug| {
                    let multimodal = slug == "mimo-v2.5";
                    ModelDescriptor {
                        slug: slug.into(),
                        metadata: Map::from_iter([
                            ("context_window".into(), json!(1_048_576)),
                            ("effective_context_window".into(), json!(996_147)),
                            ("protected_context_reserve".into(), json!(52_429)),
                            ("max_output_tokens".into(), json!(131_072)),
                            ("supports_parallel_tool_calls".into(), json!(true)),
                            ("supports_tool_calls".into(), json!(true)),
                            ("supports_streaming".into(), json!(true)),
                            ("multimodal_input".into(), json!(multimodal)),
                            (
                                "thinking_mode".into(),
                                json!("disabled_for_portable_tool_continuations"),
                            ),
                            ("capability_source".into(), json!("fallback_table_v1")),
                            ("pricing".into(), pricing_metadata(slug)),
                        ]),
                    }
                })
                .collect(),
            etag: Some("xiaomi-mimo-v2.5-fallback-v1".into()),
            not_modified: false,
        })
    }

    /// `/models` is the documented cheap credential check.  MiMo does not
    /// expose a documented machine-readable remaining-quota endpoint, so this
    /// must never be interpreted as a balance query.
    pub async fn test_connection(&self) -> Result<(), ProviderError> {
        self.transport.test_connection().await
    }

    pub async fn turn(&self, request: TurnRequest) -> Result<TurnResult, ProviderError> {
        self.turn_stream(request, |_| {}).await
    }

    pub async fn turn_stream<F>(&self, request: TurnRequest, on_event: F) -> Result<TurnResult, ProviderError>
    where
        F: FnMut(ProviderStreamEvent),
    {
        self.transport
            .turn_stream_with_profile(request, ChatRequestProfile::XiaomiMiMo, on_event)
            .await
    }
}

impl ProviderClient for MiMoClient {
    fn provider_id(&self) -> ProviderId {
        ProviderId::XiaomiMiMo
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
            let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
                continue;
            };
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
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn fixture(response_body: &'static str) -> (String, thread::JoinHandle<String>) {
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
            for chunk in response_body.as_bytes().chunks(13) {
                stream.write_all(chunk).expect("write fixture body");
            }
            request
        });
        (format!("http://{address}/v1"), join)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fallback_catalog_is_bounded_current_and_price_versioned() {
        let catalog = MiMoClient::new("https://example.invalid/v1", "secret")
            .fetch_models()
            .await
            .expect("fallback catalog");
        assert_eq!(catalog.models.len(), 2);
        assert!(catalog.models.iter().all(|model| !model.slug.contains("v2-pro")));
        let pro = catalog
            .models
            .iter()
            .find(|model| model.slug == "mimo-v2.5-pro")
            .expect("pro fallback");
        assert_eq!(pro.metadata["context_window"], 1_048_576);
        assert_eq!(pro.metadata["effective_context_window"], 996_147);
        assert_eq!(pro.metadata["pricing"]["version"], MIMO_PRICING_VERSION);
        assert!(
            (estimate_cost_usd("xiaomi/mimo-v2.5", 1_000_000, 500_000, 1_000_000).expect("price") - 0.3514)
                .abs()
                < f64::EPSILON
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn streaming_uses_documented_chat_completions_and_safe_thinking_mode() {
        let (base_url, server) = fixture(concat!(
            "data: {\"id\":\"mimo-1\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-1\",\"function\":{\"name\":\"search\",\"arguments\":\"{\\\"q\\\":\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"mimo\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":3}}\n\n",
            "data: [DONE]\n\n"
        ));
        let client = MiMoClient::new(base_url, "tp-fixture-secret");
        let result = client
            .turn(TurnRequest {
                model: "mimo-v2.5".into(),
                instructions: "work safely".into(),
                input: vec![json!({"type":"message","role":"user","content":[{"type":"input_text","text":"find docs"}]})],
                tools: vec![json!({"name":"search","parameters":{"type":"object"}})],
                ..TurnRequest::default()
            })
            .await
            .expect("MiMo fixture turn");
        assert_eq!(result.response_id.as_deref(), Some("mimo-1"));
        assert_eq!(result.tool_calls[0].arguments, "{\"q\":\"mimo\"}");
        let raw = server.join().expect("fixture server");
        assert!(raw.starts_with("POST /v1/chat/completions HTTP/1.1"));
        assert!(
            raw.to_ascii_lowercase()
                .contains("authorization: bearer tp-fixture-secret")
        );
        let body = raw.split_once("\r\n\r\n").expect("body").1;
        let body: Value = serde_json::from_str(body).expect("json body");
        assert_eq!(body["thinking"]["type"], "disabled");
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn debug_never_contains_the_key() {
        let client = MiMoClient::new("https://example.invalid/v1", "tp-secret");
        let debug = format!("{client:?}");
        assert!(!debug.contains("tp-secret"));
    }
}
