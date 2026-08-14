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
mod quick_panel;
mod quota_panel;
mod server;
mod sysinfo;
mod testing;

use std::io;
use std::time::{Duration, Instant};

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
use state::{App, MetricsSnapshot};

use event_loop::{
    check_bg_proxy_status, check_server_status, replace_request_logs_if_changed, start_db_watcher,
};
use server::start_server_background;
use testing::start_background_tests;

const MAX_EVENTS_PER_FRAME: usize = 32;
const SYSINFO_INTERVAL: Duration = Duration::from_secs(2);
const ASYNC_DRAIN_INTERVAL: Duration = Duration::from_millis(200);
const DB_WATCHER_POLL_INTERVAL: Duration = Duration::from_millis(500);
const BG_PROXY_CHECK_INTERVAL: Duration = Duration::from_millis(500);
const BG_PROXY_LOG_SYNC_INTERVAL: Duration = Duration::from_millis(500);
const METRICS_RELOAD_INTERVAL: Duration = Duration::from_secs(5);

fn should_read_next_event(processed_events: usize, has_waiting_event: bool) -> bool {
    processed_events < MAX_EVENTS_PER_FRAME && has_waiting_event
}

struct RenderScheduler {
    next_async_drain: Instant,
    next_db_watcher_poll: Instant,
    next_bg_proxy_check: Instant,
    next_bg_proxy_log_sync: Instant,
    next_metrics_reload: Instant,
    next_sysinfo: Instant,
}

impl RenderScheduler {
    fn new(now: Instant) -> Self {
        Self {
            next_async_drain: now,
            next_db_watcher_poll: now,
            next_bg_proxy_check: now,
            next_bg_proxy_log_sync: now,
            next_metrics_reload: now,
            next_sysinfo: now,
        }
    }

    fn next_wake_in(
        &self,
        now: Instant,
        message_deadline: Option<Instant>,
        bg_proxy_active: bool,
    ) -> Duration {
        let mut next = self
            .next_async_drain
            .min(self.next_db_watcher_poll)
            .min(self.next_bg_proxy_check)
            .min(self.next_metrics_reload)
            .min(self.next_sysinfo);
        if let Some(deadline) = message_deadline {
            next = next.min(deadline);
        }
        if bg_proxy_active {
            next = next.min(self.next_bg_proxy_log_sync);
        }
        next.saturating_duration_since(now)
    }
}

enum BgDbResult {
    Metrics(MetricsSnapshot),
    RequestLogs {
        data_version: i64,
        /// `None` when the database was unchanged and the reload was skipped.
        logs: Option<Vec<crate::metrics::RequestLogEntry>>,
    },
}

struct BgDbJobs {
    tx: std::sync::mpsc::Sender<BgDbResult>,
    rx: std::sync::mpsc::Receiver<BgDbResult>,
    metrics_running: bool,
    request_logs_running: bool,
    /// `PRAGMA data_version` seen by the last request-log sync; the bg proxy
    /// writes from another connection, which is the only thing that moves it.
    request_log_data_version: i64,
}

impl BgDbJobs {
    fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            tx,
            rx,
            metrics_running: false,
            request_logs_running: false,
            request_log_data_version: -1,
        }
    }

    fn start_metrics_reload(&mut self, app: &App) {
        if self.metrics_running {
            return;
        }
        self.metrics_running = true;
        let tx = self.tx.clone();
        let db = app.db.clone();
        tokio::task::spawn_blocking(move || {
            let (metrics, provider_models) = db.load_all();
            let _ = tx.send(BgDbResult::Metrics(MetricsSnapshot {
                metrics,
                provider_models,
            }));
        });
    }

    fn start_request_log_sync(&mut self, app: &App) {
        if self.request_logs_running {
            return;
        }
        self.request_logs_running = true;
        let tx = self.tx.clone();
        let db = app.db.clone();
        let request_log_limit = app.config.request_log_limit;
        let last_version = self.request_log_data_version;
        tokio::task::spawn_blocking(move || {
            // Skip the full reload (bodies included) while the bg proxy is idle.
            let data_version = db.data_version();
            let logs = (data_version != last_version)
                .then(|| db.load_recent_request_logs(request_log_limit));
            let _ = tx.send(BgDbResult::RequestLogs { data_version, logs });
        });
    }

    fn drain_results(&mut self, app: &mut App) -> bool {
        let mut dirty = false;
        while let Ok(result) = self.rx.try_recv() {
            match result {
                BgDbResult::Metrics(snapshot) => {
                    self.metrics_running = false;
                    dirty |= app.apply_metrics_snapshot(snapshot);
                }
                BgDbResult::RequestLogs { data_version, logs } => {
                    self.request_logs_running = false;
                    self.request_log_data_version = data_version;
                    if let Some(logs) = logs {
                        dirty |= replace_request_logs_if_changed(app, logs);
                    }
                }
            }
        }
        dirty
    }
}

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
    let mut scheduler = RenderScheduler::new(Instant::now());
    let mut bg_db_jobs = BgDbJobs::new();
    let mut dirty = true;
    loop {
        let now = Instant::now();
        let mut input_dirty = false;
        if event::poll(scheduler.next_wake_in(
            now,
            message_deadline(app),
            app.bg_proxy_pid.is_some(),
        ))? {
            input_dirty = process_input_events(app, server)?;
            dirty |= input_dirty;
        }

        if app.should_quit {
            break;
        }

        if input_dirty {
            terminal.draw(|f| ui::draw(f, app))?;
            dirty = false;
        }

        dirty |= run_due_scheduled_tasks(
            app,
            server,
            db_change_rx.as_ref(),
            &mut scheduler,
            &mut bg_db_jobs,
        );

        if app.should_quit {
            break;
        }

        if dirty {
            terminal.draw(|f| ui::draw(f, app))?;
            dirty = false;
        }
    }
    Ok(())
}

