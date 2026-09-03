use std::time::{Duration, Instant};

use serde_json::json;

use crate::config::{ApiFormat, OpenAiApiVersion, Provider};
use crate::proxy::executor::{
    extract_error_message, probe_provider_message, probe_provider_message_with_body,
};

const TEST_TIMEOUT_SECS: u64 = 10;

/// Result of successful API format auto-detection.
#[derive(Debug, Clone)]
pub struct DetectedFormat {
    pub api_format: ApiFormat,
    pub api_version: Option<OpenAiApiVersion>,
    /// Model list discovered while detecting, if any — reused by the caller
    /// so it doesn't need a redundant `/v1/models` round trip after saving.
    pub models: Vec<String>,
}

/// Build a throwaway probe `Provider` sharing only the fields format
/// detection needs. Never persisted — just a vehicle for the existing
/// request-building/forwarding code (which all takes a `&Provider`).
fn probe_provider(base_url: &str, api_key: &str, api_format: ApiFormat) -> Provider {
    Provider {
        id: String::new(),
        base_url: base_url.to_string(),
        api_key: api_key.to_string(),
        api_format,
        model_map: Default::default(),
        routes: vec![],
        enabled: true,
        fallback: true,
        api_version: None,
        inject_thinking_history: true,
        strict_thinking_history: false,
        quota_command: None,
        port: None,
        test_model: None,
        max_tokens_cap: None,
    }
}

