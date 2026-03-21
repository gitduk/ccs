use crossterm::event::KeyCode;

use crate::tui::app::{ConfirmAction, Mode};
use crate::tui::server::sync_proxy_config;
use crate::tui::{App, ServerHandle};

pub(super) fn handle_normal_key(
    app: &mut App,
    code: KeyCode,
    server: &mut Option<ServerHandle>,
) -> crate::error::Result<()> {
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
        KeyCode::Char('p') => {
            app.toggle_provider_enabled()?;
            sync_proxy_config(app, server);
        }
        KeyCode::Char('t') => super::super::testing::test_selected(app),
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
            super::super::server::toggle_bg_proxy(app, server);
        }
        KeyCode::Char('c') => app.confirm(ConfirmAction::Clear),
        KeyCode::Char('h') | KeyCode::Char('?') => {
            app.mode = Mode::Help;
        }
        _ => {}
    }
    Ok(())
}
