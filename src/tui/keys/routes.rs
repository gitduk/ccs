use crossterm::event::KeyCode;

use crate::config::RouteRule;
use crate::tui::state::{filter_suggestions, FormField, ProviderForm};

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

/// Return a mutable reference to whichever route field is currently active.
fn route_field(form: &mut ProviderForm) -> &mut FormField {
    if form.route_edit_target {
        &mut form.route_tgt_field
    } else {
        &mut form.route_pat_field
    }
}

/// Enter route Insert mode on either the pattern or target field.
fn enter_route_insert_mode(form: &mut ProviderForm, edit_target: bool) {
    // Auto-add a blank rule when routes are empty.
    if form.routes.is_empty() {
        form.routes.push(crate::config::RouteRule::new(""));
        form.route_cursor = 0;
    }
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
pub(super) fn handle_routes_key(
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
                route_field(form).insert(c);
                if form.route_edit_target {
                    reset_suggest(form);
                }
                sync_route_fields(form);
            }
            KeyCode::Backspace => {
                route_field(form).backspace();
                if form.route_edit_target {
                    reset_suggest(form);
                }
                sync_route_fields(form);
            }
            KeyCode::Char('h') if ctrl => {
                route_field(form).backspace();
                if form.route_edit_target {
                    reset_suggest(form);
                }
                sync_route_fields(form);
            }
            KeyCode::Delete => {
                route_field(form).delete();
                sync_route_fields(form);
            }
            KeyCode::Left => route_field(form).move_left(),
            KeyCode::Right => route_field(form).move_right(),
            KeyCode::Home | KeyCode::Char('a') if ctrl => route_field(form).home(),
            KeyCode::End | KeyCode::Char('e') if ctrl => route_field(form).end(),
            KeyCode::Char('w') if ctrl => {
                route_field(form).delete_word_back();
                if form.route_edit_target {
                    reset_suggest(form);
                }
                sync_route_fields(form);
            }
            _ => {}
        }
    } else {
        // ── Normal mode ─────────────────────────────────────────────────────
        match code {
            // a / o → add rule (append, enter Insert mode on pattern immediately).
            KeyCode::Char('a') | KeyCode::Char('o') if !ctrl => {
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