/// Auto-detect which API format a provider's `base_url` speaks, by probing
/// candidates in priority order: Anthropic, OpenAI Chat Completions, OpenAI
/// Responses, Gemini. Returns the first candidate that completes a real
/// request successfully, or `None` if none of them do.
///
/// A real model name is required to tell "wrong format" apart from "right
/// format, unknown model" — a bogus model name gets rejected by every
/// format alike, so it can't discriminate anything. `test_model`, if given,
/// is used directly. Otherwise this fetches a model list first (trying
/// OpenAI's `/v1/models` before Anthropic's, since many Anthropic-compatible
/// relays don't expose a model-list endpoint at all — an already-known real
/// case is DeepSeek's Anthropic-compatible endpoint; Gemini uses its own
/// `/v1beta/models` catalog).
pub async fn detect_api_format(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    test_model: Option<&str>,
) -> Option<DetectedFormat> {
    let (model, models) = match test_model {
        Some(m) => (m.to_string(), vec![m.to_string()]),
        None => {
            // Side-effect-free GETs, unlike the completion probes below — run
            // the candidates concurrently instead of paying full latency each.
            let openai_probe = probe_provider(base_url, api_key, ApiFormat::OpenAI);
            let anthropic_probe = probe_provider(base_url, api_key, ApiFormat::Anthropic);
            let gemini_probe = probe_provider(base_url, api_key, ApiFormat::Gemini);
            let (openai_models, anthropic_models, gemini_models) = tokio::join!(
                fetch_provider_models(client, &openai_probe),
                fetch_provider_models(client, &anthropic_probe),
                fetch_provider_models(client, &gemini_probe),
            );
            if !openai_models.is_empty() {
                (openai_models[0].clone(), openai_models)
            } else if !anthropic_models.is_empty() {
                (anthropic_models[0].clone(), anthropic_models)
            } else if !gemini_models.is_empty() {
                // The catalog is unordered and mixes text, image, TTS and
                // embedding models; the probe needs a conversational one.
                let model = first_text_model(&gemini_models)
                    .unwrap_or(gemini_models[0].as_str())
                    .to_string();
                (model, gemini_models)
            } else {
                return None;
            }
        }
    };

    let req_json = json!({
        "model": model,
        "max_tokens": 1,
        "messages": [{"role": "user", "content": "ping"}],
    });

    let candidates = [
        (ApiFormat::Anthropic, None),
        (ApiFormat::OpenAI, Some(OpenAiApiVersion::ChatCompletions)),
        (ApiFormat::OpenAI, Some(OpenAiApiVersion::Responses)),
        (ApiFormat::Gemini, None),
    ];

    for (api_format, api_version) in candidates {
        let mut candidate = probe_provider(base_url, api_key, api_format);
        candidate.api_version = api_version.clone();
        match probe_provider_message(client, &candidate, api_key, &req_json).await {
            Ok((status, _)) if status.is_success() => {
                // A Responses probe that 404s internally falls back to Chat
                // Completions and still reports success — reflect what
                // actually worked, not the candidate we started with.
                let api_version = if matches!(api_version, Some(OpenAiApiVersion::Responses))
                    && crate::proxy::executor::responses_known_unsupported(&candidate)
                {
                    Some(OpenAiApiVersion::ChatCompletions)
                } else {
                    api_version
                };
                return Some(DetectedFormat {
                    api_format,
                    api_version,
                    models,
                });
            }
            _ => continue,
        }
    }

    None
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum TestStatus {
    Ok,
    AuthFailed,
    Error(String),
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TestResult {
    pub status: TestStatus,
    pub latency_ms: u64,
    pub model_count: Option<usize>,
    pub model_names: Option<Vec<String>>,
    #[serde(skip, default = "Instant::now")]
    pub tested_at: Instant,
    /// The model name used for the connectivity test.
    pub used_model: String,
    /// Whether the provider accepted a request containing a tool definition.
    /// `None` means the check was not run (test failed before reaching this point).
    pub tools_supported: Option<bool>,
    /// Whether the model accepted a request containing an image block.
    /// `None` means the check was not run (test failed before reaching this point).
    pub images_supported: Option<bool>,
}

/// Run a latency test against the provider using `model`.
///
/// `prefetched` is for callers that already fetched the catalog this round —
/// it is attached directly to the result and the internal `/v1/models` fetch is
/// skipped, saving one extra HTTP request. Pass `None` to have the test refresh
/// the catalog; never pass a cached copy, or the catalog can never go stale-free.
pub async fn test_latency(
    client: &reqwest::Client,
    provider: &Provider,
    model: String,
    prefetched: Option<Vec<String>>,
) -> TestResult {
    // Record the start time before any fallible work so all early-return
    // paths share the same reference point.
    let tested_at = Instant::now();

    let used_model = model.clone();
    let req_json = json!({
        "model": used_model,
        "max_tokens": 1,
        "messages": [{"role": "user", "content": "ping"}],
    });

    let api_key = match provider.resolve_api_key() {
        Ok(k) => k,
        Err(e) => {
            tracing::warn!("Provider test failed to resolve API key: {e}");
            return TestResult {
                status: TestStatus::Error("Configuration error".to_string()),
                latency_ms: 0,
                model_count: prefetched.as_ref().map(|v| v.len()),
                model_names: prefetched,
                tested_at,
                used_model: used_model.clone(),
                tools_supported: None,
                images_supported: None,
            };
        }
    };

    let (status, latency_ms, resp_body) =
        match probe_provider_message_with_body(client, provider, &api_key, &req_json).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("Provider test connection failed: {e}");
                return TestResult {
                    status: TestStatus::Error("Connection error".to_string()),
                    latency_ms: 0,
                    model_count: prefetched.as_ref().map(|v| v.len()),
                    model_names: prefetched,
                    tested_at,
                    used_model: used_model.clone(),
                    tools_supported: None,
                    images_supported: None,
                };
            }
        };

    let msg_status = if status.is_success() {
        TestStatus::Ok
    } else if status.as_u16() == 401 || status.as_u16() == 403 {
        TestStatus::AuthFailed
    } else {
        // Keep the status code and append the provider's own error message
        // (e.g. OpenRouter's 429 rate-limit text) so the failure is actionable.
        let detail = extract_error_message(&resp_body);
        if detail.is_empty() || detail.starts_with('<') {
            TestStatus::Error(format!("HTTP {}", status.as_u16()))
        } else {
            TestStatus::Error(format!("HTTP {}: {detail}", status.as_u16()))
        }
    };

    // Run models fetch and the capability probes concurrently.
    // All are best-effort and don't affect status or latency.
    let test_ok = matches!(msg_status, TestStatus::Ok);
    let (tools_supported, images_supported, (model_count, model_names)) = tokio::join!(
        async {
            if test_ok {
                check_tool_support(client, provider, &api_key, &used_model).await
            } else {
                None
            }
        },
        async {
            if test_ok {
                check_image_support(client, provider, &api_key, &used_model).await
            } else {
                None
            }
        },
        async {
            if let Some(models) = prefetched {
                (Some(models.len()), Some(models))
            } else {
                fetch_models(client, provider, &api_key).await
            }
        }
    );

    TestResult {
        status: msg_status,
        latency_ms,
        model_count,
        model_names,
        tested_at,
        used_model: req_json["model"].as_str().unwrap_or_default().to_string(),
        tools_supported,
        images_supported,
    }
}

