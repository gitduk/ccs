//! Request log popup and embedded log side panel.
//!
//! This module renders both the request log viewer and the right-column
//! logs/messages panel on the main screen.

use crossterm::event::KeyCode;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::state::{App, MessageKind, Mode};
use super::theme::{self as t};
use super::ui::format::{fmt_latency, format_tokens, shorten_model_name};
use super::ui::layout::{BORDER_INNER_DIVIDER, DASH, TITLE_SIDE, centered_rect};
use crate::proxy::metrics::RequestLogEntry;

pub(super) fn handle_key(app: &mut App, code: KeyCode) -> crate::error::Result<()> {
    let total = app
        .request_log
        .lock()
        .map(|l| l.entries().len())
        .unwrap_or(0);

    match code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.mode = Mode::Normal;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if app.logs.selected > 0 {
                app.logs.selected -= 1;
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if total > 0 && app.logs.selected < total - 1 {
                app.logs.selected += 1;
            }
        }
        KeyCode::Char('G') => {
            if total > 0 {
                app.logs.selected = total - 1;
            }
        }
        KeyCode::Char('g') => {
            app.logs.selected = 0;
        }
        _ => {}
    }
    Ok(())
}

pub(super) fn draw_popup(f: &mut Frame, app: &mut App) {
    let area = centered_rect(90, 80, f.area());
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(t::PRIMARY))
        .title(" Request Log ")
        .title_style(Style::default().fg(t::TEXT).add_modifier(Modifier::BOLD))
        .padding(Padding::new(1, 1, 0, 0));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let entries = match app.request_log.lock() {
        Ok(log) => log.entries().iter().rev().cloned().collect::<Vec<_>>(),
        Err(_) => vec![],
    };

    if entries.is_empty() {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  No requests yet",
                Style::default().fg(t::MUTED),
            ))),
            inner,
        );
        return;
    }

    let total = entries.len();
    let selected = app.logs.selected.min(total.saturating_sub(1));
    app.logs.selected = selected;

    let viewport_height = inner.height.saturating_sub(1) as usize;
    let scroll = if selected < app.logs.scroll as usize {
        selected
    } else if selected >= app.logs.scroll as usize + viewport_height {
        selected + 1 - viewport_height
    } else {
        app.logs.scroll as usize
    };
    app.logs.scroll = scroll as u16;

    // Require at least 10 cols for the detail pane; fall back to list-only below that.
    const LIST_WIDTH: u16 = 37;
    if inner.width <= LIST_WIDTH + 10 {
        render_log_list(f, &entries, selected, scroll, inner);
        let footer = format!(" {} requests  [j/k] navigate  [q/Esc] close", total);
        draw_footer(f, area, &footer);
        return;
    }
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(LIST_WIDTH), Constraint::Min(0)])
        .split(inner);
    let list_area = chunks[0];
    let detail_area = chunks[1];

    render_log_list(f, &entries, selected, scroll, list_area);

    let detail_block = Block::default()
        .borders(Borders::LEFT)
        .border_style(Style::default().fg(t::MUTED))
        .padding(Padding::new(1, 0, 0, 0));
    let detail_inner = detail_block.inner(detail_area);
    f.render_widget(detail_block, detail_area);
    draw_detail(f, &entries[selected], detail_inner);

    let footer = format!(" {} requests  [j/k] navigate  [q/Esc] close", total);
    draw_footer(f, area, &footer);
}

fn render_log_list(
    f: &mut Frame,
    entries: &[RequestLogEntry],
    selected: usize,
    scroll: usize,
    area: Rect,
) {
    let viewport_height = area.height.saturating_sub(1) as usize;
    let hdr = Style::default().fg(t::MUTED).add_modifier(Modifier::BOLD);
    let mut lines: Vec<Line> = vec![Line::from(vec![
        Span::styled(format!("{:<11}", "Response"), hdr),
        Span::styled(format!("{:<22}", "Provider/Model"), hdr),
        Span::styled("Age", hdr),
    ])];
    for (i, entry) in entries
        .iter()
        .enumerate()
        .skip(scroll)
        .take(viewport_height)
    {
        let is_selected = i == selected;
        let base_status = if entry.error.is_some() || entry.status >= 400 {
            Style::default().fg(t::ERROR)
        } else {
            Style::default().fg(t::SUCCESS)
        };
        let (status_style, row_style) = if is_selected {
            let rev = Modifier::REVERSED;
            (
                base_status.add_modifier(rev),
                Style::default().add_modifier(rev),
            )
        } else {
            (base_status, Style::default())
        };
        let response = truncate(
            &format!("{} {}", entry.status, fmt_latency(entry.latency_ms)),
            10,
        );
        let pm = truncate(
            &format!("{}/{}", entry.provider, shorten_model_name(&entry.model)),
            21,
        );
        lines.push(Line::from(vec![
            Span::styled(format!("{:<11}", response), status_style),
            Span::styled(format!("{:<22}", pm), row_style.fg(t::TEXT)),
            Span::styled(format_time(entry.timestamp), row_style.fg(t::MUTED)),
        ]));
    }
    f.render_widget(Paragraph::new(lines), area);
}

