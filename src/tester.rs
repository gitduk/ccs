use std::time::{Duration, Instant};

use serde_json::json;

use crate::config::{ApiFormat, Provider};
use crate::proxy::executor::{probe_provider_message, probe_provider_message_with_body};

const TEST_TIMEOUT_SECS: u64 = 10;

#[derive(Debug, Clone)]
pub enum TestStatus {
    Ok,
    AuthFailed,
    Error(String),
}

#[derive(Debug, Clone)]
pub struct TestResult {
    pub status: TestStatus,
    pub latency_ms: u64,
    pub model_count: Option<usize>,
    pub model_names: Option<Vec<String>>,
    pub tested_at: Instant,
    /// The model name used for the connectivity test.
    pub used_model: String,
    /// Whether the provider accepted a request containing a tool definition.
    /// `None` means the check was not run (test failed before reaching this point).
    pub tools_supported: Option<bool>,
}

/// Run a latency test against the provider using `model`.
///
/// If `known_models` is supplied (e.g. pre-fetched by the caller), it is
/// attached directly to the result and the internal `/v1/models` fetch is
/// skipped, saving one extra HTTP request.
pub async fn test_latency(
    client: &reqwest::Client,
    provider: &Provider,
    model: String,
    known_models: Option<Vec<String>>,
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
                model_count: known_models.as_ref().map(|v| v.len()),
                model_names: known_models,
                tested_at,
                used_model: used_model.clone(),
                tools_supported: None,
            };
        }
    };

    let (status, latency_ms) =
        match probe_provider_message(client, provider, &api_key, &req_json).await {
            Ok((status, latency_ms)) => (status, latency_ms),
            Err(e) => {
                tracing::warn!("Provider test connection failed: {e}");
                return TestResult {
                    status: TestStatus::Error("Connection error".to_string()),
                    latency_ms: 0,
                    model_count: known_models.as_ref().map(|v| v.len()),
                    model_names: known_models,
                    tested_at,
                    used_model: used_model.clone(),
                    tools_supported: None,
                };
            }
        };

    let msg_status = if status.is_success() {
        TestStatus::Ok
    } else if status.as_u16() == 401 || status.as_u16() == 403 {
        TestStatus::AuthFailed
    } else {
        TestStatus::Error(format!("HTTP {}", status.as_u16()))
    };

    let base = provider.base_url.trim_end_matches('/');
    let auth_header = provider.auth_header(&api_key);

    // Run models fetch and tool-support probe concurrently.
    // Both are best-effort and don't affect status or latency.
    let (tools_supported, (model_count, model_names)) = tokio::join!(
        async {
            if matches!(msg_status, TestStatus::Ok) {
                check_tool_support(client, provider, &api_key, &used_model).await
            } else {
                None
            }
        },
        async {
            if let Some(models) = known_models {
                (Some(models.len()), Some(models))
            } else {
                fetch_models(client, base, &auth_header, &provider.api_format).await
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
    }
}

/// Fetch the model list for a provider. Returns an empty Vec on failure.
pub async fn fetch_provider_models(client: &reqwest::Client, provider: &Provider) -> Vec<String> {
    let api_key = match provider.resolve_api_key() {
        Ok(k) => k,
        Err(_) => return vec![],
    };
    let base = provider.base_url.trim_end_matches('/');
    let auth_header = provider.auth_header(&api_key);
    fetch_models(client, base, &auth_header, &provider.api_format)
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
/// Returns `Some(true)` if the response contains a structured tool call, `Some(false)` if the
/// provider rejected the request (4xx) or responded with text only, `None` on network error.
async fn check_tool_support(
    client: &reqwest::Client,
    provider: &Provider,
    api_key: &str,
    model: &str,
) -> Option<bool> {
    let req = json!({
        "model": model,
        "max_tokens": 64,
        "messages": [{"role": "user", "content": "call the noop tool"}],
        "tools": [{
            "name": "noop",
            "description": "no-op",
            "input_schema": {"type": "object", "properties": {}}
        }],
        // "any" converts to "required" for OpenAI providers via convert_tool_choice_to_openai.
        "tool_choice": {"type": "any"},
    });
    match probe_provider_message_with_body(client, provider, api_key, &req).await {
        Ok((status, body)) => {
            if !status.is_success() {
                return Some(false);
            }
            Some(response_body_has_tool_call(&body))
        }
        Err(_) => None,
    }
}

/// Returns true if a JSON response body contains actual structured tool-call content.
/// Handles Anthropic messages, OpenAI chat completions, and OpenAI Responses API formats.
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
    false
}

async fn fetch_models(
    client: &reqwest::Client,
    base: &str,
    auth_header: &(&str, String),
    api_format: &ApiFormat,
) -> (Option<usize>, Option<Vec<String>>) {
    let mut req = client
        .get(format!("{base}/v1/models"))
        .header(auth_header.0, &auth_header.1)
        .timeout(Duration::from_secs(TEST_TIMEOUT_SECS));
    if *api_format == ApiFormat::Anthropic {
        req = req.header("anthropic-version", "2023-06-01");
        // Some proxies require Bearer auth for /v1/models even when the messages
        // endpoint accepts x-api-key. Send both headers to cover both cases.
        req = req.header("authorization", format!("Bearer {}", auth_header.1));
    }
    let Ok(r) = req.send().await else {
        return (None, None);
    };
    if !r.status().is_success() {
        return (None, None);
    }
    let Ok(json) = r.json::<serde_json::Value>().await else {
        return (None, None);
    };
    let Some(arr) = json["data"].as_array() else {
        return (None, None);
    };
    let names: Vec<String> = arr
        .iter()
        .filter_map(|v| v["id"].as_str().map(|s| s.to_string()))
        .collect();
    (Some(names.len()), Some(names))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::net::TcpListener;

    use super::{TestStatus, test_latency};
    use crate::config::{ApiFormat, Provider};

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
            quota: None,
            quota_command: None,
            port: None,
        };

        let started = tokio::time::Instant::now();
        let result = test_latency(&reqwest::Client::new(), &provider, "m".into(), None).await;

        assert!(started.elapsed() < Duration::from_secs(12));
        assert!(matches!(result.status, TestStatus::Error(ref e) if e == "Connection error"));
    }
}
