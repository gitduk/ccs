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
    pub tested_at: Instant,
}

impl TestResult {
    pub fn status_str(&self) -> &str {
        match &self.status {
            TestStatus::Ok => "OK",
            TestStatus::AuthFailed => "Auth failed",
            TestStatus::Error(e) => e.as_str(),
        }
    }

    pub fn is_ok(&self) -> bool {
        matches!(self.status, TestStatus::Ok)
    }
}

pub async fn test_connectivity(provider: &Provider) -> TestResult {
    let api_key = match provider.resolve_api_key() {
        Ok(k) => k,
        Err(e) => {
            return TestResult {
                status: TestStatus::Error(format!("Key error: {e}")),
                latency_ms: 0,
                model_count: None,
                tested_at: Instant::now(),
            };
        }
    };

    let client = reqwest::Client::new();
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

    let body = match provider.api_format {
        ApiFormat::Anthropic => json!({
            "model": TEST_MODEL,
            "max_tokens": 1,
            "messages": [{"role": "user", "content": "hi"}]
        }),
        ApiFormat::OpenAI => json!({
            "model": TEST_MODEL,
            "max_tokens": 1,
            "messages": [{"role": "user", "content": "hi"}]
        }),
    };

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

    // Fetch model count from /v1/models.
    let model_count = async {
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
        json["data"].as_array().map(|a| a.len())
    }.await;

    TestResult {
        status: msg_status,
        latency_ms,
        model_count,
        tested_at: Instant::now(),
    }
}
