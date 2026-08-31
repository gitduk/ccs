use std::sync::Arc;

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

fn body_to_string(bytes: &[u8]) -> Arc<str> {
    Arc::from(String::from_utf8_lossy(bytes))
}

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

/// Bundles per-request shared state for the provider iteration loop.
/// Converts the three ad-hoc closures in `try_providers` into methods so
/// each concern (failure recording, error metrics, request logging) is named
/// and testable in isolation.
struct RequestPipeline<'a> {
    state: &'a SharedState,
    req_model_hint: String,
    request_body_str: Arc<str>,
    is_stream: bool,
    request_log_limit: usize,
}

impl<'a> RequestPipeline<'a> {
    fn record_failure(&self, pkey: &ProviderKey) {
        self.state.db.persist_stats_async(
            &pkey.id,
            &pkey.name,
            None,
            StatsDelta {
                requests: 1,
                failures: 1,
                ..Default::default()
            },
        );
    }

    fn record_error_metric(&self, name: &str, status: u16, msg: &str, pattern: &str) {
        if let Ok(mut m) = self.state.metrics.lock() {
            m.record_error(name, status, &self.req_model_hint, pattern, msg);
        }
    }

    /// Log a failed request. The request body is taken from `self`; only the
    /// response body (if any) is supplied by the caller.
    fn push_request_log(
        &self,
        provider: &str,
        status: u16,
        latency_ms: u64,
        error: String,
        resp_body: Option<Arc<str>>,
    ) {
        let entry = RequestLogEntry {
            id: 0,
            timestamp: std::time::SystemTime::now(),
            provider: provider.to_owned(),
            model: self.req_model_hint.clone(),
            status,
            latency_ms,
            input_tokens: 0,
            output_tokens: 0,
            is_stream: self.is_stream,
            error: Some(error),
            request_body: Some(self.request_body_str.clone()),
            response_body: resp_body,
        };
        if let Ok(mut log) = self.state.request_log.lock() {
            let id = log.push(entry.clone());
            let mut persisted = entry;
            persisted.id = id;
            self.state
                .db
                .persist_request_log_async(persisted, self.request_log_limit);
        }
    }
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

    let mut req = crate::proxy::forwarder::models_request(&state.http_client, &provider, &api_key);
    if provider.api_format == ApiFormat::Anthropic
        && let Some(beta) = headers.get("anthropic-beta")
    {
        req = req.header("anthropic-beta", beta);
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
    // Pinned listeners bypass the fallback rotation and go directly to their provider.
    if let Some(name) = &state.pinned_provider {
        let config = state.config.read().await;
        let provider = config
            .providers
            .get(name)
            .filter(|p| p.enabled)
            .ok_or_else(|| AppError::ProviderNotFound(name.clone()))?;
        return Ok((vec![(name.clone(), provider.clone())], false));
    }

    let config = state.config.read().await;
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
}

/// Request context bundled to keep [`try_providers`] argument count in check.
/// `body`/`req_json` are the Anthropic-canonical form used by the transform
/// pipeline; `raw_body` is the untouched client bytes, used only for the
/// byte-for-byte passthrough path (OpenAI client → transparent OpenAI provider).
struct RequestCtx<'a> {
    body: &'a Bytes,
    raw_body: Option<&'a Bytes>,
    req_json: Option<&'a serde_json::Value>,
    headers: &'a HeaderMap,
    is_stream: bool,
    client_format: ClientFormat,
}

/// What to do after a non-success upstream status, decided per status class.
enum FailureAction {
    /// 5xx / 429: transient — cycle providers up to `max_failures`.
    Retry,
    /// 401 / 403 / 404: auth or model-not-found — cycle up to `max_auth_failures`.
    RetryAuth,
    /// Other 4xx: client error — relay upstream's response immediately.
    ReturnNow,
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

