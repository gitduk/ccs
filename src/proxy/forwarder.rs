use axum::http::HeaderMap;
use bytes::Bytes;
use reqwest::Client;

use crate::config::{ApiFormat, Provider};
use crate::error::Result;

/// Headers that should NOT be forwarded to upstream.
const FILTERED_HEADERS: &[&str] = &[
    "host",
    "authorization",
    "x-api-key",
    "content-length",
    "transfer-encoding",
    "connection",
];

/// Forward a request to the upstream provider.
pub async fn forward_request(
    client: &Client,
    provider: &Provider,
    api_key: &str,
    body: Bytes,
    incoming_headers: &HeaderMap,
) -> Result<reqwest::Response> {
    let (url, auth_header_name, auth_header_value) = match provider.api_format {
        ApiFormat::Anthropic => (
            format!("{}/v1/messages", provider.base_url.trim_end_matches('/')),
            "x-api-key",
            api_key.to_string(),
        ),
        ApiFormat::OpenAI => (
            format!(
                "{}/chat/completions",
                provider.base_url.trim_end_matches('/')
            ),
            "authorization",
            format!("Bearer {api_key}"),
        ),
    };

    let mut request = client.post(&url);

    // Set auth header
    request = request.header(auth_header_name, auth_header_value);

    // Forward anthropic-specific headers for Anthropic format
    if provider.api_format == ApiFormat::Anthropic {
        if let Some(v) = incoming_headers.get("anthropic-version") {
            request = request.header("anthropic-version", v);
        }
        if let Some(v) = incoming_headers.get("anthropic-beta") {
            request = request.header("anthropic-beta", v);
        }
    }

    // Forward non-filtered headers
    for (name, value) in incoming_headers.iter() {
        let name_str = name.as_str().to_lowercase();
        if !FILTERED_HEADERS.contains(&name_str.as_str()) && !name_str.starts_with("anthropic-") {
            request = request.header(name, value);
        }
    }

    request = request.header("content-type", "application/json");
    request = request.body(body);

    let response = request.send().await?;
    Ok(response)
}
