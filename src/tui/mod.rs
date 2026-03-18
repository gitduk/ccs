mod app;
pub mod theme;
mod ui;

use std::io;
use std::sync::Arc;
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

use app::{App, Mode, MessageKind, ServerStatus};
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

    start_server_background(&mut app, &mut server);
    start_background_tests(&mut app);

    let result = run_loop(&mut terminal, &mut app, &mut server);

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
) -> Result<()> {
    loop {
        // Check if server task has ended unexpectedly
        check_server_status(app, server);
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
            app.confirm_quit();
        }
        KeyCode::Up | KeyCode::Char('k') => app.select_prev(),
        KeyCode::Down | KeyCode::Char('j') => app.select_next(),
        KeyCode::Char('s') => {
            app.switch_to_selected()?;
            sync_proxy_config(app, server);
        }
        KeyCode::Char('a') => app.start_add(),
        KeyCode::Char('e') => {
            if app.selected_id().is_some() {
                app.start_edit();
            }
        }
        KeyCode::Char('d') => {
            if app.selected_id().is_some() {
                app.confirm_delete();
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
        KeyCode::Char('c') => {
            app.confirm_clear();
        }
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
            // For new providers the ID comes from the form's first field;
            // for edits it's the currently selected provider.
            let provider_id = if form.is_new {
                let id = form.fields[0].value.trim().to_string();
                if id.is_empty() { None } else { Some(id) }
            } else {
                app.selected_id().map(|s| s.to_string())
            };
            app.save_form()?;
            sync_proxy_config(app, server);
            if let Some(id) = provider_id {
                test_provider_by_id(app, &id);
            }
        }
        KeyCode::Tab => {
            let len = form.fields.len();
            for offset in 1..len {
                let next = (form.focused + offset) % len;
                if form.fields[next].editable {
                    form.focused = next;
                    break;
                }
            }
        }
        KeyCode::BackTab => {
            let len = form.fields.len();
            for offset in 1..len {
                let prev = (form.focused + len - offset) % len;
                if form.fields[prev].editable {
                    form.focused = prev;
                    break;
                }
            }
        }
        KeyCode::Up | KeyCode::Char('k') if modifiers.contains(KeyModifiers::CONTROL) => {
            let len = form.fields.len();
            for offset in 1..len {
                let prev = (form.focused + len - offset) % len;
                if form.fields[prev].editable {
                    form.focused = prev;
                    break;
                }
            }
        }
        KeyCode::Up => {
            let len = form.fields.len();
            for offset in 1..len {
                let prev = (form.focused + len - offset) % len;
                if form.fields[prev].editable {
                    form.focused = prev;
                    break;
                }
            }
        }
        KeyCode::Down | KeyCode::Char('j') if modifiers.contains(KeyModifiers::CONTROL) => {
            let len = form.fields.len();
            for offset in 1..len {
                let next = (form.focused + offset) % len;
                if form.fields[next].editable {
                    form.focused = next;
                    break;
                }
            }
        }
        KeyCode::Down => {
            let len = form.fields.len();
            for offset in 1..len {
                let next = (form.focused + offset) % len;
                if form.fields[next].editable {
                    form.focused = next;
                    break;
                }
            }
        }
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
    let Some(id) = app.selected_id().map(|s| s.to_string()) else {
        return;
    };
    test_provider_by_id(app, &id);
}

fn test_provider_by_id(app: &mut App, id: &str) {
    let Some(provider) = app.config.providers.get(id) else {
        return;
    };
    let provider = provider.clone();
    let tx = app.test_tx.clone();
    let id_owned = id.to_string();

    app.pending_tests.insert(id_owned.clone());
    app.set_message(format!("Testing {id}…"), MessageKind::Info);

    tokio::spawn(async move {
        let result = crate::test_provider::test_connectivity(&provider).await;
        let _ = tx.send((id_owned, result));
    });
}

fn start_background_tests(app: &mut App) {
    let ids: Vec<String> = app.config.providers.keys().cloned().collect();
    for id in ids {
        test_provider_by_id(app, &id);
    }
}