/// Fetch the model list for a provider. Returns an empty Vec on failure.
pub async fn fetch_provider_models(client: &reqwest::Client, provider: &Provider) -> Vec<String> {
    let api_key = match provider.resolve_api_key() {
        Ok(k) => k,
        Err(_) => return vec![],
    };
    fetch_models(client, provider, &api_key)
        .await
        .1
        .unwrap_or_default()
}

/// Probe whether the provider supports tool calling by forcing a tool call and inspecting
/// the response body for actual structured tool-call output.
///
/// Uses Anthropic-format tools/tool_choice throughout: execute_provider_request's to_openai()
/// transform converts them to the correct upstream format (top-level "name" → OpenAI
/// function wrapper). Sending native OpenAI format would cause to_openai() to silently drop
/// all tools, defeating the probe.
///
/// A 4xx on the forced attempt retries once with `tool_choice` omitted (auto) —
/// some upstreams reject any forced value even though auto tool calls work fine.
///
/// Returns `Some(true)` if either attempt's response contains a structured tool call,
/// `Some(false)` if both attempts were rejected or responded with text only, `None` on
/// network error on either attempt.
async fn check_tool_support(
    client: &reqwest::Client,
    provider: &Provider,
    api_key: &str,
    model: &str,
) -> Option<bool> {
    let base_req = json!({
        "model": model,
        "max_tokens": 64,
        "messages": [{"role": "user", "content": "call the noop tool"}],
        "tools": [{
            "name": "noop",
            "description": "no-op",
            "input_schema": {"type": "object", "properties": {}}
        }],
    });

    let mut forced_req = base_req.clone();
    // "any" converts to "required" for OpenAI providers via convert_tool_choice_to_openai.
    forced_req["tool_choice"] = json!({"type": "any"});
    match probe_provider_message_with_body(client, provider, api_key, &forced_req).await {
        Ok((status, _, body)) if status.is_success() => {
            return Some(response_body_has_tool_call(&body));
        }
        Ok(_) => {}
        Err(_) => return None,
    }

    match probe_provider_message_with_body(client, provider, api_key, &base_req).await {
        Ok((status, _, body)) => {
            if !status.is_success() {
                return Some(false);
            }
            Some(response_body_has_tool_call(&body))
        }
        Err(_) => None,
    }
}

/// A 67-byte 1x1 grayscale PNG, the smallest payload that exercises image input.
const PROBE_PNG_BASE64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAAAAAA6fptVAAAACklEQVR4nGNgAAAAAgABSK+kcQAAAABJRU5ErkJggg==";

/// Probe whether the model accepts image input by sending a minimal image block.
///
/// Uses Anthropic-format content blocks: execute_provider_request's to_openai() transform
/// converts the image block to the upstream format (data URL for Chat Completions,
/// input_image for Responses), so the probe is meaningful for every provider format.
///
/// Acceptance is judged by status alone — non-vision models reject the request with 4xx
/// (e.g. "Model do not support image input"); the probe only runs after the plain-text
/// ping succeeded with the same model, so a 4xx here means the image was the problem.
/// Returns `None` on network error.
async fn check_image_support(
    client: &reqwest::Client,
    provider: &Provider,
    api_key: &str,
    model: &str,
) -> Option<bool> {
    let req = json!({
        "model": model,
        "max_tokens": 1,
        "messages": [{"role": "user", "content": [
            {"type": "image", "source": {
                "type": "base64",
                "media_type": "image/png",
                "data": PROBE_PNG_BASE64,
            }},
            {"type": "text", "text": "ping"}
        ]}],
    });
    match probe_provider_message(client, provider, api_key, &req).await {
        Ok((status, _)) => Some(status.is_success()),
        Err(_) => None,
    }
}

