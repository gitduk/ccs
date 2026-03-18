use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Padding, Paragraph, Row, Table};
use ratatui::Frame;

use super::app::{App, MessageKind, Mode};
use crate::test_provider::TestStatus;
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
        Mode::Help => draw_help(f, app),
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
    let table_height = (app.provider_ids.len() as u16 + 2).max(3).min(area.height * 2 / 3);
    // Detail panel: no top/bottom borders, 1 blank + 1 title + 1 info line = 3 lines fixed height
    let detail_height = 3u16;
    // Stats panel: 1 bottom border + 1 blank + 1 title + N provider rows minimum
    let n_providers = app.provider_ids.len() as u16;
    let stats_min_height = 3 + n_providers;
    let leftover = area.height.saturating_sub(table_height + detail_height);
    let stats_height = if leftover >= stats_min_height { stats_min_height } else { 0 };

    let mut constraints = vec![
        Constraint::Length(table_height),
        Constraint::Length(detail_height),
    ];
    if stats_height > 0 {
        constraints.push(Constraint::Length(stats_height));
    }
    constraints.push(Constraint::Min(0));

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    draw_provider_table(f, app, chunks[0]);
    draw_detail_panel(f, app, chunks[1]);
    if stats_height > 0 {
        draw_stats_panel(f, app, chunks[2]);
    }
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
                .borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
                .border_style(Style::default().fg(t::MUTED)),
        );
        f.render_widget(empty, area);
        return;
    }

    let url_col = col_width("Base URL", app.config.providers.values().map(|p| p.base_url.len()));
    let key_col = col_width("API Key",  app.config.providers.values().map(|p| api_key_display_len(&p.api_key)));

    let header = Row::new(vec![
        Cell::from("ID").style(Style::default().fg(t::MUTED).add_modifier(Modifier::BOLD)),
        Cell::from("Format").style(Style::default().fg(t::MUTED).add_modifier(Modifier::BOLD)),
        Cell::from("Base URL").style(Style::default().fg(t::MUTED).add_modifier(Modifier::BOLD)),
        Cell::from("API Key").style(Style::default().fg(t::MUTED).add_modifier(Modifier::BOLD)),
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
                    Span::styled(id.as_str(), Style::default().fg(t::SUCCESS).add_modifier(Modifier::BOLD)),
                    Span::styled(" ▲", Style::default().fg(t::SUCCESS)),
                ]))
            } else {
                Cell::from(Span::styled(id.as_str(), Style::default().fg(t::TEXT)))
            };

            let format_color = t::format_color(&provider.api_format);
            let api_key_cell = masked_api_key(&provider.api_key);

            Row::new(vec![
                id_cell,
                Cell::from(Span::styled(provider.api_format.to_string(), Style::default().fg(format_color))),
                Cell::from(Span::styled(provider.base_url.as_str(), Style::default().fg(t::MUTED))),
                api_key_cell,
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(14),
            Constraint::Length(12),
            Constraint::Length(url_col),
            Constraint::Length(key_col),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
            .border_style(Style::default().fg(t::MUTED))
            .padding(Padding::horizontal(1)),
    )
    .row_highlight_style(Style::default().bg(t::HIGHLIGHT_BG));

    f.render_stateful_widget(table, area, &mut app.table_state);
}

