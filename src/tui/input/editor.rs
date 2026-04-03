use crossterm::event::{KeyCode, KeyModifiers};

use crate::tui::server::sync_proxy_config;
use crate::tui::state::{MessageKind, Mode, VimMode};
use crate::tui::{App, ServerHandle};

use super::insert::{InsertKeyResult, consume_pending_key, handle_field_insert_key};

use super::routes::handle_routes_key;

#[inline]
fn save_and_sync(app: &mut App, server: &Option<ServerHandle>) -> crate::error::Result<()> {
    app.save_form_in_place()?;
    sync_proxy_config(app, server);
    Ok(())
}

pub(super) fn handle_editing_key(
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
    server: &Option<ServerHandle>,
) -> crate::error::Result<()> {
    let Some(form) = app.form.as_mut() else {
        app.mode = Mode::Normal;
        return Ok(());
    };

    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    let in_routes = form.in_routes();

    // ── Consume pending key (500 ms timeout) ─────────────────────────────────
    // Note: in Insert mode, pending_key is managed by handle_field_insert_key
    // for the "jk" escape sequence. Only consume it here for Normal mode sequences.
    // Also exclude route_editing: its Insert-mode field handler manages pending too.
    let prev = if form.vim_mode == VimMode::Normal && !form.route_editing {
        consume_pending_key(&mut form.pending_key)
    } else {
        None
    };

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
            save_and_sync(app, server)?;
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
            // Normal mode Esc → close the form.
            close_editor(app);
        }
        return Ok(());
    }

    // ── q — cancel (Normal mode only, not while editing a route pattern) ──────
    if form.vim_mode == VimMode::Normal && !form.route_editing && matches!(code, KeyCode::Char('q'))
    {
        close_editor(app);
        return Ok(());
    }

    // ── Delegate to routes section handler ────────────────────────────────────
    if in_routes {
        let prov_name = form
            .original_name
            .as_deref()
            .unwrap_or_else(|| form.fields[0].value.trim());
        let provider_models: Vec<String> = app
            .models
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
            save_and_sync(app, server)?;
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
            KeyCode::Char('a' | 'A') => {
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
                let focused = form.focused;
                if form.fields[focused].is_toggle {
                    form.fields[focused].toggle_value();
                    save_and_sync(app, server)?;
                    return Ok(());
                }
                form.fields[focused].move_left();
            }
            KeyCode::Char('l') | KeyCode::Right => {
                let focused = form.focused;
                if form.fields[focused].is_toggle {
                    form.fields[focused].toggle_value();
                    save_and_sync(app, server)?;
                    return Ok(());
                }
                form.fields[focused].move_right();
            }
            // Space: toggle toggle-type fields.
            KeyCode::Char(' ') => {
                let focused = form.focused;
                if form.fields[focused].is_toggle {
                    form.fields[focused].toggle_value();
                    save_and_sync(app, server)?;
                    return Ok(());
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
                    let focused = form.focused;
                    form.fields[focused].delete_current_line();
                    // Borrow on `form` ends here; safe to call app methods below.
                    save_and_sync(app, server)?;
                    return Ok(());
                }
                form.pending_key = Some(('d', std::time::Instant::now()));
            }
            // yy → copy current field value to clipboard.
            KeyCode::Char('y') => {
                if prev == Some('y') {
                    let value = form.fields[form.focused].value.clone();
                    // Borrow on `form` ends here (value is cloned); safe to call app methods.
                    if super::copy_to_clipboard(&value) {
                        app.set_message("Copied to clipboard", MessageKind::Success);
                    } else {
                        app.set_message("Copy failed (wl-copy not found?)", MessageKind::Error);
                    }
                    return Ok(());
                }
                form.pending_key = Some(('y', std::time::Instant::now()));
            }
            _ => {}
        }
        // If we just entered Insert mode, clear any stale pending key so that
        // Normal-mode sequences (dd / yy / gg) don't leak into Insert mode.
        if form.vim_mode == VimMode::Insert {
            form.pending_key = None;
        }
        return Ok(());
    }

    // ── Regular field — Insert mode ───────────────────────────────────────────

    // Editing-specific keys intercepted before the common handler.
    match code {
        KeyCode::Enter => {
            form.vim_mode = VimMode::Normal;
            save_and_sync(app, server)?;
            return Ok(());
        }
        KeyCode::Tab => {
            form.focus_next();
            return Ok(());
        }
        KeyCode::BackTab => {
            form.focus_prev();
            return Ok(());
        }
        // Ctrl+J: insert newline in multiline fields, or move to next field.
        KeyCode::Char('j') if ctrl => {
            if form.fields[form.focused].is_multiline {
                form.fields[form.focused].insert_newline();
            } else {
                form.focus_next();
            }
            return Ok(());
        }
        // Ctrl+K: move up/prev field.
        KeyCode::Char('k') if ctrl => {
            if form.fields[form.focused].is_multiline {
                if !form.fields[form.focused].move_up() {
                    form.focus_prev();
                }
            } else {
                form.focus_prev();
            }
            return Ok(());
        }
        // Up/Down: field navigation (single-line) or cursor movement (multiline).
        KeyCode::Down => {
            if form.fields[form.focused].is_multiline {
                if !form.fields[form.focused].move_down() {
                    form.focus_next();
                }
            } else {
                form.focus_next();
            }
            return Ok(());
        }
        KeyCode::Up => {
            if form.fields[form.focused].is_multiline {
                if !form.fields[form.focused].move_up() {
                    form.focus_prev();
                }
            } else {
                form.focus_prev();
            }
            return Ok(());
        }
        _ => {}
    }

    // Toggle fields have their own key handling; common editor does not apply.
    if form.fields[form.focused].is_toggle {
        match code {
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') => {
                form.fields[form.focused].toggle_value();
                save_and_sync(app, server)?;
            }
            KeyCode::Char('h' | 'l') if ctrl => {
                form.fields[form.focused].toggle_value();
                save_and_sync(app, server)?;
            }
            _ => {}
        }
        return Ok(());
    }

    // Common Insert-mode editing (Backspace/Ctrl+W/Home/End/jk/Esc/…).
    match handle_field_insert_key(
        &mut form.fields[form.focused],
        code,
        ctrl,
        &mut form.pending_key,
    ) {
        InsertKeyResult::ExitInsert => {
            form.vim_mode = VimMode::Normal;
            save_and_sync(app, server)?;
        }
        InsertKeyResult::TextChanged => {
            save_and_sync(app, server)?;
        }
        InsertKeyResult::Consumed | InsertKeyResult::NotHandled => {}
    }
    Ok(())
}

/// Close the editor and return to Normal mode.
fn close_editor(app: &mut App) {
    app.form = None;
    app.mode = Mode::Normal;
}
