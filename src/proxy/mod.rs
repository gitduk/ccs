pub mod forwarder;
pub mod handler;
pub mod transform;

use std::sync::Arc;

use axum::Router;
use axum::routing::{get, post};
use reqwest::Client;
use tokio::sync::RwLock;
use tower_http::cors::CorsLayer;

use crate::config::AppConfig;

pub struct AppState {
    pub config: Arc<RwLock<AppConfig>>,
    pub http_client: Client,
}

pub type SharedState = Arc<AppState>;

/// Build the axum router.
pub fn build_router(state: SharedState) -> Router {
    Router::new()
        .route("/v1/messages", post(handler::handle_messages))
        .route("/health", get(handler::health_check))
        .route("/reload", post(handler::reload_config))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

/// Start the proxy server (CLI mode, shuts down on Ctrl+C / SIGTERM).
pub async fn start_server(config: AppConfig) -> crate::error::Result<()> {
    let listen = config.listen.clone();
    let state = Arc::new(AppState {
        config: Arc::new(RwLock::new(config)),
        http_client: Client::new(),
    });
    let app = build_router(state);

    let listener = tokio::net::TcpListener::bind(&listen).await?;
    tracing::info!("CCS proxy listening on {listen}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

/// Start the proxy server with an external shutdown signal (TUI mode).
pub async fn start_server_with_shutdown(
    config: AppConfig,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> crate::error::Result<()> {
    let listen = config.listen.clone();
    let state = Arc::new(AppState {
        config: Arc::new(RwLock::new(config)),
        http_client: Client::new(),
    });
    let app = build_router(state);

    let listener = tokio::net::TcpListener::bind(&listen).await?;
    tracing::info!("CCS proxy listening on {listen}");

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.changed().await;
        })
        .await?;

    tracing::info!("CCS proxy stopped");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("Failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    tracing::info!("Shutdown signal received");
}
