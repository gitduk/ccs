use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, Padding, Paragraph, Row, Table,
};
use ratatui::Frame;

use super::app::{App, MessageKind, Mode};
use super::theme::{self as t};

pub fn draw(f: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title bar
            Constraint::Min(0),    // main content
            Constraint::Length(1), // keybindings
        ])
        .split(f.area());

    draw_title_bar(f, app, chunks[0]);
    draw_main(f, app, chunks[1]);
    draw_keybindings(f, app, chunks[2]);

    match &app.mode {
        Mode::Editing => draw_form(f, app),
        Mode::Confirm => draw_confirm(f, app),
        Mode::Message => {
            app.mode = Mode::Normal;
        }
        Mode::Normal => {}
    }
}

fn draw_title_bar(f: &mut Frame, app: &App, area: Rect) {
    let fallback_label = if app.config.fallback { "Fallback on  " } else { "Fallback off " };
    let title_left = " CCS  Claude Code Switch";
    let version = format!("  v{}", env!("CARGO_PKG_VERSION"));
    let left_len = title_left.len() + version.len();
    let right_len = fallback_label.len();
    let gap = (area.width as usize).saturating_sub(left_len + right_len);

    let spans: Vec<Span> = vec![
        Span::styled(" CCS ", Style::default().fg(Color::Black).bg(t::PRIMARY)),
        Span::raw(" "),
        Span::styled(
            "Claude Code Switch",
            Style::default().fg(t::PRIMARY).add_modifier(Modifier::BOLD),
        ),
        Span::styled(version, Style::default().fg(t::MUTED)),
        Span::raw(" ".repeat(gap)),
        Span::styled(
            fallback_label,
            if app.config.fallback {
                Style::default().fg(t::SUCCESS).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(t::MUTED)
            },
        ),
    ];
    let line = Line::from(spans);
    f.render_widget(Paragraph::new(line), area);
}

fn draw_main(f: &mut Frame, app: &mut App, area: Rect) {
    let table_height = (app.provider_ids.len() as u16 + 3).max(4).min(area.height * 2 / 3);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(table_height),
            Constraint::Min(0),
        ])
        .split(area);

    draw_provider_table(f, app, chunks[0]);
    draw_detail_panel(f, app, chunks[1]);
}

