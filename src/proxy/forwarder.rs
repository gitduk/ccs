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
    let base = provider.base_url.trim_end_matches('/');
    let url = match provider.api_format {
        ApiFormat::Anthropic => format!("{base}/v1/messages"),
        ApiFormat::OpenAI => {
            // Prefer new Responses API by default
            match provider.openai_api_version() {
                "chat_completions" => format!("{base}/v1/chat/completions"),
                _ => format!("{base}/v1/responses"), // Default to Responses API
            }
        }
    };
    let (auth_key, auth_val) = provider.auth_header(api_key);

    let mut request = client.post(&url);
    request = request.header(auth_key, auth_val);

    // Forward anthropic-specific headers for Anthropic format
    if provider.api_format == ApiFormat::Anthropic {
        if let Some(v) = incoming_headers.get("anthropic-version") {
            request = request.header("anthropic-version", v);
        }
        if let Some(v) = incoming_headers.get("anthropic-beta") {
            request = request.header("anthropic-beta", v);
        }
    }

    // Forward non-filtered headers (HeaderName is already lowercase)
    for (name, value) in incoming_headers.iter() {
        let n = name.as_str();
        if !FILTERED_HEADERS.contains(&n) && !n.starts_with("anthropic-") {
            request = request.header(name, value);
        }
    }

    request = request.header("content-type", "application/json");
    request = request.body(body);

    let response = request.send().await?;
    Ok(response)
}
