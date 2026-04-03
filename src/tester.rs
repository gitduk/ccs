use std::time::{Duration, Instant};

use crate::config::{ApiFormat, Provider};

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

    let api_key = match provider.resolve_api_key() {
        Ok(k) => k,
        Err(e) => {
            return TestResult {
                status: TestStatus::Error(format!("Key error: {e}")),
                latency_ms: 0,
                model_count: None,
                model_names: known_models,
                tested_at,
                used_model: model,
            };
        }
    };
    let base = provider.base_url.trim_end_matches('/');
    let auth_header = provider.auth_header(&api_key);
    // Use the shared URL/body builder — same logic as build_test_curl in the TUI.
    let (msg_url, body) = provider.chat_url_and_body(&model);

    let t0 = Instant::now();
    let mut req = client
        .post(&msg_url)
        .header(auth_header.0, &auth_header.1)
        .header("content-type", "application/json");
    if provider.api_format == ApiFormat::Anthropic {
        req = req.header("anthropic-version", "2023-06-01");
    }
    let response = req
        .body(body)
        .timeout(Duration::from_secs(TEST_TIMEOUT_SECS))
        .send()
        .await;
    let latency_ms = t0.elapsed().as_millis() as u64;

    let status = match response {
        Err(e) => {
            return TestResult {
                status: TestStatus::Error(format!("Connection failed: {e}")),
                latency_ms,
                model_count: known_models.as_ref().map(|v| v.len()),
                model_names: known_models,
                tested_at,
                used_model: model,
            };
        }
        Ok(r) => r.status(),
    };

    let msg_status = if status.is_success() {
        TestStatus::Ok
    } else if status.as_u16() == 401 || status.as_u16() == 403 {
        TestStatus::AuthFailed
    } else {
        TestStatus::Error(format!("HTTP {}", status.as_u16()))
    };

    // Use the pre-fetched model list when available; otherwise fetch now
    // (best-effort, does not affect status or latency).
    let (model_count, model_names) = if let Some(models) = known_models {
        (Some(models.len()), Some(models))
    } else {
        fetch_models(client, base, &auth_header, &provider.api_format).await
    };

    TestResult {
        status: msg_status,
        latency_ms,
        model_count,
        model_names,
        tested_at,
        used_model: model,
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
