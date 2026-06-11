use std::collections::HashSet;
use std::sync::{LazyLock, RwLock};

use axum::http::{HeaderMap, StatusCode};
use bytes::Bytes;

use crate::config::{ApiFormat, OpenAiApiVersion, Provider};
use crate::error::AppError;
use crate::proxy::{forwarder, transform};

/// Providers discovered at runtime to not support `/v1/responses` (an
/// endpoint-style 404 followed by a successful `/v1/chat/completions` retry).
/// Cached so later requests skip the doomed Responses round-trip. Keyed by
/// id + base_url so editing the provider's URL re-probes.
static RESPONSES_UNSUPPORTED: LazyLock<RwLock<HashSet<String>>> =
    LazyLock::new(|| RwLock::new(HashSet::new()));

fn responses_support_key(provider: &Provider) -> String {
    format!("{}|{}", provider.id, provider.base_url)
}

fn responses_known_unsupported(provider: &Provider) -> bool {
    RESPONSES_UNSUPPORTED
        .read()
        .map(|set| set.contains(&responses_support_key(provider)))
        .unwrap_or(false)
}

fn mark_responses_unsupported(provider: &Provider) {
    if let Ok(mut set) = RESPONSES_UNSUPPORTED.write() {
        set.insert(responses_support_key(provider));
    }
}

#[derive(Debug)]
pub(crate) enum ProviderRequestOutcome {
    Success {
        response: reqwest::Response,
        latency_ms: u64,
    },
    UpstreamError {
        status: StatusCode,
        body: Bytes,
        latency_ms: u64,
    },
}

impl ProviderRequestOutcome {
    pub(crate) fn status(&self) -> StatusCode {
        match self {
            Self::Success { response, .. } => response.status(),
            Self::UpstreamError { status, .. } => *status,
        }
    }

    pub(crate) fn latency_ms(&self) -> u64 {
        match self {
            Self::Success { latency_ms, .. } | Self::UpstreamError { latency_ms, .. } => {
                *latency_ms
            }
        }
    }

    /// Consume the outcome, yielding the error body bytes and latency.
    /// For `Success` the body is read from the response (errors read as empty).
    pub(crate) async fn into_error_parts(self) -> (Bytes, u64) {
        match self {
            Self::UpstreamError {
                body, latency_ms, ..
            } => (body, latency_ms),
            Self::Success {
                response,
                latency_ms,
            } => (response.bytes().await.unwrap_or_default(), latency_ms),
        }
    }
}

