//! Provider editor and route editor.
//!
//! This module keeps provider form rendering and editing behavior together,
//! including route-rule editing and model suggestions.

use crate::config::RouteRule;
use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph};

use super::App;
use super::ServerHandle;
use super::server::sync_proxy_config;
use super::state::{
    FORMAT_FIELD_IDX, FormField, MessageKind, Mode, NAME_FIELD_IDX, ProviderForm, VimMode,
    filter_suggestions, provider_model_suggestions,
};
use super::theme::{self as t};
use super::ui::format::{SuggestionStyle, mask_api_key_str, render_suggestion_lines};
use super::ui::layout::centered_fixed;

pub(super) fn handle_paste(app: &mut App, text: &str) -> crate::error::Result<()> {
    let Some(form) = app.form.as_mut() else {
        return Ok(());
    };
    if form.detect_token.is_some() {
        return Ok(());
    }

    if form.vim_mode != VimMode::Insert {
        form.vim_mode = VimMode::Insert;
    }
    if form.in_routes() && form.route_editing {
        let field = if form.route_edit_target {
            &mut form.route_tgt_field
        } else {
            &mut form.route_pat_field
        };
        field.insert_str(text);
        if form.route_edit_target {
            form.route_suggest.reset();
        }
    } else if let Some(field) = form.fields.get_mut(form.focused) {
        field.insert_str(text);
    }
    Ok(())
}