fn draw_provider_table(f: &mut Frame, app: &mut App, area: Rect) {
    if app.provider_ids.is_empty() {
        let empty = Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled(
                "  No providers configured",
                Style::default().fg(t::MUTED),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled("  Press ", Style::default().fg(t::MUTED)),
                Span::styled("a", Style::default().fg(t::WARNING).add_modifier(Modifier::BOLD)),
                Span::styled(" to add a provider, or edit ", Style::default().fg(t::MUTED)),
                Span::styled(
                    config_path_display(),
                    Style::default().fg(t::PRIMARY),
                ),
            ]),
        ])
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(t::MUTED)),
        );
        f.render_widget(empty, area);
        return;
    }

    let header = Row::new(vec![
        Cell::from("  ID").style(Style::default().fg(t::MUTED).add_modifier(Modifier::BOLD)),
        Cell::from("Format").style(Style::default().fg(t::MUTED).add_modifier(Modifier::BOLD)),
        Cell::from("Base URL").style(Style::default().fg(t::MUTED).add_modifier(Modifier::BOLD)),
    ])
    .height(1);

    let rows: Vec<Row> = app
        .provider_ids
        .iter()
        .map(|id| {
            let provider = &app.config.providers[id];
            let is_current = id == &app.config.current;

            let id_cell = if is_current {
                Cell::from(Line::from(vec![
                    Span::styled("▶ ", Style::default().fg(t::SUCCESS)),
                    Span::styled(id.as_str(), Style::default().fg(t::SUCCESS).add_modifier(Modifier::BOLD)),
                ]))
            } else {
                Cell::from(Span::styled(
                    format!("  {}", id),
                    Style::default().fg(t::TEXT),
                ))
            };

            let format_color = t::format_color(&provider.api_format);

            Row::new(vec![
                id_cell,
                Cell::from(Span::styled(
                    provider.api_format.to_string(),
                    Style::default().fg(format_color),
                )),
                Cell::from(Span::styled(
                    provider.base_url.as_str(),
                    Style::default().fg(t::MUTED),
                )),
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(14),
            Constraint::Length(12),
            Constraint::Min(30),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(t::MUTED)),
    )
    .row_highlight_style(Style::default().bg(t::HIGHLIGHT_BG));

    f.render_stateful_widget(table, area, &mut app.table_state);
}

fn draw_detail_panel(f: &mut Frame, app: &App, area: Rect) {
    // Show error toast only when not in editing mode (errors in form are shown inline)
    if app.mode == Mode::Normal {
        if let Some((msg, MessageKind::Error, _)) = &app.message {
            let toast = Paragraph::new(Line::from(vec![
                Span::styled(" ✗ ", Style::default().fg(t::ERROR).add_modifier(Modifier::BOLD)),
                Span::styled(msg.as_str(), Style::default().fg(t::TEXT)),
            ]))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(t::ERROR)),
            );
            f.render_widget(toast, area);
            return;
        }
    }

    let Some(id) = app
        .table_state
        .selected()
        .and_then(|i| app.provider_ids.get(i))
    else {
        f.render_widget(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(t::MUTED)),
            area,
        );
        return;
    };

    let Some(provider) = app.config.providers.get(id) else {
        return;
    };

    let title = " Provider ";

    let api_key_display = if provider.api_key.is_empty() {
        Span::styled("(not set)", Style::default().fg(t::MUTED))
    } else if provider.api_key.starts_with('$') {
        Span::styled(provider.api_key.as_str(), Style::default().fg(t::WARNING))
    } else {
        let len = provider.api_key.len();
        let masked = if len > 8 {
            format!("{}···{}", &provider.api_key[..4], &provider.api_key[len - 4..])
        } else {
            "····".to_string()
        };
        Span::styled(masked, Style::default().fg(t::MUTED))
    };

    let format_color = t::format_color(&provider.api_format);

    let label_style = Style::default().fg(t::MUTED);
    let value_style = Style::default().fg(t::TEXT);

    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("  URL      ", label_style),
            Span::styled(provider.base_url.as_str(), value_style),
        ]),
        Line::from(vec![
            Span::styled("  Format   ", label_style),
            Span::styled(provider.api_format.to_string(), Style::default().fg(format_color)),
        ]),
        Line::from(vec![
            Span::styled("  API Key  ", label_style),
            api_key_display,
        ]),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(t::MUTED))
        .title(title)
        .title_style(Style::default().fg(t::MUTED));

    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_keybindings(f: &mut Frame, app: &App, area: Rect) {
    let fallback_key_color = if app.config.fallback { t::SUCCESS } else { t::PRIMARY };
    let mut spans: Vec<Span> = vec![Span::raw(" ")];
    let keys: &[(&str, &str, Color)] = &[
        ("s", "Switch", t::PRIMARY),
        ("a", "Add", t::PRIMARY),
        ("e", "Edit", t::PRIMARY),
        ("d", "Delete", t::PRIMARY),
        ("t", "Test", t::PRIMARY),
        ("J/K", "Move", t::PRIMARY),
        ("f", "Failover", fallback_key_color),
        ("r", "Reload", t::PRIMARY),
        ("q", "Quit", t::PRIMARY),
    ];
    for (i, (key, desc, color)) in keys.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(
            format!("[{}]", key),
            Style::default().fg(*color),
        ));
        spans.push(Span::styled(
            format!(" {}", desc),
            Style::default().fg(t::MUTED),
        ));
    }

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_form(f: &mut Frame, app: &App) {
    let Some(form) = &app.form else { return };

    let title = if form.is_new {
        " Add Provider "
    } else {
        " Edit Provider "
    };

    // Fixed height: fields*2 + hint(3) + borders(2) + padding(2)
    let dialog_height = (form.fields.len() as u16) * 2 + 3 + 2 + 2;
    let area = centered_fixed(60, dialog_height, f.area());

    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(t::PRIMARY))
        .title(title)
        .title_style(Style::default().fg(t::PRIMARY).add_modifier(Modifier::BOLD))
        .padding(Padding::new(2, 2, 1, 1));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let field_constraints: Vec<Constraint> = form
        .fields
        .iter()
        .map(|_| Constraint::Length(2))
        .chain(std::iter::once(Constraint::Length(3)))
        .collect();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(field_constraints)
        .split(inner);

    for (i, field) in form.fields.iter().enumerate() {
        let is_focused = i == form.focused;
        let label_style = if is_focused {
            Style::default().fg(t::PRIMARY).add_modifier(Modifier::BOLD)
        } else if !field.editable {
            Style::default().fg(t::MUTED)
        } else {
            Style::default().fg(t::TEXT)
        };

        let value_display = if field.is_toggle {
            let (left, right) = if field.value == "anthropic" {
                (
                    Span::styled(" anthropic ", Style::default().fg(t::format_color(&crate::config::ApiFormat::Anthropic)).add_modifier(Modifier::REVERSED)),
                    Span::styled(" openai ", Style::default().fg(t::MUTED)),
                )
            } else {
                (
                    Span::styled(" anthropic ", Style::default().fg(t::MUTED)),
                    Span::styled(" openai ", Style::default().fg(t::format_color(&crate::config::ApiFormat::OpenAI)).add_modifier(Modifier::REVERSED)),
                )
            };
            Line::from(vec![
                Span::styled(format!("{:<10}", field.label), label_style),
                left,
                Span::raw(" "),
                right,
            ])
        } else {
            let display_val = if field.label == "API Key"
                && !field.value.is_empty()
                && !field.value.starts_with('$')
                && !is_focused
            {
                let len = field.value.len();
                if len > 8 {
                    format!("{}...{}", &field.value[..4], &field.value[len - 4..])
                } else {
                    "****".to_string()
                }
            } else {
                field.value.clone()
            };

            if is_focused && field.editable {
                let cursor_pos = field.cursor.min(display_val.len());
                let before = display_val[..cursor_pos].to_string();
                let cursor_char = display_val[cursor_pos..].chars().next().unwrap_or(' ');
                let after_start =
                    cursor_pos + cursor_char.len_utf8().min(display_val.len() - cursor_pos);
                let after = display_val[after_start..].to_string();
                Line::from(vec![
                    Span::styled(format!("{:<10}", field.label), label_style),
                    Span::raw(before),
                    Span::styled(
                        cursor_char.to_string(),
                        Style::default().add_modifier(Modifier::REVERSED),
                    ),
                    Span::raw(after),
                ])
            } else {
                let val_style = if !field.editable {
                    Style::default().fg(t::MUTED)
                } else {
                    Style::default()
                };
                Line::from(vec![
                    Span::styled(format!("{:<10}", field.label), label_style),
                    Span::styled(display_val, val_style),
                ])
            }
        };

        f.render_widget(Paragraph::new(value_display), chunks[i]);
    }

    let hint_idx = form.fields.len();
    if hint_idx < chunks.len() {
        let mut hint_lines = vec![
            Line::from(vec![
                Span::raw("          "),
                Span::styled("Enter", Style::default().fg(t::SUCCESS)),
                Span::styled(" Save  ", Style::default().fg(t::MUTED)),
                Span::styled("Esc", Style::default().fg(t::WARNING)),
                Span::styled(" Cancel", Style::default().fg(t::MUTED)),
            ]),
            Line::from(""),
        ];
        if let Some(err) = &form.error {
            hint_lines.push(
                Line::from(Span::styled(format!("✗ {}", err), Style::default().fg(t::ERROR))),
            );
        }
        f.render_widget(Paragraph::new(hint_lines), chunks[hint_idx]);
    }
}