/// Returns true if a JSON response body contains actual structured tool-call content.
/// Handles Anthropic messages, OpenAI chat completions, OpenAI Responses API,
/// and Gemini Interactions (`steps[*].type == "function_call"`).
fn response_body_has_tool_call(body: &[u8]) -> bool {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return false;
    };
    // Anthropic: content[*].type == "tool_use"
    if v.get("content")
        .and_then(|c| c.as_array())
        .is_some_and(|arr| {
            arr.iter()
                .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
        })
    {
        return true;
    }
    // OpenAI Chat Completions: choices[*].message.tool_calls non-empty
    if v.get("choices")
        .and_then(|c| c.as_array())
        .is_some_and(|arr| {
            arr.iter().any(|c| {
                c.get("message")
                    .and_then(|m| m.get("tool_calls"))
                    .and_then(|tc| tc.as_array())
                    .is_some_and(|calls| !calls.is_empty())
            })
        })
    {
        return true;
    }
    // OpenAI Responses API: output[*].type == "function_call"
    if v.get("output")
        .and_then(|o| o.as_array())
        .is_some_and(|arr| {
            arr.iter()
                .any(|item| item.get("type").and_then(|t| t.as_str()) == Some("function_call"))
        })
    {
        return true;
    }
    // Gemini Interactions: steps[*].type == "function_call"
    if v.get("steps")
        .and_then(|s| s.as_array())
        .is_some_and(|arr| {
            arr.iter()
                .any(|s| s.get("type").and_then(|t| t.as_str()) == Some("function_call"))
        })
    {
        return true;
    }
    false
}

async fn fetch_models(
    client: &reqwest::Client,
    provider: &Provider,
    api_key: &str,
) -> (Option<usize>, Option<Vec<String>>) {
    let req = crate::proxy::forwarder::models_request(client, provider, api_key)
        .timeout(Duration::from_secs(TEST_TIMEOUT_SECS));
    let Ok(r) = req.send().await else {
        return (None, None);
    };

    if r.status() == reqwest::StatusCode::NOT_FOUND
        && let Some(root) = crate::proxy::forwarder::root_base_url(&provider.base_url)
    {
        let fallback_req =
            crate::proxy::forwarder::models_request_at(client, provider, api_key, &root)
                .timeout(Duration::from_secs(TEST_TIMEOUT_SECS));
        if let Ok(r2) = fallback_req.send().await {
            return parse_models_response(r2).await;
        }
    }

    parse_models_response(r).await
}

async fn parse_models_response(r: reqwest::Response) -> (Option<usize>, Option<Vec<String>>) {
    if !r.status().is_success() {
        return (None, None);
    }
    let Ok(json) = r.json::<serde_json::Value>().await else {
        return (None, None);
    };
    // OpenAI/Anthropic catalogs list `data[].id`; Gemini's `/v1beta/models`
    // lists `models[].name` with a `models/` prefix.
    if let Some(models) = json["models"].as_array() {
        let names: Vec<String> = models
            .iter()
            .filter_map(|m| {
                m["name"].as_str().map(|n| {
                    n.strip_prefix("models/")
                        .unwrap_or(n)
                        .to_string()
                })
            })
            .collect();
        return (Some(names.len()), Some(names));
    }
    let Some(arr) = json["data"].as_array() else {
        return (None, None);
    };
    let names: Vec<String> = arr
        .iter()
        .filter_map(|v| v["id"].as_str().map(|s| s.to_string()))
        .collect();
    (Some(names.len()), Some(names))
}