pub(super) fn handle_key(
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
    server: &Option<ServerHandle>,
) -> crate::error::Result<()> {
    let Some(form) = app.form.as_mut() else {
        app.mode = Mode::Normal;
        return Ok(());
    };

    if form.detect_token.is_some() {
        // Freeze the form during detection; Esc cancels directly (close() would re-block).
        if matches!(code, KeyCode::Esc) {
            app.form = None;
            app.mode = Mode::Normal;
        }
        return Ok(());
    }

    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    let in_routes = form.in_routes();

    let prev = if form.vim_mode == VimMode::Normal && !form.route_editing {
        super::input::insert::consume_pending_key(&mut form.pending_key)
    } else {
        None
    };
    // Detection failed for this add: a/o pick a format and save without
    // re-detecting. Skipped in the Routes section, where `a` adds a route.
    if form.detect_failed && !in_routes && form.vim_mode == VimMode::Normal {
        let format = match code {
            KeyCode::Char('a' | 'A') => Some(crate::config::ApiFormat::Anthropic),
            KeyCode::Char('o' | 'O') => Some(crate::config::ApiFormat::OpenAI),
            _ => None,
        };
        if let Some(format) = format {
            return app.save_form_manual_format(format);
        }
    }

    // ── Format field dropdown ─────────────────────────────────────────────────
    // When the Format field is focused, show its three choices and let the
    // user navigate (`↓` / `Ctrl-J`) and select (`Enter`).
    if !in_routes && form.focused == FORMAT_FIELD_IDX && form.fields[FORMAT_FIELD_IDX].editable {
        let choices = format_suggest_choices(form);
        let len = choices.len();
        match code {
            // On the Format field, ↓/↑ always navigate the choice list (it is
            // a select field); j/k in Normal mode still move between fields.
            KeyCode::Down => {
                if !nav_suggest_down(&mut form.format_suggest, len) {
                    form.focus_next();
                }
                return Ok(());
            }
            KeyCode::Up => {
                if !form.format_suggest.active {
                    form.focus_prev();
                } else {
                    nav_suggest_up(&mut form.format_suggest);
                }
                return Ok(());
            }
            KeyCode::Char('j') if ctrl => {
                if !nav_suggest_down(&mut form.format_suggest, len) {
                    form.focus_next();
                }
                return Ok(());
            }
            KeyCode::Char('k') if ctrl => {
                if !form.format_suggest.active {
                    form.focus_prev();
                } else {
                    nav_suggest_up(&mut form.format_suggest);
                }
                return Ok(());
            }
            // Select only after the user navigated the list (↓/↑); a plain
            // Enter without navigation falls through to normal Insert mode.
            KeyCode::Enter if form.format_suggest.active && len > 0 => {
                let idx = form.format_suggest.idx.min(len - 1);
                let choice = choices[idx];
                form.fields[FORMAT_FIELD_IDX].value = choice.to_string();
                form.fields[FORMAT_FIELD_IDX].cursor = choice.len();
                form.format_suggest.reset();
                form.vim_mode = VimMode::Normal;
                return Ok(());
            }
            KeyCode::Esc if form.format_suggest.active => {
                form.format_suggest.reset();
                return Ok(());
            }
            _ => {}
        }
    }

    if in_routes
        && !form.route_editing
        && form.vim_mode == VimMode::Normal
        && prev == Some('d')
        && matches!(code, KeyCode::Char('d'))
    {
        if !form.routes.is_empty() && form.route_cursor < form.routes.len() {
            form.routes.remove(form.route_cursor);
            form.clamp_route_cursor();
        }
        return Ok(());
    }

    if matches!(code, KeyCode::Esc) {
        if in_routes && form.route_editing {
            if form.route_suggest.active {
                form.route_suggest.active = false;
            } else {
                form.reset_route_editing();
            }
        } else if form.vim_mode == VimMode::Insert {
            form.vim_mode = VimMode::Normal;
        } else {
            close(app, server);
        }
        return Ok(());
    }

    if form.vim_mode == VimMode::Normal && !form.route_editing && matches!(code, KeyCode::Char('q'))
    {
        close(app, server);
        return Ok(());
    }

    if in_routes {
        let provider_models: Vec<String> = app
            .models
            .provider_models
            .get(form.prov_key())
            .cloned()
            .unwrap_or_default();
        handle_routes_key(form, code, ctrl, &provider_models);
        if !form.in_routes() {
            form.routes.retain(|r| r.is_valid(&provider_models));
            form.clamp_route_cursor();
        }
        return Ok(());
    }

    if form.vim_mode == VimMode::Normal {
        match code {
            KeyCode::Char('i') | KeyCode::Insert => {
                form.vim_mode = VimMode::Insert;
            }
            KeyCode::Char('a' | 'A') => {
                form.vim_mode = VimMode::Insert;
                form.fields[form.focused].end();
            }
            KeyCode::Char('j') | KeyCode::Down => {
                form.focus_next();
            }
            KeyCode::Char('k') | KeyCode::Up => {
                form.focus_prev();
            }
            KeyCode::Tab => form.focus_next(),
            KeyCode::BackTab => form.focus_prev(),
            KeyCode::Char('h') | KeyCode::Left => {
                form.fields[form.focused].move_left();
            }
            KeyCode::Char('l') | KeyCode::Right => {
                form.fields[form.focused].move_right();
            }
            KeyCode::Enter => {
                form.vim_mode = VimMode::Insert;
            }
            KeyCode::Home | KeyCode::Char('0') => form.fields[form.focused].home(),
            KeyCode::End | KeyCode::Char('$') => form.fields[form.focused].end(),
            KeyCode::Char('y') => {
                if prev == Some('y') {
                    let value = form.fields[form.focused].value.clone();
                    if super::input::copy_to_clipboard(&value) {
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
        if form.vim_mode == VimMode::Insert {
            form.pending_key = None;
        }
        return Ok(());
    }

    match code {
        KeyCode::Enter => {
            form.vim_mode = VimMode::Normal;
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
        KeyCode::Char('j') if ctrl => {
            form.focus_next();
            return Ok(());
        }
        KeyCode::Char('k') if ctrl => {
            form.focus_prev();
            return Ok(());
        }
        KeyCode::Down => {
            form.focus_next();
            return Ok(());
        }
        KeyCode::Up => {
            form.focus_prev();
            return Ok(());
        }
        _ => {}
    }

    if let super::input::insert::InsertKeyResult::ExitInsert =
        super::input::insert::handle_field_insert_key(
            &mut form.fields[form.focused],
            code,
            ctrl,
            &mut form.pending_key,
        )
    {
        form.vim_mode = VimMode::Normal;
    }
    Ok(())
}

pub(super) fn draw_popup(f: &mut Frame, app: &App) {
    let Some(form) = &app.form else { return };

    let in_routes = form.in_routes();
    let prov_color = t::PRIMARY;
    let suggest_items = route_suggest_panel_height(form, app);
    let vim_tag = if form.route_editing || form.vim_mode == VimMode::Insert {
        "[I]"
    } else {
        "[N]"
    };
    let title = format!(
        " {} Provider  {} ",
        if form.original_name.is_none() {
            "Add"
        } else {
            "Edit"
        },
        vim_tag
    );

    let format_choices: Vec<&'static str> = if form.focused == FORMAT_FIELD_IDX && !in_routes {
        format_suggest_choices(form)
    } else {
        Vec::new()
    };
    let format_choices_len = format_choices.len() as u16;
    let mut field_heights: Vec<u16> = vec![2; form.fields.len()];
    if format_choices_len > 0 {
        field_heights[FORMAT_FIELD_IDX] = 2 + 1 + format_choices_len + 1;
    }
    let fields_total: u16 = field_heights.iter().sum();
    let routes_items = form.routes.len().max(1) as u16;
    let suggest_section = if suggest_items > 0 {
        1 + suggest_items
    } else {
        0
    };
    let routes_height = 1 + routes_items + suggest_section + 1;

    let dialog_height = fields_total + routes_height + 3 + 2 + 2;
    let area = centered_fixed(70, dialog_height, f.area());

    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(t::PRIMARY))
        .title(title.as_str())
        .title_style(Style::default().fg(t::PRIMARY).add_modifier(Modifier::BOLD))
        .padding(Padding::new(2, 2, 1, 1));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let field_constraints: Vec<Constraint> = field_heights
        .iter()
        .map(|&h| Constraint::Length(h))
        .chain(std::iter::once(Constraint::Length(routes_height)))
        .chain(std::iter::once(Constraint::Length(3)))
        .collect();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(field_constraints)
        .split(inner);

    for (i, field) in form.fields.iter().enumerate() {
        let ci = i;
        let is_focused = i == form.focused;
        let show_cursor = is_focused && field.editable && form.vim_mode == VimMode::Insert;

        let label_style = if is_focused {
            Style::default().fg(prov_color).add_modifier(Modifier::BOLD)
        } else if !field.editable {
            Style::default().fg(t::MUTED)
        } else {
            Style::default().fg(t::TEXT)
        };

        let display_val = if field.label == "API Key" && !is_focused {
            mask_api_key_str(&field.value).unwrap_or_else(|| field.value.clone())
        } else {
            field.value.clone()
        };

        let value_display = if show_cursor {
            let mut spans = vec![Span::styled(format!("{:<11}", field.label), label_style)];
            spans.extend(super::ui::format::cursor_split_spans(
                &display_val,
                field.cursor,
                prov_color,
            ));
            Line::from(spans)
        } else {
            let val_style = if !field.editable {
                Style::default().fg(t::MUTED)
            } else {
                Style::default()
            };
            Line::from(vec![
                Span::styled(format!("{:<11}", field.label), label_style),
                Span::styled(display_val, val_style),
            ])
        };

        if i == FORMAT_FIELD_IDX && format_choices_len > 0 {
            // Split the tall chunk: field row on top, dropdown below.
            let sub = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(2), Constraint::Min(0)])
                .split(chunks[ci]);
            f.render_widget(Paragraph::new(vec![value_display]), sub[0]);
            let render_target = sub[1];
            if render_target.width > 0 && render_target.height > 0 {
                let lines = render_suggestion_lines(
                    &format_choices,
                    &form.format_suggest,
                    prov_color,
                    SuggestionStyle::Classic,
                );
                f.render_widget(Paragraph::new(lines), render_target);
            }
        } else {
            f.render_widget(Paragraph::new(vec![value_display]), chunks[ci]);
        }
    }

    let routes_chunk = chunks[form.fields.len()];
    draw_routes_section(f, form, app, routes_chunk, prov_color, in_routes);

    let hint_idx = form.fields.len() + 1;
    if hint_idx < chunks.len() {
        let hint_line = if form.detect_token.is_some() {
            Line::from(vec![
                Span::raw("   "),
                Span::styled(
                    "Detecting API format… ",
                    Style::default().fg(t::PRIMARY).add_modifier(Modifier::BOLD),
                ),
                Span::styled("Esc", Style::default().fg(t::WARNING)),
                Span::styled(" Cancel", Style::default().fg(t::MUTED)),
            ])
        } else if in_routes {
            if form.route_editing {
                if form.route_edit_target {
                    Line::from(vec![
                        Span::raw("   "),
                        Span::styled("Esc", Style::default().fg(t::WARNING)),
                        Span::styled("/", Style::default().fg(t::MUTED)),
                        Span::styled("Esc", Style::default().fg(t::WARNING)),
                        Span::styled(" Normal  ", Style::default().fg(t::MUTED)),
                        Span::styled("↓", Style::default().fg(t::PRIMARY)),
                        Span::styled("/", Style::default().fg(t::MUTED)),
                        Span::styled("^J", Style::default().fg(t::PRIMARY)),
                        Span::styled(" Suggest  ", Style::default().fg(t::MUTED)),
                        Span::styled("Tab", Style::default().fg(t::PRIMARY)),
                        Span::styled(" Pat↔Tgt  ", Style::default().fg(t::MUTED)),
                        Span::styled("←/→", Style::default().fg(t::PRIMARY)),
                        Span::styled(" Move cursor", Style::default().fg(t::MUTED)),
                    ])
                } else {
                    Line::from(vec![
                        Span::raw("   "),
                        Span::styled("Esc", Style::default().fg(t::WARNING)),
                        Span::styled(" Normal  ", Style::default().fg(t::MUTED)),
                        Span::styled("Tab", Style::default().fg(t::PRIMARY)),
                        Span::styled(" Pat↔Tgt  ", Style::default().fg(t::MUTED)),
                        Span::styled("←/→", Style::default().fg(t::PRIMARY)),
                        Span::styled(" Move cursor", Style::default().fg(t::MUTED)),
                    ])
                }
            } else {
                Line::from(vec![
                    Span::raw("   "),
                    Span::styled("a", Style::default().fg(t::SUCCESS)),
                    Span::styled(" Add  ", Style::default().fg(t::MUTED)),
                    Span::styled("Space", Style::default().fg(t::PRIMARY)),
                    Span::styled(" Toggle  ", Style::default().fg(t::MUTED)),
                    Span::styled("dd", Style::default().fg(t::WARNING)),
                    Span::styled(" Del  ", Style::default().fg(t::MUTED)),
                    Span::styled("i", Style::default().fg(t::PRIMARY)),
                    Span::styled(" Pat  ", Style::default().fg(t::MUTED)),
                    Span::styled("t", Style::default().fg(t::PRIMARY)),
                    Span::styled(" Tgt  ", Style::default().fg(t::MUTED)),
                    Span::styled("q", Style::default().fg(t::WARNING)),
                    Span::styled(" Quit", Style::default().fg(t::MUTED)),
                ])
            }
        } else if form.detect_failed {
            Line::from(vec![
                Span::raw("   "),
                Span::styled("a", Style::default().fg(t::SUCCESS)),
                Span::styled(" Anthropic  ", Style::default().fg(t::MUTED)),
                Span::styled("o", Style::default().fg(t::PRIMARY)),
                Span::styled(" OpenAI  ", Style::default().fg(t::MUTED)),
                Span::styled("q", Style::default().fg(t::WARNING)),
                Span::styled(" Retry detection", Style::default().fg(t::MUTED)),
            ])
        } else if form.vim_mode == VimMode::Insert {
            Line::from(vec![
                Span::raw("   "),
                Span::styled("Esc", Style::default().fg(t::WARNING)),
                Span::styled(" Normal  ", Style::default().fg(t::MUTED)),
                Span::styled("Tab", Style::default().fg(t::PRIMARY)),
                Span::styled(" Next field", Style::default().fg(t::MUTED)),
            ])
        } else {
            Line::from(vec![
                Span::raw("   "),
                Span::styled("i", Style::default().fg(t::PRIMARY)),
                Span::styled(" Insert  ", Style::default().fg(t::MUTED)),
                Span::styled("j/k", Style::default().fg(t::PRIMARY)),
                Span::styled(" Field  ", Style::default().fg(t::MUTED)),
                Span::styled("q", Style::default().fg(t::WARNING)),
                Span::styled(" Quit", Style::default().fg(t::MUTED)),
            ])
        };

        let mut hint_lines = vec![hint_line, Line::from("")];
        if let Some(err) = &form.error {
            hint_lines.push(Line::from(Span::styled(
                format!("✗ {}", err),
                Style::default().fg(t::ERROR),
            )));
        }
        f.render_widget(Paragraph::new(hint_lines), chunks[hint_idx]);
    }
}

fn close(app: &mut App, server: &Option<ServerHandle>) {
    // New empty forms with no name entered are just discarded.
    let should_save = app.form.as_ref().is_some_and(|f| {
        f.original_name.is_some() || !f.fields[NAME_FIELD_IDX].value.trim().is_empty()
    });

    if should_save {
        if let Err(e) = app.save_form_and_close() {
            // IO error: force-close and show message.
            app.set_message(
                format!("Save failed: {e}"),
                super::state::MessageKind::Error,
            );
            app.form = None;
            app.mode = Mode::Normal;
        }
        // Validation error, or format detection now running in the
        // background: do_save_form keeps the form open in both cases.
        if app
            .form
            .as_ref()
            .is_some_and(|f| f.error.is_some() || f.detect_token.is_some())
        {
            return;
        }
    } else {
        app.form = None;
        app.mode = Mode::Normal;
    }

    sync_proxy_config(app, server);
}

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

/// Format choices shown for the Format field. A complete current choice
/// (exact match in [`crate::config::FORMAT_CHOICES`]) shows all three — it's
/// a select field, not free text; a partial typed prefix narrows by prefix.
fn format_suggest_choices(form: &ProviderForm) -> Vec<&'static str> {
    let raw = form.fields[FORMAT_FIELD_IDX].value.trim().to_lowercase();
    let filter = if crate::config::FORMAT_CHOICES.contains(&raw.as_str()) {
        ""
    } else {
        raw.as_str()
    };
    crate::config::FORMAT_CHOICES
        .iter()
        .copied()
        .filter(|c| filter.is_empty() || c.starts_with(filter))
        .collect()
}
/// False (nothing to suggest) lets callers fall back to plain field nav.
fn nav_suggest_down(suggest: &mut super::state::SuggestState, len: usize) -> bool {
    if len > 0 {
        suggest.advance(len);
        true
    } else {
        false
    }
}

