//! Models browser popup.
//!
//! This module owns the searchable models dialog, its navigation, clipboard
//! actions for the currently highlighted model, testing it (`t`), and pinning
//! it as the provider's Test Model (`p`). Test results render after each model
//! name.

use std::time::Instant;

use super::state::{App, MessageKind, Mode};
use super::theme::{self as t};
use super::ui::format::{fmt_latency, truncate_error};
use super::ui::layout::centered_fixed;
use crate::tester::TestStatus;
use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph};

/// Estimated visible rows used for scroll look-ahead in nav and 'G' jump.
/// Matches the viewport assumption in draw_popup's dialog_height calculation.
const SCROLL_LOOKAHEAD: usize = 8;

pub(super) fn handle_key(
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> crate::error::Result<()> {
    let models = build_models(app);
    let refs: Vec<&str> = models.iter().map(|s| s.as_str()).collect();
    app.models.selected = app.models.selected.min(refs.len().saturating_sub(1));

    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    let total = refs.len();

    if app.models.search_active {
        if ctrl && code == KeyCode::Char('c') {
            app.mode = Mode::Normal;
            return Ok(());
        }
        match code {
            KeyCode::Down => {
                nav_down(app, total, 1);
                return Ok(());
            }
            KeyCode::Up => {
                nav_up(app, 1);
                return Ok(());
            }
            KeyCode::Char('j') if ctrl => {
                nav_down(app, total, 1);
                return Ok(());
            }
            KeyCode::Char('k') if ctrl => {
                nav_up(app, 1);
                return Ok(());
            }
            _ => {}
        }

        match super::input::insert::handle_field_insert_key(
            &mut app.models.search_field,
            code,
            ctrl,
            &mut app.models.pending_key,
        ) {
            super::input::insert::InsertKeyResult::ExitInsert => {
                app.models.search_active = false;
            }
            super::input::insert::InsertKeyResult::TextChanged => {
                app.models.selected = 0;
                app.models.scroll = 0;
            }
            super::input::insert::InsertKeyResult::Consumed
            | super::input::insert::InsertKeyResult::NotHandled => {}
        }
    } else {
        let prev = super::input::insert::consume_pending_key(&mut app.models.pending_key);

        if let Some(pk) = prev {
            match (pk, &code) {
                ('y', KeyCode::Char('y')) => {
                    copy_selected(app, &refs);
                    return Ok(());
                }
                ('g', KeyCode::Char('g')) => {
                    app.models.selected = 0;
                    app.models.scroll = 0;
                    return Ok(());
                }
                _ => {}
            }
        }

        match code {
            KeyCode::Esc | KeyCode::Char('q') => {
                app.mode = Mode::Normal;
            }
            KeyCode::Char('c') if ctrl => {
                app.mode = Mode::Normal;
            }
            KeyCode::Char('i') => {
                app.models.search_active = true;
            }
            KeyCode::Char('t') => {
                if let Some((prov, name)) = selected_model(app, &refs) {
                    super::testing::test_specific_model(app, &prov, name);
                }
            }
            KeyCode::Char('p') => {
                if let Some((prov, name)) = selected_model(app, &refs) {
                    set_test_model(app, &prov, name);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => nav_down(app, total, 1),
            KeyCode::Up | KeyCode::Char('k') => nav_up(app, 1),
            KeyCode::Char('G') => {
                if total > 0 {
                    app.models.selected = total - 1;
                    app.models.scroll = total.saturating_sub(SCROLL_LOOKAHEAD) as u16;
                }
            }
            KeyCode::PageDown | KeyCode::Char('d') if ctrl => nav_down(app, total, 10),
            KeyCode::PageUp | KeyCode::Char('u') if ctrl => nav_up(app, 10),
            KeyCode::Enter => copy_selected(app, &refs),
            KeyCode::Char('y') => {
                app.models.pending_key = Some(('y', Instant::now()));
            }
            KeyCode::Char('g') => {
                app.models.pending_key = Some(('g', Instant::now()));
            }
            _ => {}
        }
    }

    Ok(())
}

pub(super) fn handle_paste(app: &mut App, text: &str) {
    app.models.search_active = true;
    app.models.search_field.insert_str(text);
    app.models.selected = 0;
    app.models.scroll = 0;
}

pub(super) fn draw_popup(f: &mut Frame, app: &App) {
    let models = build_models(app);
    let prov_name = current_provider(app).unwrap_or("—");
    let prov_color = t::PRIMARY;

    let content_lines = models.len().min(u16::MAX as usize) as u16;
    let dialog_height = (content_lines + 4).min(f.area().height * 4 / 5).max(6);
    let area = centered_fixed(80, dialog_height, f.area());
    f.render_widget(Clear, area);
    let mode_tag = if app.models.search_active {
        "[I]"
    } else {
        "[N]"
    };
    let outer_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(t::PRIMARY))
        .title(format!(" {prov_name} — Models  {mode_tag} "))
        .title_style(Style::default().fg(t::TEXT).add_modifier(Modifier::BOLD))
        .padding(Padding::new(1, 1, 0, 0));
    let inner = outer_block.inner(area);
    f.render_widget(outer_block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(inner);

    let search_line = if app.models.search_active {
        let val = &app.models.search_field.value;
        let cur = app.models.search_field.cursor.min(val.len());
        let before = &val[..cur];
        let cursor_ch = val[cur..].chars().next().unwrap_or(' ');
        let after_start = cur
            + if cur < val.len() {
                cursor_ch.len_utf8()
            } else {
                0
            };
        let after = if after_start <= val.len() {
            &val[after_start..]
        } else {
            ""
        };
        Line::from(vec![
            Span::styled("Search  ", Style::default().fg(t::MUTED)),
            Span::raw(before.to_string()),
            Span::styled(
                cursor_ch.to_string(),
                Style::default()
                    .fg(t::PRIMARY)
                    .add_modifier(Modifier::REVERSED),
            ),
            Span::raw(after.to_string()),
        ])
    } else {
        Line::from(vec![
            Span::styled("Search  ", Style::default().fg(t::MUTED)),
            Span::styled(
                &app.models.search_field.value,
                Style::default().fg(t::MUTED),
            ),
        ])
    };
    f.render_widget(Paragraph::new(search_line), chunks[0]);

    let divider = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(t::MUTED));
    f.render_widget(divider, chunks[1]);

    let list_height = chunks[2].height;
    let mut lines: Vec<Line> = Vec::new();

    if models.is_empty() {
        lines.push(Line::from(Span::styled(
            "  No matches",
            Style::default().fg(t::MUTED),
        )));
    } else {
        for (i, model) in models.iter().enumerate() {
            let is_selected = i == app.models.selected;
            let mut spans = if is_selected {
                vec![
                    Span::styled("  ▶ ", Style::default().fg(prov_color)),
                    Span::styled(
                        model.to_string(),
                        Style::default().fg(prov_color).add_modifier(Modifier::BOLD),
                    ),
                ]
            } else {
                vec![
                    Span::raw("    "),
                    Span::styled(model.to_string(), Style::default().fg(t::TEXT)),
                ]
            };
            spans.extend(model_result_suffix(app, prov_name, model));
            lines.push(Line::from(spans));
        }
    }

    let max_scroll = (lines.len().min(u16::MAX as usize) as u16).saturating_sub(list_height);
    let scroll = app.models.scroll.min(max_scroll);

    f.render_widget(Paragraph::new(lines).scroll((scroll, 0)), chunks[2]);
}

fn current_test_model(app: &App) -> Option<&str> {
    current_provider(app).and_then(|name| {
        app.config
            .providers
            .get(name)
            .and_then(|p| p.test_model.as_deref())
    })
}

/// Pin the highlighted model as the provider's Test Model and persist it.
/// Pressing `p` again on the pinned model clears it.
fn set_test_model(app: &mut App, provider_name: &str, model: &str) {
    let clearing = current_test_model(app) == Some(model);
    let mut next = app.config.clone();
    if let Some(p) = next.providers.get_mut(provider_name) {
        if clearing {
            p.test_model = None;
        } else {
            p.test_model = Some(model.to_string());
        }
    }
    if let Err(e) = crate::config::save_config(&next) {
        app.set_message(
            format!("Failed to save test model: {e}"),
            MessageKind::Error,
        );
        return;
    }
    app.config = next;
    if clearing {
        app.set_message("Test model cleared".to_string(), MessageKind::Success);
    } else {
        app.set_message(format!("Test model set: {model}"), MessageKind::Success);
    }
}
/// Suffix spans rendered after a model name: a live "Testing…" indicator or
/// the last test result for that model (latency for OK, reason otherwise).
fn model_result_suffix(app: &App, provider: &str, model: &str) -> Vec<Span<'static>> {
    // Only show the live indicator while a test is actually running; after
    // completion the row falls through to the stored result below.
    if app.tests.pending.contains(provider)
        && app.tests.testing_model.get(provider).map(String::as_str) == Some(model)
    {
        return vec![Span::styled(
            "  Testing…",
            Style::default().fg(t::MUTED).add_modifier(Modifier::ITALIC),
        )];
    }
    let Some(r) = app.tests.results.get(provider).and_then(|m| m.get(model)) else {
        return vec![];
    };
    match &r.status {
        TestStatus::Ok => vec![Span::styled(
            format!("  ✓ {}", fmt_latency(r.latency_ms)),
            Style::default().fg(t::SUCCESS),
        )],
        TestStatus::AuthFailed => vec![Span::styled("  ✗ auth", Style::default().fg(t::ERROR))],
        TestStatus::Error(e) => vec![Span::styled(
            format!("  ✗ {}", truncate_error(e)),
            Style::default().fg(t::ERROR),
        )],
    }
}

fn build_models(app: &App) -> Vec<String> {
    let Some(prov) = current_provider(app) else {
        return vec![];
    };
    let filter = app.models.search_field.value.to_lowercase();
    let models = app
        .models
        .provider_models
        .get(prov)
        .map(|v| v.as_slice())
        .unwrap_or(&[]);
    let mut matched: Vec<String> = models
        .iter()
        .filter(|m| filter.is_empty() || m.to_lowercase().contains(&filter))
        .cloned()
        .collect();
    matched.sort_unstable();
    matched
}

fn current_provider(app: &App) -> Option<&str> {
    app.providers
        .table_state
        .selected()
        .and_then(|i| app.provider_name_at(i))
}

/// Provider name and the highlighted model name, if both exist.
fn selected_model<'a>(app: &App, refs: &'a [&str]) -> Option<(String, &'a str)> {
    let prov = current_provider(app)?.to_string();
    let model = *refs.get(app.models.selected)?;
    Some((prov, model))
}

fn nav_down(app: &mut App, total: usize, step: usize) {
    if total == 0 {
        return;
    }
    app.models.selected = (app.models.selected + step).min(total - 1);
    let bottom = app.models.scroll as usize + SCROLL_LOOKAHEAD;
    if app.models.selected >= bottom {
        app.models.scroll = (app.models.selected + 1).saturating_sub(8) as u16;
    }
}

fn nav_up(app: &mut App, step: usize) {
    app.models.selected = app.models.selected.saturating_sub(step);
    if app.models.selected < app.models.scroll as usize {
        app.models.scroll = app.models.selected as u16;
    }
}

fn copy_selected(app: &mut App, flat: &[&str]) {
    if let Some(&name) = flat.get(app.models.selected) {
        if super::input::copy_to_clipboard(name) {
            app.set_message(format!("Copied: {name}"), MessageKind::Success);
        } else {
            app.set_message("Copy failed (wl-copy not found?)", MessageKind::Error);
        }
    }
}
