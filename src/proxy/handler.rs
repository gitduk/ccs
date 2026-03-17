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
            let is_current_anthropic = config.current == "anthropic";
            let len = config.providers.len();
            let list: Vec<crate::config::Provider> = (0..len)
                .map(|i| (current_idx + i) % len)
                .filter_map(|i| config.providers.get_index(i).map(|(k, v)| (k.clone(), v.clone())))
                .filter(|(k, _)| k != "anthropic" || is_current_anthropic)
                .map(|(_, v)| v)
                .collect();
            (list, true)
        } else {
            let (_, p) = config.current_provider()?;
            (vec![p.clone()], false)
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

    for provider in pool.iter().cycle() {
        let api_key = match provider.resolve_api_key() {
            Ok(k) => k,
            Err(e) => {
                tracing::warn!("Skipping provider {}: {e}", provider.base_url);
                consecutive_failures += 1;
                if !do_cycle || consecutive_failures >= max_failures {
                    break;
                }
                continue;
            }
        };

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
            last_status = Some(status);
            consecutive_failures += 1;
            if !do_cycle || consecutive_failures >= max_failures {
                break;
            }
            continue;
        }

        // 4xx: client error, forward as-is without retrying
        if !status.is_success() {
            let error_body = response.bytes().await?;
            tracing::warn!("Upstream returned {status}: {}", String::from_utf8_lossy(&error_body));
            return Ok((
                StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
                [("content-type", "application/json")],
                error_body,
            )
                .into_response());
        }

        // Success
        return if is_stream {
            handle_streaming_response(response, is_openai).await
        } else {
            handle_buffered_response(response, is_openai).await
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
) -> Result<Response, AppError> {
    let body = response.bytes().await?;

    let response_body = if is_openai {
        let openai_json: serde_json::Value = serde_json::from_slice(&body)?;
        let anthropic_json = transform::openai_to_anthropic_response(&openai_json)?;
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

/// Handle streaming response.
async fn handle_streaming_response(
    response: reqwest::Response,
    is_openai: bool,
) -> Result<Response, AppError> {
    if !is_openai {
        // Anthropic format: pass through SSE directly
        let stream = response.bytes_stream().map(|result| {
            result.map_err(|e| {
                tracing::error!("Stream error: {e}");
                std::io::Error::other(e)
            })
        });

        let body = Body::from_stream(stream);
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-cache")
            .body(body)
            .unwrap());
    }

    // OpenAI format: convert SSE to Anthropic SSE
    let stream = transform::openai_stream_to_anthropic(response);
    let body = Body::from_stream(stream);

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(body)
        .unwrap())
}
