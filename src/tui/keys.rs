use crossterm::event::{KeyCode, KeyModifiers};

use crate::config::RouteRule;

use super::app::{filter_suggestions, ConfirmAction, FormField, Mode, ProviderForm, VimMode};
use super::server::sync_proxy_config;
use super::App;
use super::ServerHandle;

pub(super) fn handle_key(
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
    server: &mut Option<ServerHandle>,
) -> crate::error::Result<()> {
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
        KeyCode::Char('t') => super::testing::test_selected(app),
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
            super::server::toggle_bg_proxy(app, server);
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

/// Remove the rule at `route_cursor` if it is invalid.
fn prune_current_rule(form: &mut ProviderForm, provider_models: &[String]) {
    let valid = form
        .routes
        .get(form.route_cursor)
        .map(|r| r.is_valid(provider_models))
        .unwrap_or(true);
    if !valid {
        form.routes.remove(form.route_cursor);
        form.clamp_route_cursor();
    }
}

/// Navigate the suggestion list downward (↓ / Ctrl+J).
fn suggest_nav_down(form: &mut ProviderForm, provider_models: &[String]) {
    let filter = form
        .routes
        .get(form.route_cursor)
        .map(|r| r.target.as_str())
        .unwrap_or("");
    let suggestions = filter_suggestions(provider_models, filter);
    if !suggestions.is_empty() {
        if !form.route_suggest_active {
            form.route_suggest_active = true;
            form.route_suggest_idx = 0;
        } else {
            form.route_suggest_idx =
                (form.route_suggest_idx + 1).min(suggestions.len().saturating_sub(1));
        }
    }
}

/// Navigate the suggestion list upward (↑ / Ctrl+K).
fn suggest_nav_up(form: &mut ProviderForm) {
    if form.route_suggest_active {
        if form.route_suggest_idx == 0 {
            form.route_suggest_active = false;
        } else {
            form.route_suggest_idx -= 1;
        }
    }
}

/// Exit route Insert mode, prune invalid rule, and reset editing state.
fn exit_route_insert(form: &mut ProviderForm, provider_models: &[String]) {
    form.reset_route_editing();
    prune_current_rule(form, provider_models);
}

/// Dismiss the suggestion list.
fn reset_suggest(form: &mut ProviderForm) {
    form.route_suggest_active = false;
    form.route_suggest_idx = 0;
}

/// Enter route Insert mode on either the pattern or target field.
fn enter_route_insert_mode(form: &mut ProviderForm, edit_target: bool) {
    if form.route_cursor < form.routes.len() {
        let rule = &form.routes[form.route_cursor];
        form.route_pat_field = FormField::text("", &rule.pattern);
        form.route_tgt_field = FormField::text("", &rule.target);
        form.route_editing = true;
        form.route_edit_target = edit_target;
    }
}

/// Sync FormField values back to the active RouteRule.
fn sync_route_fields(form: &mut ProviderForm) {
    if let Some(rule) = form.routes.get_mut(form.route_cursor) {
        rule.pattern = form.route_pat_field.value.clone();
        rule.target = form.route_tgt_field.value.clone();
    }
}

/// Switch focus to the pattern field, dismissing any active suggestion list.
fn focus_pattern(form: &mut ProviderForm) {
    form.route_edit_target = false;
    reset_suggest(form);
    if let Some(rule) = form.routes.get(form.route_cursor) {
        form.route_pat_field = FormField::text("", &rule.pattern);
    }
}

/// Handle a key press when the Routes section of the provider form has focus.
/// Operates in either "route Insert mode" (editing a pattern) or "route Normal
/// mode" (navigating / managing rules).
fn handle_routes_key(
    form: &mut ProviderForm,
    code: KeyCode,
    ctrl: bool,
    provider_models: &[String],
) {
    if form.route_editing {
        // ── Insert mode ────────────────────────────────────────────────────
        match code {
            // Confirm / exit Insert mode.
            KeyCode::Esc => {
                if form.route_suggest_active {
                    form.route_suggest_active = false;
                } else {
                    exit_route_insert(form, provider_models);
                }
            }
            KeyCode::Enter => {
                if form.route_suggest_active {
                    // Select highlighted suggestion.
                    let filter = form
                        .routes
                        .get(form.route_cursor)
                        .map(|r| r.target.as_str())
                        .unwrap_or("");
                    let suggestions = filter_suggestions(provider_models, filter);
                    if let Some(&model) = suggestions.get(form.route_suggest_idx) {
                        form.route_tgt_field = FormField::text("", model);
                        sync_route_fields(form);
                    }
                }
                exit_route_insert(form, provider_models);
            }
            // Tab: cycle pattern → target → pattern.
            KeyCode::Tab => {
                if !form.route_edit_target {
                    form.route_edit_target = true;
                    if let Some(rule) = form.routes.get(form.route_cursor) {
                        form.route_tgt_field = FormField::text("", &rule.target);
                    }
                } else {
                    focus_pattern(form);
                }
            }
            // BackTab: switch target → pattern; if on pattern → exit Insert.
            KeyCode::BackTab => {
                if form.route_edit_target {
                    focus_pattern(form);
                } else {
                    exit_route_insert(form, provider_models);
                    form.focus_prev();
                }
            }
            // ↓ / Ctrl+J: navigate suggestion list down (only when editing target).
            KeyCode::Down if form.route_edit_target => {
                suggest_nav_down(form, provider_models);
            }
            KeyCode::Char('j') if ctrl && form.route_edit_target => {
                suggest_nav_down(form, provider_models);
            }
            // ↑ / Ctrl+K: navigate suggestion list up (only when editing target).
            KeyCode::Up if form.route_edit_target => {
                suggest_nav_up(form);
            }
            KeyCode::Char('k') if ctrl && form.route_edit_target => {
                suggest_nav_up(form);
            }
            // Character input (no spaces: model names never contain spaces).
            KeyCode::Char(c) if !ctrl && c != ' ' => {
                if form.route_edit_target {
                    form.route_tgt_field.insert(c);
                    reset_suggest(form);
                } else {
                    form.route_pat_field.insert(c);
                }
                sync_route_fields(form);
            }
            KeyCode::Backspace | KeyCode::Char('h') if ctrl => {
                if form.route_edit_target {
                    form.route_tgt_field.backspace();
                    reset_suggest(form);
                } else {
                    form.route_pat_field.backspace();
                }
                sync_route_fields(form);
            }
            KeyCode::Delete => {
                if form.route_edit_target {
                    form.route_tgt_field.delete();
                } else {
                    form.route_pat_field.delete();
                }
                sync_route_fields(form);
            }
            KeyCode::Left => {
                if form.route_edit_target {
                    form.route_tgt_field.move_left();
                } else {
                    form.route_pat_field.move_left();
                }
            }
            KeyCode::Right => {
                if form.route_edit_target {
                    form.route_tgt_field.move_right();
                } else {
                    form.route_pat_field.move_right();
                }
            }
            KeyCode::Home | KeyCode::Char('a') if ctrl => {
                if form.route_edit_target {
                    form.route_tgt_field.home();
                } else {
                    form.route_pat_field.home();
                }
            }
            KeyCode::End | KeyCode::Char('e') if ctrl => {
                if form.route_edit_target {
                    form.route_tgt_field.end();
                } else {
                    form.route_pat_field.end();
                }
            }
            KeyCode::Char('w') if ctrl => {
                if form.route_edit_target {
                    form.route_tgt_field.delete_word_back();
                    reset_suggest(form);
                } else {
                    form.route_pat_field.delete_word_back();
                }
                sync_route_fields(form);
            }
            _ => {}
        }
    } else {
        // ── Normal mode ─────────────────────────────────────────────────────
        match code {
            // a → add rule (append, enter Insert mode on pattern immediately).
            KeyCode::Char('a') if !ctrl => {
                form.routes.push(RouteRule::new(""));
                form.route_cursor = form.routes.len() - 1;
                form.route_pat_field = FormField::text("", "");
                form.route_tgt_field = FormField::text("", "");
                form.route_edit_target = false;
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

            // i / Enter → enter Insert mode for pattern.
            KeyCode::Enter | KeyCode::Char('i') if !ctrl => {
                enter_route_insert_mode(form, false);
            }

            // t → enter Insert mode for target.
            KeyCode::Char('t') if !ctrl => {
                enter_route_insert_mode(form, true);
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
            // j / Down → move cursor down, or leave section when at the last rule.
            KeyCode::Char('j') | KeyCode::Down => {
                if !form.routes.is_empty() && form.route_cursor + 1 < form.routes.len() {
                    form.route_cursor += 1;
                } else {
                    form.focus_next();
                }
            }

            // Tab / BackTab → leave routes section.
            KeyCode::Tab => form.focus_next(),
            KeyCode::BackTab => form.focus_prev(),

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
            // Enter on a regular field: enter Insert for multiline, no-op otherwise.
            KeyCode::Enter => {
                if form.fields[form.focused].is_multiline {
                    form.vim_mode = VimMode::Insert;
                }
            }
            // Cursor jumps.
            KeyCode::Home | KeyCode::Char('0') => form.fields[form.focused].home(),
            KeyCode::End | KeyCode::Char('$') => form.fields[form.focused].end(),
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

// ─── Confirm mode ─────────────────────────────────────────────────────────────

fn handle_confirm_key(
    app: &mut App,
    code: KeyCode,
    server: &Option<ServerHandle>,
) -> crate::error::Result<()> {
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
