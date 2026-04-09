//! Quota command editor and preview panel.
//!
//! This module handles editing the saved shell command for quota checks and
//! rendering the command/preview popup.

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph, Wrap};

use crate::config;

use super::App;
use super::state::{Mode, VimMode};
use super::theme::{self as t};
use super::ui::layout::centered_fixed;

pub(super) fn handle_key(
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> crate::error::Result<()> {
    let Some(form) = app.quota_form.as_mut() else {
        app.mode = Mode::Normal;
        return Ok(());
    };

    match form.vim_mode {
        VimMode::Normal => handle_normal_mode(app, code, modifiers),
        VimMode::Insert => {
            handle_insert_mode(app, code, modifiers);
            Ok(())
        }
    }
}

pub(super) fn handle_paste(app: &mut App, text: &str) -> crate::error::Result<()> {
    let Some(form) = app.quota_form.as_mut() else {
        return Ok(());
    };

    if form.vim_mode != VimMode::Insert {
        form.vim_mode = VimMode::Insert;
    }
    form.curl_field
        .value
        .insert_str(form.curl_field.cursor, text);
    form.curl_field.cursor += text.len();
    Ok(())
}

pub(super) fn draw_popup(f: &mut Frame, app: &App) {
    let Some(form) = &app.quota_form else {
        return;
    };

    let vim_tag = if form.vim_mode == VimMode::Insert {
        "[I]"
    } else {
        "[N]"
    };
    let title = format!(
        " Quota Command Preview: {}  {} ",
        form.provider_name, vim_tag
    );

    let preview_lines = preview_line_count(form).clamp(1, 12) as u16;
    let preview_height = preview_lines + 2;
    let dialog_height = 18 + preview_height;
    let area = centered_fixed(92, dialog_height, f.area());
    f.render_widget(Clear, area);

    let outer = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(t::PRIMARY))
        .title(title)
        .title_style(Style::default().fg(t::PRIMARY).add_modifier(Modifier::BOLD))
        .padding(Padding::new(1, 1, 1, 1));
    let inner = outer.inner(area);
    f.render_widget(outer, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(13),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(preview_height),
            Constraint::Length(1),
        ])
        .split(inner);

    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                "Command",
                Style::default().fg(t::PRIMARY).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" (paste in Insert mode)", Style::default().fg(t::MUTED)),
        ])),
        chunks[0],
    );

    let curl_block =
        Block::default()
            .borders(Borders::ALL)
            .border_style(if form.vim_mode == VimMode::Insert {
                Style::default().fg(t::PRIMARY)
            } else {
                Style::default().fg(t::MUTED)
            });
    let curl_inner = curl_block.inner(chunks[1]);
    f.render_widget(curl_block, chunks[1]);
    f.render_widget(
        Paragraph::new(render_editor_lines(form)).wrap(Wrap { trim: false }),
        curl_inner,
    );

    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "Response Preview",
            Style::default().fg(t::PRIMARY).add_modifier(Modifier::BOLD),
        ))),
        chunks[3],
    );

    let preview_text = if form.preview_loading {
        "[loading ...]".to_string()
    } else if let Some(err) = &form.error {
        err.clone()
    } else if let Some(preview) = &form.preview {
        preview.clone()
    } else {
        "Press 's' in Normal mode to execute the command and preview output.".to_string()
    };

    let preview_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(t::MUTED));
    let preview_inner = preview_block.inner(chunks[4]);
    f.render_widget(preview_block, chunks[4]);
    f.render_widget(
        Paragraph::new(preview_text)
            .style(Style::default().fg(t::TEXT))
            .scroll((form.preview_scroll, 0))
            .wrap(Wrap { trim: false }),
        preview_inner,
    );

    let hint = Line::from(vec![
        Span::styled("i", Style::default().fg(t::PRIMARY)),
        Span::styled(" insert  ", Style::default().fg(t::MUTED)),
        Span::styled("Ctrl+L", Style::default().fg(t::PRIMARY)),
        Span::styled(" clear  ", Style::default().fg(t::MUTED)),
        Span::styled("jk/Esc", Style::default().fg(t::PRIMARY)),
        Span::styled(" normal  ", Style::default().fg(t::MUTED)),
        Span::styled("s", Style::default().fg(t::PRIMARY)),
        Span::styled(" run preview  ", Style::default().fg(t::MUTED)),
        Span::styled("j/k", Style::default().fg(t::PRIMARY)),
        Span::styled(" scroll preview  ", Style::default().fg(t::MUTED)),
        Span::styled("q", Style::default().fg(t::PRIMARY)),
        Span::styled(" close", Style::default().fg(t::MUTED)),
    ]);
    f.render_widget(Paragraph::new(hint), chunks[5]);
}

