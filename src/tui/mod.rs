mod app;
pub mod theme;
mod ui;

use std::io;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::config::RouteRule;
use crate::error::Result;
use app::{App, ConfirmAction, MessageKind, Mode, ProviderForm, VimMode};

struct ServerHandle {
    task: JoinHandle<()>,
    shutdown_tx: watch::Sender<bool>,
    proxy_config: Arc<tokio::sync::RwLock<crate::config::AppConfig>>,
}

pub fn run_tui() -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let mut app = App::new()?;
    let mut server: Option<ServerHandle> = None;

    let (db_change_rx, _watcher) = start_db_watcher(&app).unzip();

    start_server_background(&mut app, &mut server);
    start_background_tests(&mut app);

    let result = run_loop(&mut terminal, &mut app, &mut server, db_change_rx);

    if let Some(handle) = server.take() {
        let _ = handle.shutdown_tx.send(true);
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    server: &mut Option<ServerHandle>,
    db_change_rx: Option<std::sync::mpsc::Receiver<()>>,
) -> Result<()> {
    let mut proc_tick: u8 = 0;
    loop {
        check_server_status(app, server);
        if proc_tick == 0 {
            check_bg_proxy_status(app);
        }
        proc_tick = proc_tick.wrapping_add(1) % 8;
        if app.bg_proxy_pid.is_some() {
            if let Some(rx) = &db_change_rx {
                if rx.try_recv().is_ok() {
                    while rx.try_recv().is_ok() {}
                    reload_metrics_from_db(app);
                }
            }
        }
        app.drain_test_results();
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

fn start_db_watcher(
    app: &App,
) -> Option<(std::sync::mpsc::Receiver<()>, notify::RecommendedWatcher)> {
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

fn reload_metrics_from_db(app: &mut App) {
    if let (Ok(conn), Ok(mut m)) = (app.db.lock(), app.metrics.lock()) {
        *m = crate::db::load_metrics(&conn);
    }
}

fn check_server_status(app: &mut App, server: &mut Option<ServerHandle>) {
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
            app.mode = Mode::Normal;
            Ok(())
        }
    }
}

// ─── Normal (list) mode ───────────────────────────────────────────────────────

fn handle_normal_key(
    app: &mut App,
    code: KeyCode,
    server: &mut Option<ServerHandle>,
) -> Result<()> {
    // Clear any status-bar message on next key press.
    if app.message.is_some() {
        app.message = None;
    }

    // Consume and validate the pending key (500 ms timeout).
    let prev = app
        .pending_key
        .take()
        .and_then(|(k, t)| (t.elapsed() < std::time::Duration::from_millis(500)).then_some(k));

    // Two-key sequences: dd → delete, gg → go to top.
    if let Some(pk) = prev {
        match (pk, &code) {
            ('d', KeyCode::Char('d')) => {
                if let Some(name) = app.selected_name().map(|s| s.to_string()) {
                    app.confirm(ConfirmAction::Delete(name));
                }
                return Ok(());
            }
            ('g', KeyCode::Char('g')) => {
                if !app.provider_names.is_empty() {
                    app.table_state.select(Some(0));
                }
                return Ok(());
            }
            _ => {
                // Unrecognised two-key combo — fall through and handle current
                // key as a standalone press.
            }
        }
    }

    match code {
        // ── Quit ──────────────────────────────────────────────────────────────
        KeyCode::Char('q') | KeyCode::Esc => {
            if app.bg_proxy_pid.is_some() {
                app.should_quit = true;
            } else {
                app.confirm(ConfirmAction::Quit);
            }
        }

        // ── Navigation ────────────────────────────────────────────────────────
        KeyCode::Up | KeyCode::Char('k') => app.select_prev(),
        KeyCode::Down | KeyCode::Char('j') => app.select_next(),
        KeyCode::Char('G') => {
            if !app.provider_names.is_empty() {
                let last = app.provider_names.len() - 1;
                app.table_state.select(Some(last));
            }
        }
        // First key of gg / dd — store in buffer.
        KeyCode::Char('g') => {
            app.pending_key = Some(('g', std::time::Instant::now()));
        }
        KeyCode::Char('d') => {
            app.pending_key = Some(('d', std::time::Instant::now()));
        }

        // ── Provider actions ──────────────────────────────────────────────────
        KeyCode::Char('s') => {
            app.switch_to_selected()?;
            sync_proxy_config(app, server);
        }
        // a / o → add (Vim: 'o' opens new line below, 'a' appends)
        KeyCode::Char('a') | KeyCode::Char('o') => app.start_add(),
        // e / Enter → edit
        KeyCode::Enter | KeyCode::Char('e') => {
            if app.selected_name().is_some() {
                app.start_edit();
            }
        }
        KeyCode::Char('t') => test_selected(app),
        KeyCode::Char('K') => {
            let _ = app.move_provider_up();
        }
        KeyCode::Char('J') => {
            let _ = app.move_provider_down();
        }

        // ── Config / server ───────────────────────────────────────────────────
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

// ─── Routes section key handler ───────────────────────────────────────────────

/// Handle a key press when the Routes section of the provider form has focus.
/// Operates in either "route Insert mode" (editing a pattern) or "route Normal
/// mode" (navigating / managing rules).
fn handle_routes_key(form: &mut ProviderForm, code: KeyCode, ctrl: bool) {
    if form.route_editing {
        // ── Insert mode: editing a route's pattern string ──────────────────
        match code {
            // Confirm / exit Insert mode.
            KeyCode::Enter | KeyCode::Esc => {
                form.route_editing = false;
            }
            // Tab exits the routes section entirely.
            KeyCode::Tab => {
                form.route_editing = false;
                form.focus_next();
            }
            KeyCode::BackTab => {
                form.route_editing = false;
                form.focus_prev();
            }
            // Character input (no spaces: model names never contain spaces).
            KeyCode::Char(c) if !ctrl && c != ' ' => {
                if let Some(rule) = form.routes.get_mut(form.route_cursor) {
                    rule.pattern.insert(form.route_pat_cursor, c);
                    form.route_pat_cursor += c.len_utf8();
                }
            }
            KeyCode::Backspace => {
                if let Some(rule) = form.routes.get_mut(form.route_cursor) {
                    if form.route_pat_cursor > 0 {
                        let char_len = rule.pattern[..form.route_pat_cursor]
                            .chars()
                            .next_back()
                            .map(|c| c.len_utf8())
                            .unwrap_or(1);
                        form.route_pat_cursor -= char_len;
                        rule.pattern.remove(form.route_pat_cursor);
                    }
                }
            }
            KeyCode::Delete => {
                if let Some(rule) = form.routes.get_mut(form.route_cursor) {
                    if form.route_pat_cursor < rule.pattern.len() {
                        rule.pattern.remove(form.route_pat_cursor);
                    }
                }
            }
            KeyCode::Left => {
                if let Some(rule) = form.routes.get(form.route_cursor) {
                    if form.route_pat_cursor > 0 {
                        let char_len = rule.pattern[..form.route_pat_cursor]
                            .chars()
                            .next_back()
                            .map(|c| c.len_utf8())
                            .unwrap_or(1);
                        form.route_pat_cursor -= char_len;
                    }
                }
            }
            KeyCode::Right => {
                if let Some(rule) = form.routes.get(form.route_cursor) {
                    if form.route_pat_cursor < rule.pattern.len() {
                        let char_len = rule.pattern[form.route_pat_cursor..]
                            .chars()
                            .next()
                            .map(|c| c.len_utf8())
                            .unwrap_or(1);
                        form.route_pat_cursor += char_len;
                    }
                }
            }
            KeyCode::Home => {
                form.route_pat_cursor = 0;
            }
            KeyCode::End => {
                if let Some(rule) = form.routes.get(form.route_cursor) {
                    form.route_pat_cursor = rule.pattern.len();
                }
            }
            KeyCode::Char('w') if ctrl => {
                // Ctrl+W: delete word backwards in pattern.
                if let Some(rule) = form.routes.get_mut(form.route_cursor) {
                    let mut pos = form.route_pat_cursor;
                    while pos > 0 {
                        let c = rule.pattern[..pos].chars().next_back().unwrap();
                        if c != '-' && c != '_' {
                            break;
                        }
                        pos -= c.len_utf8();
                    }
                    while pos > 0 {
                        let c = rule.pattern[..pos].chars().next_back().unwrap();
                        if c == '-' || c == '_' {
                            break;
                        }
                        pos -= c.len_utf8();
                    }
                    rule.pattern.drain(pos..form.route_pat_cursor);
                    form.route_pat_cursor = pos;
                }
            }
            _ => {}
        }
    } else {
        // ── Normal mode: navigation and rule management ─────────────────────
        match code {
            // n → new rule (append, enter Insert mode immediately).
            KeyCode::Char('n') if !ctrl => {
                form.routes.push(RouteRule::new(""));
                form.route_cursor = form.routes.len() - 1;
                form.route_pat_cursor = 0;
                form.route_editing = true;
            }

            // Space → toggle enabled flag of selected rule.
            KeyCode::Char(' ') => {
                if let Some(rule) = form.routes.get_mut(form.route_cursor) {
                    rule.enabled = !rule.enabled;
                }
            }

            // First 'd' of 'dd' — stash as pending; actual deletion is handled
            // at the top of handle_editing_key when the second 'd' arrives.
            KeyCode::Char('d') if !ctrl => {
                form.pending_key = Some(('d', std::time::Instant::now()));
            }

            // Enter / i → enter Insert mode for the selected rule's pattern.
            KeyCode::Enter | KeyCode::Char('i') => {
                if form.route_cursor < form.routes.len() {
                    form.route_editing = true;
                    form.route_pat_cursor = form.routes[form.route_cursor].pattern.len();
                }
            }

            // K / J → move rule up / down (reorder priority).
            KeyCode::Char('K') => {
                if form.route_cursor > 0 {
                    form.routes.swap(form.route_cursor, form.route_cursor - 1);
                    form.route_cursor -= 1;
                }
            }
            KeyCode::Char('J') => {
                if form.route_cursor + 1 < form.routes.len() {
                    form.routes.swap(form.route_cursor, form.route_cursor + 1);
                    form.route_cursor += 1;
                }
            }

            // k / Up → move cursor up, or exit section when at the top.
            KeyCode::Char('k') | KeyCode::Up => {
                if form.route_cursor == 0 || form.routes.is_empty() {
                    form.focus_prev();
                } else {
                    form.route_cursor -= 1;
                }
            }
            // j / Down → move cursor down (stops at last rule).
            KeyCode::Char('j') | KeyCode::Down => {
                if !form.routes.is_empty() && form.route_cursor + 1 < form.routes.len() {
                    form.route_cursor += 1;
                }
            }

            // Tab / BackTab → leave routes section.
            KeyCode::Tab => form.focus_next(),
            KeyCode::BackTab => form.focus_prev(),

            // 'Z' — first key of ZZ / ZQ: store as pending.
            KeyCode::Char('Z') => {
                form.pending_key = Some(('Z', std::time::Instant::now()));
            }
            _ => {}
        }
    }
}

// ─── Editing (form) mode ──────────────────────────────────────────────────────

fn handle_editing_key(
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
    server: &Option<ServerHandle>,
) -> Result<()> {
    let Some(form) = &mut app.form else {
        app.mode = Mode::Normal;
        return Ok(());
    };

    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    let in_routes = form.in_routes();

    // ── Consume pending key (500 ms timeout) ─────────────────────────────────
    let prev = form
        .pending_key
        .take()
        .and_then(|(k, t)| (t.elapsed() < std::time::Duration::from_millis(500)).then_some(k));

    // ── ZZ / ZQ (Normal mode only) ────────────────────────────────────────────
    if prev == Some('Z') && form.vim_mode == VimMode::Normal {
        match code {
            KeyCode::Char('Z') | KeyCode::Char('Q') => {
                let is_save = matches!(code, KeyCode::Char('Z'));
                // Capture everything we need from form before releasing the borrow.
                let pname = if form.is_new {
                    let n = form.fields[0].value.trim().to_string();
                    (!n.is_empty()).then_some(n)
                } else {
                    None
                };
                // form borrow ends here; we only hold owned values now.
                if is_save {
                    app.save_form()?;
                    sync_proxy_config(app, server);
                    let name = pname.or_else(|| app.selected_name().map(|s| s.to_string()));
                    if let Some(n) = name {
                        test_provider_by_name(app, &n);
                    }
                } else {
                    app.form = None;
                    app.mode = Mode::Normal;
                }
                return Ok(());
            }
            _ => { /* unrecognised after Z — fall through */ }
        }
    }

    // ── dd in route Normal nav mode ───────────────────────────────────────────
    if in_routes
        && !form.route_editing
        && form.vim_mode == VimMode::Normal
        && prev == Some('d')
        && matches!(code, KeyCode::Char('d'))
    {
        if !form.routes.is_empty() && form.route_cursor < form.routes.len() {
            form.routes.remove(form.route_cursor);
            if form.route_cursor > 0 && form.route_cursor >= form.routes.len() {
                form.route_cursor -= 1;
            }
        }
        return Ok(());
    }

    // ── Esc ───────────────────────────────────────────────────────────────────
    if matches!(code, KeyCode::Esc) {
        let is_route_insert = in_routes && form.route_editing;
        let is_vim_insert = form.vim_mode == VimMode::Insert;
        if is_route_insert {
            form.route_editing = false;
        } else if is_vim_insert {
            form.vim_mode = VimMode::Normal;
        } else {
            // Normal mode Esc → cancel the form.
            app.form = None;
            app.mode = Mode::Normal;
        }
        return Ok(());
    }

    // ── Ctrl+S — save (Insert-mode compatible shortcut) ───────────────────────
    if ctrl && matches!(code, KeyCode::Char('s')) {
        let pname = if form.is_new {
            let n = form.fields[0].value.trim().to_string();
            (!n.is_empty()).then_some(n)
        } else {
            None
        };
        app.save_form()?;
        sync_proxy_config(app, server);
        let name = pname.or_else(|| app.selected_name().map(|s| s.to_string()));
        if let Some(n) = name {
            test_provider_by_name(app, &n);
        }
        return Ok(());
    }

    // ── Delegate to routes section handler ────────────────────────────────────
    if in_routes {
        handle_routes_key(form, code, ctrl);
        return Ok(());
    }

    // ── Regular field — Normal mode ───────────────────────────────────────────
    if form.vim_mode == VimMode::Normal {
        match code {
            // Enter Insert mode.
            KeyCode::Char('i') | KeyCode::Insert => {
                form.vim_mode = VimMode::Insert;
            }
            // 'a' → Insert at end of current value.
            KeyCode::Char('a') => {
                form.vim_mode = VimMode::Insert;
                form.fields[form.focused].end();
            }
            // 'A' → same as 'a'.
            KeyCode::Char('A') => {
                form.vim_mode = VimMode::Insert;
                form.fields[form.focused].end();
            }
            // First key of ZZ / ZQ.
            KeyCode::Char('Z') => {
                form.pending_key = Some(('Z', std::time::Instant::now()));
            }
            // Navigation.
            KeyCode::Char('j') | KeyCode::Down | KeyCode::Tab => form.focus_next(),
            KeyCode::Char('k') | KeyCode::Up | KeyCode::BackTab => form.focus_prev(),
            // h / l: move cursor in text fields, or toggle toggle-fields.
            KeyCode::Char('h') | KeyCode::Left => {
                let f = &mut form.fields[form.focused];
                if f.is_toggle {
                    f.toggle_value();
                } else {
                    f.move_left();
                }
            }
            KeyCode::Char('l') | KeyCode::Right => {
                let f = &mut form.fields[form.focused];
                if f.is_toggle {
                    f.toggle_value();
                } else {
                    f.move_right();
                }
            }
            // Space: toggle toggle-type fields.
            KeyCode::Char(' ') => {
                if form.fields[form.focused].is_toggle {
                    form.fields[form.focused].toggle_value();
                }
            }
            // Enter on a regular field: move to next (or enter Insert for multiline).
            KeyCode::Enter => {
                if form.fields[form.focused].is_multiline {
                    form.vim_mode = VimMode::Insert;
                } else {
                    form.focus_next();
                }
            }
            // Cursor jumps.
            KeyCode::Home | KeyCode::Char('0') => form.fields[form.focused].home(),
            KeyCode::End | KeyCode::Char('$') => form.fields[form.focused].end(),
            _ => {}
        }
        return Ok(());
    }

    // ── Regular field — Insert mode ───────────────────────────────────────────
    match code {
        KeyCode::Enter => {
            let is_ml = form.fields[form.focused].is_multiline;
            if is_ml {
                form.fields[form.focused].insert_newline();
            } else {
                form.focus_next();
            }
        }
        KeyCode::Tab => form.focus_next(),
        KeyCode::BackTab => form.focus_prev(),
        // Up / Down on single-line fields moves to prev/next field.
        KeyCode::Down if !form.fields[form.focused].is_multiline => form.focus_next(),
        KeyCode::Up if !form.fields[form.focused].is_multiline => form.focus_prev(),
        KeyCode::Down => {
            if !form.fields[form.focused].move_down() {
                form.focus_next();
            }
        }
        KeyCode::Up => {
            if !form.fields[form.focused].move_up() {
                form.focus_prev();
            }
        }
        KeyCode::Char('j') if ctrl => {
            if form.fields[form.focused].is_multiline {
                if !form.fields[form.focused].move_down() {
                    form.focus_next();
                }
            } else {
                form.focus_next();
            }
        }
        KeyCode::Char('k') if ctrl => {
            if form.fields[form.focused].is_multiline {
                if !form.fields[form.focused].move_up() {
                    form.focus_prev();
                }
            } else {
                form.focus_prev();
            }
        }
        _ => {
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

// ─── Confirm mode ─────────────────────────────────────────────────────────────

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

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Write the current TUI config into the proxy's shared RwLock so changes take
/// effect immediately.  When a detached background proxy is running, sends a
/// POST /reload request instead.
fn sync_proxy_config(app: &App, server: &Option<ServerHandle>) {
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

fn start_server_background(app: &mut App, server: &mut Option<ServerHandle>) {
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
fn toggle_bg_proxy(app: &mut App, server: &mut Option<ServerHandle>) {
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