fn draw_detail(f: &mut Frame, entry: &RequestLogEntry, area: Rect) {
    let label = Style::default().fg(t::MUTED);
    let value = Style::default().fg(t::TEXT);
    let status_style = if entry.error.is_some() || entry.status >= 400 {
        Style::default().fg(t::ERROR)
    } else {
        Style::default().fg(t::SUCCESS)
    };

    let mut lines: Vec<Line> = vec![
        Line::from(vec![
            Span::styled(format!("{:<12}", "Provider"), label),
            Span::styled(entry.provider.clone(), value),
        ]),
        Line::from(vec![
            Span::styled(format!("{:<12}", "Model"), label),
            Span::styled(entry.model.clone(), value),
        ]),
        Line::from(vec![
            Span::styled(format!("{:<12}", "Status"), label),
            Span::styled(format!("{}", entry.status), status_style),
        ]),
        Line::from(vec![
            Span::styled(format!("{:<12}", "Latency"), label),
            Span::styled(fmt_latency(entry.latency_ms), value),
        ]),
        Line::from(vec![
            Span::styled(format!("{:<12}", "Tokens In"), label),
            Span::styled(format!("{}", entry.input_tokens), value),
        ]),
        Line::from(vec![
            Span::styled(format!("{:<12}", "Tokens Out"), label),
            Span::styled(format!("{}", entry.output_tokens), value),
        ]),
        Line::from(vec![
            Span::styled(format!("{:<12}", "Stream"), label),
            Span::styled(if entry.is_stream { "yes" } else { "no" }, value),
        ]),
        Line::from(vec![
            Span::styled(format!("{:<12}", "Time"), label),
            Span::styled(format_time(entry.timestamp), Style::default().fg(t::MUTED)),
        ]),
    ];

    if let Some(err) = &entry.error {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "Error:",
            Style::default().fg(t::ERROR).add_modifier(Modifier::BOLD),
        )));
        let avail = area.width.saturating_sub(2) as usize;
        for chunk in wrap_text(err, avail) {
            lines.push(Line::from(Span::styled(
                format!("  {chunk}"),
                Style::default().fg(t::ERROR),
            )));
        }
    }

    f.render_widget(Paragraph::new(lines), area);
}

pub(super) fn draw_panel(f: &mut Frame, app: &App, area: Rect, messages_height: u16) {
    let block = Block::default()
        .borders(Borders::LEFT | Borders::RIGHT | Borders::BOTTOM)
        .border_set(BORDER_INNER_DIVIDER)
        .border_style(Style::default().fg(t::MUTED))
        .padding(Padding::new(1, 1, 0, 0));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.height < 4 {
        return;
    }

    let chunks = ratatui::layout::Layout::default()
        .direction(ratatui::layout::Direction::Vertical)
        .constraints([Constraint::Length(messages_height), Constraint::Min(0)])
        .split(inner);

    draw_messages_content(f, app, chunks[0]);
    draw_requests_content(f, app, chunks[1]);
}

fn make_dash_title<'a>(title: &'static str, width: usize) -> Line<'a> {
    let muted = Style::default().fg(t::MUTED);
    let right = width.saturating_sub(TITLE_SIDE + 1 + title.len() + 1);
    Line::from(vec![
        Span::styled(DASH.repeat(TITLE_SIDE), muted),
        Span::raw(" "),
        Span::styled(
            title,
            Style::default().fg(t::TEXT).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(DASH.repeat(right), muted),
    ])
}

