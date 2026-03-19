mod app;
pub mod theme;
mod ui;

use std::io;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use app::{App, ConfirmAction, Mode, MessageKind, ServerStatus};
use crate::error::Result;

struct ServerHandle {
    task: JoinHandle<()>,
    shutdown_tx: watch::Sender<bool>,
    proxy_config: Arc<tokio::sync::RwLock<crate::config::AppConfig>>,
}

pub fn run_tui() -> Result<()> {
    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let mut app = App::new()?;
    let mut server: Option<ServerHandle> = None;

    // Watch the DB file for cross-process writes (background proxy mode).
    // Hold `_watcher` for its Drop impl to stay alive for the TUI lifetime.
    let (db_change_rx, _watcher) = start_db_watcher(&app).unzip();

    start_server_background(&mut app, &mut server);
    start_background_tests(&mut app);

    let result = run_loop(&mut terminal, &mut app, &mut server, db_change_rx);

    // Stop server on exit
    if let Some(handle) = server.take() {
        let _ = handle.shutdown_tx.send(true);
    }

    // Restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    server: &mut Option<ServerHandle>,
    db_change_rx: Option<mpsc::Receiver<()>>,
) -> Result<()> {
    // /proc reads are cheap but not free — throttle to every 8 frames (~2 s).
    let mut proc_tick: u8 = 0;
    loop {
        // Check if server task has ended unexpectedly
        check_server_status(app, server);
        // Check if background proxy process is still alive (throttled).
        if proc_tick == 0 {
            check_bg_proxy_status(app);
        }
        proc_tick = proc_tick.wrapping_add(1) % 8;
        // When background proxy is active, reload metrics whenever the DB file
        // changes (inotify-driven, not a timer).  Drain all pending events so
        // we reload at most once per frame even if many writes batched up.
        if app.bg_proxy_pid.is_some() {
            if let Some(rx) = &db_change_rx {
                if rx.try_recv().is_ok() {
                    // Drain any additional pending events from the same batch.
                    while rx.try_recv().is_ok() {}
                    reload_metrics_from_db(app);
                }
            }
        }
        // Collect completed background test results
        app.drain_test_results();
        // Auto-dismiss expired messages
        app.tick_message();

        terminal.draw(|f| ui::draw(f, app))?;

        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                handle_key(app, key.code, key.modifiers, server)?;
            }
        }

        if app.should_quit {
            break;
        }
    }
    Ok(())
}

fn check_bg_proxy_status(app: &mut App) {
    if let Some(pid) = app.bg_proxy_pid {
        if !app::is_process_alive(pid) {
            app.on_bg_proxy_died();
            app.set_message("Background proxy exited", MessageKind::Info);
        }
    }
}

/// Start an inotify watcher on the SQLite DB directory.
/// Returns `(Receiver, Watcher)` — the caller must hold the Watcher alive
/// for its Drop impl to keep the kernel watch descriptor open.
/// Returns `None` if the DB path is unavailable or watcher init fails (non-fatal).
fn start_db_watcher(app: &App) -> Option<(mpsc::Receiver<()>, notify::RecommendedWatcher)> {
    use notify::{EventKind, RecursiveMode, Watcher, recommended_watcher};
    use notify::event::ModifyKind;

    let db_path = app.config.resolve_db_path();
    let db_file = std::path::PathBuf::from(&db_path);
    let watch_dir = db_file.parent()?.to_path_buf();

    let (event_tx, event_rx) = mpsc::channel::<()>();

    let db_name = db_file.file_name()?.to_os_string();
    let mut watcher = recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            // Only react to actual content modifications, not metadata changes.
            let is_modify = matches!(
                event.kind,
                EventKind::Modify(ModifyKind::Data(_) | ModifyKind::Any)
                    | EventKind::Create(_)
            );
            if !is_modify {
                return;
            }
            // Filter to the DB file and its WAL/SHM siblings.
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

    watcher.watch(&watch_dir, RecursiveMode::NonRecursive).ok()?;

    Some((event_rx, watcher))
}

fn reload_metrics_from_db(app: &mut App) {
    if let (Ok(conn), Ok(mut m)) = (app.db.lock(), app.metrics.lock()) {
        *m = crate::db::load_metrics(&conn);
    }
}

fn check_server_status(app: &mut App, server: &mut Option<ServerHandle>) {
    if let Some(handle) = server.as_ref() {
        if handle.task.is_finished() {
            let handle = server.take().unwrap();
            // Try to get the error from the finished task
            let result = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(handle.task)
            });
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
}

