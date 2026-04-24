//! TUI runtime and feature modules.
//!
//! End-user features live as top-level modules in `src/tui/`.
//! Shared rendering infrastructure remains under `src/tui/ui/`.

mod editor;
mod logs;
mod models;
mod provider_stats;
mod providers;
mod quota_command;
mod state;
pub mod theme;
mod ui;

mod event_loop;
mod input;
mod quota_panel;
mod server;
mod testing;

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::error::Result;
use state::App;

use event_loop::{
    check_bg_proxy_status, check_server_status, reload_metrics_from_db, start_db_watcher,
};
use server::start_server_background;
use testing::start_background_tests;

struct ServerHandle {
    task: JoinHandle<crate::error::Result<()>>,
    shutdown_tx: watch::Sender<bool>,
    config_tx: watch::Sender<crate::config::AppConfig>,
}

pub fn run_tui() -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        crossterm::event::EnableBracketedPaste,
        crossterm::event::EnableFocusChange
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let mut app = App::new()?;
    let mut server: Option<ServerHandle> = None;

    let (db_change_rx, _watcher) = start_db_watcher(&app).unzip();

    if app.bg_proxy_pid.is_none() {
        start_server_background(&mut app, &mut server);
    }
    start_background_tests(&mut app);

    let result = run_loop(&mut terminal, &mut app, &mut server, db_change_rx);

    if let Some(handle) = server.take() {
        let _ = handle.shutdown_tx.send(true);
    }

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        crossterm::event::DisableFocusChange,
        crossterm::event::DisableBracketedPaste,
        LeaveAlternateScreen
    )?;
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
    let mut metrics_tick: u8 = 0;
    loop {
        check_server_status(app, server);
        if proc_tick == 0 {
            check_bg_proxy_status(app);
        }
        proc_tick = proc_tick.wrapping_add(1) % 8;

        let mut db_changed = false;
        if let Some(rx) = db_change_rx.as_ref() {
            while rx.try_recv().is_ok() {
                db_changed = true;
            }
        }

        // Reload metrics every 4 frames (~1s) in all modes; also reload immediately
        // when the DB watcher fires (bg_proxy mode only).
        // In bg-proxy mode, also sync request logs every frame to ensure low latency.
        if db_changed || metrics_tick == 0 {
            reload_metrics_from_db(app);
        } else if app.bg_proxy_pid.is_some() {
            // Sync request logs only (lightweight) every frame in bg-proxy mode.
            let logs = app
                .db
                .load_recent_request_logs(app.config.request_log_limit);
            if let Ok(mut log) = app.request_log.lock() {
                log.replace(logs);
            }
        }
        metrics_tick = metrics_tick.wrapping_add(1) % 4;

        app.drain_test_results();
        app.tick_message();

        terminal.draw(|f| ui::draw(f, app))?;

        if event::poll(Duration::from_millis(250))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    input::handle_key(app, key.code, key.modifiers, server)?;
                }
                Event::Paste(text) => {
                    input::handle_paste(app, &text)?;
                }
                Event::FocusGained => {
                    app.terminal_focused = true;
                }
                Event::FocusLost => {
                    app.terminal_focused = false;
                }
                _ => {}
            }
        }

        if app.should_quit {
            break;
        }
    }
    Ok(())
}