fn draw_confirm(f: &mut Frame, app: &App) {
    let area = centered_rect(40, 20, f.area());
    let area = Rect {
        height: area.height.max(5),
        ..area
    };

    f.render_widget(Clear, area);

    let id = app.confirm_action.as_deref().unwrap_or("?");
    let text = vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("  Delete "),
            Span::styled(id, Style::default().fg(t::ERROR).add_modifier(Modifier::BOLD)),
            Span::raw(" ?"),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("y", Style::default().fg(t::SUCCESS)),
            Span::styled(" Yes    ", Style::default().fg(t::MUTED)),
            Span::styled("n", Style::default().fg(t::WARNING)),
            Span::styled(" No", Style::default().fg(t::MUTED)),
        ]),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(t::ERROR))
        .title(" Confirm ")
        .title_style(Style::default().fg(t::ERROR).add_modifier(Modifier::BOLD));

    f.render_widget(Paragraph::new(text).block(block), area);
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

fn centered_fixed(percent_x: u16, height: u16, r: Rect) -> Rect {
    let height = height.min(r.height);
    let v_margin = (r.height - height) / 2;
    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(v_margin),
            Constraint::Length(height),
            Constraint::Min(0),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vert[1])[1]
}

fn config_path_display() -> String {
    crate::config::config_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "~/.ccs/config.json".to_string())
}
