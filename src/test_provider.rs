use serde_json::json;

use crate::config::{ApiFormat, Provider};

/// Test provider connectivity. Returns a human-readable result string.
pub async fn test_connectivity(provider: &Provider) -> String {
    let api_key = match provider.resolve_api_key() {
        Ok(k) => k,
        Err(e) => return format!("Config error: {e}"),
    };

    let client = reqwest::Client::new();

    let (url, request_body, auth_header) = match provider.api_format {
        ApiFormat::Anthropic => {
            let url = format!("{}/v1/messages", provider.base_url.trim_end_matches('/'));
            let body = json!({
                "model": "claude-haiku-4-5-20251001",
                "max_tokens": 1,
                "messages": [{"role": "user", "content": "hi"}]
            });
            (url, body, ("x-api-key".to_string(), api_key))
        }
        ApiFormat::OpenAI => {
            let url = format!("{}/chat/completions", provider.base_url.trim_end_matches('/'));
            let body = json!({
                "model": "claude-haiku-4-5-20251001",
                "max_tokens": 1,
                "messages": [{"role": "user", "content": "hi"}]
            });
            (url, body, ("authorization".to_string(), format!("Bearer {api_key}")))
        }
    };

    let response = match client
        .post(&url)
        .header(&auth_header.0, &auth_header.1)
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .json(&request_body)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return format!("Connection failed: {e}"),
    };

    let status = response.status();
    if status.is_success() {
        format!("{} — OK (HTTP {})", provider.base_url, status.as_u16())
    } else if status.as_u16() == 401 || status.as_u16() == 403 {
        format!(
            "{} — Connected but auth failed (HTTP {}). Check API key.",
            provider.base_url,
            status.as_u16()
        )
    } else {
        let body = response.text().await.unwrap_or_default();
        let short = if body.len() > 200 { &body[..200] } else { &body };
        format!(
            "{} — HTTP {} — {}",
            provider.base_url,
            status.as_u16(),
            short,
        )
    }
}