    let pipeline = RequestPipeline {
        state,
        req_model_hint: ctx
            .req_json
            .and_then(|v| v.get("model").and_then(|m| m.as_str()))
            .unwrap_or("")
            .to_string(),
        request_body_str: body_to_string(ctx.body),
        is_stream: ctx.is_stream,
        request_log_limit,
    };

    let t0 = std::time::Instant::now();

    for (provider_name, provider) in pool.iter().cycle() {
        let pkey = ProviderKey {
            id: provider.id.clone(),
            name: provider_name.clone(),
        };

        // Per-provider route resolution: the model actually sent upstream, plus
        // the rule (if any) of *this* provider that matched the requested model.
        let (upstream_model, route_pattern) = provider.resolve_model(&pipeline.req_model_hint);
        let route_pattern = route_pattern.unwrap_or_default();

        let api_key = match provider.resolve_api_key() {
            Ok(k) => k,
            Err(e) => {
                tracing::warn!("Skipping provider {}: {e}", provider.base_url);
                pipeline.record_failure(&pkey);
                pipeline.record_error_metric(provider_name, 0, &e.to_string(), &route_pattern);
                consecutive_failures += 1;
                if !do_cycle || consecutive_failures >= max_failures {
                    break;
                }
                continue;
            }
        };

        // Byte-for-byte passthrough when the provider and client both speak
        // the same OpenAI wire format, nothing configured forces a rewrite,
        // and no transform would be needed. In particular a request whose
        // assistant tool-call history needs `reasoning_content` injection
        // must go through the transform path (see `needs_reasoning_injection`).
        let passthrough = ctx.raw_body.and_then(|raw| {
            let client = ctx.client_format.openai_variant()?;
            provider.passthrough_for(client).then_some(raw)
        });
        let passthrough = if passthrough.is_some()
            && provider.inject_thinking_history
            && needs_reasoning_injection(ctx.raw_body.unwrap())
        {
            None
        } else {
            passthrough
        };
        let outcome = match execute_provider_request(
            &state.http_client,
            provider,
            &api_key,
            ctx.body,
            ctx.req_json,
            ctx.headers,
            passthrough,
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(e) => {
                tracing::warn!(
                    "Provider {} network error: {e}, trying next",
                    provider.base_url
                );
                pipeline.record_failure(&pkey);
                pipeline.record_error_metric(provider_name, 0, &e.to_string(), &route_pattern);
                consecutive_failures += 1;
                if !do_cycle || consecutive_failures >= max_failures {
                    break;
                }
                continue;
            }
        };

        let status = outcome.status();
        let status_u16 = status.as_u16();

        if !status.is_success() {
            // Retry policy by status class; bookkeeping below is shared.
            let action = match status_u16 {
                s if s >= 500 || s == 429 => FailureAction::Retry,
                // Auth error or model not found — worth trying the next
                // provider in fallback mode, bounded by max_auth_failures.
                401 | 403 | 404 => FailureAction::RetryAuth,
                // Other 4xx: client error (bad request format etc.).
                _ => FailureAction::ReturnNow,
            };
            let (error_body, latency_ms) = outcome.into_error_parts().await;
            let preview = extract_error_message(&error_body);
            tracing::warn!(
                "Provider {} returned {status}: {preview}",
                provider.base_url
            );
            pipeline.record_failure(&pkey);
            pipeline.record_error_metric(provider_name, status_u16, &preview, &route_pattern);

            let keep_cycling = do_cycle
                && match action {
                    FailureAction::Retry => {
                        consecutive_failures += 1;
                        consecutive_failures < max_failures
                    }
                    FailureAction::RetryAuth => {
                        auth_failures += 1;
                        auth_failures < max_auth_failures
                    }
                    FailureAction::ReturnNow => false,
                };

            if keep_cycling {
                last_status = Some(status);
                last_error_body = Some(error_body);
                continue;
            }
            match action {
                // Exhausted retries: fall through to the shared final-error path.
                FailureAction::Retry => {
                    last_status = Some(status);
                    last_error_body = Some(error_body);
                    break;
                }
                // Auth exhausted or plain client error: relay upstream's response.
                FailureAction::RetryAuth | FailureAction::ReturnNow => {
                    pipeline.push_request_log(
                        provider_name,
                        status_u16,
                        latency_ms,
                        preview,
                        Some(body_to_string(&error_body)),
                    );
                    return Ok((status, [("content-type", "application/json")], error_body)
                        .into_response());
                }
            }
        }

        let latency_ms = outcome.latency_ms();
        if let Ok(mut m) = state.metrics.lock() {
            m.clear_error(provider_name);
            m.by_provider
                .entry(provider_name.clone())
                .or_default()
                .latency_total += latency_ms;
        }
        // Log successful requests — tokens will be filled in by the response handlers.
        // For buffered responses, handle_buffered_response updates the log entry with token counts.
        let initial_entry = RequestLogEntry {
            id: 0, // assigned by push()
            timestamp: std::time::SystemTime::now(),
            provider: provider_name.clone(),
            model: pipeline.req_model_hint.clone(),
            status: status_u16,
            latency_ms,
            input_tokens: 0,
            output_tokens: 0,
            is_stream: ctx.is_stream,
            error: None,
            request_body: Some(pipeline.request_body_str.clone()),
            response_body: None,
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
                    .persist_request_log_async(persisted, pipeline.request_log_limit);
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
                // Defensive: this branch should not be reached for successful statuses.
                tracing::error!(
                    "BUG: Successful status {} but got UpstreamError outcome",
                    status
                );
                return Ok((status, [("content-type", "application/json")], body).into_response());
            }
        };
        let model_override = Some(pipeline.req_model_hint.clone()).filter(|m| !m.is_empty());
        return if ctx.is_stream {
            handle_streaming_response(
                response,
                provider.api_format == ApiFormat::OpenAI,
                ctx.client_format,
                passthrough.is_some(),
                StreamTrackingCtx {
                    db: state.db.clone(),
                    provider_id: pkey.id,
                    provider_name: pkey.name,
                    request_log: state.request_log.clone(),
                    entry_id,
                    latency: latency_ms,
                    request_log_limit: pipeline.request_log_limit,
                    model_override,
                    upstream_model,
                },
            )
            .await
        } else {
            handle_buffered_response(
                response,
                provider.api_format == ApiFormat::OpenAI,
                ctx.client_format,
                passthrough.is_some(),
                BufferedTrackingCtx {
                    db: state.db.clone(),
                    pkey,
                    request_log: state.request_log.clone(),
                    log_entry: RequestLogEntry {
                        id: entry_id,
                        ..initial_entry
                    },
                    request_log_limit: pipeline.request_log_limit,
                    model_override,
                    upstream_model,
                },
            )
            .await
        };
    }

    // All providers failed — log the final failure.
    let final_status = last_status.unwrap_or(StatusCode::BAD_GATEWAY);
    pipeline.push_request_log(
        pool.first().map(|(n, _)| n.as_str()).unwrap_or_default(),
        final_status.as_u16(),
        t0.elapsed().as_millis() as u64,
        "all providers failed".into(),
        last_error_body.as_ref().map(|b| body_to_string(b)),
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
    // The untouched client bytes survive for the byte-for-byte passthrough
    // path (OpenAI client → transparent OpenAI provider). Borrowed here (no
    // clone): only OpenAI clients keep `body` alive for the passthrough,
    // Anthropic clients move it into the canonical form and get `None`.
    let (canonical_body, canonical_json, raw_body) =
        if let Some(api_version) = client_format.openai_variant() {
            let incoming = serde_json::from_slice::<serde_json::Value>(&body)
                .map_err(|e| AppError::Transform(format!("Invalid JSON body: {e}")))?;
            let anthropic = transform::openai_to_anthropic_request(&incoming, api_version)?;
            let bytes = Bytes::from(serde_json::to_vec(&anthropic)?);
            (bytes, Some(anthropic), Some(&body))
        } else {
            let json = serde_json::from_slice::<serde_json::Value>(&body).ok();
            (body, json, None)
        };

    let is_stream = canonical_json
        .as_ref()
        .and_then(|v| v.get("stream").and_then(|s| s.as_bool()))
        .unwrap_or(false);

    let (pool, do_cycle) = resolve_provider_pool(&state).await?;

    let ctx = RequestCtx {
        body: &canonical_body,
        raw_body,
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
    passthrough: bool,
    bctx: BufferedTrackingCtx,
) -> Result<Response, AppError> {
    let BufferedTrackingCtx {
        db,
        pkey,
        request_log,
        mut log_entry,
        request_log_limit,
        model_override,
        upstream_model,
    } = bctx;
    let entry_id = log_entry.id;
    let ProviderKey {
        id: provider_id,
        name: provider_name,
    } = pkey;
    let body = response.bytes().await?;

    // Capture the raw upstream response for diagnostics.
    let resp_body_str = body_to_string(&body);
    log_entry.response_body = Some(resp_body_str.clone());
    if let Ok(mut log) = request_log.lock()
        && let Some(entry) = log
            .entries_mut()
            .iter_mut()
            .rev()
            .find(|e| e.id == entry_id)
    {
        entry.response_body = Some(resp_body_str);
    }

    // Normalise provider response to the Anthropic canonical form for token
    // extraction. Keep the raw bytes around so the Anthropic→Anthropic
    // pass-through path can return them untouched (no reserialize).
    let mut usage_json: Option<serde_json::Value> = if is_openai {
        let openai_json: serde_json::Value = serde_json::from_slice(&body)?;
        Some(transform::openai_to_anthropic_response(&openai_json)?)
    } else {
        serde_json::from_slice::<serde_json::Value>(&body).ok()
    };

    // Stats record what actually served the request, so read the model before
    // the client-facing rewrite below replaces it with the requested alias.
    let model = served_model(
        usage_json.as_ref().and_then(|json| json["model"].as_str()),
        &upstream_model,
    );

    // Echo the requested name back — only in a re-serialized body; the
    // Anthropic pass-through below returns raw upstream bytes untouched.
    if let (Some(m), Some(json)) = (&model_override, &mut usage_json) {
        json["model"] = serde_json::Value::String(m.clone());
    }

    // Passthrough: the upstream already speaks the exact wire format the
    // client asked for, so return its bytes untouched. Token extraction above
    // still parsed the body for stats, so nothing is lost.
    let response_body = if passthrough {
        body
    } else {
        // Serialize once, directly to the client's expected wire format.
        match (client_format.openai_variant(), &usage_json) {
            (Some(api_version), Some(anthropic_json)) => {
                let out = transform::anthropic_to_openai_response(anthropic_json, api_version)?;
                Bytes::from(serde_json::to_vec(&out)?)
            }
            (None, Some(anthropic_json)) if is_openai => {
                Bytes::from(serde_json::to_vec(anthropic_json)?)
            }
            _ => body,
        }
    };

    let (input, output) = if let Some(ref json) = usage_json {
        (
            json["usage"]["input_tokens"].as_u64().unwrap_or(0),
            json["usage"]["output_tokens"].as_u64().unwrap_or(0),
        )
    } else {
        (0, 0)
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
    passthrough: bool,
    ctx: StreamTrackingCtx,
) -> Result<Response, AppError> {
    let model_override = ctx.model_override.clone();

    // Passthrough: upstream already speaks the client's wire format, so relay
    // the raw SSE untouched. Usage is still tracked by parsing the OpenAI
    // usage events (final chunk for chat, response.completed for Responses).
    if passthrough {
        let raw: std::pin::Pin<
            Box<dyn futures::Stream<Item = std::io::Result<Bytes>> + Send>,
        > = Box::pin(response.bytes_stream().map(|r| {
            r.map_err(|e| {
                tracing::error!("Stream error: {e}");
                std::io::Error::other(e)
            })
        }));
        let tracked = track_tokens_in_stream(raw, ctx, extract_openai_usage);
        let body = Body::from_stream(tracked);
        return Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-cache")
            .body(body)
            .map_err(|e| AppError::Transform(e.to_string()));
    }

    // Canonical Anthropic SSE, still carrying the upstream's own model name.
    let raw_stream: std::pin::Pin<Box<dyn futures::Stream<Item = std::io::Result<Bytes>> + Send>> =
        if !is_openai {
            Box::pin(response.bytes_stream().map(|r| {
                r.map_err(|e| {
                    tracing::error!("Stream error: {e}");
                    std::io::Error::other(e)
                })
            }))
        } else {
            Box::pin(transform::openai_stream_to_anthropic(response, None))
        };

    // Track first, rewrite second: stats must see what actually served the
    // request, the client sees the alias it asked for.
    let tracked = track_tokens_in_stream(raw_stream, ctx, extract_anthropic_usage);

    let renamed: std::pin::Pin<Box<dyn futures::Stream<Item = std::io::Result<Bytes>> + Send>> =
        match model_override {
            Some(m) => Box::pin(transform::rewrite_model_in_stream(tracked, m)),
            None => Box::pin(tracked),
        };

    let final_stream: std::pin::Pin<
        Box<dyn futures::Stream<Item = std::io::Result<Bytes>> + Send>,
    > = if let Some(api_version) = client_format.openai_variant() {
        Box::pin(transform::anthropic_stream_to_openai(renamed, api_version))
    } else {
        renamed
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
    model_override: Option<String>,
    /// Model sent upstream after route resolution — recorded when the response
    /// itself carries no model name.
    upstream_model: String,
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
    model_override: Option<String>,
    /// Model sent upstream after route resolution — recorded when the response
    /// itself carries no model name.
    upstream_model: String,
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
/// An empty model name means the source reported none — treat it as absent.
fn non_empty_model(name: Option<&str>) -> Option<&str> {
    name.filter(|m| !m.is_empty())
}

/// Model to record for a request: the name the response reported, falling back
/// to the model routing actually sent upstream.
fn served_model(reported: Option<&str>, upstream: &str) -> Option<String> {
    non_empty_model(reported)
        .or_else(|| non_empty_model(Some(upstream)))
        .map(str::to_string)
}

/// True when the transform path would inject an empty `reasoning_content`
/// into the request (assistant `tool_calls` missing it, with
/// `inject_thinking_history` on — see `inject_missing_reasoning_content`).
/// DeepSeek-family upstreams reject such history, so passthrough must back
/// off and let the normalise-then-inject path run.
fn needs_reasoning_injection(body: &Bytes) -> bool {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return false;
    };
    let Some(msgs) = v.get("messages").and_then(|m| m.as_array()) else {
        return false;
    };
    msgs.iter().any(|msg| {
        msg.get("role").and_then(|r| r.as_str()) == Some("assistant")
            && msg.get("reasoning_content").is_none()
            && msg.get("tool_calls")
                .and_then(|t| t.as_array())
                .is_some_and(|calls| !calls.is_empty())
    })
}


/// Shared SSE passthrough loop: hands each `data:` payload to `extract`
/// (which records usage/model onto `finalizer`), then yields the raw bytes
/// untouched. `extract` decides for itself whether a line is worth parsing —
/// the Anthropic extractor cheaply skips the hundreds of content_block_delta
/// lines, the OpenAI one parses only chunks that look like usage.
fn track_tokens_in_stream(
    mut inner: std::pin::Pin<Box<dyn futures::Stream<Item = std::io::Result<Bytes>> + Send>>,
    ctx: StreamTrackingCtx,
    extract: fn(&mut StreamFinalizer, &str),
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
            // Falls back to the routed model when the stream carries no name.
            model: served_model(None, &ctx.upstream_model),
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
                        && data.trim() != "[DONE]"
                    {
                        extract(&mut finalizer, data);
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

/// Extract usage from Anthropic `message_start` / `message_delta` events.
/// A cheap substring check skips the JSON parse for the many
/// `content_block_delta` lines in between (which never carry usage).
fn extract_anthropic_usage(finalizer: &mut StreamFinalizer, data: &str) {
    if !(data.contains("\"message_start\"") || data.contains("\"message_delta\"")) {
        return;
    }
    let Ok(json) = serde_json::from_str::<serde_json::Value>(data) else {
        return;
    };
    match json["type"].as_str() {
        Some("message_start") => {
            finalizer.input_tokens = json["message"]["usage"]["input_tokens"]
                .as_u64()
                .unwrap_or(0);
            if let Some(m) = non_empty_model(json["message"]["model"].as_str()) {
                finalizer.model = Some(m.to_string());
            }
        }
        Some("message_delta") => {
            if let Some(it) = json["usage"]["input_tokens"].as_u64() {
                finalizer.input_tokens = it;
            }
            finalizer.output_tokens = json["usage"]["output_tokens"].as_u64().unwrap_or(0);
        }
        _ => {}
    }
}

/// Extract usage from an OpenAI stream. Chat Completions reports usage in a
/// final chunk (`usage` on `choices`-less data); Responses reports it in the
/// `response.completed` event. Any chunk carrying `model` or `usage` is worth
/// parsing; the rest are skipped.
fn extract_openai_usage(finalizer: &mut StreamFinalizer, data: &str) {
    if !(data.contains("\"usage\"") || data.contains("\"response.completed\"")) {
        return;
    }
    let Ok(json) = serde_json::from_str::<serde_json::Value>(data) else {
        return;
    };
    if json["type"].as_str() == Some("response.completed")
        && let Some(u) = json.get("usage")
    {
        finalizer.input_tokens = u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
        finalizer.output_tokens = u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
        return;
    }
    if let Some(model) = json.get("model").and_then(|m| m.as_str())
        && !model.is_empty()
    {
        if finalizer.model.is_none() {
            finalizer.model = Some(model.to_string());
        }
        if let Some(u) = json.get("usage") {
            finalizer.input_tokens = u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
            finalizer.output_tokens = u
                .get("completion_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use crate::config::{ApiFormat, AppConfig, OpenAiApiVersion, Provider};
    use crate::proxy::metrics::RequestLog;
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
            routes: Vec::new(),
            enabled,
            fallback,
            api_version: Some(OpenAiApiVersion::Responses),
            inject_thinking_history: true,
            strict_thinking_history: false,
            quota_command: None,
            port: None,
            test_model: None,
            max_tokens_cap: None,
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
            pinned_provider: None,
        })
    }

    /// Spin up a transparent OpenAI (Chat Completions) proxy wired to a
    /// fake upstream, returning the proxy address. Both e2e passthrough tests
    /// share this scaffold so the transform-vs-raw assertions stay in one place.
    async fn spawn_passthrough_proxy(upstream: Router) -> std::net::SocketAddr {
        use tokio::net::TcpListener;

        let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream_listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(upstream_listener, upstream).await;
        });

        let mut providers = IndexMap::new();
        let mut p = make_provider(true, true);
        p.api_format = ApiFormat::OpenAI;
        p.api_version = Some(OpenAiApiVersion::ChatCompletions);
        p.base_url = format!("http://{upstream_addr}");
        providers.insert("prov-a".into(), p);
        let state = make_state(AppConfig {
            current: "prov-a".into(),
            listen: "127.0.0.1:7896".into(),
            providers,
            db_path: None,
            request_log_limit: 100,
        });

        let router = crate::proxy::build_router(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        proxy_addr
    }

    /// Stats must record what served the request. An upstream that reports no
    /// model name must not overwrite the routed fallback with an empty string.
    #[test]
    fn served_model_prefers_the_reported_name_over_the_routed_one() {
        assert_eq!(
            served_model(Some("deepseek-v4-flash"), "coder-ds4"),
            Some("deepseek-v4-flash".into())
        );
        // A blank or absent report must not shadow the routed fallback.
        assert_eq!(
            served_model(Some(""), "coder-ds4"),
            Some("coder-ds4".into())
        );
        assert_eq!(served_model(None, "coder-ds4"), Some("coder-ds4".into()));
        // Nothing known at all — record no model rather than an empty name.
        assert_eq!(served_model(Some(""), ""), None);
        assert_eq!(served_model(None, ""), None);
    }

    #[tokio::test]
    async fn fallback_pool_rejects_missing_current_provider() {
        let mut providers = IndexMap::new();
        providers.insert("prov-a".into(), make_provider(true, true));
        let state = make_state(AppConfig {
            current: "missing".into(),
            listen: "127.0.0.1:7896".into(),
            providers,
            db_path: None,
            request_log_limit: 100,
        });

        assert!(resolve_provider_pool(&state).await.is_err());
    }
    #[tokio::test]
    async fn buffered_openai_passthrough_returns_upstream_bytes_untouched() {
        use axum::http::StatusCode as AxumStatus;
        use axum::routing::post;
        use axum::Json;

        // Fake upstream that reflects the request's model back, echoing the
        // exact request body as a marker so the test can detect rewrites.
        async fn chat(body: axum::body::Bytes) -> impl axum::response::IntoResponse {
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
            (
                AxumStatus::OK,
                Json(serde_json::json!({
                    "id": "chatcmpl-test",
                    "object": "chat.completion",
                    "created": 12345,
                    "model": v["model"],
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": "pong"},
                        "finish_reason": "stop"
                    }],
                    "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15},
                    "x_upstream_marker": "raw-passthrough"
                })),
            )
        }

        let proxy_addr = spawn_passthrough_proxy(
            Router::new().route("/v1/chat/completions", post(chat)),
        )
        .await;

        let body = r#"{"model":"deepseek-v4-flash","max_tokens":1,"messages":[{"role":"user","content":"ping"}]}"#;
        let resp = reqwest::Client::new()
            .post(format!("http://{proxy_addr}/v1/chat/completions"))
            .header("content-type", "application/json")
            .header("authorization", "Bearer sk-test")
            .body(body)
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), AxumStatus::OK);
        let json: serde_json::Value = resp.json().await.unwrap();
        // Passthrough: the model is echoed back exactly as sent, not rewritten
        assert_eq!(json["model"], "deepseek-v4-flash");
        assert_eq!(json["choices"][0]["message"]["content"], "pong");
        assert_eq!(json["usage"]["prompt_tokens"], 10);
        // The transform path rebuilds the response object and would drop an
        // unknown field; its survival proves the raw bytes were relayed.
        assert_eq!(json["x_upstream_marker"], "raw-passthrough");
    }

    #[tokio::test]
    async fn streaming_openai_passthrough_relays_raw_sse() {
        use axum::body::Body as AxumBody;
        use axum::http::StatusCode as AxumStatus;
        use axum::response::Response as AxumResponse;
        use axum::routing::post;

        async fn chat() -> AxumResponse {
            // Raw OpenAI SSE with an unknown field and a non-standard chunk
            // order, so any re-encode would be visible.
            let sse = concat!(
                "data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"model\":\"m\",",
                "\"x_marker\":\"keep\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"p\"}}]}\n\n",
                "data: {\"id\":\"x\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                "data: [DONE]\n\n",
            );
            AxumResponse::builder()
                .header("content-type", "text/event-stream")
                .body(AxumBody::from(sse))
                .unwrap()
        }

        let proxy_addr = spawn_passthrough_proxy(
            Router::new().route("/v1/chat/completions", post(chat)),
        )
        .await;

        let resp = reqwest::Client::new()
            .post(format!("http://{proxy_addr}/v1/chat/completions"))
            .header("content-type", "application/json")
            .header("authorization", "Bearer sk-test")
            .body(r#"{"model":"m","stream":true,"messages":[{"role":"user","content":"hi"}]}"#)
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), AxumStatus::OK);
        let text = resp.text().await.unwrap();
        // Raw upstream SSE relayed byte-for-byte: the unknown marker survives
        // (a re-encode would drop it) and the finish_reason chunk order is kept.
        assert!(text.contains("\"x_marker\":\"keep\""), "raw chunk was re-encoded: {text}");
        assert!(text.contains("data: [DONE]"));
    }

    #[test]
    fn needs_reasoning_injection_detects_assistant_tool_calls_without_reasoning() {
        let body = Bytes::from(
            r#"{"model":"m","messages":[
                {"role":"user","content":"hi"},
                {"role":"assistant","content":null,"tool_calls":[{"id":"c1","type":"function","function":{"name":"f","arguments":"{}"}}]},
                {"role":"tool","tool_call_id":"c1","content":"ok"}
            ]}"#,
        );
        assert!(needs_reasoning_injection(&body));
    }

    #[test]
    fn needs_reasoning_injection_false_when_reasoning_present_or_no_tools() {
        // Assistant already carries reasoning_content → no injection needed.
        let body = Bytes::from(
            r#"{"model":"m","messages":[
                {"role":"assistant","reasoning_content":"","content":null,"tool_calls":[{"id":"c1","type":"function","function":{"name":"f","arguments":"{}"}}]}
            ]}"#,
        );
        assert!(!needs_reasoning_injection(&body));

        // Plain text assistant, no tool_calls → no injection needed.
        let body = Bytes::from(r#"{"model":"m","messages":[{"role":"assistant","content":"ok"}]}"#);
        assert!(!needs_reasoning_injection(&body));
    }

    #[test]
    fn needs_reasoning_injection_false_on_invalid_or_absent_messages() {
        assert!(!needs_reasoning_injection(&Bytes::from("not json")));
        assert!(!needs_reasoning_injection(&Bytes::from(r#"{"model":"m"}"#)));
    }

    #[tokio::test]
    async fn tool_call_history_disables_passthrough_and_keeps_injection() {
        use axum::http::StatusCode as AxumStatus;
        use axum::routing::post;
        use axum::Json;

        // Echo the request's model + an unknown marker back. If passthrough
        // were active, the marker would survive byte-for-byte; the transform
        // path rebuilds the response and drops it.
        async fn chat(body: axum::body::Bytes) -> impl axum::response::IntoResponse {
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
            (
                AxumStatus::OK,
                Json(serde_json::json!({
                    "id": "chatcmpl-tool",
                    "object": "chat.completion",
                    "created": 12345,
                    "model": v["model"],
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": "done"},
                        "finish_reason": "stop"
                    }],
                    "usage": {"prompt_tokens": 8, "completion_tokens": 2, "total_tokens": 10},
                    "x_upstream_marker": "raw-passthrough"
                })),
            )
        }

        let proxy_addr = spawn_passthrough_proxy(
            Router::new().route("/v1/chat/completions", post(chat)),
        )
        .await;

        // Assistant tool-call history without reasoning_content: the transform
        // path must run to inject it, so passthrough is disabled and the
        // response is rebuilt (marker dropped).
        let body = r#"{"model":"deepseek-v4-flash","max_tokens":1,"messages":[
            {"role":"user","content":"hi"},
            {"role":"assistant","content":null,"tool_calls":[{"id":"c1","type":"function","function":{"name":"f","arguments":"{}"}}]},
            {"role":"tool","tool_call_id":"c1","content":"ok"}
        ]}"#;
        let resp = reqwest::Client::new()
            .post(format!("http://{proxy_addr}/v1/chat/completions"))
            .header("content-type", "application/json")
            .header("authorization", "Bearer sk-test")
            .body(body)
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), AxumStatus::OK);
        let json: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(json["choices"][0]["message"]["content"], "done");
        // Transform path rebuilt the response — the upstream marker must not
        // survive (it would if passthrough had forwarded raw bytes).
        assert!(json.get("x_upstream_marker").is_none());
    }
}
