use crossterm::event::KeyCode;

use crate::config::{ApiFormat, Provider};
use crate::tui::server::sync_proxy_config;
use crate::tui::state::{ConfirmAction, MessageKind, Mode};
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
    let prev = super::insert::consume_pending_key(&mut app.pending_key);

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
            ('y', KeyCode::Char('y')) => {
                if let Some(provider) = app
                    .selected_name()
                    .and_then(|name| app.config.providers.get(name))
                {
                    let url = provider.base_url.clone();
                    if super::copy_to_clipboard(&url) {
                        app.set_message("Copied base URL", MessageKind::Success);
                    } else {
                        app.set_message("Copy failed (wl-copy not found?)", MessageKind::Error);
                    }
                }
                return Ok(());
            }
            ('y', KeyCode::Char('c')) => {
                if let Some(name) = app.selected_name().map(|s| s.to_string())
                    && let Some(provider) = app.config.providers.get(&name)
                {
                    // Mirror the model-selection logic in testing::test_provider_by_name:
                    // 1. Most-used model (by token volume) from the known list.
                    // 2. First model from the known list if no usage data.
                    // 3. Empty string if no models known yet (provider not yet tested).
                    let supported = app.provider_models.get(&name);
                    let model = supported
                        .filter(|s| !s.is_empty())
                        .and_then(|models| {
                            app.metrics
                                .lock()
                                .ok()
                                .and_then(|m| {
                                    models
                                        .iter()
                                        .max_by_key(|mdl| {
                                            m.by_model.get(*mdl).map_or(0, |s| s.input + s.output)
                                        })
                                        .filter(|mdl| m.by_model.contains_key(*mdl))
                                        .map(|s| s.to_string())
                                })
                                .or_else(|| models.first().map(|s| s.to_string()))
                        })
                        .unwrap_or_default();

                    match build_test_curl(provider, &model) {
                        Ok(cmd) => {
                            if super::copy_to_clipboard(&cmd) {
                                app.set_message("Copied curl command", MessageKind::Success);
                            } else {
                                app.set_message(
                                    "Copy failed (wl-copy not found?)",
                                    MessageKind::Error,
                                );
                            }
                        }
                        Err(e) => {
                            app.set_message(format!("Cannot build curl: {e}"), MessageKind::Error);
                        }
                    }
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
        // First key of gg / dd / yy — store in buffer.
        KeyCode::Char('g') => {
            app.pending_key = Some(('g', std::time::Instant::now()));
        }
        KeyCode::Char('d') => {
            app.pending_key = Some(('d', std::time::Instant::now()));
        }
        KeyCode::Char('y') => {
            app.pending_key = Some(('y', std::time::Instant::now()));
        }

        // ── Provider actions ──────────────────────────────────────────────────
        KeyCode::Char('s') => {
            app.switch_to_selected()?;
            sync_proxy_config(app, server);
        }
        // a / o → add (Vim: 'o' opens new line below, 'a' appends)
        KeyCode::Char('a' | 'o') => app.add(),
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
        KeyCode::Char('c') => app.confirm(ConfirmAction::ClearCurrent),
        KeyCode::Char('C') => app.confirm(ConfirmAction::Clear),
        KeyCode::Char('h' | '?') => {
            app.mode = Mode::Help;
        }
        KeyCode::Char('m') => {
            app.mode = Mode::Models;
            app.models_insert = true;
            app.models_selected = 0;
            app.models_scroll = 0;
        }
        _ => {}
    }
    Ok(())
}

/// Build a `curl` command that replicates the test request sent by the tester.
/// Returns an error string if the API key cannot be resolved.
fn build_test_curl(provider: &Provider, model: &str) -> Result<String, String> {
    let api_key = provider.resolve_api_key().map_err(|e| e.to_string())?;

    let base = provider.base_url.trim_end_matches('/');
    let (url, body) = match provider.api_format {
        ApiFormat::Anthropic => (
            format!("{base}/v1/messages"),
            format!(
                r#"{{"model":"{model}","max_tokens":1,"messages":[{{"role":"user","content":"ping"}}]}}"#
            ),
        ),
        ApiFormat::OpenAI => (
            format!("{base}/v1/chat/completions"),
            format!(
                r#"{{"model":"{model}","max_tokens":1,"messages":[{{"role":"user","content":"ping"}}]}}"#
            ),
        ),
    };

    let mut cmd = format!("curl -s -X POST '{url}' \\\n  -H 'Content-Type: application/json' \\\n");

    match provider.api_format {
        ApiFormat::Anthropic => {
            cmd.push_str(&format!(
                "  -H 'x-api-key: {api_key}' \\\n  -H 'anthropic-version: 2023-06-01' \\\n"
            ));
        }
        ApiFormat::OpenAI => {
            cmd.push_str(&format!("  -H 'Authorization: Bearer {api_key}' \\\n"));
        }
    }

    cmd.push_str(&format!("  -d '{body}'"));
    Ok(cmd)
}
