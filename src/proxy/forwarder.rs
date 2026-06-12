use axum::http::HeaderMap;
use bytes::Bytes;
use reqwest::Client;

use crate::config::{ApiFormat, OpenAiApiVersion, Provider};
use crate::error::Result;

/// Headers that should NOT be forwarded to upstream.
/// `accept-encoding` is filtered because reqwest is built without compression
/// features: a forwarded `accept-encoding: gzip` makes the upstream compress
/// bodies that ccs can neither decompress (for transforms/logging) nor relay
/// correctly (the response is rebuilt without `content-encoding`).
const FILTERED_HEADERS: &[&str] = &[
    "host",
    "authorization",
    "x-api-key",
    "content-type",
    "content-length",
    "transfer-encoding",
    "connection",
    "accept-encoding",
];

fn should_forward_header(name: &str) -> bool {
    !FILTERED_HEADERS.contains(&name) && !name.starts_with("anthropic-")
}

/// Build the GET /v1/models request for a provider. Anthropic-format
/// providers get both x-api-key and Bearer auth because some proxies require
/// Bearer on /v1/models even when /v1/messages accepts x-api-key.
pub fn models_request(
    client: &Client,
    provider: &Provider,
    api_key: &str,
) -> reqwest::RequestBuilder {
    let base = provider.base_url.trim_end_matches('/');
    let (auth_key, auth_val) = provider.auth_header(api_key);
    let mut req = client
        .get(format!("{base}/v1/models"))
        .header(auth_key, auth_val);
    if provider.api_format == ApiFormat::Anthropic {
        req = req
            .header("anthropic-version", "2023-06-01")
            .header("authorization", format!("Bearer {api_key}"));
    }
    req
}

/// Forward a request to the upstream provider.
pub async fn forward_request(
    client: &Client,
    provider: &Provider,
    api_key: &str,
    body: Bytes,
    incoming_headers: &HeaderMap,
    openai_api_version: OpenAiApiVersion,
) -> Result<reqwest::Response> {
    let url = provider.endpoint_url(openai_api_version);
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
        if should_forward_header(n) {
            request = request.header(name, value);
        }
    }

    request = request.header("content-type", "application/json");
    request = request.body(body);

    let response = request.send().await?;
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::should_forward_header;

    #[test]
    fn filters_hop_auth_and_payload_headers() {
        for header in [
            "host",
            "authorization",
            "x-api-key",
            "content-type",
            "content-length",
            "transfer-encoding",
            "connection",
            "accept-encoding",
            "anthropic-version",
            "anthropic-beta",
        ] {
            assert!(
                !should_forward_header(header),
                "{header} should be filtered"
            );
        }
    }

    #[test]
    fn forwards_regular_headers() {
        assert!(should_forward_header("accept"));
        assert!(should_forward_header("user-agent"));
        assert!(should_forward_header("x-request-id"));
    }
}
