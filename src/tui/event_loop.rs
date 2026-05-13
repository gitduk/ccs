use std::collections::HashMap;
use std::sync::mpsc::Receiver;

use crate::proxy::metrics::TokenMetrics;

use super::App;
use super::ServerHandle;
use super::state::{MessageKind, ServerStatus, is_process_alive};

pub(super) fn check_bg_proxy_status(app: &mut App) -> bool {
    if let Some(pid) = app.bg_proxy_pid
        && !is_process_alive(pid)
    {
        app.on_bg_proxy_died();
        app.set_message("Background proxy exited", MessageKind::Info);
        return true;
    }
    false
}

pub(super) fn start_db_watcher(app: &App) -> Option<(Receiver<()>, notify::RecommendedWatcher)> {
    use notify::event::ModifyKind;
    use notify::{EventKind, RecursiveMode, Watcher, recommended_watcher};

    let db_path = app.config.resolve_db_path();
    let db_file = std::path::PathBuf::from(&db_path);
    let watch_dir = db_file.parent()?.to_path_buf();

    let (event_tx, event_rx) = std::sync::mpsc::channel::<()>();

    let db_name = db_file.file_name()?.to_os_string();
    let mut watcher = recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            let is_modify = matches!(
                event.kind,
                EventKind::Modify(ModifyKind::Data(_) | ModifyKind::Any) | EventKind::Create(_)
            );
            if !is_modify {
                return;
            }
            let relevant = event
                .paths
                .iter()
                .any(|p| p.file_name().map(|n| n == db_name).unwrap_or(false));
            if relevant {
                let _ = event_tx.send(());
            }
        }
    })
    .ok()?;

    watcher
        .watch(&watch_dir, RecursiveMode::NonRecursive)
        .ok()?;

    Some((event_rx, watcher))
}

pub(crate) struct MetricsSnapshot {
    pub metrics: TokenMetrics,
    pub provider_models: HashMap<String, Vec<String>>,
}

pub(crate) fn load_metrics_snapshot(app: &App) -> MetricsSnapshot {
    let (metrics, provider_models) = app.db.load_all();
    MetricsSnapshot {
        metrics,
        provider_models,
    }
}

pub(crate) fn apply_metrics_snapshot(app: &mut App, snapshot: MetricsSnapshot) -> bool {
    let MetricsSnapshot {
        metrics,
        provider_models,
    } = snapshot;

    let error_snapshot: Vec<(String, u16, String)> = if let Ok(mut m) = app.metrics.lock() {
        let saved_errors = std::mem::take(&mut m.last_error);
        *m = metrics;
        m.last_error = saved_errors;
        m.last_error
            .iter()
            .map(|(p, e)| (p.clone(), e.status, e.message.clone()))
            .collect()
    } else {
        vec![]
    };

    app.seen_provider_errors
        .retain(|p, _| error_snapshot.iter().any(|(ep, _, _)| ep == p));

    for (provider, status, message) in error_snapshot {
        let key = format!("{status}:{message}");
        if app
            .seen_provider_errors
            .get(&provider)
            .is_none_or(|k| k != &key)
        {
            app.seen_provider_errors.insert(provider.clone(), key);
            let text = if status == 0 {
                format!("[{provider}] network error — {message}")
            } else {
                format!("[{provider}] HTTP {status} — {message}")
            };
            app.push_message_log(text, MessageKind::Error);
        }
    }

    app.models.provider_models = provider_models;
    true
}

pub(crate) fn reload_metrics_from_db(app: &mut App) {
    let snapshot = load_metrics_snapshot(app);
    apply_metrics_snapshot(app, snapshot);
}

pub(crate) fn replace_request_logs_if_changed(
    app: &mut App,
    logs: Vec<crate::proxy::metrics::RequestLogEntry>,
) -> bool {
    let Ok(mut current) = app.request_log.lock() else {
        return false;
    };
    if current.entries().len() == logs.len()
        && current
            .entries()
            .iter()
            .zip(logs.iter())
            .all(|(a, b)| a == b)
    {
        return false;
    }
    current.replace(logs);
    true
}

pub(super) fn check_server_status(app: &mut App, server: &mut Option<ServerHandle>) -> bool {
    if let Some(handle) = server.as_ref()
        && handle.task.is_finished()
    {
        let handle = server.take().unwrap();
        let result =
            tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(handle.task));
        match result {
            Ok(Ok(())) => {
                app.server_status = ServerStatus::Stopped;
                app.set_message("Proxy stopped", MessageKind::Info);
            }
            Ok(Err(e)) => {
                let msg = format!("Proxy error: {e}");
                app.server_status = ServerStatus::Error(msg.clone());
                app.set_message(msg, MessageKind::Error);
            }
            Err(e) => {
                let msg = format!("Proxy crashed: {e}");
                app.server_status = ServerStatus::Error(msg.clone());
                app.set_message(msg, MessageKind::Error);
            }
        }
        return true;
    }
    false
}