/// First model in a Gemini catalog that a chat probe can use. The catalog is
/// unordered and lists image, TTS, embedding and audio models too; a probe
/// against one of those fails regardless of the key's validity. Falls back to
/// the first entry when nothing looks conversational.
fn first_text_model(models: &[String]) -> Option<&str> {
    let non_chat = ["-image", "-tts", "embedding", "-audio", "-search"];
    models
        .iter()
        .find(|m| !non_chat.iter().any(|s| m.contains(s)))
        .map(|m| m.as_str())
        .or_else(|| models.first().map(|m| m.as_str()))
}

#[cfg(test)]
pub(crate) mod tests {
    use std::time::Duration;

    use axum::Router;
    use tokio::net::TcpListener;

    use super::{TestStatus, check_tool_support, test_latency};
    use crate::config::{ApiFormat, Provider};

    fn test_provider(base_url: String) -> Provider {
        Provider {
            id: "test-id".into(),
            base_url,
            api_key: "test-key".into(),
            api_format: ApiFormat::OpenAI,
            model_map: Default::default(),
            routes: vec![],
            enabled: true,
            fallback: true,
            api_version: Some(crate::config::OpenAiApiVersion::ChatCompletions),
            inject_thinking_history: true,
            strict_thinking_history: false,
            quota_command: None,
            port: None,
            test_model: None,
            max_tokens_cap: None,
        }
    }