fn handle_normal_mode(
    app: &mut App,
    code: KeyCode,
    _modifiers: KeyModifiers,
) -> crate::error::Result<()> {
    let Some(form) = app.quota_form.as_mut() else {
        return Ok(());
    };

    match code {
        KeyCode::Esc | KeyCode::Char('q') => {
            save_command(app)?;
            app.quota_form = None;
            app.mode = Mode::Normal;
        }
        KeyCode::Char('i') => {
            form.vim_mode = VimMode::Insert;
        }
        KeyCode::Char('a') => {
            form.vim_mode = VimMode::Insert;
            form.curl_field.end();
        }
        KeyCode::Char('s') => {
            run_preview(app);
        }
        KeyCode::Char('j') | KeyCode::Down => {
            let lines = preview_line_count(form);
            let max_scroll = lines.saturating_sub(1) as u16;
            form.preview_scroll = form.preview_scroll.saturating_add(1).min(max_scroll);
        }
        KeyCode::Char('k') | KeyCode::Up => {
            form.preview_scroll = form.preview_scroll.saturating_sub(1);
        }
        _ => {}
    }
    Ok(())
}

fn handle_insert_mode(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    let Some(form) = app.quota_form.as_mut() else {
        return;
    };
    let ctrl = modifiers.contains(KeyModifiers::CONTROL);

    if ctrl && code == KeyCode::Char('l') {
        form.curl_field.value.clear();
        form.curl_field.cursor = 0;
        return;
    }

    use super::input::insert::InsertKeyResult;
    match super::input::insert::handle_field_insert_key(
        &mut form.curl_field,
        code,
        ctrl,
        &mut form.pending_key,
    ) {
        InsertKeyResult::ExitInsert => {
            form.vim_mode = VimMode::Normal;
        }
        InsertKeyResult::TextChanged | InsertKeyResult::Consumed => {}
        InsertKeyResult::NotHandled => match code {
            KeyCode::Enter => form.curl_field.insert_newline(),
            KeyCode::Up => {
                let _ = form.curl_field.move_up();
            }
            KeyCode::Down => {
                let _ = form.curl_field.move_down();
            }
            _ => {}
        },
    }
}

fn save_command(app: &mut App) -> crate::error::Result<()> {
    let Some(form) = app.quota_form.as_ref() else {
        return Ok(());
    };
    let provider_name = form.provider_name.clone();
    let command = form.curl_field.value.trim().to_string();

    if let Some(provider) = app.config.providers.get_mut(&provider_name) {
        provider.quota_command = if command.is_empty() {
            None
        } else {
            Some(command)
        };
        config::save_config(&app.config)?;
    }
    Ok(())
}

fn run_preview(app: &mut App) {
    let command = app
        .quota_form
        .as_ref()
        .map(|f| f.curl_field.value.trim().to_string())
        .unwrap_or_default();
    let provider_id = app
        .quota_form
        .as_ref()
        .and_then(|f| app.config.providers.get(&f.provider_name))
        .map(|p| p.id.clone());

    if command.is_empty() {
        if let Some(form) = app.quota_form.as_mut() {
            form.error = Some("Command cannot be empty".to_string());
        }
        return;
    }

    if let Some(form) = app.quota_form.as_mut() {
        form.preview_loading = true;
        form.preview = None;
        form.error = None;
        form.preview_scroll = 0;
    }
    if let Some(provider_id) = provider_id.as_ref() {
        app.quota_status
            .insert(provider_id.clone(), super::state::QuotaStatus::Running);
    }

    if let Some(provider_id) = provider_id {
        let tx = app.tests.tx.clone();
        tokio::spawn(async move {
            let result = super::quota_command::run(&command).await;
            let _ = tx.send(super::state::TestEvent::QuotaCompleted {
                provider_id,
                result,
            });
        });
    }
}

fn render_editor_lines(form: &super::state::QuotaForm) -> Vec<Line<'static>> {
    if form.curl_field.value.is_empty() {
        return vec![Line::from(Span::styled(
            "paste shell command here...",
            Style::default().fg(t::MUTED),
        ))];
    }

    let before_cursor =
        &form.curl_field.value[..form.curl_field.cursor.min(form.curl_field.value.len())];
    let cursor_line_idx = before_cursor.chars().filter(|&c| c == '\n').count();
    let line_start = before_cursor.rfind('\n').map(|p| p + 1).unwrap_or(0);
    let cursor_col = form.curl_field.cursor.saturating_sub(line_start);

    form.curl_field
        .value
        .split('\n')
        .enumerate()
        .map(|(line_idx, line_text)| {
            if form.vim_mode == VimMode::Insert && line_idx == cursor_line_idx {
                let col = cursor_col.min(line_text.len());
                let before = &line_text[..col];
                let cursor_char = line_text[col..].chars().next().unwrap_or(' ').to_string();
                let cursor_len = if col < line_text.len() {
                    cursor_char.len()
                } else {
                    0
                };
                let after = &line_text[(col + cursor_len).min(line_text.len())..];

                Line::from(vec![
                    Span::styled(before.to_string(), Style::default().fg(t::TEXT)),
                    Span::styled(cursor_char, Style::default().bg(t::PRIMARY)),
                    Span::styled(after.to_string(), Style::default().fg(t::TEXT)),
                ])
            } else {
                Line::from(Span::styled(
                    line_text.to_string(),
                    Style::default().fg(t::TEXT),
                ))
            }
        })
        .collect()
}

fn preview_line_count(form: &super::state::QuotaForm) -> usize {
    if form.preview_loading {
        1
    } else if let Some(err) = &form.error {
        err.lines().count().max(1)
    } else if let Some(preview) = &form.preview {
        preview.lines().count().max(1)
    } else {
        1
    }
}
