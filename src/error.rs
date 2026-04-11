use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("Config error: {0}")]
    Config(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("HTTP request error: {0}")]
    Request(#[from] reqwest::Error),

    #[error("Provider not found: {0}")]
    ProviderNotFound(String),

    #[error("No current provider configured")]
    NoCurrentProvider,

    #[error("Transform error: {0}")]
    Transform(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            AppError::ProviderNotFound(_) | AppError::NoCurrentProvider => (
                StatusCode::BAD_REQUEST,
                "Provider not found or not configured".to_string(),
            ),
            AppError::Request(_) => (
                StatusCode::BAD_GATEWAY,
                "Upstream provider error".to_string(),
            ),
            _ => {
                // Log internal errors for debugging, but don't expose details to client
                tracing::error!("Internal proxy error: {}", self);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Internal proxy error".to_string(),
                )
            }
        };

        let body = serde_json::json!({
            "type": "error",
            "error": {
                "type": "proxy_error",
                "message": message
            }
        });

        (status, axum::Json(body)).into_response()
    }
}

pub type Result<T> = std::result::Result<T, AppError>;
