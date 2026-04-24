use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures::StreamExt;

use super::SharedState;
use crate::config::{ApiFormat, OpenAiApiVersion};
use crate::error::AppError;
use crate::proxy::executor::{
    ProviderRequestOutcome, execute_provider_request, extract_error_message,
};
use crate::proxy::metrics::{RequestLogEntry, SharedRequestLog};
use crate::proxy::transform;

/// Wire format expected by the *client* (the caller of this proxy). The proxy
/// always normalises to Anthropic internally, then converts to the client
/// format on the way out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClientFormat {
    Anthropic,
    OpenAIChat,
    OpenAIResponses,
}

impl ClientFormat {
    fn openai_variant(self) -> Option<OpenAiApiVersion> {
        match self {
            ClientFormat::Anthropic => None,
            ClientFormat::OpenAIChat => Some(OpenAiApiVersion::ChatCompletions),
            ClientFormat::OpenAIResponses => Some(OpenAiApiVersion::Responses),
        }
    }
}

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

    // Return OpenAI shape when the caller identifies itself via Bearer token;
    // return Anthropic shape for x-api-key or unauthenticated callers.
    let client_wants_openai = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.starts_with("Bearer "))
        .unwrap_or(false);
    let provider_is_openai = provider.api_format == ApiFormat::OpenAI;

    // Pass-through when both sides already speak the same wire format (no
    // parse+reserialize needed).
    let response_body = match (provider_is_openai, client_wants_openai) {
        (false, false) => body,
        (true, false) => {
            let openai_json: serde_json::Value = serde_json::from_slice(&body)?;
            Bytes::from(serde_json::to_vec(&transform::openai_to_anthropic_models(
                &openai_json,
            ))?)
        }
        (false, true) => {
            let anthropic_json: serde_json::Value = serde_json::from_slice(&body)?;
            Bytes::from(serde_json::to_vec(&transform::anthropic_to_openai_models(
                &anthropic_json,
            ))?)
        }
        (true, true) => body,
    };

    Ok((
        StatusCode::OK,
        [("content-type", "application/json")],
        response_body,
    )
        .into_response())
}

/// Build the candidate provider list. Routes are per-provider and are applied
/// at request execution time (see [`try_providers`]) so that a fallback to a
/// different-format provider uses *its own* routes, not the current provider's.
async fn resolve_provider_pool(
    state: &SharedState,
) -> Result<(Vec<(String, crate::config::Provider)>, bool), AppError> {
    let config = state.config.read().await;

    if config.fallback {
        let (_, current_provider) = config.current_enabled_provider()?;
        let start_idx = config
            .providers
            .get_index_of(&config.current)
            .ok_or_else(|| AppError::ProviderNotFound(config.current.clone()))?;
        let len = config.providers.len();
        let list: Vec<(String, crate::config::Provider)> = (0..len)
            .map(|i| (start_idx + i) % len)
            .filter_map(|i| {
                config
                    .providers
                    .get_index(i)
                    .filter(|(k, v)| v.enabled && (v.fallback || k.as_str() == config.current))
                    .map(|(k, v)| (k.clone(), v.clone()))
            })
            .collect();
        if !current_provider.enabled || list.is_empty() {
            return Err(AppError::ProviderNotFound(config.current.clone()));
        }
        Ok((list, true))
    } else {
        let (_, current_provider) = config.current_enabled_provider()?;
        Ok((
            vec![(config.current.clone(), current_provider.clone())],
            false,
        ))
    }
}