fn draw_detail_panel(f: &mut Frame, app: &App, area: Rect) {
    let border_style = Style::default().fg(t::MUTED);
    let block = Block::default()
        .borders(Borders::LEFT | Borders::RIGHT)
        .border_style(border_style)
        .padding(Padding::horizontal(1));

    // Show error toast only when not in editing mode (errors in form are shown inline)
    if app.mode == Mode::Normal {
        if let Some((msg, MessageKind::Error, _)) = &app.message {
            let error_block = Block::default()
                .borders(Borders::LEFT | Borders::RIGHT)
                .border_style(Style::default().fg(t::ERROR))
                .padding(Padding::horizontal(1));
            let lines = vec![
                Line::from(""),
                Line::from(Span::styled("Info", Style::default().fg(t::ERROR).add_modifier(Modifier::BOLD))),
                Line::from(vec![
                    Span::styled("✗ ", Style::default().fg(t::ERROR).add_modifier(Modifier::BOLD)),
                    Span::styled(msg.as_str(), Style::default().fg(t::TEXT)),
                ]),
            ];
            f.render_widget(Paragraph::new(lines).block(error_block), area);
            return;
        }
    }

    let label = Style::default().fg(t::MUTED);
    let title_line = Line::from(Span::styled("Info", Style::default().fg(t::MUTED).add_modifier(Modifier::BOLD)));

    let Some(id) = app
        .table_state
        .selected()
        .and_then(|i| app.provider_ids.get(i))
    else {
        f.render_widget(Paragraph::new(vec![Line::from(""), title_line]).block(block), area);
        return;
    };

    let mut lines = vec![Line::from(""), title_line];
    if app.pending_tests.contains(id.as_str()) {
        let prev = app.test_results.get(id.as_str());
        let latency_str = prev.map(|r| fmt_latency(r.latency_ms)).unwrap_or_else(|| "—".to_string());
        let models_str = prev.and_then(|r| r.model_count).map(|n| format!("{n} models")).unwrap_or_else(|| "—".to_string());
        lines.push(Line::from(vec![
            Span::styled("Status ", label),
            Span::styled("Testing", Style::default().fg(t::MUTED).add_modifier(Modifier::ITALIC)),
            Span::styled("   Latency ", label),
            Span::styled(latency_str, Style::default().fg(t::MUTED)),
            Span::styled("   Models ", label),
            Span::styled(models_str, Style::default().fg(t::MUTED)),
        ]));
    } else if let Some(r) = app.test_results.get(id.as_str()) {
        let (status_str, status_style) = match &r.status {
            TestStatus::Ok => ("✓ OK", Style::default().fg(t::SUCCESS).add_modifier(Modifier::BOLD)),
            TestStatus::AuthFailed => ("✗ Auth failed", Style::default().fg(t::ERROR).add_modifier(Modifier::BOLD)),
            TestStatus::Error(e) => (e.as_str(), Style::default().fg(t::ERROR)),
        };
        let models_str = match r.model_count {
            Some(n) => Span::styled(format!("{n} models"), Style::default().fg(t::TEXT)),
            None    => Span::styled("—", Style::default().fg(t::MUTED)),
        };
        lines.push(Line::from(vec![
            Span::styled("Status ", label),
            Span::styled(status_str, status_style),
            Span::styled("   Latency ", label),
            Span::styled(fmt_latency(r.latency_ms), Style::default().fg(t::TEXT)),
            Span::styled("   Models ", label),
            models_str,
        ]));
    } else {
        lines.push(Line::from(vec![
            Span::styled("Press ", Style::default().fg(t::MUTED)),
            Span::styled("[t]", Style::default().fg(t::PRIMARY).add_modifier(Modifier::BOLD)),
            Span::styled(" to test connectivity", Style::default().fg(t::MUTED)),
        ]));
    }

    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_keybindings(f: &mut Frame, app: &App, area: Rect) {
    let fallback_key_color = if app.config.fallback { t::SUCCESS } else { t::PRIMARY };
    let mut spans: Vec<Span> = vec![Span::raw(" ")];
    // Show only the most common shortcuts; press h to see all
    let keys: &[(&str, &str, Color)] = &[
        ("s", "Switch", t::PRIMARY),
        ("a", "Add", t::PRIMARY),
        ("e", "Edit", t::PRIMARY),
        ("f", "Fallback", fallback_key_color),
        ("q", "Quit", t::PRIMARY),
        ("h", "Help", t::MUTED),
    ];
    for (i, (key, desc, color)) in keys.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
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

fn draw_stats_panel(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::LEFT | Borders::RIGHT | Borders::BOTTOM)
        .border_style(Style::default().fg(t::MUTED))
        .padding(Padding::horizontal(1));

    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.height == 0 {
        return;
    }

    let Ok(m) = app.metrics.lock() else { return };
    let by_provider = m.by_provider.clone();
    drop(m);

    let muted = Style::default().fg(t::MUTED);
    let id_col_width = app.provider_ids.iter().map(|s| s.len()).max().unwrap_or(8).max(8);

    let mut lines: Vec<Line> = vec![
        Line::from(""),
        Line::from(Span::styled("Token Usage", Style::default().fg(t::MUTED).add_modifier(Modifier::BOLD))),
    ];
    lines.extend(app
        .provider_ids
        .iter()
        .enumerate()
        .map(|(i, id)| {
            let color = t::provider_color(i);
            let s = by_provider.get(id).cloned().unwrap_or_default();
            Line::from(vec![
                Span::styled(
                    format!("{:<width$}", id, width = id_col_width),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled("  In ",       muted),
                Span::styled(format!("{:>7}", format_tokens(s.input)),  Style::default().fg(color)),
                Span::styled("  Out ",      muted),
                Span::styled(format!("{:>7}", format_tokens(s.output)), Style::default().fg(color)),
                Span::styled("  Req ", muted),
                Span::styled(format!("{:>4}", s.requests), Style::default().fg(t::TEXT).add_modifier(Modifier::BOLD)),
                Span::styled("  Fail ", muted),
                Span::styled(
                    format!("{:>4}", s.failures),
                    if s.failures > 0 {
                        Style::default().fg(t::ERROR).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(t::MUTED)
                    },
                ),
                {
                    let rate = if s.failures > 0 && s.requests > 0 {
                        format!(" ({:.0}%)", s.failures as f64 / s.requests as f64 * 100.0)
                    } else {
                        String::new()
                    };
                    Span::styled(rate, Style::default().fg(t::ERROR))
                },
            ])
        }));

    f.render_widget(Paragraph::new(lines), inner);
}

fn fmt_latency(ms: u64) -> String {
    if ms >= 1000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{ms}ms")
    }
}

fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        format!("{n}")
    }
}

