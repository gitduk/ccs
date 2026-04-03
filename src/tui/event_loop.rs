use std::sync::mpsc::Receiver;

use super::App;
use super::ServerHandle;
use super::state::{MessageKind, ServerStatus, is_process_alive};

pub(super) fn check_bg_proxy_status(app: &mut App) {
    if let Some(pid) = app.bg_proxy_pid
        && !is_process_alive(pid)
    {
        app.on_bg_proxy_died();
        app.set_message("Background proxy exited", MessageKind::Info);
    }
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

pub(crate) fn reload_metrics_from_db(app: &mut App) {
    let (fresh, fresh_models) = app.db.load_all();
    if let Ok(mut m) = app.metrics.lock() {
        // Preserve last_error — it's ephemeral session state, not stored in DB.
        let saved_errors = std::mem::take(&mut m.last_error);
        *m = fresh;
        m.last_error = saved_errors;
    }
    app.models.provider_models = fresh_models;
    // NOTE: models_scroll is intentionally NOT reset here.
    // draw_models already clamps scroll to max_scroll on every frame,
    // so stale offsets are harmless. Resetting would jump the viewport
    // back to the top every time the DB watcher fires.
}

pub(super) fn check_server_status(app: &mut App, server: &mut Option<ServerHandle>) {
    if let Some(handle) = server.as_ref()
        && handle.task.is_finished()
    {
        let handle = server.take().unwrap();
        let result =
            tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(handle.task));
        match result {
            Ok(()) => {
                app.server_status = ServerStatus::Stopped;
                app.set_message("Proxy stopped", MessageKind::Info);
            }
            Err(e) => {
                let msg = format!("Proxy crashed: {e}");
                app.server_status = ServerStatus::Error(msg.clone());
                app.set_message(msg, MessageKind::Error);
            }
        }
    }
}
