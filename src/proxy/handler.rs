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
    // Read config without hot-reload on every request
    let provider = {
        let config = state.config.read().await;
        let (_id, provider) = config.current_provider()?;
        provider.clone()
    };
    let api_key = provider.resolve_api_key()?;

    // Check if streaming from original request body
    let is_stream = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("stream").and_then(|s| s.as_bool()))
        .unwrap_or(false);

    // Determine if we need format conversion
    let is_openai = provider.api_format == ApiFormat::OpenAI;

    // Transform request body if needed
    let upstream_body = if is_openai {
        let request_json: serde_json::Value = serde_json::from_slice(&body)?;
        let transformed = transform::anthropic_to_openai_request(&request_json, &provider)?;
        Bytes::from(serde_json::to_vec(&transformed)?)
    } else {
        body
    };

    let response = forwarder::forward_request(
        &state.http_client,
        &provider,
        &api_key,
        upstream_body,
        &headers,
    )
    .await?;

    let status = response.status();

    if !status.is_success() {
        // Forward error response as-is
        let error_body = response.bytes().await?;
        tracing::warn!("Upstream returned {status}: {}", String::from_utf8_lossy(&error_body));
        return Ok((
            StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
            [("content-type", "application/json")],
            error_body,
        )
            .into_response());
    }

    if is_stream {
        handle_streaming_response(response, is_openai).await
    } else {
        handle_buffered_response(response, is_openai).await
    }
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