fn handle_key(
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
    server: &mut Option<ServerHandle>,
) -> Result<()> {
    match &app.mode {
        Mode::Normal => handle_normal_key(app, code, server),
        Mode::Editing => handle_editing_key(app, code, modifiers, server),
        Mode::Confirm => handle_confirm_key(app, code, server),
        Mode::Help => {
            // Any key closes help panel
            app.mode = Mode::Normal;
            Ok(())
        }
    }
}

fn handle_normal_key(app: &mut App, code: KeyCode, server: &mut Option<ServerHandle>) -> Result<()> {
    // Clear any status bar message on next key press
    if app.message.is_some() {
        app.message = None;
    }

    match code {
        KeyCode::Char('q') | KeyCode::Esc => {
            if app.bg_proxy_pid.is_some() {
                app.should_quit = true;
            } else {
                app.confirm(ConfirmAction::Quit);
            }
        }
        KeyCode::Up | KeyCode::Char('k') => app.select_prev(),
        KeyCode::Down | KeyCode::Char('j') => app.select_next(),
        KeyCode::Char('s') => {
            app.switch_to_selected()?;
            sync_proxy_config(app, server);
        }
        KeyCode::Char('a') => app.start_add(),
        KeyCode::Char('e') => {
            if app.selected_name().is_some() {
                app.start_edit();
            }
        }
        KeyCode::Char('d') => {
            if let Some(name) = app.selected_name().map(|s| s.to_string()) {
                app.confirm(ConfirmAction::Delete(name));
            }
        }
        KeyCode::Char('t') => {
            test_selected(app);
        }
        KeyCode::Char('K') => { let _ = app.move_provider_up(); }
        KeyCode::Char('J') => { let _ = app.move_provider_down(); }
        KeyCode::Char('f') => {
            let _ = app.toggle_fallback();
            sync_proxy_config(app, server);
        }
        KeyCode::Char('r') => {
            let _ = app.reload_config();
            sync_proxy_config(app, server);
        }
        KeyCode::Char('S') => {
            toggle_bg_proxy(app, server);
        }
        KeyCode::Char('c') => app.confirm(ConfirmAction::Clear),
        KeyCode::Char('h') | KeyCode::Char('?') => {
            app.mode = Mode::Help;
        }
        _ => {}
    }
    Ok(())
}

/// Write the current TUI config into the proxy's shared RwLock so changes take effect immediately.
fn sync_proxy_config(app: &App, server: &Option<ServerHandle>) {
    if let Some(handle) = server {
        let config = app.config.clone();
        let proxy_config = handle.proxy_config.clone();
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                *proxy_config.write().await = config;
            });
        });
    }
}

fn start_server_background(app: &mut App, server: &mut Option<ServerHandle>) {
    // Check if there's a current provider
    if app.config.current.is_empty() || app.config.providers.is_empty() {
        app.set_message("No provider configured. Add one first.", MessageKind::Error);
        return;
    }

    // Start the server
    let listen = app.config.listen.clone();
    let proxy_config = Arc::new(tokio::sync::RwLock::new(app.config.clone()));
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    app.server_status = ServerStatus::Starting;

    let metrics = app.metrics.clone();
    let db = app.db.clone();
    let proxy_config_server = proxy_config.clone();
    let task = tokio::spawn(async move {
        if let Err(e) = crate::proxy::start_server_with_shutdown(proxy_config_server, shutdown_rx, metrics, db).await {
            tracing::error!("Proxy server error: {e}");
        }
    });

    *server = Some(ServerHandle { task, shutdown_tx, proxy_config });
    app.server_status = ServerStatus::Running;
    app.set_message(format!("Proxy started on {listen}"), MessageKind::Success);
}

