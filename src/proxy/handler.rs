use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures::StreamExt;

use super::SharedState;
use crate::config::ApiFormat;
use crate::error::AppError;
use crate::proxy::{forwarder, metrics::SharedMetrics, transform};

/// Health check endpoint.
pub async fn health_check(State(state): State<SharedState>) -> impl IntoResponse {
    let config = state.config.read().await;
    let id = match config.current_provider() {
        Ok((id, _p)) => id.to_string(),
        Err(_) => "none".to_string(),
    };

    axum::Json(serde_json::json!({
        "status": "ok",
        "provider": id,
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

/// Reload configuration from disk.
pub async fn reload_config(State(state): State<SharedState>) -> impl IntoResponse {
    match crate::config::load_config() {
        Ok(fresh_config) => {
            let mut config = state.config.write().await;
            *config = fresh_config;
            (
                StatusCode::OK,
                axum::Json(serde_json::json!({
                    "status": "ok",
                    "message": "Configuration reloaded"
                }))
            )
        }
        Err(e) => {
            tracing::error!("Failed to reload config: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(serde_json::json!({
                    "status": "error",
                    "message": "Failed to reload configuration"
                }))
            )
        }
    }
}

/// Handler for GET /v1/models — proxies to current provider and normalises to Anthropic format.
pub async fn handle_models(State(state): State<SharedState>) -> Result<Response, AppError> {
    let (provider, api_key) = {
        let config = state.config.read().await;
        let (_, p) = config.current_provider()?;
        let key = p.resolve_api_key()?;
        (p.clone(), key)
    };

    let base = provider.base_url.trim_end_matches('/');
    let url = format!("{base}/v1/models");

    let (auth_key, auth_val) = match provider.api_format {
        ApiFormat::Anthropic => ("x-api-key".to_string(), api_key),
        ApiFormat::OpenAI => ("authorization".to_string(), format!("Bearer {api_key}")),
    };

    let response = state
        .http_client
        .get(&url)
        .header(&auth_key, &auth_val)
        .header("anthropic-version", "2023-06-01")
        .send()
        .await?;

    let status = response.status();
    if !status.is_success() {
        let body = response.bytes().await.unwrap_or_default();
        return Ok((
            StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
            [("content-type", "application/json")],
            body,
        )
            .into_response());
    }

    let body = response.bytes().await?;
    let response_body = if provider.api_format == ApiFormat::OpenAI {
        let openai_json: serde_json::Value = serde_json::from_slice(&body)?;
        let anthropic_json = transform::openai_to_anthropic_models(&openai_json);
        Bytes::from(serde_json::to_vec(&anthropic_json)?)
    } else {
        body
    };

    Ok((
        StatusCode::OK,
        [("content-type", "application/json")],
        response_body,
    )
        .into_response())
}

/// Main handler for POST /v1/messages.
pub async fn handle_messages(
    State(state): State<SharedState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    // Build the candidate provider list
    let (pool, do_cycle) = {
        let config = state.config.read().await;
        if config.fallback {
            let current_idx = config.providers.get_index_of(&config.current).unwrap_or(0);
            let len = config.providers.len();
            let list: Vec<(String, crate::config::Provider)> = (0..len)
                .map(|i| (current_idx + i) % len)
                .filter_map(|i| config.providers.get_index(i).map(|(k, v)| (k.clone(), v.clone())))
                .collect();
            (list, true)
        } else {
            let (id, p) = config.current_provider()?;
            (vec![(id.to_string(), p.clone())], false)
        }
    };

    let is_stream = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("stream").and_then(|s| s.as_bool()))
        .unwrap_or(false);

    // Cycle infinitely; terminate when a full round of consecutive failures occurs
    let round_size = pool.len();
    let max_failures = round_size * 3;
    let mut consecutive_failures = 0usize;
    let mut last_status = None;
    for (provider_id, provider) in pool.iter().cycle() {
        let api_key = match provider.resolve_api_key() {
            Ok(k) => k,
            Err(e) => {
                tracing::warn!("Skipping provider {}: {e}", provider.base_url);
                if let Ok(mut m) = state.metrics.lock() {
                    m.record_request(provider_id);
                    m.record_failure(provider_id);
                }
                consecutive_failures += 1;
                if !do_cycle || consecutive_failures >= max_failures {
                    break;
                }
                continue;
            }
        };

        // Record request count before forwarding (immediate feedback in TUI)
        if let Ok(mut m) = state.metrics.lock() { m.record_request(provider_id); }

        let is_openai = provider.api_format == ApiFormat::OpenAI;
        let upstream_body = if is_openai {
            let request_json: serde_json::Value = serde_json::from_slice(&body)?;
            let transformed = transform::anthropic_to_openai_request(&request_json, provider)?;
            Bytes::from(serde_json::to_vec(&transformed)?)
        } else {
            body.clone()
        };

        let response = match forwarder::forward_request(
            &state.http_client,
            provider,
            &api_key,
            upstream_body,
            &headers,
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("Provider {} network error: {e}, trying next", provider.base_url);
                if let Ok(mut m) = state.metrics.lock() { m.record_failure(provider_id); }
                consecutive_failures += 1;
                if !do_cycle || consecutive_failures >= max_failures {
                    break;
                }
                continue;
            }
        };

        let status = response.status();

        // 5xx or 429: try next provider
        if status.as_u16() >= 500 || status.as_u16() == 429 {
            tracing::warn!("Provider {} returned {status}, trying next", provider.base_url);
            if let Ok(mut m) = state.metrics.lock() { m.record_failure(provider_id); }
            last_status = Some(status);
            consecutive_failures += 1;
            if !do_cycle || consecutive_failures >= max_failures {
                break;
            }
            continue;
        }

        // 401/403: auth error — try next provider in fallback mode
        if status.as_u16() == 401 || status.as_u16() == 403 {
            tracing::warn!("Provider {} auth error {status}, trying next", provider.base_url);
            if let Ok(mut m) = state.metrics.lock() { m.record_failure(provider_id); }
            last_status = Some(status);
            consecutive_failures += 1;
            if !do_cycle || consecutive_failures >= max_failures {
                let error_body = response.bytes().await.unwrap_or_default();
                return Ok((
                    StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::UNAUTHORIZED),
                    [("content-type", "application/json")],
                    error_body,
                )
                    .into_response());
            }
            continue;
        }

        // Other 4xx: client error (bad request format etc.), return immediately
        if !status.is_success() {
            let error_body = response.bytes().await?;
            let preview = String::from_utf8_lossy(&error_body[..error_body.len().min(200)]);
            tracing::warn!("Upstream returned {status}: {preview}");
            if let Ok(mut m) = state.metrics.lock() { m.record_failure(provider_id); }
            return Ok((
                StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
                [("content-type", "application/json")],
                error_body,
            )
                .into_response());
        }

        return if is_stream {
            handle_streaming_response(response, is_openai, state.metrics.clone(), state.db.clone(), provider_id.clone()).await
        } else {
            handle_buffered_response(response, is_openai, state.metrics.clone(), state.db.clone(), provider_id.clone()).await
        };
    }

    let code = last_status.unwrap_or(StatusCode::BAD_GATEWAY);
    Ok((
        StatusCode::from_u16(code.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
        [("content-type", "application/json")],
        Bytes::from(r#"{"error":"all providers failed"}"#),
    )
        .into_response())
}

/// Handle non-streaming response.
async fn handle_buffered_response(
    response: reqwest::Response,
    is_openai: bool,
    metrics: SharedMetrics,
    db: crate::db::SharedDb,
    provider_id: String,
) -> Result<Response, AppError> {
    let body = response.bytes().await?;

    let response_body = if is_openai {
        let openai_json: serde_json::Value = serde_json::from_slice(&body)?;
        let anthropic_json = transform::openai_to_anthropic_response(&openai_json)?;
        Bytes::from(serde_json::to_vec(&anthropic_json)?)
    } else {
        body
    };

    // Parse token usage and model from the anthropic-format response body.
    if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&response_body) {
        let input = json["usage"]["input_tokens"].as_u64().unwrap_or(0);
        let output = json["usage"]["output_tokens"].as_u64().unwrap_or(0);
        if input > 0 || output > 0 {
            let model = json["model"].as_str().map(|s| s.to_string());
            let (provider_stats, model_stats) = {
                if let Ok(mut m) = metrics.lock() {
                    m.record_tokens(input, output, &provider_id);
                    if let Some(ref name) = model {
                        m.record_model_tokens(input, output, name);
                    }
                    let ps = m.by_provider.get(&provider_id).cloned();
                    let ms = model.as_deref().and_then(|n| m.by_model.get(n).cloned());
                    (ps, ms)
                } else {
                    (None, None)
                }
            };
            persist_stats(&db, &provider_id, provider_stats, model.as_deref(), model_stats);
        }
    }

    Ok((
        StatusCode::OK,
        [("content-type", "application/json")],
        response_body,
    )
        .into_response())
}

