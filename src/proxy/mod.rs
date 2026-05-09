pub mod executor;
pub mod forwarder;
pub mod handler;
pub mod metrics;
pub mod transform;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::routing::{get, post};
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
    /// When set, all requests on this listener are routed exclusively to this
    /// provider (no fallback). Used by per-provider pinned port listeners.
    pub pinned_provider: Option<String>,
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

/// Extract host part from a listen address like "127.0.0.1:7896" or "[::1]:7896".
fn listen_host(listen: &str) -> &str {
    listen.rsplit_once(':').map(|(h, _)| h).unwrap_or(listen)
}

/// Compute desired set of pinned listeners from config: port → provider_name.
/// Only enabled providers with a port field are included.
fn desired_pinned(config: &AppConfig) -> HashMap<u16, String> {
    config
        .providers
        .iter()
        .filter(|(_, p)| p.enabled && p.port.is_some())
        .map(|(name, p)| (p.port.unwrap(), name.clone()))
        .collect()
}

type PinnedEntry = (
    String,
    tokio::sync::watch::Sender<bool>,
    tokio::task::JoinHandle<()>,
);
type PinnedListeners = HashMap<u16, PinnedEntry>;

/// Spawn one pinned listener bound to `host:port`, routing to `provider_name`.
/// All shared resources are cloned from `base`.
async fn spawn_pinned_listener(
    port: u16,
    provider_name: String,
    host: &str,
    base: &AppState,
) -> crate::error::Result<PinnedEntry> {
    let addr = format!("{host}:{port}");
    let state = Arc::new(AppState {
        config: base.config.clone(),
        http_client: base.http_client.clone(),
        metrics: base.metrics.clone(),
        request_log: base.request_log.clone(),
        db: base.db.clone(),
        pinned_provider: Some(provider_name.clone()),
    });
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("CCS pinned listener for '{provider_name}' on {addr}");
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let pname = provider_name.clone();
    let handle = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.changed().await;
            })
            .await
        {
            tracing::error!("Pinned listener {addr} error: {e}");
        }
        tracing::info!("CCS pinned listener for '{pname}' on {addr} stopped");
    });
    Ok((provider_name, shutdown_tx, handle))
}

/// Reconcile running pinned listeners against the desired set from config.
/// Stops listeners whose port is removed or whose provider changed; starts new ones.
async fn reconcile_pinned(
    desired: HashMap<u16, String>,
    listeners: &mut PinnedListeners,
    host: &str,
    base: &AppState,
) {
    let to_stop: Vec<u16> = listeners
        .iter()
        .filter(|(port, (name, _, _))| desired.get(port).map(|n| n != name).unwrap_or(true))
        .map(|(p, _)| *p)
        .collect();
    for port in to_stop {
        let (name, tx, handle) = listeners.remove(&port).unwrap();
        tracing::info!("CCS stopping pinned listener for '{name}' on port {port}");
        let _ = tx.send(true);
        let _ = handle.await;
    }
    for (port, provider_name) in desired {
        if listeners.contains_key(&port) {
            continue;
        }
        match spawn_pinned_listener(port, provider_name, host, base).await {
            Ok(entry) => {
                listeners.insert(port, entry);
            }
            Err(e) => tracing::error!("Failed to start pinned listener on port {port}: {e}"),
        }
    }
}

/// Stop and drain all pinned listeners, shutting them down concurrently.
async fn stop_all_pinned(listeners: &mut PinnedListeners) {
    let handles: Vec<_> = listeners
        .drain()
        .map(|(port, (name, tx, handle))| {
            tracing::info!("CCS stopping pinned listener for '{name}' on port {port}");
            let _ = tx.send(true);
            handle
        })
        .collect();
    futures::future::join_all(handles).await;
}

/// Start the proxy server (CLI mode, shuts down on Ctrl+C / SIGTERM).
pub async fn start_server(config: AppConfig) -> crate::error::Result<()> {
    let listen = config.listen.clone();
    let host = listen_host(&listen).to_owned();
    let db = crate::repo::Repository::open(&config.resolve_db_path());
    if let Err(e) = db.migrate(&config.name_to_id_map()) {
        tracing::warn!("DB schema migration failed: {e}");
    }
    let shared_config = Arc::new(RwLock::new(config.clone()));
    let base_state = Arc::new(AppState {
        config: shared_config.clone(),
        http_client: build_http_client(),
        metrics: Arc::new(std::sync::Mutex::new(metrics::TokenMetrics::default())),
        request_log: Arc::new(std::sync::Mutex::new(metrics::RequestLog::default())),
        db,
        pinned_provider: None,
    });
    let app = build_router(base_state.clone());

    // Spawn initial pinned listeners.
    let pinned = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    reconcile_pinned(
        desired_pinned(&config),
        &mut *pinned.lock().await,
        &host,
        &base_state,
    )
    .await;

    // Reload config from disk on SIGHUP so the TUI can signal changes.
    #[cfg(unix)]
    {
        let reload_config = shared_config;
        let pinned = pinned.clone();
        let host = host.clone();
        let base_state = base_state.clone();
        tokio::spawn(async move {
            let mut sig = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
                .expect("Failed to install SIGHUP handler");
            while sig.recv().await.is_some() {
                match crate::config::load_config() {
                    Ok(new_cfg) => {
                        let desired = {
                            let mut cfg = reload_config.write().await;
                            if cfg.listen != new_cfg.listen {
                                tracing::warn!("SIGHUP: listen address changed — restart required");
                            }
                            *cfg = new_cfg;
                            desired_pinned(&cfg)
                        };
                        tracing::info!("SIGHUP: config reloaded");
                        reconcile_pinned(desired, &mut *pinned.lock().await, &host, &base_state)
                            .await;
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

    stop_all_pinned(&mut *pinned.lock().await).await;
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
    let host = listen_host(&listen).to_owned();
    let shared_config = Arc::new(RwLock::new(initial_config.clone()));

    let base_state = Arc::new(AppState {
        config: shared_config,
        http_client: build_http_client(),
        metrics,
        request_log,
        db,
        pinned_provider: None,
    });
    let app = build_router(base_state.clone());

    let listener = tokio::net::TcpListener::bind(&listen).await?;
    tracing::info!("CCS proxy listening on {listen}");

    let (global_shutdown_tx, mut global_shutdown_rx) = tokio::sync::watch::channel(false);
    let main_handle = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = global_shutdown_rx.changed().await;
            })
            .await
        {
            tracing::error!("Main proxy listener error: {e}");
        }
    });

    // Spawn initial pinned listeners.
    let mut pinned: PinnedListeners = HashMap::new();
    reconcile_pinned(
        desired_pinned(&initial_config),
        &mut pinned,
        &host,
        &base_state,
    )
    .await;

    // Event loop: react to config changes and global shutdown.
    loop {
        tokio::select! {
            res = shutdown_rx.changed() => {
                if res.is_err() || *shutdown_rx.borrow() {
                    break;
                }
            }
            res = config_rx.changed() => {
                if res.is_err() { break; }
                let new_cfg = config_rx.borrow_and_update().clone();
                *base_state.config.write().await = new_cfg.clone();
                reconcile_pinned(desired_pinned(&new_cfg), &mut pinned, &host, &base_state).await;
            }
        }
    }

    // Graceful shutdown.
    let _ = global_shutdown_tx.send(true);
    stop_all_pinned(&mut pinned).await;
    let _ = main_handle.await;
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
