use std::time::{Duration, Instant};

use serde_json::json;

use crate::config::{ApiFormat, Provider};

const TEST_TIMEOUT_SECS: u64 = 10;
const TEST_MODEL: &str = "claude-haiku-4-5-20251001";

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
}

pub async fn test_connectivity(client: &reqwest::Client, provider: &Provider) -> TestResult {
    let api_key = match provider.resolve_api_key() {
        Ok(k) => k,
        Err(e) => {
            return TestResult {
                status: TestStatus::Error(format!("Key error: {e}")),
                latency_ms: 0,
                model_count: None,
                model_names: None,
                tested_at: Instant::now(),
            };
        }
    };
    let base = provider.base_url.trim_end_matches('/');

    let (msg_url, auth_header) = match provider.api_format {
        ApiFormat::Anthropic => (
            format!("{base}/v1/messages"),
            ("x-api-key".to_string(), api_key.clone()),
        ),
        ApiFormat::OpenAI => (
            format!("{base}/chat/completions"),
            ("authorization".to_string(), format!("Bearer {api_key}")),
        ),
    };

    let body = json!({
        "model": TEST_MODEL,
        "max_tokens": 1,
        "messages": [{"role": "user", "content": "hi"}]
    });

    let t0 = Instant::now();
    let response = client
        .post(&msg_url)
        .header(&auth_header.0, &auth_header.1)
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .timeout(Duration::from_secs(TEST_TIMEOUT_SECS))
        .send()
        .await;
    let latency_ms = t0.elapsed().as_millis() as u64;

    let status = match response {
        Err(e) => {
            return TestResult {
                status: TestStatus::Error(format!("Connection failed: {e}")),
                latency_ms,
                model_count: None,
                model_names: None,
                tested_at: Instant::now(),
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

    // Fetch model list from /v1/models.
    let (model_count, model_names) = async {
        let r = client
            .get(format!("{base}/v1/models"))
            .header(&auth_header.0, &auth_header.1)
            .header("anthropic-version", "2023-06-01")
            .timeout(Duration::from_secs(TEST_TIMEOUT_SECS))
            .send()
            .await
            .ok()?;
        if !r.status().is_success() { return None; }
        let json: serde_json::Value = r.json().await.ok()?;
        let arr = json["data"].as_array()?;
        let names: Vec<String> = arr.iter()
            .filter_map(|v| v["id"].as_str().map(|s| s.to_string()))
            .collect();
        Some((names.len(), names))
    }.await.map(|(c, n)| (Some(c), Some(n))).unwrap_or((None, None));

    TestResult {
        status: msg_status,
        latency_ms,
        model_count,
        model_names,
        tested_at: Instant::now(),
    }
}