pub(crate) async fn execute_provider_request(
    client: &reqwest::Client,
    provider: &Provider,
    api_key: &str,
    body: &Bytes,
    req_json: Option<&serde_json::Value>,
    headers: &HeaderMap,
) -> Result<ProviderRequestOutcome, AppError> {
    let t0 = std::time::Instant::now();
    let initial_api_version = if provider.api_format == ApiFormat::OpenAI {
        match provider.openai_api_version_enum() {
            // A previous request proved /v1/responses is missing upstream.
            OpenAiApiVersion::Responses if responses_known_unsupported(provider) => {
                Some(OpenAiApiVersion::ChatCompletions)
            }
            v => Some(v),
        }
    } else {
        None
    };
    let request_json = if initial_api_version.is_some() {
        Some(req_json.ok_or_else(|| AppError::Transform("Invalid JSON body".into()))?)
    } else {
        None
    };
    let make_openai_body = |api_version: OpenAiApiVersion| -> Result<Bytes, AppError> {
        let transformed = transform::to_openai(
            request_json.expect("OpenAI request JSON should exist"),
            provider,
            api_version,
        )?;
        Ok(Bytes::from(serde_json::to_vec(&transformed)?))
    };

    let upstream_body = match initial_api_version.as_ref() {
        Some(api_version) => make_openai_body(api_version.clone())?,
        None => {
            let patched = req_json.and_then(|req| {
                let after_model_map = transform::map_anthropic_model(req, provider);
                if provider.inject_thinking_history {
                    let base = after_model_map.as_ref().unwrap_or(req);
                    transform::patch_thinking_history(base).or(after_model_map)
                } else {
                    after_model_map
                }
            });
            match patched {
                Some(v) => Bytes::from(serde_json::to_vec(&v)?),
                None => body.clone(),
            }
        }
    };

    let mut response = match initial_api_version.as_ref() {
        Some(api_version) => {
            forwarder::forward_request(
                client,
                provider,
                api_key,
                upstream_body,
                headers,
                api_version.clone(),
            )
            .await?
        }
        None => {
            forwarder::forward_request(
                client,
                provider,
                api_key,
                upstream_body,
                headers,
                OpenAiApiVersion::Responses,
            )
            .await?
        }
    };

    if matches!(initial_api_version, Some(OpenAiApiVersion::Responses))
        && response.status() == StatusCode::NOT_FOUND
    {
        let not_found_body = response.bytes().await?;
        if should_fallback_from_responses_404(&not_found_body) {
            tracing::warn!(
                "Provider {} returned endpoint-style 404 for /v1/responses, retrying /v1/chat/completions",
                provider.base_url
            );
            response = forwarder::forward_request(
                client,
                provider,
                api_key,
                make_openai_body(OpenAiApiVersion::ChatCompletions)?,
                headers,
                OpenAiApiVersion::ChatCompletions,
            )
            .await?;
            // Only cache once the retry proves chat/completions works, so a
            // transient or misleading 404 can't pin the wrong endpoint.
            if response.status().is_success() {
                mark_responses_unsupported(provider);
            }
        } else {
            return Ok(ProviderRequestOutcome::UpstreamError {
                status: StatusCode::NOT_FOUND,
                body: not_found_body,
                latency_ms: t0.elapsed().as_millis() as u64,
            });
        }
    }

    Ok(ProviderRequestOutcome::Success {
        response,
        latency_ms: t0.elapsed().as_millis() as u64,
    })
}

pub(crate) async fn probe_provider_message(
    client: &reqwest::Client,
    provider: &Provider,
    api_key: &str,
    req_json: &serde_json::Value,
) -> Result<(StatusCode, u64), AppError> {
    let body = Bytes::from(serde_json::to_vec(req_json)?);
    let headers = HeaderMap::new();
    let outcome =
        execute_provider_request(client, provider, api_key, &body, Some(req_json), &headers)
            .await?;
    Ok((outcome.status(), outcome.latency_ms()))
}

/// Like `probe_provider_message` but also returns the response body.
/// Used to inspect the response content for capability detection.
pub(crate) async fn probe_provider_message_with_body(
    client: &reqwest::Client,
    provider: &Provider,
    api_key: &str,
    req_json: &serde_json::Value,
) -> Result<(StatusCode, Bytes), AppError> {
    let body = Bytes::from(serde_json::to_vec(req_json)?);
    let headers = HeaderMap::new();
    let outcome =
        execute_provider_request(client, provider, api_key, &body, Some(req_json), &headers)
            .await?;
    let status = outcome.status();
    let resp_body = match outcome {
        ProviderRequestOutcome::Success { response, .. } => {
            response.bytes().await.unwrap_or_default()
        }
        ProviderRequestOutcome::UpstreamError { body, .. } => body,
    };
    Ok((status, resp_body))
}

fn should_fallback_from_responses_404(body: &[u8]) -> bool {
    let message = extract_error_message(body).to_ascii_lowercase();
    if message.is_empty() {
        return false;
    }

    let deny = [
        "model",
        "deployment",
        "resource",
        "permission",
        "api key",
        "unauthorized",
        "forbidden",
    ];
    if deny.iter().any(|term| message.contains(term)) {
        return false;
    }

    let allow = [
        "endpoint not found",
        "unknown endpoint",
        "path not found",
        "route not found",
        "unsupported route",
        "does not support /v1/responses",
        "/v1/responses not found",
    ];
    allow.iter().any(|term| message.contains(term))
}

pub(crate) fn extract_error_message(body: &[u8]) -> String {
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) {
        if let Some(msg) = v
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
        {
            return msg.chars().take(120).collect();
        }
        if let Some(msg) = v.get("message").and_then(|m| m.as_str()) {
            return msg.chars().take(120).collect();
        }
    }
    String::from_utf8_lossy(&body[..body.len().min(120)])
        .trim()
        .to_string()
}