fn draw_messages_content(f: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 {
        return;
    }
    let muted = Style::default().fg(t::MUTED);
    let width = area.width as usize;
    let title_line = make_dash_title("Messages", width);

    let msg_width = width;
    let visible_rows = area.height.saturating_sub(1) as usize;

    let mut lines: Vec<Line> = vec![title_line];

    if app.message_log.is_empty() {
        lines.push(Line::from(Span::styled("no messages yet", muted)));
    } else {
        let mut collected: Vec<Line> = Vec::new();
        'outer: for entry in app.message_log.iter().rev() {
            let msg_style = match entry.kind {
                MessageKind::Error => Style::default().fg(t::ERROR),
                MessageKind::Success => Style::default().fg(t::SUCCESS),
                MessageKind::Info => muted,
            };
            let chunks = wrap_text(&entry.text, msg_width);
            for chunk in chunks.iter().rev() {
                if collected.len() >= visible_rows {
                    break 'outer;
                }
                collected.push(Line::from(vec![Span::styled(chunk.clone(), msg_style)]));
            }
        }
        collected.reverse();
        lines.extend(collected);
    }

    f.render_widget(Paragraph::new(lines), area);
}

fn draw_requests_content(f: &mut Frame, app: &App, area: Rect) {
    if area.height < 3 {
        return;
    }
    let muted = Style::default().fg(t::MUTED);
    let width = area.width as usize;
    let title_line = make_dash_title("Recent Requests", width);
    let visible_rows = area.height.saturating_sub(2) as usize;

    let entries = match app.request_log.lock() {
        Ok(log) => log
            .entries()
            .iter()
            .rev()
            .take(visible_rows)
            .cloned()
            .collect::<Vec<_>>(),
        Err(_) => vec![],
    };

    if entries.is_empty() {
        f.render_widget(
            Paragraph::new(vec![
                title_line,
                Line::from(Span::styled("  no requests yet  (session only)", muted)),
            ]),
            area,
        );
        return;
    }

    let mut lines: Vec<Line> = vec![
        title_line,
        Line::from(vec![
            Span::styled(
                format!("{:<11}", "Response"),
                muted.add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{:<18}", "Provider/Model"),
                muted.add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{:<8}", "Tokens"),
                muted.add_modifier(Modifier::BOLD),
            ),
            Span::styled("Age", muted.add_modifier(Modifier::BOLD)),
        ]),
    ];

    for entry in &entries {
        let status_style = if entry.error.is_some() || entry.status >= 400 {
            Style::default().fg(t::ERROR)
        } else {
            Style::default().fg(t::SUCCESS)
        };
        let response = truncate(
            &format!("{} {}", entry.status, fmt_latency(entry.latency_ms)),
            11,
        );
        let provider_model = truncate(
            &format!("{}/{}", entry.provider, shorten_model_name(&entry.model)),
            16,
        );
        lines.push(Line::from(vec![
            Span::styled(format!("{:<11}", response), status_style),
            Span::styled(
                format!("{:<18}", provider_model),
                Style::default().fg(t::TEXT),
            ),
            Span::styled(
                format!(
                    "{:<8}",
                    format_tokens(entry.input_tokens + entry.output_tokens)
                ),
                muted,
            ),
            Span::styled(format!("{:<3}", format_time(entry.timestamp)), muted),
        ]));
    }

    f.render_widget(Paragraph::new(lines), area);
}

fn draw_footer(f: &mut Frame, area: Rect, text: &str) {
    let footer_area = Rect {
        x: area.x + 2,
        y: area.y + area.height - 1,
        width: area.width.saturating_sub(4),
        height: 1,
    };
    f.render_widget(
        Paragraph::new(Span::styled(text, Style::default().fg(t::MUTED))),
        footer_area,
    );
}

fn display_width(s: &str) -> usize {
    s.width()
}

fn truncate(s: &str, max: usize) -> String {
    if display_width(s) <= max {
        return s.to_string();
    }
    let mut w = 0usize;
    let mut end = 0usize;
    for (i, ch) in s.char_indices() {
        let cw = ch.width().unwrap_or(0);
        if w + cw > max.saturating_sub(1) {
            break;
        }
        w += cw;
        end = i + ch.len_utf8();
    }
    format!("{}…", &s[..end])
}

fn wrap_text(s: &str, max: usize) -> Vec<String> {
    if max == 0 {
        return vec![s.to_string()];
    }
    let mut chunks = Vec::new();
    let mut remaining = s;
    while !remaining.is_empty() {
        if display_width(remaining) <= max {
            chunks.push(remaining.to_string());
            break;
        }
        let mut w = 0usize;
        let mut split = remaining.len();
        for (i, ch) in remaining.char_indices() {
            let cw = ch.width().unwrap_or(0);
            if w + cw > max {
                split = i;
                break;
            }
            w += cw;
        }
        chunks.push(remaining[..split].to_string());
        remaining = &remaining[split..];
    }
    chunks
}

fn format_time(ts: std::time::SystemTime) -> String {
    let elapsed = ts.elapsed().unwrap_or_default();
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 3600)
    }
}
