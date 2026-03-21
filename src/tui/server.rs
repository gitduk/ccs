use std::sync::Arc;

use tokio::sync::watch;

use super::app::{self, MessageKind};
use super::App;
use super::ServerHandle;

/// Write the current TUI config into the proxy's shared RwLock so changes take
/// effect immediately.  When a detached background proxy is running, sends a
/// POST /reload request instead.
pub(super) fn sync_proxy_config(app: &App, server: &Option<ServerHandle>) {
    if let Some(handle) = server {
        let config = app.config.clone();
        let proxy_config = handle.proxy_config.clone();
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                *proxy_config.write().await = config;
            });
        });
    } else if app.bg_proxy_pid.is_some() {
        let port = app.config.listen.rsplit(':').next().unwrap_or("7896");
        let url = format!("http://127.0.0.1:{port}/reload");
        let client = app.test_client.clone();
        tokio::spawn(async move {
            let result =
                tokio::time::timeout(std::time::Duration::from_secs(5), client.post(&url).send())
                    .await;
            match result {
                Err(_) => tracing::warn!("Reload request to background proxy timed out"),
                Ok(Err(e)) => {
                    tracing::warn!("Failed to notify background proxy of config change: {e}")
                }
                Ok(Ok(resp)) if !resp.status().is_success() => {
                    tracing::warn!("Background proxy reload returned {}", resp.status());
                }
                Ok(Ok(_)) => {}
            }
        });
    }
}

pub(super) fn start_server_background(app: &mut App, server: &mut Option<ServerHandle>) {
    if app.config.current.is_empty() || app.config.providers.is_empty() {
        app.set_message("No provider configured. Add one first.", MessageKind::Error);
        return;
    }

    let listen = app.config.listen.clone();
    let proxy_config = Arc::new(tokio::sync::RwLock::new(app.config.clone()));
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    app.server_status = app::ServerStatus::Starting;

    let metrics = app.metrics.clone();
    let db = app.db.clone();
    let proxy_config_server = proxy_config.clone();
    let task = tokio::spawn(async move {
        if let Err(e) =
            crate::proxy::start_server_with_shutdown(proxy_config_server, shutdown_rx, metrics, db)
                .await
        {
            tracing::error!("Proxy server error: {e}");
        }
    });

    *server = Some(ServerHandle {
        task,
        shutdown_tx,
        proxy_config,
    });
    app.server_status = app::ServerStatus::Running;
    app.set_message(format!("Proxy started on {listen}"), MessageKind::Success);
}

/// Toggle the detached background proxy (Shift+S).
pub(super) fn toggle_bg_proxy(app: &mut App, server: &mut Option<ServerHandle>) {
    if app.bg_proxy_pid.is_some() {
        app.stop_bg_proxy();
        app.set_message("Background proxy stopped", MessageKind::Info);
        start_server_background(app, server);
    } else {
        if let Some(handle) = server.take() {
            let _ = handle.shutdown_tx.send(true);
        }
        app.server_status = app::ServerStatus::Stopped;

        tokio::task::block_in_place(|| {
            std::thread::sleep(std::time::Duration::from_millis(200));
        });

        match app.spawn_bg_proxy() {
            Ok(()) => app.set_message(
                format!(
                    "Background proxy running on {}  — safe to quit TUI",
                    app.config.listen
                ),
                MessageKind::Success,
            ),
            Err(e) => {
                app.set_message(
                    format!("Failed to start background proxy: {e}"),
                    MessageKind::Error,
                );
                start_server_background(app, server);
            }
        }
    }
}