/// Column width = max(header length, max content length) + 4 gap.
fn col_width(header: &str, content_lens: impl Iterator<Item = usize>) -> u16 {
    (content_lens.max().unwrap_or(0).max(header.len()) + 4) as u16
}

fn api_key_display_len(key: &str) -> usize {
    if key.is_empty() {
        "(not set)".len()
    } else if key.starts_with('$') {
        key.len()
    } else if key.len() > 8 {
        11 // "abcd···wxyz"
    } else {
        4  // "····"
    }
}

fn masked_api_key(key: &str) -> Cell<'static> {
    if key.is_empty() {
        Cell::from(Span::styled("(not set)", Style::default().fg(t::MUTED)))
    } else if key.starts_with('$') {
        Cell::from(Span::styled(key.to_string(), Style::default().fg(t::WARNING)))
    } else {
        let len = key.len();
        let masked = if len > 8 {
            format!("{}···{}", &key[..4], &key[len - 4..])
        } else {
            "····".to_string()
        };
        Cell::from(Span::styled(masked, Style::default().fg(t::MUTED)))
    }
}

fn draw_help(f: &mut Frame, _app: &App) {
    let entries: &[(&str, &str)] = &[
        ("s", "Switch to selected provider"),
        ("a", "Add new provider"),
        ("e", "Edit selected provider"),
        ("d", "Delete selected provider"),
        ("t", "Test provider connectivity"),
        ("J / K", "Move provider down / up"),
        ("j / k", "Select next / previous"),
        ("↑ / ↓", "Select next / previous"),
        ("f", "Toggle fallback mode"),
        ("r", "Reload config from disk"),
        ("q / Esc", "Quit"),
        ("h / ?", "Show this help"),
    ];

    let width: u16 = 50;
    let height: u16 = entries.len() as u16 + 4; // entries + border + title + footer
    let area = centered_fixed(
        (width * 100 / f.area().width.max(1)).min(80),
        height,
        f.area(),
    );

    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(t::PRIMARY))
        .title(" Help ")
        .title_style(Style::default().fg(t::PRIMARY).add_modifier(Modifier::BOLD))
        .padding(Padding::new(2, 2, 1, 1));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line> = entries
        .iter()
        .map(|(key, desc)| {
            Line::from(vec![
                Span::styled(format!("{:<10}", key), Style::default().fg(t::PRIMARY).add_modifier(Modifier::BOLD)),
                Span::styled(*desc, Style::default().fg(t::TEXT)),
            ])
        })
        .collect();

    // Footer hint
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("Press any key to close", Style::default().fg(t::MUTED)),
    ]));

    f.render_widget(Paragraph::new(lines), inner);
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
