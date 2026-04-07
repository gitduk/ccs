use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph};

use super::super::state::{App, MessageKind};
use super::super::theme::{self as t};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::format::{fmt_latency, format_tokens, shorten_model_name};
use super::layout::{BORDER_INNER_DIVIDER, DASH, TITLE_SIDE, centered_rect};

pub(super) fn draw_logs(f: &mut Frame, app: &mut App) {
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
        let empty = Paragraph::new(Line::from(Span::styled(
            "  No requests yet",
            Style::default().fg(t::MUTED),
        )));
        f.render_widget(empty, inner);
        return;
    }

    let mut lines: Vec<Line> = Vec::new();

    // Header — col widths: Status(8) Latency(10) Provider(11) Model(32) In(8) Out(8) Time(8)
    let hdr = Style::default().fg(t::MUTED).add_modifier(Modifier::BOLD);
    lines.push(Line::from(vec![
        Span::styled(format!("{:<7}", "Status"), hdr),
        Span::styled(format!("{:<10}", "Latency"), hdr),
        Span::styled(format!("{:<11}", "Provider"), hdr),
        Span::styled(format!("{:<32}", "Model"), hdr),
        Span::styled(format!("{:<6}", "In"), hdr),
        Span::styled(format!("{:<6}", "Out"), hdr),
        Span::styled("Age", hdr),
    ]));

    let viewport_height = inner.height.saturating_sub(1) as usize; // minus header
    let selected = app.logs.selected.min(entries.len().saturating_sub(1));

    let scroll = if selected < app.logs.scroll as usize {
        selected
    } else if selected >= app.logs.scroll as usize + viewport_height {
        selected + 1 - viewport_height
    } else {
        app.logs.scroll as usize
    };
    app.logs.scroll = scroll as u16;

    let visible = entries
        .iter()
        .enumerate()
        .skip(scroll)
        .take(viewport_height);

    for (i, entry) in visible {
        let is_selected = i == selected;

        let status_style = if entry.error.is_some() || entry.status >= 400 {
            Style::default().fg(t::ERROR)
        } else {
            Style::default().fg(t::SUCCESS)
        };

        let status_str = format!("{}", entry.status);
        let latency_str = fmt_latency(entry.latency_ms);
        let provider = truncate(&entry.provider, 9);
        let model = truncate(&entry.model, 31);
        let in_str = format_tokens(entry.input_tokens);
        let out_str = format_tokens(entry.output_tokens);
        let time_str = format_time(entry.timestamp);
        let row_style = if is_selected {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };

        lines.push(Line::from(vec![
            Span::styled(format!("{:<7}", status_str), status_style),
            Span::styled(format!("{:<10}", latency_str), row_style),
            Span::styled(format!("{:<11}", provider), row_style.fg(t::TEXT)),
            Span::styled(format!("{:<32}", model), row_style.fg(t::MUTED)),
            Span::styled(format!("{:<6}", in_str), row_style.fg(t::TEXT)),
            Span::styled(format!("{:<6}", out_str), row_style.fg(t::TEXT)),
            Span::styled(format!("{:<8}", time_str), row_style.fg(t::MUTED)),
        ]));
    }

    let footer = format!(" {} requests  [j/k] navigate  [q/Esc] close", entries.len());
    draw_footer(f, area, &footer);

    f.render_widget(Paragraph::new(lines), inner);
}

/// Embedded right-column panel: Messages (top) + Recent Requests (bottom).
/// `messages_height` is set by the caller to `table_height + detail_height` so that
/// the Recent Requests title aligns with By Provider on the left column.
pub(super) fn draw_logs_panel(f: &mut Frame, app: &App, area: Rect, messages_height: u16) {
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

    let splits = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(messages_height), Constraint::Min(0)])
        .split(inner);
    let (msg_area, req_area) = (splits[0], splits[1]);

    draw_messages_content(f, app, msg_area);
    draw_requests_content(f, app, req_area);
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

/// Renders the Messages sub-panel (newest entry at the bottom).
/// Long messages wrap onto continuation lines with a 2-space indent.
fn draw_messages_content(f: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 {
        return;
    }
    let muted = Style::default().fg(t::MUTED);
    let width = area.width as usize;
    let title_line = make_dash_title("Messages", width);

    let msg_width = width;
    let visible_rows = area.height.saturating_sub(1) as usize; // minus title line

    let mut lines: Vec<Line> = vec![title_line];

    if app.message_log.is_empty() {
        lines.push(Line::from(Span::styled("no messages yet", muted)));
    } else {
        // Build wrapped lines from newest-last, keeping only what fits.
        // We iterate in reverse to fill from the bottom up, then reverse the result.
        let mut collected: Vec<Line> = Vec::new();
        'outer: for entry in app.message_log.iter().rev() {
            let msg_style = match entry.kind {
                MessageKind::Error => Style::default().fg(t::ERROR),
                MessageKind::Success => Style::default().fg(t::SUCCESS),
                MessageKind::Info => muted,
            };
            // Split text into chunks of msg_width characters.
            let chunks = wrap_text(&entry.text, msg_width);
            // Walk chunks in reverse (last chunk first) so we can break early.
            for (_ci, chunk) in chunks.iter().enumerate().rev() {
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

/// Renders the Recent Requests sub-panel.
fn draw_requests_content(f: &mut Frame, app: &App, area: Rect) {
    if area.height < 3 {
        return;
    }
    let muted = Style::default().fg(t::MUTED);
    let width = area.width as usize;
    let title_line = make_dash_title("Recent Requests", width);

    // title + header = 2 lines used
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
                format!("{:<7}", "Status"),
                muted.add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{:<8}", "Latency"),
                muted.add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{:<15}", "Provider/Model"),
                muted.add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{:<7}", "Tokens"),
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
        let provider_model = truncate(
            &format!("{}/{}", entry.provider, shorten_model_name(&entry.model)),
            14,
        );
        lines.push(Line::from(vec![
            Span::styled(format!("{:<7}", entry.status), status_style),
            Span::styled(
                format!("{:<8}", fmt_latency(entry.latency_ms)),
                Style::default().fg(t::TEXT),
            ),
            Span::styled(
                format!("{:<15}", provider_model),
                Style::default().fg(t::TEXT),
            ),
            Span::styled(
                format!(
                    "{:<7}",
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

/// Display column width of `s`, using the same unicode-width table ratatui uses.
fn display_width(s: &str) -> usize {
    s.width()
}

fn truncate(s: &str, max: usize) -> String {
    if display_width(s) <= max {
        return s.to_string();
    }
    // Take chars until adding the next one would exceed max-1 (leaving room for '…').
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

/// Split `s` into lines of at most `max` display columns, matching ratatui's rendering.
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
        // Accumulate chars until adding the next would exceed `max` display columns.
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