/// Fire-and-forget: persist provider and model stats to DB outside hot path.
fn persist_stats(
    db: &crate::db::SharedDb,
    provider_id: &str,
    provider_stats: Option<crate::proxy::metrics::ProviderStats>,
    model_name: Option<&str>,
    model_stats: Option<crate::proxy::metrics::ModelStats>,
) {
    let db = db.clone();
    let pid = provider_id.to_string();
    let mid = model_name.map(|s| s.to_string());
    tokio::task::spawn_blocking(move || {
        if let Ok(conn) = db.lock() {
            if let Some(s) = provider_stats {
                let _ = crate::db::upsert_provider(&conn, &pid, &s);
            }
            if let (Some(name), Some(s)) = (mid, model_stats) {
                let _ = crate::db::upsert_model(&conn, &name, &s);
            }
        }
    });
}

/// Handle streaming response.
async fn handle_streaming_response(
    response: reqwest::Response,
    is_openai: bool,
    metrics: SharedMetrics,
    db: crate::db::SharedDb,
    provider_id: String,
) -> Result<Response, AppError> {
    let raw_stream: std::pin::Pin<Box<dyn futures::Stream<Item = std::io::Result<Bytes>> + Send>> =
        if !is_openai {
            Box::pin(response.bytes_stream().map(|r| {
                r.map_err(|e| {
                    tracing::error!("Stream error: {e}");
                    std::io::Error::other(e)
                })
            }))
        } else {
            Box::pin(transform::openai_stream_to_anthropic(response))
        };

    let tracked = track_tokens_in_stream(raw_stream, metrics, db, provider_id);
    let body = Body::from_stream(tracked);

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(body)
        .unwrap())
}

