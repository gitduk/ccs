use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures::StreamExt;

use super::SharedState;
use crate::config::ApiFormat;
use crate::error::AppError;
use crate::proxy::executor::{
    ProviderRequestOutcome, execute_provider_request, extract_error_message,
};
use crate::proxy::metrics::{RequestLogEntry, SharedRequestLog};
use crate::proxy::transform;

/// Bundles a provider's stable UUID and display name for passing through the request pipeline.
#[derive(Clone)]
struct ProviderKey {
    id: String,
    name: String,
}

use crate::repo::{Repository, StatsDelta};

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
) -> Result<
    (
        Vec<(String, crate::config::Provider)>,
        bool,
        Option<String>,
        String,
    ),
    AppError,
> {
    let config = state.config.read().await;

    // Route lookup: only check the current provider's routes for model rewriting.
    let (_, current_provider) = config.current_enabled_provider()?;
    let (routed_target, route_pattern) = current_provider
        .routes
        .iter()
        .find(|r| r.matches(req_model))
        .map(|r| {
            let target = if r.target.is_empty() {
                None
            } else {
                Some(r.target.clone())
            };
            let pattern = if target.is_some() {
                r.pattern.clone()
            } else {
                String::new()
            };
            (target, pattern)
        })
        .unwrap_or((None, String::new()));

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
        Ok((list, true, routed_target, route_pattern))
    } else {
        Ok((
            vec![(config.current.clone(), current_provider.clone())],
            false,
            routed_target,
            route_pattern,
        ))
    }
}

/// Request context bundled to keep [`try_providers`] argument count in check.
struct RequestCtx<'a> {
    body: &'a Bytes,
    req_json: Option<&'a serde_json::Value>,
    headers: &'a HeaderMap,
    is_stream: bool,
    route_pattern: &'a str,
}

