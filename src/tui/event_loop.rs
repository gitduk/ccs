use std::sync::mpsc::Receiver;

use super::app::{self, MessageKind};
use super::App;
use super::ServerHandle;

pub(super) fn check_bg_proxy_status(app: &mut App) {
    if let Some(pid) = app.bg_proxy_pid {
        if !app::is_process_alive(pid) {
            app.on_bg_proxy_died();
            app.set_message("Background proxy exited", MessageKind::Info);
        }
    }
}

pub(super) fn start_db_watcher(app: &App) -> Option<(Receiver<()>, notify::RecommendedWatcher)> {
    use notify::event::ModifyKind;
    use notify::{recommended_watcher, EventKind, RecursiveMode, Watcher};

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
            let relevant = event.paths.iter().any(|p| {
                p.file_name()
                    .map(|n| n.to_string_lossy().starts_with(&*db_name.to_string_lossy()))
                    .unwrap_or(false)
            });
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

pub(super) fn reload_metrics_from_db(app: &mut App) {
    if let (Ok(conn), Ok(mut m)) = (app.db.lock(), app.metrics.lock()) {
        *m = crate::db::load_metrics(&conn);
    }
}

pub(super) fn check_server_status(app: &mut App, server: &mut Option<ServerHandle>) {
    if let Some(handle) = server.as_ref() {
        if handle.task.is_finished() {
            let handle = server.take().unwrap();
            let result = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(handle.task)
            });
            match result {
                Ok(()) => {
                    app.server_status = app::ServerStatus::Stopped;
                    app.set_message("Proxy stopped", MessageKind::Info);
                }
                Err(e) => {
                    let msg = format!("Proxy crashed: {e}");
                    app.server_status = app::ServerStatus::Error(msg.clone());
                    app.set_message(msg, MessageKind::Error);
                }
            }
        }
    }
}
