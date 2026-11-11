use axum::http::{HeaderMap, HeaderName, HeaderValue};
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

/// Attach a provider's configured `extra_headers` to an outgoing request
/// builder, resolving `$ENV_VAR` values at send time. Header names or values
/// that are invalid HTTP header syntax are skipped with a warning rather than
/// panicking inside reqwest.
fn apply_extra_headers(
    mut request: reqwest::RequestBuilder,
    provider: &Provider,
) -> reqwest::RequestBuilder {
    if provider.extra_headers.is_empty() {
        return request;
    }
    for (name, value) in provider.resolved_extra_headers() {
        match (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(&value),
        ) {
            (Ok(n), Ok(v)) => request = request.header(n, v),
            _ => tracing::warn!("extra_header {name} rejected: invalid header name or value"),
        }
    }
    request
}

/// Build the GET `{base_url}/models` request for a provider. Anthropic-format
/// providers get both x-api-key and Bearer auth because some proxies require
/// Bearer on `/models` even when `/messages` accepts x-api-key.
pub fn models_request(
    client: &Client,
    provider: &Provider,
    api_key: &str,
) -> reqwest::RequestBuilder {
    models_request_at(client, provider, api_key, &provider.base_url)
}

/// Like `models_request`, but probes `base_url` directly instead of the
/// provider's configured base — used by the `/models` root-domain retry.
pub fn models_request_at(
    client: &Client,
    provider: &Provider,
    api_key: &str,
    base_url: &str,
) -> reqwest::RequestBuilder {
    let base = base_url.trim_end_matches('/');
    let (auth_key, auth_val) = provider.auth_header(api_key);
    // The catalog sits beside the chat endpoint, below whatever version
    // segment `base_url` already carries (`/v1/models`, `/v1beta/models`, …).
    let mut req = client
        .get(format!("{base}/models"))
        .header(auth_key, auth_val);
    if provider.api_format == ApiFormat::Anthropic {
        req = req
            .header("anthropic-version", "2023-06-01")
            .header("authorization", format!("Bearer {api_key}"));
    }
    apply_extra_headers(req, provider)
}

/// The scheme+host(+port) root of `base_url`; `None` if `base_url` is
/// already bare (nothing to strip) or fails to parse.
pub fn root_base_url(base_url: &str) -> Option<String> {
    let root = reqwest::Url::parse(base_url)
        .ok()?
        .origin()
        .ascii_serialization();
    (root != base_url.trim_end_matches('/')).then_some(root)
}

/// Forward a request to the upstream provider.
///
/// `stream` selects Gemini's SSE form (`?alt=sse` on the interactions
/// endpoint); it is ignored by the Anthropic and OpenAI paths.
pub async fn forward_request(
    client: &Client,
    provider: &Provider,
    api_key: &str,
    body: Bytes,
    incoming_headers: &HeaderMap,
    openai_api_version: OpenAiApiVersion,
    stream: bool,
) -> Result<reqwest::Response> {
    let mut url = provider.endpoint_url(openai_api_version);
    if provider.api_format == ApiFormat::Gemini && stream {
        url.push_str("?alt=sse");
    }
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

    request = apply_extra_headers(request, provider);

    let response = request.send().await?;
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ApiFormat;
    use std::collections::HashMap;

    fn provider_with_extra_headers(extra: HashMap<String, String>) -> Provider {
        Provider {
            id: "test-id".into(),
            base_url: "https://api.example.com".into(),
            api_key: "key".into(),
            api_format: ApiFormat::Anthropic,
            model_map: HashMap::new(),
            routes: Vec::new(),
            enabled: true,
            fallback: false,
            api_version: None,
            inject_thinking_history: true,
            strict_thinking_history: false,
            quota_command: None,
            port: None,
            test_model: None,
            max_tokens_cap: None,
            extra_headers: extra,
        }
    }

    #[test]
    fn models_request_keeps_the_base_url_version() {
        let mut provider = provider_with_extra_headers(HashMap::new());
        provider.base_url = "https://api.example.com/v1".into();
        let req = models_request(&Client::new(), &provider, "key")
            .build()
            .unwrap();
        assert_eq!(req.url().as_str(), "https://api.example.com/v1/models");
    }

    #[test]
    fn models_request_injects_extra_headers() {
        let client = Client::new();
        let mut extra = HashMap::new();
        extra.insert("x-opencode-session".into(), "sess-123".into());
        let provider = provider_with_extra_headers(extra);
        let req = models_request(&client, &provider, "key").build().unwrap();
        assert_eq!(req.headers().get("x-opencode-session").unwrap(), "sess-123");
    }

    #[test]
    fn models_request_without_extra_headers_has_none() {
        let client = Client::new();
        let provider = provider_with_extra_headers(HashMap::new());
        let req = models_request(&client, &provider, "key").build().unwrap();
        assert!(req.headers().get("x-opencode-session").is_none());
    }

    #[test]
    fn invalid_extra_header_names_and_values_are_skipped() {
        let client = Client::new();
        let mut extra = HashMap::new();
        extra.insert("x-ok".into(), "fine".into());
        extra.insert("x-bad name".into(), "v".into()); // space in header name
        extra.insert("x-newline".into(), "v\nbad".into()); // newline in value
        let provider = provider_with_extra_headers(extra);
        let req = models_request(&client, &provider, "key").build().unwrap();
        assert_eq!(req.headers().get("x-ok").unwrap(), "fine");
        assert!(req.headers().get("x-bad name").is_none());
        assert!(req.headers().get("x-newline").is_none());
    }
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