fn nav_suggest_up(suggest: &mut super::state::SuggestState) -> bool {
    if suggest.active {
        suggest.retreat();
        true
    } else {
        false
    }
}

/// Number of models currently matching the route Target field's filter.
fn route_target_suggest_len(form: &ProviderForm, provider_models: &[String]) -> usize {
    let filter = form
        .routes
        .get(form.route_cursor)
        .map(|r| r.target.as_str())
        .unwrap_or("");
    filter_suggestions(provider_models, filter).len()
}

fn exit_route_insert(form: &mut ProviderForm, provider_models: &[String]) {
    form.reset_route_editing();
    prune_current_rule(form, provider_models);
}

fn reset_suggest(form: &mut ProviderForm) {
    form.route_suggest.reset();
}

fn enter_route_insert_mode(form: &mut ProviderForm, edit_target: bool) {
    form.pending_key = None;
    if form.routes.is_empty() {
        form.routes.push(RouteRule::new(""));
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

fn sync_route_fields(form: &mut ProviderForm) {
    if let Some(rule) = form.routes.get_mut(form.route_cursor) {
        rule.pattern = form.route_pat_field.value.clone();
        rule.target = form.route_tgt_field.value.clone();
    }
}

fn focus_pattern(form: &mut ProviderForm) {
    form.route_edit_target = false;
    reset_suggest(form);
    if let Some(rule) = form.routes.get(form.route_cursor) {
        form.route_pat_field = FormField::text("", &rule.pattern);
    }
}

fn handle_routes_key(
    form: &mut ProviderForm,
    code: KeyCode,
    ctrl: bool,
    provider_models: &[String],
) {
    if form.route_editing {
        if code == KeyCode::Esc {
            if form.route_suggest.active {
                reset_suggest(form);
            } else {
                exit_route_insert(form, provider_models);
            }
            return;
        }

        match code {
            KeyCode::Enter => {
                if form.route_suggest.active {
                    let filter = form
                        .routes
                        .get(form.route_cursor)
                        .map(|r| r.target.as_str())
                        .unwrap_or("");
                    let suggestions = filter_suggestions(provider_models, filter);
                    if let Some(&model) = suggestions.get(form.route_suggest.idx) {
                        form.route_tgt_field = FormField::text("", model);
                        sync_route_fields(form);
                    }
                }
                exit_route_insert(form, provider_models);
                return;
            }
            KeyCode::Tab => {
                if !form.route_edit_target {
                    form.route_edit_target = true;
                    if let Some(rule) = form.routes.get(form.route_cursor) {
                        form.route_tgt_field = FormField::text("", &rule.target);
                    }
                } else {
                    focus_pattern(form);
                }
                return;
            }
            KeyCode::BackTab => {
                if form.route_edit_target {
                    focus_pattern(form);
                } else {
                    exit_route_insert(form, provider_models);
                    form.focus_prev();
                }
                return;
            }
            KeyCode::Down if form.route_edit_target => {
                let len = route_target_suggest_len(form, provider_models);
                nav_suggest_down(&mut form.route_suggest, len);
                return;
            }
            KeyCode::Char('j') if ctrl && form.route_edit_target => {
                let len = route_target_suggest_len(form, provider_models);
                nav_suggest_down(&mut form.route_suggest, len);
                return;
            }
            KeyCode::Up if form.route_edit_target => {
                nav_suggest_up(&mut form.route_suggest);
                return;
            }
            KeyCode::Char('k') if ctrl && form.route_edit_target => {
                nav_suggest_up(&mut form.route_suggest);
                return;
            }
            KeyCode::Char(' ') if !ctrl => return,
            _ => {}
        }

        let edit_target = form.route_edit_target;
        let field = if edit_target {
            &mut form.route_tgt_field
        } else {
            &mut form.route_pat_field
        };
        match super::input::insert::handle_field_insert_key(
            field,
            code,
            ctrl,
            &mut form.pending_key,
        ) {
            super::input::insert::InsertKeyResult::ExitInsert => {
                if edit_target {
                    reset_suggest(form);
                }
                sync_route_fields(form);
                exit_route_insert(form, provider_models);
            }
            super::input::insert::InsertKeyResult::TextChanged => {
                if edit_target {
                    reset_suggest(form);
                }
                sync_route_fields(form);
            }
            super::input::insert::InsertKeyResult::Consumed
            | super::input::insert::InsertKeyResult::NotHandled => {}
        }
    } else {
        match code {
            KeyCode::Char('a') | KeyCode::Char('o') if !ctrl => {
                form.routes.push(RouteRule::new(""));
                form.route_cursor = form.routes.len() - 1;
                form.route_pat_field = FormField::text("", "");
                form.route_tgt_field = FormField::text("", "");
                form.route_edit_target = false;
                form.route_editing = true;
            }
            KeyCode::Char(' ') => {
                if let Some(rule) = form.routes.get_mut(form.route_cursor) {
                    rule.enabled = !rule.enabled;
                }
            }
            KeyCode::Char('d') if !ctrl => {
                form.pending_key = Some(('d', std::time::Instant::now()));
            }
            KeyCode::Enter | KeyCode::Char('i') if !ctrl => {
                enter_route_insert_mode(form, false);
            }
            KeyCode::Char('t') if !ctrl => {
                enter_route_insert_mode(form, true);
            }
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
            KeyCode::Char('k') | KeyCode::Up => {
                if form.route_cursor == 0 || form.routes.is_empty() {
                    form.focus_prev();
                } else {
                    form.route_cursor -= 1;
                }
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if !form.routes.is_empty() && form.route_cursor + 1 < form.routes.len() {
                    form.route_cursor += 1;
                } else {
                    form.focus_next();
                }
            }
            KeyCode::Tab => form.focus_next(),
            KeyCode::BackTab => form.focus_prev(),
            _ => {}
        }
    }
}

fn get_route_suggestions<'a>(form: &ProviderForm, app: &'a App) -> Vec<&'a str> {
    if form.route_editing && form.route_edit_target && form.in_routes() {
        let tgt_filter = form
            .routes
            .get(form.route_cursor)
            .map(|r| r.target.as_str())
            .unwrap_or("");
        provider_model_suggestions(&app.models.provider_models, form.prov_key(), tgt_filter)
    } else {
        vec![]
    }
}