    /// Bind a test server and serve `app` in the background; returns its address.
    pub(crate) async fn spawn_test_server(app: Router) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        addr
    }

    /// Regression test for the `openc` false negative: a backend that rejects any forced
    /// `tool_choice` (both `"required"` and a named-function choice) with 4xx, but honors
    /// tool calling fine under the default `auto` choice. `check_tool_support` must fall
    /// back to the unforced attempt instead of reporting "no tool support".
    #[tokio::test]
    async fn check_tool_support_falls_back_when_forced_choice_is_rejected() {
        use axum::http::StatusCode;
        use axum::response::IntoResponse;
        use axum::routing::post;
        use axum::Json;

        async fn chat(body: axum::body::Bytes) -> impl IntoResponse {
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
            if v.get("tool_choice").is_some() {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error": {"message": "Upstream request failed"}})),
                )
                    .into_response();
            }
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "choices": [{"message": {"tool_calls": [
                        {"id": "call_0", "type": "function", "function": {"name": "noop", "arguments": "{}"}}
                    ]}}]
                })),
            )
                .into_response()
        }

        let addr = spawn_test_server(Router::new().route("/v1/chat/completions", post(chat))).await;

        let provider = test_provider(format!("http://{addr}"));
        let result = check_tool_support(&reqwest::Client::new(), &provider, "test-key", "m").await;
        assert_eq!(result, Some(true));
    }

    /// Regression test for the DeepSeek-style asymmetric gateway: `/v1/models`
    /// 404s under the provider's configured path (however deeply nested) but
    /// is reachable at the bare scheme+host root. `fetch_models` must retry
    /// there instead of reporting no models.
    #[tokio::test]
    async fn fetch_models_falls_back_to_root_domain_on_404() {
        use axum::extract::Path;
        use axum::http::StatusCode;
        use axum::response::IntoResponse;
        use axum::routing::get;

        async fn nested_404(Path(_rest): Path<String>) -> impl IntoResponse {
            StatusCode::NOT_FOUND
        }
        async fn root_models() -> impl IntoResponse {
            axum::Json(serde_json::json!({
                "data": [{"id": "deepseek-v4-flash"}, {"id": "deepseek-v4-pro"}]
            }))
        }

        let addr = spawn_test_server(
            Router::new()
                // Two levels deep — proves the fallback goes straight to the
                // root, not just one segment up.
                .route("/{*rest}", get(nested_404))
                .route("/v1/models", get(root_models)),
        )
        .await;

        let provider = test_provider(format!("http://{addr}/org123/anthropic"));
        let (count, names) = super::fetch_models(&reqwest::Client::new(), &provider, "key").await;

        assert_eq!(count, Some(2));
        assert_eq!(
            names,
            Some(vec![
                "deepseek-v4-flash".to_string(),
                "deepseek-v4-pro".to_string()
            ])
        );
    }

    #[tokio::test]
    async fn test_latency_times_out_and_returns_connection_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = listener.accept().await;
            tokio::time::sleep(Duration::from_secs(30)).await;
        });

        let provider = Provider {
            id: "test-id".into(),
            base_url: format!("http://{}", addr),
            api_key: "test-key".into(),
            api_format: ApiFormat::OpenAI,
            model_map: Default::default(),
            routes: vec![],
            enabled: true,
            fallback: true,
            api_version: None,
            inject_thinking_history: true,
            strict_thinking_history: false,
            quota_command: None,
            port: None,
            test_model: None,
            max_tokens_cap: None,
        };

        let started = tokio::time::Instant::now();
        let result = test_latency(&reqwest::Client::new(), &provider, "m".into(), None).await;

        assert!(started.elapsed() < Duration::from_secs(12));
        assert!(matches!(result.status, TestStatus::Error(ref e) if e == "Connection error"));
    }

    #[tokio::test]
    async fn test_latency_includes_upstream_error_message_on_429() {
        use axum::http::StatusCode;
        use axum::response::IntoResponse;
        use axum::routing::post;
        use axum::Json;

        async fn chat() -> impl IntoResponse {
            (
                StatusCode::TOO_MANY_REQUESTS,
                Json(serde_json::json!({
                    "error": {
                        "message": "You have exceeded the rate limit for this model. Please retry in 60 seconds."
                    }
                })),
            )
        }

        let addr = spawn_test_server(Router::new().route("/v1/chat/completions", post(chat))).await;

        let provider = test_provider(format!("http://{addr}"));
        let result = test_latency(&reqwest::Client::new(), &provider, "m".into(), None).await;

        assert!(matches!(
            result.status,
            TestStatus::Error(ref e)
                if e == "HTTP 429: You have exceeded the rate limit for this model. Please retry in 60 seconds."
        ));
    }

    /// Which of the four endpoints a scenario server accepts (200) vs
    /// rejects (400) — `detect_api_format` only looks at status codes
    /// (see `execute_provider_request`), so bodies don't need real shapes.
    /// `pub(crate)`: reused by `tui::testing`'s tests, not just this module's.
    #[derive(Clone, Copy, Default)]
    pub(crate) struct Scenario {
        pub(crate) messages_ok: bool,
        pub(crate) chat_ok: bool,
        pub(crate) responses_ok: bool,
        pub(crate) models_ok: bool,
    }

    pub(crate) async fn spawn_scenario_server(scenario: Scenario) -> String {
        use axum::extract::State;
        use axum::http::StatusCode;
        use axum::response::IntoResponse;
        use axum::routing::{get, post};
        use axum::{Json, Router};
        use std::sync::Arc;

        fn resp(ok: bool) -> impl IntoResponse {
            if ok {
                (StatusCode::OK, Json(serde_json::json!({"ok": true})))
            } else {
                (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error": {"message": "nope"}})),
                )
            }
        }

        async fn messages(State(s): State<Arc<Scenario>>) -> impl IntoResponse {
            resp(s.messages_ok)
        }
        async fn chat(State(s): State<Arc<Scenario>>) -> impl IntoResponse {
            resp(s.chat_ok)
        }
        async fn responses(State(s): State<Arc<Scenario>>) -> impl IntoResponse {
            resp(s.responses_ok)
        }
        async fn models(State(s): State<Arc<Scenario>>) -> impl IntoResponse {
            if s.models_ok {
                (
                    StatusCode::OK,
                    Json(serde_json::json!({"object": "list", "data": [{"id": "probe-model"}]})),
                )
            } else {
                (StatusCode::NOT_FOUND, Json(serde_json::json!({})))
            }
        }

        let app = Router::new()
            .route("/v1/messages", post(messages))
            .route("/v1/chat/completions", post(chat))
            .route("/v1/responses", post(responses))
            .route("/v1/models", get(models))
            .with_state(Arc::new(scenario));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn detect_api_format_prefers_anthropic_when_all_succeed() {
        let base = spawn_scenario_server(Scenario {
            messages_ok: true,
            chat_ok: true,
            responses_ok: true,
            models_ok: true,
        })
        .await;

        let detected = super::detect_api_format(&reqwest::Client::new(), &base, "key", None).await;

        assert!(matches!(
            detected,
            Some(super::DetectedFormat {
                api_format: ApiFormat::Anthropic,
                api_version: None,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn detect_api_format_falls_back_to_chat_completions() {
        let base = spawn_scenario_server(Scenario {
            messages_ok: false,
            chat_ok: true,
            responses_ok: true,
            models_ok: true,
        })
        .await;

        let detected = super::detect_api_format(&reqwest::Client::new(), &base, "key", None).await;

        assert!(matches!(
            detected,
            Some(super::DetectedFormat {
                api_format: ApiFormat::OpenAI,
                api_version: Some(crate::config::OpenAiApiVersion::ChatCompletions),
                ..
            })
        ));
    }

    #[tokio::test]
    async fn detect_api_format_falls_back_to_responses() {
        let base = spawn_scenario_server(Scenario {
            messages_ok: false,
            chat_ok: false,
            responses_ok: true,
            models_ok: true,
        })
        .await;

        let detected = super::detect_api_format(&reqwest::Client::new(), &base, "key", None).await;

        assert!(matches!(
            detected,
            Some(super::DetectedFormat {
                api_format: ApiFormat::OpenAI,
                api_version: Some(crate::config::OpenAiApiVersion::Responses),
                ..
            })
        ));
    }

    #[tokio::test]
    async fn detect_api_format_none_when_nothing_works() {
        let base = spawn_scenario_server(Scenario {
            messages_ok: false,
            chat_ok: false,
            responses_ok: false,
            models_ok: true,
        })
        .await;

        let detected = super::detect_api_format(&reqwest::Client::new(), &base, "key", None).await;
        assert!(detected.is_none());
    }

    #[tokio::test]
    async fn detect_api_format_none_without_models_list_or_test_model() {
        // No /v1/models on either format and no test_model given: there's no
        // real model name to probe with, so detection can't proceed at all.
        let base = spawn_scenario_server(Scenario {
            messages_ok: true,
            chat_ok: true,
            responses_ok: true,
            models_ok: false,
        })
        .await;

        let detected = super::detect_api_format(&reqwest::Client::new(), &base, "key", None).await;
        assert!(detected.is_none());
    }

    #[tokio::test]
    async fn detect_api_format_uses_explicit_test_model_without_models_list() {
        let base = spawn_scenario_server(Scenario {
            messages_ok: true,
            chat_ok: true,
            responses_ok: true,
            models_ok: false,
        })
        .await;

        let detected =
            super::detect_api_format(&reqwest::Client::new(), &base, "key", Some("my-model")).await;

        assert!(matches!(
            detected,
            Some(super::DetectedFormat {
                api_format: ApiFormat::Anthropic,
                ..
            })
        ));
    }
    #[test]
    fn first_text_model_skips_non_conversational_catalog_entries() {
        // A realistic Gemini catalog slice: image/TTS models come first, the
        // conversational model is buried further down.
        let models = vec![
            "gemini-3.1-flash-image".to_string(),
            "gemini-2.5-flash-preview-tts".to_string(),
            "gemini-2.5-flash".to_string(),
            "gemini-2.5-pro".to_string(),
        ];
        assert_eq!(super::first_text_model(&models), Some("gemini-2.5-flash"));
        // Falls back to the head when nothing looks conversational.
        let only_image = vec!["gemini-3.1-flash-image".to_string()];
        assert_eq!(
            super::first_text_model(&only_image),
            Some("gemini-3.1-flash-image")
        );
    }
}