fn process_input_events(app: &mut App, server: &mut Option<ServerHandle>) -> Result<bool> {
    let mut processed_events = 0;
    let mut dirty = false;
    loop {
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                input::handle_key(app, key.code, key.modifiers, server)?;
                dirty = true;
            }
            Event::Paste(text) => {
                input::handle_paste(app, &text)?;
                dirty = true;
            }
            Event::FocusGained => {
                app.terminal_focused = true;
                dirty = true;
            }
            Event::FocusLost => {
                app.terminal_focused = false;
                dirty = true;
            }
            Event::Resize(_, _) => {
                dirty = true;
            }
            _ => {}
        }
        processed_events += 1;

        if !should_read_next_event(processed_events, event::poll(Duration::from_millis(0))?) {
            break;
        }
    }
    Ok(dirty)
}

fn run_due_scheduled_tasks(
    app: &mut App,
    server: &mut Option<ServerHandle>,
    db_change_rx: Option<&std::sync::mpsc::Receiver<()>>,
    scheduler: &mut RenderScheduler,
    bg_db_jobs: &mut BgDbJobs,
) -> bool {
    let now = Instant::now();
    let mut dirty = bg_db_jobs.drain_results(app);
    dirty |= check_server_status(app, server);

    if now >= scheduler.next_bg_proxy_check {
        dirty |= check_bg_proxy_status(app);
        scheduler.next_bg_proxy_check = now + BG_PROXY_CHECK_INTERVAL;
    }

    let db_changed = if now >= scheduler.next_db_watcher_poll {
        scheduler.next_db_watcher_poll = now + DB_WATCHER_POLL_INTERVAL;
        db_changed(db_change_rx)
    } else {
        false
    };

    if db_changed || now >= scheduler.next_metrics_reload {
        bg_db_jobs.start_metrics_reload(app);
        if now >= scheduler.next_metrics_reload {
            scheduler.next_metrics_reload = now + METRICS_RELOAD_INTERVAL;
        }
    }

    if app.bg_proxy_pid.is_some() && now >= scheduler.next_bg_proxy_log_sync {
        bg_db_jobs.start_request_log_sync(app);
        scheduler.next_bg_proxy_log_sync = now + BG_PROXY_LOG_SYNC_INTERVAL;
    }

    if now >= scheduler.next_async_drain {
        dirty |= app.drain_test_results();
        if std::mem::take(&mut app.config_needs_sync) {
            server::sync_proxy_config(app, server);
        }
        scheduler.next_async_drain = now + ASYNC_DRAIN_INTERVAL;
    }

    if now >= scheduler.next_sysinfo {
        dirty |= app.sysinfo_sampler.sample();
        scheduler.next_sysinfo = now + SYSINFO_INTERVAL;
    }

    dirty |= app.tick_message();
    dirty
}

fn db_changed(rx: Option<&std::sync::mpsc::Receiver<()>>) -> bool {
    let mut changed = false;
    if let Some(rx) = rx {
        while rx.try_recv().is_ok() {
            changed = true;
        }
    }
    changed
}

fn message_deadline(app: &App) -> Option<Instant> {
    app.message
        .as_ref()
        .map(|(_, _, created)| *created + Duration::from_secs(state::MESSAGE_TIMEOUT_SECS))
}

#[cfg(test)]
mod tests {
    use super::{
        ASYNC_DRAIN_INTERVAL, BG_PROXY_CHECK_INTERVAL, DB_WATCHER_POLL_INTERVAL,
        MAX_EVENTS_PER_FRAME, METRICS_RELOAD_INTERVAL, RenderScheduler, SYSINFO_INTERVAL,
        should_read_next_event,
    };
    use std::time::{Duration, Instant};

    #[test]
    fn queued_events_yield_after_frame_budget() {
        assert!(should_read_next_event(0, true));
        assert!(should_read_next_event(MAX_EVENTS_PER_FRAME - 1, true));
        assert!(!should_read_next_event(MAX_EVENTS_PER_FRAME, true));
        assert!(!should_read_next_event(0, false));
    }

    #[test]
    fn inactive_bg_proxy_log_sync_does_not_force_immediate_wake() {
        let now = Instant::now();
        let mut scheduler = RenderScheduler::new(now);
        scheduler.next_async_drain = now + ASYNC_DRAIN_INTERVAL;
        scheduler.next_db_watcher_poll = now + DB_WATCHER_POLL_INTERVAL;
        scheduler.next_bg_proxy_check = now + BG_PROXY_CHECK_INTERVAL;
        scheduler.next_metrics_reload = now + METRICS_RELOAD_INTERVAL;
        scheduler.next_sysinfo = now + SYSINFO_INTERVAL;

        assert_eq!(
            scheduler.next_wake_in(now + Duration::from_millis(1), None, false),
            ASYNC_DRAIN_INTERVAL - Duration::from_millis(1)
        );
    }
}
