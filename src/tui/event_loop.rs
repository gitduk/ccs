use std::sync::mpsc::Receiver;

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

// MetricsSnapshot, load_metrics_snapshot, apply_metrics_snapshot, and
// reload_metrics_from_db are App methods defined in state/metrics_sync.rs.
// tui/mod.rs imports MetricsSnapshot directly from state.

pub(crate) fn replace_request_logs_if_changed(
    app: &mut App,
    logs: Vec<crate::metrics::RequestLogEntry>,
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