fn handle_editing_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers, server: &Option<ServerHandle>) -> Result<()> {
    let Some(form) = &mut app.form else {
        app.mode = Mode::Normal;
        return Ok(());
    };

    match code {
        KeyCode::Esc => {
            app.form = None;
            app.mode = Mode::Normal;
        }
        KeyCode::Enter | KeyCode::Char('s') if code == KeyCode::Enter || modifiers.contains(KeyModifiers::CONTROL) => {
            // For new providers the name comes from the form's first field;
            // for edits it's the currently selected provider.
            let provider_name = if form.is_new {
                let name = form.fields[0].value.trim().to_string();
                if name.is_empty() { None } else { Some(name) }
            } else {
                app.selected_name().map(|s| s.to_string())
            };
            app.save_form()?;
            sync_proxy_config(app, server);
            if let Some(name) = provider_name {
                test_provider_by_name(app, &name);
            }
        }
        KeyCode::Tab | KeyCode::Down => form.focus_next(),
        KeyCode::BackTab | KeyCode::Up => form.focus_prev(),
        KeyCode::Char('j') if modifiers.contains(KeyModifiers::CONTROL) => form.focus_next(),
        KeyCode::Char('k') if modifiers.contains(KeyModifiers::CONTROL) => form.focus_prev(),
        _ => {
            let ctrl = modifiers.contains(KeyModifiers::CONTROL);
            let field = &mut form.fields[form.focused];
            if field.is_toggle {
                match code {
                    KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') => {
                        field.toggle_value();
                    }
                    KeyCode::Char('h') | KeyCode::Char('l') if ctrl => {
                        field.toggle_value();
                    }
                    _ => {}
                }
            } else {
                match code {
                    KeyCode::Char(c) if !ctrl => field.insert(c),
                    KeyCode::Char('w') if ctrl => field.delete_word_back(),
                    KeyCode::Char('h') if ctrl => field.backspace(),
                    KeyCode::Char('a') if ctrl => field.home(),
                    KeyCode::Char('e') if ctrl => field.end(),
                    KeyCode::Backspace => field.backspace(),
                    KeyCode::Delete => field.delete(),
                    KeyCode::Left => field.move_left(),
                    KeyCode::Right => field.move_right(),
                    KeyCode::Home => field.home(),
                    KeyCode::End => field.end(),
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

fn handle_confirm_key(app: &mut App, code: KeyCode, server: &Option<ServerHandle>) -> Result<()> {
    match code {
        KeyCode::Char('y') | KeyCode::Enter => {
            app.confirm_action_execute()?;
            sync_proxy_config(app, server);
        }
        _ => {
            app.confirm_action = None;
            app.mode = Mode::Normal;
        }
    }
    Ok(())
}

fn test_selected(app: &mut App) {
    let Some(name) = app.selected_name().map(|s| s.to_string()) else {
        return;
    };
    test_provider_by_name(app, &name);
}

fn test_provider_by_name(app: &mut App, name: &str) {
    let Some(provider) = app.config.providers.get(name) else {
        return;
    };
    let provider = provider.clone();
    let tx = app.test_tx.clone();
    let name_owned = name.to_string();

    app.pending_tests.insert(name_owned.clone());
    app.set_message(format!("Testing {name}…"), MessageKind::Info);

    let client = app.test_client.clone();
    tokio::spawn(async move {
        let result = crate::test_provider::test_connectivity(&client, &provider).await;
        let _ = tx.send((name_owned, result));
    });
}

fn start_background_tests(app: &mut App) {
    let names: Vec<String> = app.config.providers.keys().cloned().collect();
    for name in names {
        test_provider_by_name(app, &name);
    }
}

/// Toggle the detached background proxy (Shift+S).
///
/// ON:  stop the in-TUI server, wait briefly for the port to be released,
///      then spawn a detached `ccs serve` child process.
/// OFF: kill the background child and restart the in-TUI server.
fn toggle_bg_proxy(app: &mut App, server: &mut Option<ServerHandle>) {
    if app.bg_proxy_pid.is_some() {
        // Stop background proxy and bring server back into TUI.
        app.stop_bg_proxy();
        app.set_message("Background proxy stopped", MessageKind::Info);
        start_server_background(app, server);
    } else {
        // Stop the in-TUI server so it releases the port.
        if let Some(handle) = server.take() {
            let _ = handle.shutdown_tx.send(true);
        }
        app.server_status = ServerStatus::Stopped;

        // Brief blocking sleep to let the OS release the port before the new
        // process tries to bind it.  200 ms is imperceptible to the user and
        // well within the 250 ms poll interval.  block_in_place tells tokio to
        // migrate pending tasks off this worker thread while we block.
        tokio::task::block_in_place(|| {
            std::thread::sleep(std::time::Duration::from_millis(200));
        });

        match app.spawn_bg_proxy() {
            Ok(()) => app.set_message(
                format!("Background proxy running on {}  — safe to quit TUI", app.config.listen),
                MessageKind::Success,
            ),
            Err(e) => {
                app.set_message(format!("Failed to start background proxy: {e}"), MessageKind::Error);
                // Bring the in-TUI server back up on failure.
                start_server_background(app, server);
            }
        }
    }
}
