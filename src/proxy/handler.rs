use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures::StreamExt;

use super::SharedState;
use crate::config::ApiFormat;
use crate::error::AppError;
use crate::proxy::{forwarder, transform};

/// Bundles a provider's stable UUID and display name for passing through the request pipeline.
#[derive(Clone)]
struct ProviderKey {
    id: String,
    name: String,
}

/// Delta bag for a single DB upsert — avoids positional u64 parameter confusion.
#[derive(Clone, Debug, Default)]
struct StatsDelta {
    input: u64,
    output: u64,
    requests: u64,
    failures: u64,
}

/// Health check endpoint.
pub async fn health_check(State(state): State<SharedState>) -> impl IntoResponse {
    let config = state.config.read().await;
    let name = match config.current_provider() {
        Ok((name, _p)) => name.to_string(),
        Err(_) => "none".to_string(),
    };

    axum::Json(serde_json::json!({
        "status": "ok",
        "provider": name,
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

/// Handler for GET /v1/models — proxies to current provider and normalises to Anthropic format.
pub async fn handle_models(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let (provider, api_key) = {
        let config = state.config.read().await;
        let (_, p) = config.current_enabled_provider()?;
        let key = p.resolve_api_key()?;
        (p.clone(), key)
    };

    let base = provider.base_url.trim_end_matches('/');
    let url = format!("{base}/v1/models");

    let (auth_key, auth_val) = provider.auth_header(&api_key);

    let mut req = state.http_client.get(&url).header(auth_key, &auth_val);
    if provider.api_format == ApiFormat::Anthropic {
        req = req.header("anthropic-version", "2023-06-01");
        if let Some(beta) = headers.get("anthropic-beta") {
            req = req.header("anthropic-beta", beta);
        }
    }
    let response = req.send().await?;

    let status = response.status();
    if !status.is_success() {
        let body = response.bytes().await.unwrap_or_default();
        return Ok((status, [("content-type", "application/json")], body).into_response());
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

/// Build the candidate provider list and resolve the current provider's route.
/// Routes are per-provider model rewrites — they never change which provider is selected.
/// Returns `(pool, should_cycle, optional_routed_target)`.
async fn resolve_provider_pool(
    state: &SharedState,
    req_model: &str,
) -> Result<(Vec<(String, crate::config::Provider)>, bool, Option<String>), AppError> {
    let config = state.config.read().await;

    // Route lookup: only check the current provider's routes for model rewriting.
    let (_, current_provider) = config.current_enabled_provider()?;
    let routed_target = current_provider
        .routes
        .iter()
        .find(|r| r.matches(req_model))
        .map(|r| r.target.clone())
        .filter(|t| !t.is_empty());

    if config.fallback {
        let start_idx = config.providers.get_index_of(&config.current).unwrap_or(0);
        let len = config.providers.len();
        let list: Vec<(String, crate::config::Provider)> = (0..len)
            .map(|i| (start_idx + i) % len)
            .filter_map(|i| {
                config
                    .providers
                    .get_index(i)
                    .filter(|(_, v)| v.enabled)
                    .map(|(k, v)| (k.clone(), v.clone()))
            })
            .collect();
        Ok((list, true, routed_target))
    } else {
        Ok((
            vec![(config.current.clone(), current_provider.clone())],
            false,
            routed_target,
        ))
    }
}

/// Try each provider in the pool; cycle on retryable errors (5xx, 429, auth).
/// Returns the first successful response or a final error response.
async fn try_providers(
    state: &SharedState,
    pool: &[(String, crate::config::Provider)],
    do_cycle: bool,
    body: &Bytes,
    req_json: Option<&serde_json::Value>,
    headers: &HeaderMap,
    is_stream: bool,
) -> Result<Response, AppError> {
    let round_size = pool.len();
    let max_failures = round_size * 3;
    let mut consecutive_failures = 0usize;
    let mut last_status = None;
    let req_model_hint = req_json
        .and_then(|v| v.get("model").and_then(|m| m.as_str()))
        .unwrap_or("")
        .to_string();

    let record_failure = |state: &SharedState, pkey: &ProviderKey| {
        persist_stats(
            &state.db,
            pkey,
            None,
            StatsDelta {
                requests: 1,
                failures: 1,
                ..Default::default()
            },
        );
    };

    let record_error_metric = |state: &SharedState, name: &str, status: u16, msg: &str| {
        if let Ok(mut m) = state.metrics.lock() {
            m.record_error(name, status, &req_model_hint, msg);
        }
    };

    for (provider_name, provider) in pool.iter().cycle() {
        let pkey = ProviderKey {
            id: provider.id.clone(),
            name: provider_name.clone(),
        };

        let api_key = match provider.resolve_api_key() {
            Ok(k) => k,
            Err(e) => {
                tracing::warn!("Skipping provider {}: {e}", provider.base_url);
                record_failure(state, &pkey);
                record_error_metric(state, provider_name, 0, &e.to_string());
                consecutive_failures += 1;
                if !do_cycle || consecutive_failures >= max_failures {
                    break;
                }
                continue;
            }
        };

        let is_openai = provider.api_format == ApiFormat::OpenAI;
        let upstream_body = if is_openai {
            let request_json =
                req_json.ok_or_else(|| AppError::Transform("Invalid JSON body".into()))?;
            let transformed = transform::anthropic_to_openai_request(request_json, provider)?;
            Bytes::from(serde_json::to_vec(&transformed)?)
        } else {
            body.clone()
        };

        let response = match forwarder::forward_request(
            &state.http_client,
            provider,
            &api_key,
            upstream_body,
            headers,
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    "Provider {} network error: {e}, trying next",
                    provider.base_url
                );
                record_failure(state, &pkey);
                record_error_metric(state, provider_name, 0, &e.to_string());
                consecutive_failures += 1;
                if !do_cycle || consecutive_failures >= max_failures {
                    break;
                }
                continue;
            }
        };

        let status = response.status();
        let status_u16 = status.as_u16();

        // 5xx or 429: try next provider
        if status_u16 >= 500 || status_u16 == 429 {
            let error_body = response.bytes().await.unwrap_or_default();
            let preview = extract_error_message(&error_body);
            tracing::warn!(
                "Provider {} returned {status}, trying next",
                provider.base_url
            );
            record_failure(state, &pkey);
            record_error_metric(state, provider_name, status_u16, &preview);
            last_status = Some(status);
            consecutive_failures += 1;
            if !do_cycle || consecutive_failures >= max_failures {
                break;
            }
            continue;
        }

        // 401/403/404: auth error or model not found — try next provider in fallback mode
        if status_u16 == 401 || status_u16 == 403 || status_u16 == 404 {
            let error_body = response.bytes().await.unwrap_or_default();
            let preview = extract_error_message(&error_body);
            tracing::warn!(
                "Provider {} returned {status} ({}), trying next",
                provider.base_url,
                preview
            );
            record_failure(state, &pkey);
            record_error_metric(state, provider_name, status_u16, &preview);
            last_status = Some(status);
            consecutive_failures += 1;
            if !do_cycle || consecutive_failures >= max_failures {
                return Ok(
                    (status, [("content-type", "application/json")], error_body).into_response()
                );
            }
            continue;
        }

        // Other 4xx: client error (bad request format etc.), return immediately
        if !status.is_success() {
            let error_body = response.bytes().await.unwrap_or_default();
            let preview = extract_error_message(&error_body);
            tracing::warn!("Upstream returned {status}: {preview}");
            record_failure(state, &pkey);
            record_error_metric(state, provider_name, status_u16, &preview);
            return Ok((status, [("content-type", "application/json")], error_body).into_response());
        }

        if let Ok(mut m) = state.metrics.lock() {
            m.clear_error(provider_name);
        }
        return if is_stream {
            handle_streaming_response(response, is_openai, state.db.clone(), pkey).await
        } else {
            handle_buffered_response(response, is_openai, state.db.clone(), pkey).await
        };
    }

    Ok((
        last_status.unwrap_or(StatusCode::BAD_GATEWAY),
        [("content-type", "application/json")],
        Bytes::from(r#"{"error":"all providers failed"}"#),
    )
        .into_response())
}

/// Main handler for POST /v1/messages.
pub async fn handle_messages(
    State(state): State<SharedState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    // Parse body once to extract routing hints (model name + stream flag).
    let req_json = serde_json::from_slice::<serde_json::Value>(&body).ok();
    let is_stream = req_json
        .as_ref()
        .and_then(|v| v.get("stream").and_then(|s| s.as_bool()))
        .unwrap_or(false);
    let req_model = req_json
        .as_ref()
        .and_then(|v| v.get("model").and_then(|m| m.as_str()))
        .unwrap_or("")
        .to_string();

    let (pool, do_cycle, routed_target) = resolve_provider_pool(&state, &req_model).await?;

    // Patch body: rewrite `model` field with route target when applicable.
    let (body, req_json) = if let Some(target) = &routed_target {
        match req_json {
            Some(mut json) => {
                json["model"] = serde_json::Value::String(target.clone());
                let bytes =
                    Bytes::from(serde_json::to_vec(&json).unwrap_or_else(|_| body.to_vec()));
                (bytes, Some(json))
            }
            None => (body, None),
        }
    } else {
        (body, req_json)
    };

    try_providers(
        &state,
        &pool,
        do_cycle,
        &body,
        req_json.as_ref(),
        &headers,
        is_stream,
    )
    .await
}

/// Handle non-streaming response.
async fn handle_buffered_response(
    response: reqwest::Response,
    is_openai: bool,
    db: crate::db::SharedDb,
    pkey: ProviderKey,
) -> Result<Response, AppError> {
    let ProviderKey {
        id: provider_id,
        name: provider_name,
    } = pkey;
    let body = response.bytes().await?;

    // Parse once; extract tokens from the in-memory Value before serializing.
    let (response_body, usage_json) = if is_openai {
        let openai_json: serde_json::Value = serde_json::from_slice(&body)?;
        let anthropic_json = transform::openai_to_anthropic_response(&openai_json)?;
        let bytes = Bytes::from(serde_json::to_vec(&anthropic_json)?);
        (bytes, Some(anthropic_json))
    } else {
        let parsed = serde_json::from_slice::<serde_json::Value>(&body).ok();
        (body, parsed)
    };

    let (input, output, model) = if let Some(ref json) = usage_json {
        let input = json["usage"]["input_tokens"].as_u64().unwrap_or(0);
        let output = json["usage"]["output_tokens"].as_u64().unwrap_or(0);
        let model = json["model"].as_str().map(|s| s.to_string());
        (input, output, model)
    } else {
        (0, 0, None)
    };
    record_and_persist(
        &db,
        &ProviderKey {
            id: provider_id,
            name: provider_name,
        },
        model.as_deref(),
        input,
        output,
    );

    Ok((
        StatusCode::OK,
        [("content-type", "application/json")],
        response_body,
    )
        .into_response())
}

/// Persist token usage and request count to DB. TUI will reload from DB on next tick.
fn record_and_persist(
    db: &crate::db::SharedDb,
    pkey: &ProviderKey,
    model: Option<&str>,
    input: u64,
    output: u64,
) {
    persist_stats(
        db,
        pkey,
        model,
        StatsDelta {
            requests: 1,
            input,
            output,
            ..Default::default()
        },
    );
}

/// Fire-and-forget: persist provider and model deltas to DB outside the hot path.
fn persist_stats(
    db: &crate::db::SharedDb,
    pkey: &ProviderKey,
    model_name: Option<&str>,
    delta: StatsDelta,
) {
    let db = db.clone();
    let pkey = pkey.clone();
    let mid = model_name.map(|s| s.to_string());
    tokio::task::spawn_blocking(move || {
        if let Ok(mut conn) = db.lock() {
            let result = conn.transaction().and_then(|tx| {
                crate::db::upsert_provider(
                    &tx,
                    &pkey.id,
                    &pkey.name,
                    delta.input,
                    delta.output,
                    delta.requests,
                    delta.failures,
                )?;
                if let Some(ref model) = mid {
                    crate::db::upsert_model(
                        &tx,
                        &pkey.id,
                        &pkey.name,
                        model,
                        delta.input,
                        delta.output,
                    )?;
                }
                tx.commit()
            });
            if let Err(e) = result {
                tracing::warn!("Failed to persist stats for {}: {e}", pkey.name);
            }
        }
    });
}

/// Handle streaming response.
async fn handle_streaming_response(
    response: reqwest::Response,
    is_openai: bool,
    db: crate::db::SharedDb,
    pkey: ProviderKey,
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

    let tracked = track_tokens_in_stream(raw_stream, db, pkey);
    let body = Body::from_stream(tracked);

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(body)
        .map_err(|e| AppError::Transform(e.to_string()))
}

/// Wrap a byte stream to extract token usage from anthropic SSE events.
/// Passes all bytes through unchanged; records metrics when the stream ends.
fn track_tokens_in_stream(
    mut inner: std::pin::Pin<Box<dyn futures::Stream<Item = std::io::Result<Bytes>> + Send>>,
    db: crate::db::SharedDb,
    pkey: ProviderKey,
) -> impl futures::Stream<Item = std::io::Result<Bytes>> + Send {
    let provider_id = pkey.id;
    let provider_name = pkey.name;
    async_stream::stream! {
        const LINE_BUF_MAX: usize = 1024 * 1024; // 1 MB safety cap
        let mut input_tokens = 0u64;
        let mut output_tokens = 0u64;
        let mut model: Option<String> = None;
        let mut line_buf = String::new();

        while let Some(chunk) = inner.next().await {
            if let Ok(ref bytes) = chunk {
                if line_buf.len() + bytes.len() <= LINE_BUF_MAX {
                    line_buf.push_str(&String::from_utf8_lossy(bytes));
                } else {
                    // Abnormally large line — skip parsing, drain buffer to free memory.
                    line_buf.clear();
                }
                // Process complete SSE lines; single drain at the end.
                let mut start = 0;
                while let Some(rel) = line_buf[start..].find('\n') {
                    let pos = start + rel;
                    let line = line_buf[start..pos].trim_end_matches('\r');
                    if let Some(data) = line.strip_prefix("data: ") {
                        if let Ok(json) = serde_json::from_str::<serde_json::Value>(data) {
                            match json["type"].as_str() {
                                Some("message_start") => {
                                    input_tokens = json["message"]["usage"]["input_tokens"]
                                        .as_u64()
                                        .unwrap_or(0);
                                    if let Some(m) = json["message"]["model"].as_str() {
                                        model = Some(m.to_string());
                                    }
                                }
                                Some("message_delta") => {
                                    if let Some(it) = json["usage"]["input_tokens"].as_u64() {
                                        input_tokens = it;
                                    }
                                    output_tokens = json["usage"]["output_tokens"]
                                        .as_u64()
                                        .unwrap_or(0);
                                }
                                _ => {}
                            }
                        }
                    }
                    start = pos + 1;
                }
                if start > 0 {
                    line_buf.drain(..start);
                }
            }
            yield chunk;
        }

        // Stream ended: persist request count and token usage atomically.
        record_and_persist(
            &db,
            &ProviderKey { id: provider_id, name: provider_name },
            model.as_deref(),
            input_tokens,
            output_tokens,
        );
    }
}

/// Extract a short human-readable error summary from an upstream error body.
/// Tries to parse `error.message` from JSON; falls back to raw text preview.
fn extract_error_message(body: &[u8]) -> String {
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
