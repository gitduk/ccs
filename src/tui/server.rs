use tokio::sync::watch;

use super::App;
use super::ServerHandle;
use super::state::{MessageKind, ServerStatus};

/// Sync config to the running proxy. For the in-process server, sends the
/// latest config through watch. For the background proxy, saves config to disk
/// and sends SIGHUP to trigger a reload.
pub(super) fn sync_proxy_config(app: &App, server: &Option<ServerHandle>) {
    if let Some(handle) = server {
        let _ = handle.config_tx.send(app.config.clone());
    } else if let Some(pid) = app.bg_proxy_pid {
        match crate::config::save_config(&app.config) {
            Ok(()) => super::state::send_sighup(pid),
            Err(e) => tracing::error!("Failed to save config before SIGHUP: {e}"),
        }
    }
}

pub(super) fn start_server_background(app: &mut App, server: &mut Option<ServerHandle>) {
    if app.config.current.is_empty() || app.config.providers.is_empty() {
        app.set_message("No provider configured. Add one first.", MessageKind::Error);
        return;
    }

    let listen = app.config.listen.clone();
    let (config_tx, config_rx) = watch::channel(app.config.clone());
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    app.server_status = ServerStatus::Starting;

    let metrics = app.metrics.clone();
    let request_log = app.request_log.clone();
    let db = app.db.clone();
    let task = tokio::spawn(async move {
        crate::proxy::start_server_with_shutdown(config_rx, shutdown_rx, metrics, request_log, db)
            .await
    });

    *server = Some(ServerHandle {
        task,
        shutdown_tx,
        config_tx,
    });
    app.server_status = ServerStatus::Running;
    app.set_message(format!("Proxy started on {listen}"), MessageKind::Success);
}

/// Toggle the detached background proxy (Shift+S). When the running
/// background proxy is a stale build, restart it with the current binary
/// (stop + spawn) instead, so the new version takes over the port.
pub(super) fn toggle_bg_proxy(app: &mut App, server: &mut Option<ServerHandle>) {
    if app.bg_proxy_pid.is_some() {
        if app.bg_proxy_stale_version.is_some() {
            restart_bg_proxy(app, server);
            return;
        }
        app.stop_bg_proxy();
        app.set_message("Background proxy stopped", MessageKind::Info);
        start_server_background(app, server);
    } else {
        if let Some(handle) = server.take() {
            let _ = handle.shutdown_tx.send(true);
        }
        app.server_status = ServerStatus::Stopped;

        if !wait_until_port_free(&app.config.listen) {
            app.set_message(
                "Proxy port is still in use — press S to retry",
                MessageKind::Error,
            );
            return;
        }

        match app.spawn_bg_proxy() {
            Ok(()) => {
                // The new child is our own binary, so it cannot be stale.
                app.bg_proxy_stale_version = None;
                app.set_message(
                    format!(
                        "Background proxy running on {}  — safe to quit TUI",
                        app.config.listen
                    ),
                    MessageKind::Success,
                );
            }
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

/// Stop the stale background proxy and spawn a fresh one from the current
/// binary (current by construction). The port is only spawned on once it is
/// free; falls back to the in-process server when the spawn itself fails.
fn restart_bg_proxy(app: &mut App, server: &mut Option<ServerHandle>) {
    if let Some(handle) = server.take() {
        let _ = handle.shutdown_tx.send(true);
    }
    app.stop_bg_proxy();
    // The stale process is gone whatever happens next, so clear the badge.
    app.bg_proxy_stale_version = None;

    // Spawn only once the port is actually free: a child that fails to bind
    // would exit right away. The bound keeps a wedged process from freezing
    // the TUI; in that case the old proxy keeps draining and S retries later.
    if !wait_until_port_free(&app.config.listen) {
        app.set_message(
            "Old proxy is still draining requests — press S to retry",
            MessageKind::Error,
        );
        return;
    }

    match app.spawn_bg_proxy() {
        Ok(()) => app.set_message(
            format!(
                "Background proxy restarted — running v{}",
                env!("CARGO_PKG_VERSION")
            ),
            MessageKind::Success,
        ),
        Err(e) => {
            app.set_message(
                format!("Failed to restart background proxy: {e}"),
                MessageKind::Error,
            );
            start_server_background(app, server);
        }
    }
}

/// Wait until nothing binds `addr` anymore, up to a bound; the old proxy
/// drains its in-flight requests on SIGTERM before closing its listener.
/// Returns false when the port is still held after ~2 s.
fn wait_until_port_free(addr: &str) -> bool {
    tokio::task::block_in_place(|| {
        for _ in 0..40 {
            if std::net::TcpListener::bind(addr).is_ok() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        false
    })
}
