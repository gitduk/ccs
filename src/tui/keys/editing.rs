use crossterm::event::{KeyCode, KeyModifiers};

use crate::tui::app::{Mode, VimMode};
use crate::tui::server::sync_proxy_config;
use crate::tui::{App, ServerHandle};

use super::routes::handle_routes_key;

pub(super) fn handle_editing_key(
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
    server: &Option<ServerHandle>,
) -> crate::error::Result<()> {
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

    // ── dd in route Normal nav mode ───────────────────────────────────────────
    if in_routes
        && !form.route_editing
        && form.vim_mode == VimMode::Normal
        && prev == Some('d')
        && matches!(code, KeyCode::Char('d'))
    {
        if !form.routes.is_empty() && form.route_cursor < form.routes.len() {
            form.routes.remove(form.route_cursor);
            form.clamp_route_cursor();
            app.save_form_in_place()?;
            sync_proxy_config(app, server);
        }
        return Ok(());
    }

    // ── Esc ───────────────────────────────────────────────────────────────────
    if matches!(code, KeyCode::Esc) {
        if in_routes && form.route_editing {
            if form.route_suggest_active {
                form.route_suggest_active = false;
            } else {
                form.reset_route_editing();
            }
        } else if form.vim_mode == VimMode::Insert {
            form.vim_mode = VimMode::Normal;
        } else {
            // Normal mode Esc → cancel the form.
            app.form = None;
            app.mode = Mode::Normal;
        }
        return Ok(());
    }

    // ── q — cancel (Normal mode only, not while editing a route pattern) ──────
    if form.vim_mode == VimMode::Normal && !form.route_editing && matches!(code, KeyCode::Char('q'))
    {
        app.form = None;
        app.mode = Mode::Normal;
        return Ok(());
    }

    // ── Delegate to routes section handler ────────────────────────────────────
    if in_routes {
        let prov_name = form
            .original_name
            .as_deref()
            .unwrap_or_else(|| form.fields[0].value.trim());
        let provider_models: Vec<String> = app
            .provider_models
            .get(prov_name)
            .cloned()
            .unwrap_or_default();
        handle_routes_key(form, code, ctrl, &provider_models);
        // If focus just left the routes section, prune invalid routes immediately
        // so the user gets instant visual feedback (don't wait for save).
        if !form.in_routes() {
            form.routes.retain(|r| r.is_valid(&provider_models));
            form.clamp_route_cursor();
        }
        // Only save when not actively editing a route — do_save_form retains only
        // valid routes, which would immediately prune any rule being typed.
        if !form.route_editing {
            app.save_form_in_place()?;
            sync_proxy_config(app, server);
        }
        return Ok(());
    }

    // ── Regular field — Normal mode ───────────────────────────────────────────
    if form.vim_mode == VimMode::Normal {
        match code {
            // Enter Insert mode.
            KeyCode::Char('i') | KeyCode::Insert => {
                form.vim_mode = VimMode::Insert;
            }
            // 'a' / 'A' → Insert at end of current value.
            KeyCode::Char('a') | KeyCode::Char('A') => {
                form.vim_mode = VimMode::Insert;
                form.fields[form.focused].end();
            }
            // Navigation: j/k move within multiline fields, otherwise switch field.
            KeyCode::Char('j') | KeyCode::Down => {
                if form.fields[form.focused].is_multiline {
                    if !form.fields[form.focused].move_down() {
                        form.focus_next();
                    }
                } else {
                    form.focus_next();
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if form.fields[form.focused].is_multiline {
                    if !form.fields[form.focused].move_up() {
                        form.focus_prev();
                    }
                } else {
                    form.focus_prev();
                }
            }
            KeyCode::Tab => form.focus_next(),
            KeyCode::BackTab => form.focus_prev(),
            // h / l: move cursor in text fields, or toggle toggle-fields.
            KeyCode::Char('h') | KeyCode::Left => {
                let f = &mut form.fields[form.focused];
                if f.is_toggle {
                    f.toggle_value();
                    app.save_form_in_place()?;
                    sync_proxy_config(app, server);
                } else {
                    f.move_left();
                }
            }
            KeyCode::Char('l') | KeyCode::Right => {
                let f = &mut form.fields[form.focused];
                if f.is_toggle {
                    f.toggle_value();
                    app.save_form_in_place()?;
                    sync_proxy_config(app, server);
                } else {
                    f.move_right();
                }
            }
            // Space: toggle toggle-type fields.
            KeyCode::Char(' ') => {
                if form.fields[form.focused].is_toggle {
                    form.fields[form.focused].toggle_value();
                    app.save_form_in_place()?;
                    sync_proxy_config(app, server);
                }
            }
            // Enter on a text field → Insert mode (toggle fields don't have a text cursor).
            KeyCode::Enter if !form.fields[form.focused].is_toggle => {
                form.vim_mode = VimMode::Insert;
            }
            // Cursor jumps.
            KeyCode::Home | KeyCode::Char('0') => form.fields[form.focused].home(),
            KeyCode::End | KeyCode::Char('$') => form.fields[form.focused].end(),
            // o → open new line below cursor (multiline fields only).
            KeyCode::Char('o') if form.fields[form.focused].is_multiline => {
                let f = &mut form.fields[form.focused];
                if f.value.is_empty() {
                    // Empty field: just enter Insert mode, no newline needed.
                    form.vim_mode = VimMode::Insert;
                } else {
                    // Move to end of current line, insert newline, enter Insert mode.
                    let rest = f.value[f.cursor..].find('\n');
                    f.cursor = rest.map_or(f.value.len(), |r| f.cursor + r);
                    f.insert_newline();
                    form.vim_mode = VimMode::Insert;
                }
            }
            // dd → delete current line in multiline fields.
            KeyCode::Char('d') if form.fields[form.focused].is_multiline => {
                if prev == Some('d') {
                    form.fields[form.focused].delete_current_line();
                    app.save_form_in_place()?;
                    sync_proxy_config(app, server);
                } else {
                    form.pending_key = Some(('d', std::time::Instant::now()));
                }
            }
            _ => {}
        }
        return Ok(());
    }

    // ── Regular field — Insert mode ───────────────────────────────────────────
    match code {
        KeyCode::Enter => {
            form.vim_mode = VimMode::Normal;
            app.save_form_in_place()?;
            sync_proxy_config(app, server);
        }
        // Ctrl+J inserts a newline in multiline fields.
        KeyCode::Char('j') if ctrl && form.fields[form.focused].is_multiline => {
            form.fields[form.focused].insert_newline();
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
                    KeyCode::Char(c) if !ctrl => {
                        field.insert(c);
                        app.save_form_in_place()?;
                        sync_proxy_config(app, server);
                    }
                    KeyCode::Char('w') if ctrl => {
                        field.delete_word_back();
                        app.save_form_in_place()?;
                        sync_proxy_config(app, server);
                    }
                    KeyCode::Char('h') if ctrl => {
                        field.backspace();
                        app.save_form_in_place()?;
                        sync_proxy_config(app, server);
                    }
                    KeyCode::Backspace => {
                        field.backspace();
                        app.save_form_in_place()?;
                        sync_proxy_config(app, server);
                    }
                    KeyCode::Delete => {
                        field.delete();
                        app.save_form_in_place()?;
                        sync_proxy_config(app, server);
                    }
                    KeyCode::Left => field.move_left(),
                    KeyCode::Right => field.move_right(),
                    KeyCode::Home => field.home(),
                    KeyCode::End => field.end(),
                    KeyCode::Char('a') if ctrl => field.home(),
                    KeyCode::Char('e') if ctrl => field.end(),
                    _ => {}
                }
            }
        }
    }
    Ok(())
}
