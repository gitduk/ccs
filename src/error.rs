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
        let status = match &self {
            AppError::ProviderNotFound(_) | AppError::NoCurrentProvider => StatusCode::BAD_REQUEST,
            AppError::Request(_) => StatusCode::BAD_GATEWAY,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };

        let body = serde_json::json!({
            "type": "error",
            "error": {
                "type": "proxy_error",
                "message": self.to_string()
            }
        });

        (status, axum::Json(body)).into_response()
    }
}

pub type Result<T> = std::result::Result<T, AppError>;
