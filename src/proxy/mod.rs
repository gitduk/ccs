pub mod executor;
pub mod forwarder;
pub mod handler;
pub mod metrics;
pub mod transform;

use std::sync::Arc;

use axum::Router;
use axum::routing::{get, post};
use std::time::Duration;

use reqwest::Client;
use tokio::sync::RwLock;
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::config::AppConfig;
use crate::repo::Repository;
use metrics::{SharedMetrics, SharedRequestLog};

pub struct AppState {
    pub config: Arc<RwLock<AppConfig>>,
    pub http_client: Client,
    pub metrics: SharedMetrics,
    pub request_log: SharedRequestLog,
    pub db: Repository,
}

pub type SharedState = Arc<AppState>;

/// Build the axum router.
pub fn build_router(state: SharedState) -> Router {
    Router::new()
        .route("/v1/messages", post(handler::handle_messages))
        .route(
            "/v1/chat/completions",
            post(handler::handle_chat_completions),
        )
        .route("/v1/responses", post(handler::handle_responses))
        .route("/v1/models", get(handler::handle_models))
        .route("/health", get(handler::health_check))
        .layer(
            CorsLayer::new()
                .allow_origin(AllowOrigin::list([
                    "http://localhost".parse().unwrap(),
                    "http://127.0.0.1".parse().unwrap(),
                    "http://[::1]".parse().unwrap(),
                ]))
                .allow_methods(tower_http::cors::Any)
                .allow_headers(tower_http::cors::Any),
        )
        .with_state(state)
}

fn build_http_client() -> Client {
    Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(300))
        .build()
        .expect("Failed to build HTTP client")
}

/// Start the proxy server (CLI mode, shuts down on Ctrl+C / SIGTERM).
pub async fn start_server(config: AppConfig) -> crate::error::Result<()> {
    let listen = config.listen.clone();
    let db = crate::repo::Repository::open(&config.resolve_db_path());
    if let Err(e) = db.migrate(&config.name_to_id_map()) {
        tracing::warn!("DB schema migration failed: {e}");
    }
    let shared_config = Arc::new(RwLock::new(config));
    let state = Arc::new(AppState {
        config: shared_config.clone(),
        http_client: build_http_client(),
        metrics: Arc::new(std::sync::Mutex::new(metrics::TokenMetrics::default())),
        request_log: Arc::new(std::sync::Mutex::new(metrics::RequestLog::default())),
        db,
    });
    let app = build_router(state);

    // Reload config from disk on SIGHUP so the TUI can signal changes.
    #[cfg(unix)]
    {
        let reload_config = shared_config;
        tokio::spawn(async move {
            let mut sig = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
                .expect("Failed to install SIGHUP handler");
            while sig.recv().await.is_some() {
                match crate::config::load_config() {
                    Ok(new_cfg) => {
                        let mut cfg = reload_config.write().await;
                        if cfg.listen != new_cfg.listen {
                            tracing::warn!("SIGHUP: listen address changed — restart required");
                        }
                        *cfg = new_cfg;
                        tracing::info!("SIGHUP: config reloaded");
                    }
                    Err(e) => tracing::error!("SIGHUP: failed to reload config: {e}"),
                }
            }
        });
    }

    let listener = tokio::net::TcpListener::bind(&listen).await?;
    tracing::info!("CCS proxy listening on {listen}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

/// Start the proxy server with an external shutdown signal (TUI mode).
/// Receives config updates via watch so the latest config wins deterministically.
pub async fn start_server_with_shutdown(
    mut config_rx: tokio::sync::watch::Receiver<AppConfig>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
    metrics: SharedMetrics,
    request_log: SharedRequestLog,
    db: Repository,
) -> crate::error::Result<()> {
    let initial_config = config_rx.borrow().clone();
    let listen = initial_config.listen.clone();
    let shared_config = Arc::new(RwLock::new(initial_config));
    let config_for_watcher = shared_config.clone();
    tokio::spawn(async move {
        while config_rx.changed().await.is_ok() {
            let new_cfg = config_rx.borrow_and_update().clone();
            *config_for_watcher.write().await = new_cfg;
        }
    });

    let state = Arc::new(AppState {
        config: shared_config,
        http_client: build_http_client(),
        metrics,
        request_log,
        db,
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
