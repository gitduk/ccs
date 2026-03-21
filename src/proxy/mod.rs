pub mod forwarder;
pub mod handler;
pub mod metrics;
pub mod transform;

use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;
use std::time::Duration;

use reqwest::Client;
use tokio::sync::RwLock;
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::config::AppConfig;
use crate::db::SharedDb;
use metrics::SharedMetrics;

pub struct AppState {
    pub config: Arc<RwLock<AppConfig>>,
    pub http_client: Client,
    pub metrics: SharedMetrics,
    pub db: SharedDb,
}

pub type SharedState = Arc<AppState>;

/// Build the axum router.
pub fn build_router(state: SharedState) -> Router {
    Router::new()
        .route("/v1/messages", post(handler::handle_messages))
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
    let db = crate::db::open_with_fallback(&config.resolve_db_path());
    crate::db::migrate_schema(&db, &config.name_to_id_map());
    // Load persisted metrics so the in-memory counters continue from wherever
    // the previous session left off.  Without this, the first upsert would
    // overwrite the DB with counts starting from zero.
    let initial_metrics = {
        let conn = db.lock().unwrap();
        crate::db::load_metrics(&conn)
    };
    let state = Arc::new(AppState {
        config: Arc::new(RwLock::new(config)),
        http_client: build_http_client(),
        metrics: Arc::new(std::sync::Mutex::new(initial_metrics)),
        db,
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
/// Accepts a pre-built `Arc<RwLock<AppConfig>>` so the TUI can mutate it live.
pub async fn start_server_with_shutdown(
    shared_config: Arc<RwLock<AppConfig>>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
    metrics: SharedMetrics,
    db: SharedDb,
) -> crate::error::Result<()> {
    let listen = shared_config.read().await.listen.clone();
    let state = Arc::new(AppState {
        config: shared_config,
        http_client: build_http_client(),
        metrics,
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
