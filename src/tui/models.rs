//! Models browser popup.
//!
//! This module owns the searchable models dialog, its navigation, and
//! clipboard actions for the currently highlighted model.

use std::time::Instant;

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph};

use super::state::{App, MessageKind, Mode};
use super::theme::{self as t};
use super::ui::layout::centered_fixed;

pub(super) fn handle_key(
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> crate::error::Result<()> {
    let flat = build_flat_models(app);
    let line_offsets = build_line_offsets(app, &flat);
    let flat_refs: Vec<&str> = flat.iter().map(|s| s.as_str()).collect();
    app.models.selected = app.models.selected.min(flat_refs.len().saturating_sub(1));

    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    let total = flat_refs.len();

    if app.models.search_active {
        if ctrl && code == KeyCode::Char('c') {
            app.mode = Mode::Normal;
            return Ok(());
        }
        match code {
            KeyCode::Down => {
                nav_down(app, total, 1, &line_offsets);
                return Ok(());
            }
            KeyCode::Up => {
                nav_up(app, 1, &line_offsets);
                return Ok(());
            }
            KeyCode::Char('j') if ctrl => {
                nav_down(app, total, 1, &line_offsets);
                return Ok(());
            }
            KeyCode::Char('k') if ctrl => {
                nav_up(app, 1, &line_offsets);
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
                    copy_selected(app, &flat_refs);
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
            KeyCode::Down | KeyCode::Char('j') => nav_down(app, total, 1, &line_offsets),
            KeyCode::Up | KeyCode::Char('k') => nav_up(app, 1, &line_offsets),
            KeyCode::Char('G') => {
                if total > 0 {
                    app.models.selected = total - 1;
                    if let Some(&row) = line_offsets.get(total - 1) {
                        app.models.scroll = (row + 1).saturating_sub(8) as u16;
                    }
                }
            }
            KeyCode::PageDown | KeyCode::Char('d') if ctrl => {
                nav_down(app, total, 10, &line_offsets)
            }
            KeyCode::PageUp | KeyCode::Char('u') if ctrl => nav_up(app, 10, &line_offsets),
            KeyCode::Enter => copy_selected(app, &flat_refs),
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
    app.models
        .search_field
        .value
        .insert_str(app.models.search_field.cursor, text);
    app.models.search_field.cursor += text.len();
}

pub(super) fn draw_popup(f: &mut Frame, app: &App) {
    let filtered = build_grouped_models(app);
    let content_lines = (filtered.iter().map(|(_, ms)| 1 + ms.len()).sum::<usize>()
        + filtered.len().saturating_sub(1))
    .min(u16::MAX as usize) as u16;

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
        .title(format!(" Models  {mode_tag} "))
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
    let mut flat_idx: usize = 0;

    if filtered.is_empty() {
        lines.push(Line::from(Span::styled(
            "  No matches",
            Style::default().fg(t::MUTED),
        )));
    } else {
        for (gi, (prov, models)) in filtered.iter().enumerate() {
            if gi > 0 {
                lines.push(Line::from(""));
            }
            lines.push(Line::from(Span::styled(
                format!("  {prov}"),
                Style::default()
                    .fg(t::provider_color(prov))
                    .add_modifier(Modifier::BOLD),
            )));
            let last = models.len().saturating_sub(1);
            for (mi, model) in models.iter().enumerate() {
                let is_selected = flat_idx == app.models.selected;
                let prefix = if mi == last { "    └ " } else { "    ├ " };
                if is_selected {
                    let prov_color = t::provider_color(prov);
                    lines.push(Line::from(vec![
                        Span::styled("  ▶ ", Style::default().fg(prov_color)),
                        Span::styled(
                            model.to_string(),
                            Style::default().fg(prov_color).add_modifier(Modifier::BOLD),
                        ),
                    ]));
                } else {
                    lines.push(Line::from(vec![
                        Span::styled(prefix, Style::default().fg(t::MUTED)),
                        Span::styled(model.to_string(), Style::default().fg(t::TEXT)),
                    ]));
                }
                flat_idx += 1;
            }
        }
    }

    let max_scroll = (lines.len().min(u16::MAX as usize) as u16).saturating_sub(list_height);
    let scroll = app.models.scroll.min(max_scroll);

    f.render_widget(Paragraph::new(lines).scroll((scroll, 0)), chunks[2]);
}

fn build_grouped_models(app: &App) -> Vec<(String, Vec<String>)> {
    let filter = app.models.search_field.value.to_lowercase();
    let mut providers: Vec<&String> = app.models.provider_models.keys().collect();
    providers.sort_unstable();

    providers
        .iter()
        .filter_map(|prov| {
            let models = app.models.provider_models.get(*prov)?;
            let mut matched: Vec<String> = models
                .iter()
                .filter(|m| filter.is_empty() || m.to_lowercase().contains(&filter))
                .cloned()
                .collect();
            if matched.is_empty() {
                return None;
            }
            matched.sort_unstable();
            Some(((*prov).clone(), matched))
        })
        .collect()
}

fn build_flat_models(app: &App) -> Vec<String> {
    build_grouped_models(app)
        .into_iter()
        .flat_map(|(_, models)| models)
        .collect()
}

fn build_line_offsets(app: &App, flat: &[String]) -> Vec<usize> {
    let filter = app.models.search_field.value.to_lowercase();
    let mut providers: Vec<&String> = app.models.provider_models.keys().collect();
    providers.sort_unstable();

    let mut line_offsets: Vec<usize> = Vec::with_capacity(flat.len());
    let mut line: usize = 0;
    let mut first_group = true;

    for prov in &providers {
        let models = app
            .models
            .provider_models
            .get(*prov)
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        let mut matched: Vec<String> = models
            .iter()
            .filter(|m| filter.is_empty() || m.to_lowercase().contains(&filter))
            .cloned()
            .collect();
        if matched.is_empty() {
            continue;
        }
        matched.sort_unstable();
        if !first_group {
            line += 1;
        }
        first_group = false;
        line += 1;
        for _ in matched {
            line_offsets.push(line);
            line += 1;
        }
    }

    line_offsets
}

fn nav_down(app: &mut App, total: usize, step: usize, line_offsets: &[usize]) {
    if total == 0 {
        return;
    }
    app.models.selected = (app.models.selected + step).min(total - 1);
    if let Some(&row) = line_offsets.get(app.models.selected) {
        let bottom = app.models.scroll as usize + 8;
        if row >= bottom {
            app.models.scroll = (row + 1).saturating_sub(8) as u16;
        }
    }
}

fn nav_up(app: &mut App, step: usize, line_offsets: &[usize]) {
    app.models.selected = app.models.selected.saturating_sub(step);
    if let Some(&row) = line_offsets.get(app.models.selected)
        && row < app.models.scroll as usize
    {
        app.models.scroll = row as u16;
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