fn route_suggest_panel_height(form: &ProviderForm, app: &App) -> u16 {
    get_route_suggestions(form, app).len().min(8) as u16
}

fn draw_routes_section(
    f: &mut Frame,
    form: &ProviderForm,
    app: &App,
    area: Rect,
    prov_color: Color,
    in_routes: bool,
) {
    let routes_label_style = if in_routes {
        Style::default().fg(prov_color).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(t::TEXT)
    };

    let mut lines: Vec<Line> = vec![Line::from(Span::styled("Routes    ", routes_label_style))];

    if form.routes.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no rules)",
            Style::default().fg(t::MUTED),
        )));
    } else {
        for (i, rule) in form.routes.iter().enumerate() {
            let is_selected = in_routes && i == form.route_cursor;
            let toggle_ch = if rule.enabled { '✓' } else { ' ' };
            let toggle_style = if rule.enabled {
                Style::default().fg(t::SUCCESS)
            } else {
                Style::default().fg(t::MUTED)
            };

            let render_field = |text: &str, cursor_pos: usize, active: bool, color: Color| {
                if active {
                    super::ui::format::cursor_split_spans(text, cursor_pos, color)
                } else {
                    vec![Span::raw(text.to_string())]
                }
            };

            if is_selected && form.route_editing {
                let pat_active = !form.route_edit_target;
                let tgt_active = form.route_edit_target;

                let pat_spans = render_field(
                    &rule.pattern,
                    form.route_pat_field.cursor,
                    pat_active,
                    prov_color,
                );
                let tgt_text_owned;
                let tgt_text = if rule.target.is_empty() && !tgt_active {
                    "target"
                } else {
                    tgt_text_owned = rule.target.clone();
                    &tgt_text_owned
                };
                let tgt_spans = render_field(
                    tgt_text,
                    form.route_tgt_field.cursor,
                    tgt_active,
                    prov_color,
                );

                let mut spans = vec![
                    Span::raw("  "),
                    Span::styled(format!("[{toggle_ch}] "), toggle_style),
                ];
                spans.extend(pat_spans);
                spans.push(Span::styled(" -> ", Style::default().fg(t::MUTED)));
                spans.extend(tgt_spans);
                lines.push(Line::from(spans));
            } else if is_selected {
                let tgt_text = if rule.target.is_empty() {
                    "target".to_string()
                } else {
                    rule.target.clone()
                };
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(format!("[{toggle_ch}] "), Style::default().fg(prov_color)),
                    Span::styled(
                        rule.pattern.as_str(),
                        Style::default().fg(t::TEXT).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(" -> ", Style::default().fg(t::MUTED)),
                    Span::styled(tgt_text, Style::default().fg(t::TEXT)),
                ]));
            } else {
                let pat_style = if rule.enabled {
                    Style::default().fg(t::TEXT)
                } else {
                    Style::default().fg(t::MUTED)
                };
                let tgt_text = if rule.target.is_empty() {
                    "target".to_string()
                } else {
                    rule.target.clone()
                };
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(format!("[{toggle_ch}] "), toggle_style),
                    Span::styled(rule.pattern.as_str(), pat_style),
                    Span::styled(" -> ", Style::default().fg(t::MUTED)),
                    Span::styled(tgt_text, pat_style),
                ]));
            }
        }
    }

    let suggestions = get_route_suggestions(form, app);
    lines.extend(render_suggestion_lines(
        &suggestions,
        &form.route_suggest,
        prov_color,
        SuggestionStyle::Classic,
    ));

    lines.push(Line::from(""));
    f.render_widget(Paragraph::new(lines), area);
}
