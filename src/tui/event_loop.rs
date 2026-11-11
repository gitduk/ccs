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

/// Apply one background request-log sync: merge the rows read from the DB (or
/// replace the window when the table was cleared under us), then patch the
/// usage a finished stream wrote into rows already held. Returns whether the
/// panel changed and so needs a redraw.
pub(crate) fn append_request_log_rows(app: &mut App, patch: super::RequestLogPatch) -> bool {
    let Ok(mut log) = app.request_log.lock() else {
        return false;
    };
    let keep = app.config.request_log_limit;
    let mut changed = if patch.reset {
        log.replace(patch.rows, keep);
        true
    } else {
        log.merge(patch.rows, keep)
    };
    changed |= log.apply_usage(&patch.usage);
    changed
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