/// Try each provider in the pool; cycle on retryable errors (5xx, 429, auth).
/// Returns the first successful response or a final error response.
async fn try_providers(
    state: &SharedState,
    pool: &[(String, crate::config::Provider)],
    do_cycle: bool,
    ctx: &RequestCtx<'_>,
) -> Result<Response, AppError> {
    let request_log_limit = state.config.read().await.request_log_limit;
    let round_size = pool.len();
    let max_failures = round_size * 3;
    let max_auth_failures = round_size.max(1);
    let mut consecutive_failures = 0usize;
    let mut auth_failures = 0usize;
    let mut last_status = None;
    let mut last_error_body: Option<Bytes> = None;
    let req_model_hint = ctx
        .req_json
        .and_then(|v| v.get("model").and_then(|m| m.as_str()))
        .unwrap_or("")
        .to_string();

    let record_failure = |state: &SharedState, pkey: &ProviderKey| {
        state.db.persist_stats_async(
            &pkey.id,
            &pkey.name,
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
            m.record_error(name, status, &req_model_hint, ctx.route_pattern, msg);
        }
    };

    let t0 = std::time::Instant::now();

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

        let outcome = match execute_provider_request(
            &state.http_client,
            provider,
            &api_key,
            ctx.body,
            ctx.req_json,
            ctx.headers,
        )
        .await
        {
            Ok(outcome) => outcome,
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

        let status = outcome.status();
        let status_u16 = status.as_u16();

        // 5xx or 429: try next provider
        if status_u16 >= 500 || status_u16 == 429 {
            let error_body = match outcome {
                ProviderRequestOutcome::UpstreamError { body, .. } => body,
                ProviderRequestOutcome::Success { response, .. } => {
                    response.bytes().await.unwrap_or_default()
                }
            };
            let preview = extract_error_message(&error_body);
            tracing::warn!(
                "Provider {} returned {status}, trying next",
                provider.base_url
            );
            record_failure(state, &pkey);
            record_error_metric(state, provider_name, status_u16, &preview);
            last_status = Some(status);
            last_error_body = Some(error_body);
            consecutive_failures += 1;
            if !do_cycle || consecutive_failures >= max_failures {
                break;
            }
            continue;
        }

        // 401/403/404: auth error or model not found — try next provider in fallback mode
        if status_u16 == 401 || status_u16 == 403 || status_u16 == 404 {
            let error_body = match outcome {
                ProviderRequestOutcome::UpstreamError { body, .. } => body,
                ProviderRequestOutcome::Success { response, .. } => {
                    response.bytes().await.unwrap_or_default()
                }
            };
            let preview = extract_error_message(&error_body);
            tracing::warn!(
                "Provider {} returned {status} ({}), trying next",
                provider.base_url,
                preview
            );
            record_failure(state, &pkey);
            record_error_metric(state, provider_name, status_u16, &preview);
            last_status = Some(status);
            last_error_body = Some(error_body.clone());
            auth_failures += 1;
            if !do_cycle || auth_failures >= max_auth_failures {
                return Ok(
                    (status, [("content-type", "application/json")], error_body).into_response()
                );
            }
            continue;
        }

        // Other 4xx: client error (bad request format etc.), return immediately
        if !status.is_success() {
            let error_body = match outcome {
                ProviderRequestOutcome::UpstreamError { body, .. } => body,
                ProviderRequestOutcome::Success { response, .. } => {
                    response.bytes().await.unwrap_or_default()
                }
            };
            let preview = extract_error_message(&error_body);
            tracing::warn!("Upstream returned {status}: {preview}");
            record_failure(state, &pkey);
            record_error_metric(state, provider_name, status_u16, &preview);
            return Ok((status, [("content-type", "application/json")], error_body).into_response());
        }

        let latency_ms = outcome.latency_ms();
        if let Ok(mut m) = state.metrics.lock() {
            m.clear_error(provider_name);
            m.by_provider
                .entry(provider_name.clone())
                .or_default()
                .latency_total += latency_ms;
        }
        // Log successful requests — tokens will be filled in by the response handlers,
        // but we log the entry here for latency and provider info. For buffered responses,
        // handle_buffered_response updates the log entry with token counts.
        let initial_entry = RequestLogEntry {
            id: 0, // assigned by push()
            timestamp: std::time::SystemTime::now(),
            provider: provider_name.clone(),
            model: req_model_hint.clone(),
            status: status_u16,
            latency_ms,
            input_tokens: 0,
            output_tokens: 0,
            is_stream: ctx.is_stream,
            error: None,
        };
        let entry_id = if let Ok(mut log) = state.request_log.lock() {
            let id = log.push(initial_entry.clone());
            if ctx.is_stream {
                // For streaming, persist now with zero tokens; tokens are back-filled
                // via update_request_log_tokens_async once the stream ends.
                let mut persisted = initial_entry.clone();
                persisted.id = id;
                state
                    .db
                    .persist_request_log_async(persisted, request_log_limit);
            }
            // For buffered responses, the full entry (with tokens) is persisted inside
            // handle_buffered_response — no two-phase write needed.
            id
        } else {
            0
        };
        let response = match outcome {
            ProviderRequestOutcome::Success { response, .. } => response,
            ProviderRequestOutcome::UpstreamError { body, .. } => {
                // Defensive: this branch should not be reached for successful statuses,
                // but if it is, log and return the error body rather than panic.
                tracing::error!(
                    "BUG: Successful status {} but got UpstreamError outcome",
                    status
                );
                return Ok((status, [("content-type", "application/json")], body).into_response());
            }
        };
        return if ctx.is_stream {
            handle_streaming_response(
                response,
                provider.api_format == ApiFormat::OpenAI,
                StreamTrackingCtx {
                    db: state.db.clone(),
                    provider_id: pkey.id,
                    provider_name: pkey.name,
                    request_log: state.request_log.clone(),
                    entry_id,
                    latency: latency_ms,
                    request_log_limit,
                },
            )
            .await
        } else {
            handle_buffered_response(
                response,
                provider.api_format == ApiFormat::OpenAI,
                state.db.clone(),
                pkey,
                &state.request_log,
                initial_entry,
                request_log_limit,
            )
            .await
        };
    }

    // All providers failed — log the final failure.
    let final_status = last_status.unwrap_or(StatusCode::BAD_GATEWAY);
    let failed_entry = RequestLogEntry {
        id: 0,
        timestamp: std::time::SystemTime::now(),
        provider: pool.first().map(|(n, _)| n.clone()).unwrap_or_default(),
        model: req_model_hint,
        status: final_status.as_u16(),
        latency_ms: t0.elapsed().as_millis() as u64,
        input_tokens: 0,
        output_tokens: 0,
        is_stream: ctx.is_stream,
        error: Some("all providers failed".into()),
    };
    if let Ok(mut log) = state.request_log.lock() {
        let id = log.push(failed_entry.clone());
        let mut persisted = failed_entry;
        persisted.id = id;
        state
            .db
            .persist_request_log_async(persisted, request_log_limit);
    }

    let body =
        last_error_body.unwrap_or_else(|| Bytes::from(r#"{"error":"all providers failed"}"#));
    Ok((final_status, [("content-type", "application/json")], body).into_response())
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

    let (pool, do_cycle, routed_target, route_pattern) =
        resolve_provider_pool(&state, &req_model).await?;

    // Patch body: rewrite `model` field with route target when applicable.
    let (body, req_json) = if let Some(target) = &routed_target {
        match req_json {
            Some(mut json) => {
                json["model"] = serde_json::Value::String(target.clone());
                match serde_json::to_vec(&json) {
                    Ok(vec) => (Bytes::from(vec), Some(json)),
                    Err(e) => {
                        tracing::error!(
                            "Failed to re-serialize request body after route rewrite: {e}"
                        );
                        return Err(AppError::Transform(format!(
                            "Failed to apply route rewrite: {e}"
                        )));
                    }
                }
            }
            None => (body, None),
        }
    } else {
        (body, req_json)
    };

    let ctx = RequestCtx {
        body: &body,
        req_json: req_json.as_ref(),
        headers: &headers,
        is_stream,
        route_pattern: &route_pattern,
    };
    try_providers(&state, &pool, do_cycle, &ctx).await
}

/// Handle non-streaming response.
async fn handle_buffered_response(
    response: reqwest::Response,
    is_openai: bool,
    db: Repository,
    pkey: ProviderKey,
    request_log: &crate::proxy::metrics::SharedRequestLog,
    mut log_entry: RequestLogEntry,
    request_log_limit: usize,
) -> Result<Response, AppError> {
    let entry_id = log_entry.id;
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
    db.persist_stats_async(
        &provider_id,
        &provider_name,
        model.as_deref(),
        StatsDelta {
            requests: 1,
            input,
            output,
            latency: log_entry.latency_ms,
            ..Default::default()
        },
    );

    // Back-fill token counts on the log entry we created before sending.
    if let Ok(mut log) = request_log.lock() {
        log.backfill(entry_id, input, output, model.as_deref());
    }
    // Persist the complete entry in one shot — no two-phase write, no race condition.
    if entry_id != 0 {
        log_entry.id = entry_id;
        log_entry.input_tokens = input;
        log_entry.output_tokens = output;
        if let Some(ref m) = model {
            log_entry.model = m.clone();
        }
        db.persist_request_log_async(log_entry, request_log_limit);
    }

    Ok((
        StatusCode::OK,
        [("content-type", "application/json")],
        response_body,
    )
        .into_response())
}

/// Handle streaming response.
async fn handle_streaming_response(
    response: reqwest::Response,
    is_openai: bool,
    ctx: StreamTrackingCtx,
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

    let tracked = track_tokens_in_stream(raw_stream, ctx);
    let body = Body::from_stream(tracked);

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(body)
        .map_err(|e| AppError::Transform(e.to_string()))
}

/// Context for tracking tokens in a streaming response.
struct StreamTrackingCtx {
    db: Repository,
    provider_id: String,
    provider_name: String,
    request_log: SharedRequestLog,
    entry_id: u64,
    latency: u64,
    request_log_limit: usize,
}

/// Wrap a byte stream to extract token usage from anthropic SSE events.
/// Passes all bytes through unchanged; records metrics when the stream ends.
fn track_tokens_in_stream(
    mut inner: std::pin::Pin<Box<dyn futures::Stream<Item = std::io::Result<Bytes>> + Send>>,
    ctx: StreamTrackingCtx,
) -> impl futures::Stream<Item = std::io::Result<Bytes>> + Send {
    let StreamTrackingCtx {
        db,
        provider_id,
        provider_name,
        request_log,
        entry_id,
        latency,
        request_log_limit,
    } = ctx;
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
                    // Buffer would exceed limit. Log and clear, but do NOT discard incoming bytes.
                    tracing::warn!("SSE buffer exceeded {}B limit; resetting", LINE_BUF_MAX);
                    line_buf.clear();
                    line_buf.push_str(&String::from_utf8_lossy(bytes));
                }
                // Process complete SSE lines; single drain at the end.
                let mut start = 0;
                while let Some(rel) = line_buf[start..].find('\n') {
                    let pos = start + rel;
                    let line = line_buf[start..pos].trim_end_matches('\r');
                    if let Some(data) = line.strip_prefix("data: ")
                        && let Ok(json) = serde_json::from_str::<serde_json::Value>(data) {
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
                    start = pos + 1;
                }
                if start > 0 {
                    line_buf.drain(..start);
                }
            }
            yield chunk;
        }

        // Stream ended: persist request count and token usage atomically.
        db.persist_stats_async(
            &provider_id,
            &provider_name,
            model.as_deref(),
            StatsDelta {
                requests: 1,
                input: input_tokens,
                output: output_tokens,
                latency,
                ..Default::default()
            },
        );

        // Back-fill token counts into the request log entry we created before streaming.
        if let Ok(mut log) = request_log.lock() {
            log.backfill(entry_id, input_tokens, output_tokens, model.as_deref());
        }
        if entry_id != 0 {
            db.update_request_log_tokens_async(entry_id, input_tokens, output_tokens, model, request_log_limit);
        }
    }
}