/// Request context bundled to keep [`try_providers`] argument count in check.
struct RequestCtx<'a> {
    body: &'a Bytes,
    req_json: Option<&'a serde_json::Value>,
    headers: &'a HeaderMap,
    is_stream: bool,
    client_format: ClientFormat,
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

    let record_error_metric =
        |state: &SharedState, name: &str, status: u16, msg: &str, pattern: &str| {
            if let Ok(mut m) = state.metrics.lock() {
                m.record_error(name, status, &req_model_hint, pattern, msg);
            }
        };

    let push_request_log = |provider: &str, status: u16, latency_ms: u64, error: String| {
        let entry = RequestLogEntry {
            id: 0,
            timestamp: std::time::SystemTime::now(),
            provider: provider.to_owned(),
            model: req_model_hint.clone(),
            status,
            latency_ms,
            input_tokens: 0,
            output_tokens: 0,
            is_stream: ctx.is_stream,
            error: Some(error),
        };
        if let Ok(id) = state
            .request_log
            .lock()
            .map(|mut log| log.push(entry.clone()))
        {
            let mut persisted = entry;
            persisted.id = id;
            state
                .db
                .persist_request_log_async(persisted, request_log_limit);
        }
    };

    let t0 = std::time::Instant::now();

    for (provider_name, provider) in pool.iter().cycle() {
        let pkey = ProviderKey {
            id: provider.id.clone(),
            name: provider_name.clone(),
        };

        // Per-provider route pattern for metrics: reflects which rule (if any)
        // of *this* provider matched the requested model.
        let route_pattern = provider
            .resolve_model(&req_model_hint)
            .1
            .unwrap_or_default();

        let api_key = match provider.resolve_api_key() {
            Ok(k) => k,
            Err(e) => {
                tracing::warn!("Skipping provider {}: {e}", provider.base_url);
                record_failure(state, &pkey);
                record_error_metric(state, provider_name, 0, &e.to_string(), &route_pattern);
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
                record_error_metric(state, provider_name, 0, &e.to_string(), &route_pattern);
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
            record_error_metric(state, provider_name, status_u16, &preview, &route_pattern);
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
            let (error_body, latency_ms) = match outcome {
                ProviderRequestOutcome::UpstreamError {
                    body, latency_ms, ..
                } => (body, latency_ms),
                ProviderRequestOutcome::Success {
                    response,
                    latency_ms,
                } => (response.bytes().await.unwrap_or_default(), latency_ms),
            };
            let preview = extract_error_message(&error_body);
            tracing::warn!(
                "Provider {} returned {status} ({}), trying next",
                provider.base_url,
                preview
            );
            record_failure(state, &pkey);
            record_error_metric(state, provider_name, status_u16, &preview, &route_pattern);
            last_status = Some(status);
            last_error_body = Some(error_body.clone());
            auth_failures += 1;
            if !do_cycle || auth_failures >= max_auth_failures {
                push_request_log(provider_name, status_u16, latency_ms, preview);
                return Ok(
                    (status, [("content-type", "application/json")], error_body).into_response()
                );
            }
            continue;
        }

        // Other 4xx: client error (bad request format etc.), return immediately
        if !status.is_success() {
            let (error_body, latency_ms) = match outcome {
                ProviderRequestOutcome::UpstreamError {
                    body, latency_ms, ..
                } => (body, latency_ms),
                ProviderRequestOutcome::Success {
                    response,
                    latency_ms,
                } => (response.bytes().await.unwrap_or_default(), latency_ms),
            };
            let preview = extract_error_message(&error_body);
            tracing::warn!("Upstream returned {status}: {preview}");
            record_failure(state, &pkey);
            record_error_metric(state, provider_name, status_u16, &preview, &route_pattern);
            push_request_log(provider_name, status_u16, latency_ms, preview);
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
                ctx.client_format,
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
                ctx.client_format,
                BufferedTrackingCtx {
                    db: state.db.clone(),
                    pkey,
                    request_log: state.request_log.clone(),
                    log_entry: RequestLogEntry {
                        id: entry_id,
                        ..initial_entry
                    },
                    request_log_limit,
                },
            )
            .await
        };
    }

    // All providers failed — log the final failure.
    let final_status = last_status.unwrap_or(StatusCode::BAD_GATEWAY);
    push_request_log(
        pool.first().map(|(n, _)| n.as_str()).unwrap_or_default(),
        final_status.as_u16(),
        t0.elapsed().as_millis() as u64,
        "all providers failed".into(),
    );

    let body =
        last_error_body.unwrap_or_else(|| Bytes::from(r#"{"error":"all providers failed"}"#));
    Ok((final_status, [("content-type", "application/json")], body).into_response())
}

/// Shared dispatch logic for all completion endpoints.
/// Normalises the incoming body to Anthropic canonical form, then forwards.
async fn dispatch_completion(
    state: SharedState,
    headers: HeaderMap,
    body: Bytes,
    client_format: ClientFormat,
) -> Result<Response, AppError> {
    let (canonical_body, canonical_json) = if let Some(api_version) = client_format.openai_variant()
    {
        let incoming = serde_json::from_slice::<serde_json::Value>(&body)
            .map_err(|e| AppError::Transform(format!("Invalid JSON body: {e}")))?;
        let anthropic = transform::openai_to_anthropic_request(&incoming, api_version)?;
        let bytes = Bytes::from(serde_json::to_vec(&anthropic)?);
        (bytes, Some(anthropic))
    } else {
        let json = serde_json::from_slice::<serde_json::Value>(&body).ok();
        (body, json)
    };

    let is_stream = canonical_json
        .as_ref()
        .and_then(|v| v.get("stream").and_then(|s| s.as_bool()))
        .unwrap_or(false);

    let (pool, do_cycle) = resolve_provider_pool(&state).await?;

    let ctx = RequestCtx {
        body: &canonical_body,
        req_json: canonical_json.as_ref(),
        headers: &headers,
        is_stream,
        client_format,
    };
    try_providers(&state, &pool, do_cycle, &ctx).await
}

/// POST /v1/messages — Anthropic Messages API.
pub async fn handle_messages(
    State(state): State<SharedState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    dispatch_completion(state, headers, body, ClientFormat::Anthropic).await
}

/// POST /v1/chat/completions — OpenAI Chat Completions API.
pub async fn handle_chat_completions(
    State(state): State<SharedState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    dispatch_completion(state, headers, body, ClientFormat::OpenAIChat).await
}

/// POST /v1/responses — OpenAI Responses API.
pub async fn handle_responses(
    State(state): State<SharedState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    dispatch_completion(state, headers, body, ClientFormat::OpenAIResponses).await
}

/// Handle non-streaming response.
async fn handle_buffered_response(
    response: reqwest::Response,
    is_openai: bool,
    client_format: ClientFormat,
    bctx: BufferedTrackingCtx,
) -> Result<Response, AppError> {
    let BufferedTrackingCtx {
        db,
        pkey,
        request_log,
        mut log_entry,
        request_log_limit,
    } = bctx;
    let entry_id = log_entry.id;
    let ProviderKey {
        id: provider_id,
        name: provider_name,
    } = pkey;
    let body = response.bytes().await?;

    // Normalise provider response to the Anthropic canonical form for token
    // extraction. Keep the raw bytes around so the Anthropic→Anthropic
    // pass-through path can return them untouched (no reserialize).
    let usage_json: Option<serde_json::Value> = if is_openai {
        let openai_json: serde_json::Value = serde_json::from_slice(&body)?;
        Some(transform::openai_to_anthropic_response(&openai_json)?)
    } else {
        serde_json::from_slice::<serde_json::Value>(&body).ok()
    };

    // Serialize once, directly to the client's expected wire format.
    let response_body = match (client_format.openai_variant(), &usage_json) {
        (Some(api_version), Some(anthropic_json)) => {
            let out = transform::anthropic_to_openai_response(anthropic_json, api_version)?;
            Bytes::from(serde_json::to_vec(&out)?)
        }
        (None, Some(anthropic_json)) if is_openai => {
            Bytes::from(serde_json::to_vec(anthropic_json)?)
        }
        _ => body,
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
    client_format: ClientFormat,
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

    let final_stream: std::pin::Pin<
        Box<dyn futures::Stream<Item = std::io::Result<Bytes>> + Send>,
    > = if let Some(api_version) = client_format.openai_variant() {
        Box::pin(transform::anthropic_stream_to_openai(tracked, api_version))
    } else {
        Box::pin(tracked)
    };

    let body = Body::from_stream(final_stream);

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(body)
        .map_err(|e| AppError::Transform(e.to_string()))
}

/// Context for tracking tokens in a buffered response.
struct BufferedTrackingCtx {
    db: Repository,
    pkey: ProviderKey,
    request_log: crate::proxy::metrics::SharedRequestLog,
    log_entry: RequestLogEntry,
    request_log_limit: usize,
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

struct StreamFinalizer {
    db: Repository,
    provider_id: String,
    provider_name: String,
    request_log: SharedRequestLog,
    entry_id: u64,
    latency: u64,
    request_log_limit: usize,
    input_tokens: u64,
    output_tokens: u64,
    model: Option<String>,
    finished: bool,
}

impl StreamFinalizer {
    fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;

        self.db.persist_stats_async(
            &self.provider_id,
            &self.provider_name,
            self.model.as_deref(),
            StatsDelta {
                requests: 1,
                input: self.input_tokens,
                output: self.output_tokens,
                latency: self.latency,
                ..Default::default()
            },
        );

        if let Ok(mut log) = self.request_log.lock() {
            log.backfill(
                self.entry_id,
                self.input_tokens,
                self.output_tokens,
                self.model.as_deref(),
            );
        }
        if self.entry_id != 0 {
            self.db.update_request_log_tokens_async(
                self.entry_id,
                self.input_tokens,
                self.output_tokens,
                self.model.clone(),
                self.request_log_limit,
            );
        }
    }
}

impl Drop for StreamFinalizer {
    fn drop(&mut self) {
        self.finish();
    }
}

/// Wrap a byte stream to extract token usage from anthropic SSE events.
/// Passes all bytes through unchanged; records metrics when the stream ends.
fn track_tokens_in_stream(
    mut inner: std::pin::Pin<Box<dyn futures::Stream<Item = std::io::Result<Bytes>> + Send>>,
    ctx: StreamTrackingCtx,
) -> impl futures::Stream<Item = std::io::Result<Bytes>> + Send {
    async_stream::stream! {
        const LINE_BUF_MAX: usize = 1024 * 1024; // 1 MB safety cap
        let mut finalizer = StreamFinalizer {
            db: ctx.db,
            provider_id: ctx.provider_id,
            provider_name: ctx.provider_name,
            request_log: ctx.request_log,
            entry_id: ctx.entry_id,
            latency: ctx.latency,
            request_log_limit: ctx.request_log_limit,
            input_tokens: 0,
            output_tokens: 0,
            model: None,
            finished: false,
        };
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
                                    finalizer.input_tokens = json["message"]["usage"]["input_tokens"]
                                        .as_u64()
                                        .unwrap_or(0);
                                    if let Some(m) = json["message"]["model"].as_str() {
                                        finalizer.model = Some(m.to_string());
                                    }
                                }
                                Some("message_delta") => {
                                    if let Some(it) = json["usage"]["input_tokens"].as_u64() {
                                        finalizer.input_tokens = it;
                                    }
                                    finalizer.output_tokens = json["usage"]["output_tokens"]
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

        finalizer.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ApiFormat, AppConfig, OpenAiApiVersion, Provider};
    use crate::proxy::metrics::RequestLog;
    use futures::{StreamExt, stream};
    use indexmap::IndexMap;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tokio::sync::RwLock;

    fn make_provider(enabled: bool, fallback: bool) -> Provider {
        Provider {
            id: uuid::Uuid::new_v4().to_string(),
            base_url: "https://api.example.com".into(),
            api_key: "key".into(),
            api_format: ApiFormat::Anthropic,
            model_map: HashMap::new(),
            notes: String::new(),
            routes: Vec::new(),
            enabled,
            fallback,
            api_version: Some(OpenAiApiVersion::Responses),
            quota: None,
            quota_command: None,
        }
    }

    fn make_state(config: AppConfig) -> SharedState {
        let path = format!("/tmp/ccs-handler-test-{}.db", uuid::Uuid::new_v4());
        Arc::new(crate::proxy::AppState {
            config: Arc::new(RwLock::new(config)),
            http_client: super::super::build_http_client(),
            metrics: Arc::new(Mutex::new(crate::proxy::metrics::TokenMetrics::default())),
            request_log: Arc::new(Mutex::new(RequestLog::default())),
            db: Repository::open(&path),
        })
    }

    #[tokio::test]
    async fn fallback_pool_rejects_missing_current_provider() {
        let mut providers = IndexMap::new();
        providers.insert("prov-a".into(), make_provider(true, true));
        let state = make_state(AppConfig {
            current: "missing".into(),
            listen: "127.0.0.1:7896".into(),
            providers,
            fallback: true,
            db_path: None,
            request_log_limit: 100,
        });

        assert!(resolve_provider_pool(&state).await.is_err());
    }

    #[tokio::test]
    async fn streaming_drop_still_persists_stats_and_backfills_log() {
        let path = format!("/tmp/ccs-stream-test-{}.db", uuid::Uuid::new_v4());
        let db = Repository::open(&path);
        let request_log = Arc::new(Mutex::new(RequestLog::default()));
        let entry_id = {
            let mut log = request_log.lock().unwrap();
            log.push(RequestLogEntry {
                id: 0,
                timestamp: std::time::SystemTime::now(),
                provider: "prov-a".into(),
                model: String::new(),
                status: 200,
                latency_ms: 12,
                input_tokens: 0,
                output_tokens: 0,
                is_stream: true,
                error: None,
            })
        };

        let first_chunk = Bytes::from(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-test\",\"usage\":{\"input_tokens\":7}}}\n\n",
        );
        let raw_stream: std::pin::Pin<
            Box<dyn futures::Stream<Item = std::io::Result<Bytes>> + Send>,
        > = Box::pin(stream::once(async move { Ok(first_chunk) }).chain(stream::pending()));

        let mut tracked = Box::pin(track_tokens_in_stream(
            raw_stream,
            StreamTrackingCtx {
                db: db.clone(),
                provider_id: "provider-id".into(),
                provider_name: "prov-a".into(),
                request_log: request_log.clone(),
                entry_id,
                latency: 12,
                request_log_limit: 100,
            },
        ));

        let chunk = tracked.next().await;
        assert!(chunk.is_some());
        drop(tracked);

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let metrics = db.load_metrics();
        let provider = metrics.by_provider.get("prov-a").unwrap();
        assert_eq!(provider.requests, 1);
        assert_eq!(provider.input, 7);

        let log = request_log.lock().unwrap();
        let entry = log.entries().back().unwrap();
        assert_eq!(entry.input_tokens, 7);
        assert_eq!(entry.model, "claude-test");
    }
}
