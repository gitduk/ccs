use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, Padding, Paragraph, Row, Table,
};
use ratatui::Frame;

use super::app::{App, Mode, ServerStatus};

const CYAN: Color = Color::Cyan;
const GREEN: Color = Color::Green;
const YELLOW: Color = Color::Yellow;
const RED: Color = Color::Red;
const DIM: Color = Color::DarkGray;

pub fn draw(f: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title + proxy status
            Constraint::Min(6),   // main content
            Constraint::Length(1), // keybindings
        ])
        .split(f.area());

    draw_title_bar(f, app, chunks[0]);
    draw_provider_table(f, app, chunks[1]);
    draw_keybindings(f, app, chunks[2]);

    // Draw overlays
    match &app.mode {
        Mode::Editing => draw_form(f, app),
        Mode::Confirm => draw_confirm(f, app),
        Mode::Message => {
            // Dismiss on next tick
            app.mode = Mode::Normal;
        }
        Mode::Normal => {}
    }
}

fn draw_title_bar(f: &mut Frame, app: &App, area: Rect) {
    let (indicator, status_text, color) = match &app.server_status {
        ServerStatus::Stopped => ("○", "Stopped", DIM),
        ServerStatus::Starting => ("◌", "Starting...", YELLOW),
        ServerStatus::Running => ("●", "Running", GREEN),
        ServerStatus::Error(_) => ("✗", "Error", RED),
    };

    let status_part = format!("{indicator} {status_text} ");

    // Left side: title; right side: proxy status
    // Calculate padding to right-align the status
    let title_left = " CCS  Claude Code Switch";
    let version = format!("  v{}", env!("CARGO_PKG_VERSION"));
    let left_len = title_left.len() + version.len();
    let right_len = status_part.len();
    let gap = (area.width as usize).saturating_sub(left_len + right_len);

    let line = Line::from(vec![
        Span::styled(" CCS ", Style::default().fg(Color::Black).bg(CYAN)),
        Span::raw(" "),
        Span::styled(
            "Claude Code Switch",
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
        ),
        Span::styled(version, Style::default().fg(DIM)),
        Span::raw(" ".repeat(gap)),
        Span::styled(
            status_part,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn draw_provider_table(f: &mut Frame, app: &mut App, area: Rect) {
    if app.provider_ids.is_empty() {
        let empty = Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled(
                "  No providers configured",
                Style::default().fg(DIM),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled("  Press ", Style::default().fg(DIM)),
                Span::styled("a", Style::default().fg(YELLOW).add_modifier(Modifier::BOLD)),
                Span::styled(" to add a provider, or edit ", Style::default().fg(DIM)),
                Span::styled(
                    config_path_display(),
                    Style::default().fg(CYAN),
                ),
            ]),
        ])
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(DIM))
                .title(" Providers "),
        );
        f.render_widget(empty, area);
        return;
    }

    let header = Row::new(vec![
        Cell::from("  "),
        Cell::from("ID"),
        Cell::from("Format"),
        Cell::from("Base URL"),
    ])
    .style(Style::default().fg(DIM))
    .height(1);

    // 256-color palette for subtle highlight bar
    const HIGHLIGHT_BG: Color = Color::Indexed(236); // dark gray

    let rows: Vec<Row> = app
        .provider_ids
        .iter()
        .map(|id| {
            let provider = &app.config.providers[id];
            let is_current = id == &app.config.current;

            let id_style = if is_current {
                Style::default().fg(GREEN)
            } else {
                Style::default()
            };
            let marker = if is_current {
                Span::styled(" ● ", Style::default().fg(GREEN))
            } else {
                Span::raw("   ")
            };
            let format_color = if provider.api_format == crate::config::ApiFormat::OpenAI {
                YELLOW
            } else {
                CYAN
            };

            Row::new(vec![
                Cell::from(marker),
                Cell::from(Span::styled(id.as_str(), id_style)),
                Cell::from(Span::styled(provider.api_format.to_string(), Style::default().fg(format_color))),
                Cell::from(Span::styled(provider.base_url.as_str(), Style::default().fg(DIM))),
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(3),
            Constraint::Percentage(25),
            Constraint::Length(12),
            Constraint::Percentage(55),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(DIM))
            .title(" Providers ")
            .title_style(Style::default().fg(CYAN)),
    )
    .row_highlight_style(Style::default().bg(HIGHLIGHT_BG));

    f.render_stateful_widget(table, area, &mut app.table_state);
}

fn draw_keybindings(f: &mut Frame, app: &App, area: Rect) {
    let proxy_label = match &app.server_status {
        ServerStatus::Running => "Stop",
        _ => "Start",
    };
    let hints = Line::from(vec![
        key_hint("↑↓", "Navigate"),
        Span::raw("  "),
        key_hint("s", "Switch"),
        Span::raw("  "),
        key_hint("a", "Add"),
        Span::raw("  "),
        key_hint("e", "Edit"),
        Span::raw("  "),
        key_hint("d", "Delete"),
        Span::raw("  "),
        key_hint("t", "Test"),
        Span::raw("  "),
        key_hint("p", proxy_label),
        Span::raw("  "),
        key_hint("q", "Quit"),
    ]);
    f.render_widget(Paragraph::new(hints).style(Style::default().fg(DIM)), area);
}

fn key_hint<'a>(key: &'a str, desc: &'a str) -> Span<'a> {
    Span::styled(
        format!(" {key} {desc} "),
        Style::default().fg(Color::White),
    )
}

fn draw_form(f: &mut Frame, app: &App) {
    let Some(form) = &app.form else { return };

    let title = if form.is_new {
        " Add Provider "
    } else {
        " Edit Provider "
    };

    let area = centered_rect(60, 60, f.area());
    // Ensure minimum height
    let area = if area.height < 12 {
        centered_rect(60, 90, f.area())
    } else {
        area
    };

    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(CYAN))
        .title(title)
        .title_style(Style::default().fg(CYAN).add_modifier(Modifier::BOLD))
        .padding(Padding::new(2, 2, 1, 1));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let field_constraints: Vec<Constraint> = form
        .fields
        .iter()
        .map(|_| Constraint::Length(2))
        .chain(std::iter::once(Constraint::Length(2))) // save/cancel hint
        .chain(std::iter::once(Constraint::Min(0)))
        .collect();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(field_constraints)
        .split(inner);

    for (i, field) in form.fields.iter().enumerate() {
        let is_focused = i == form.focused;
        let label_style = if is_focused {
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD)
        } else if !field.editable {
            Style::default().fg(DIM)
        } else {
            Style::default().fg(Color::White)
        };

        let value_display = if field.is_toggle {
            let (left, right) = if field.value == "anthropic" {
                (
                    Span::styled(" anthropic ", Style::default().fg(CYAN).add_modifier(Modifier::REVERSED)),
                    Span::styled(" openai ", Style::default().fg(DIM)),
                )
            } else {
                (
                    Span::styled(" anthropic ", Style::default().fg(DIM)),
                    Span::styled(" openai ", Style::default().fg(YELLOW).add_modifier(Modifier::REVERSED)),
                )
            };
            Line::from(vec![
                Span::styled(format!("{:<10}", field.label), label_style),
                left,
                Span::raw(" "),
                right,
                if is_focused {
                    Span::styled("  ←→/Space toggle", Style::default().fg(DIM))
                } else {
                    Span::raw("")
                },
            ])
        } else {
            let display_val = if field.label == "API Key"
                && !field.value.is_empty()
                && !field.value.starts_with('$')
                && !is_focused
            {
                // Mask API key when not focused
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
                // Show cursor
                let cursor_pos = field.cursor.min(display_val.len());
                let before = display_val[..cursor_pos].to_string();
                let cursor_char = display_val[cursor_pos..]
                    .chars()
                    .next()
                    .unwrap_or(' ');
                let after_start = cursor_pos + cursor_char.len_utf8().min(display_val.len() - cursor_pos);
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
                    Style::default().fg(DIM)
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

    // Save/Cancel hint
    let hint_idx = form.fields.len();
    if hint_idx < chunks.len() {
        let hints = Line::from(vec![
            Span::raw("          "),
            Span::styled("Enter", Style::default().fg(GREEN)),
            Span::styled(" Save  ", Style::default().fg(DIM)),
            Span::styled("Esc", Style::default().fg(YELLOW)),
            Span::styled(" Cancel", Style::default().fg(DIM)),
        ]);
        f.render_widget(Paragraph::new(hints), chunks[hint_idx]);
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
            Span::styled(id, Style::default().fg(RED).add_modifier(Modifier::BOLD)),
            Span::raw(" ?"),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("y", Style::default().fg(GREEN)),
            Span::styled(" Yes    ", Style::default().fg(DIM)),
            Span::styled("n", Style::default().fg(YELLOW)),
            Span::styled(" No", Style::default().fg(DIM)),
        ]),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(RED))
        .title(" Confirm ")
        .title_style(Style::default().fg(RED).add_modifier(Modifier::BOLD));

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

fn config_path_display() -> String {
    crate::config::config_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "~/.ccs/config.json".to_string())
}