/// Wrap a byte stream to extract token usage from anthropic SSE events.
/// Passes all bytes through unchanged; records metrics when the stream ends.
fn track_tokens_in_stream(
    mut inner: std::pin::Pin<Box<dyn futures::Stream<Item = std::io::Result<Bytes>> + Send>>,
    metrics: SharedMetrics,
    db: crate::db::SharedDb,
    provider_id: String,
) -> impl futures::Stream<Item = std::io::Result<Bytes>> + Send {
    async_stream::stream! {
        let mut input_tokens = 0u64;
        let mut output_tokens = 0u64;
        let mut model = String::new();
        let mut line_buf = String::new();

        while let Some(chunk) = inner.next().await {
            if let Ok(ref bytes) = chunk {
                line_buf.push_str(&String::from_utf8_lossy(bytes));
                // Process complete SSE lines to extract token counts and model.
                while let Some(pos) = line_buf.find('\n') {
                    let line = line_buf[..pos].trim_end_matches('\r').to_owned();
                    line_buf.drain(..=pos);
                    if let Some(data) = line.strip_prefix("data: ") {
                        if let Ok(json) = serde_json::from_str::<serde_json::Value>(data) {
                            match json["type"].as_str() {
                                Some("message_start") => {
                                    input_tokens = json["message"]["usage"]["input_tokens"]
                                        .as_u64()
                                        .unwrap_or(0);
                                    if let Some(m) = json["message"]["model"].as_str() {
                                        model = m.to_string();
                                    }
                                }
                                Some("message_delta") => {
                                    output_tokens = json["usage"]["output_tokens"]
                                        .as_u64()
                                        .unwrap_or(0);
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
            yield chunk;
        }

        // Stream ended: persist the token counts we collected.
        if input_tokens > 0 || output_tokens > 0 {
            let (provider_stats, model_stats) = {
                if let Ok(mut m) = metrics.lock() {
                    m.record_tokens(input_tokens, output_tokens, &provider_id);
                    if !model.is_empty() {
                        m.record_model_tokens(input_tokens, output_tokens, &model);
                    }
                    let ps = m.by_provider.get(&provider_id).cloned();
                    let ms = if model.is_empty() { None } else { m.by_model.get(&model).cloned() };
                    (ps, ms)
                } else {
                    (None, None)
                }
            };
            let model_opt = if model.is_empty() { None } else { Some(model.as_str()) };
            persist_stats(&db, &provider_id, provider_stats, model_opt, model_stats);
        }
    }
}
