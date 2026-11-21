//! Request log popup and embedded log side panel.
//!
//! This module renders both the request log viewer and the right-column
//! logs/messages panel on the main screen.

use std::borrow::Cow;
use std::time::Instant;

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::state::{App, LogsState, MessageKind, Mode, list_move, step};
use super::theme::{self as t};
use super::ui::format::{fmt_latency, format_tokens, shorten_model_name, truncate_width};
use super::ui::layout::{BORDER_INNER_DIVIDER, DASH, TITLE_SIDE, centered_rect};
use crate::metrics::RequestLogEntry;

pub(super) fn handle_key(
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> crate::error::Result<()> {
    let total = app
        .request_log
        .lock()
        .map(|l| l.entries().len())
        .unwrap_or(0);
    let prev = super::input::insert::consume_pending_key(&mut app.logs.pending_key);

    if let Some('g') = prev
        && code == KeyCode::Char('g')
    {
        app.logs.detail_scroll = 0;
        return Ok(());
    }

    if let Some(dir) = list_move(code, modifiers.contains(KeyModifiers::CONTROL), false) {
        let next = step(app.logs.selected, dir.delta(), total);
        if next != app.logs.selected {
            app.logs.selected = next;
            app.logs.detail_scroll = 0;
        }
        return Ok(());
    }

    match code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.mode = Mode::Normal;
        }
        KeyCode::Char('G') => {
            app.logs.detail_scroll = u16::MAX;
        }
        KeyCode::Char('g') => {
            app.logs.pending_key = Some(('g', Instant::now()));
        }
        KeyCode::Home => {
            app.logs.detail_scroll = 0;
        }
        KeyCode::End => {
            app.logs.detail_scroll = u16::MAX;
        }
        KeyCode::Char('J') => {
            let step = detail_scroll_half_page(app);
            app.logs.detail_scroll = app.logs.detail_scroll.saturating_add(step);
        }
        KeyCode::Char('K') => {
            let step = detail_scroll_half_page(app);
            app.logs.detail_scroll = app.logs.detail_scroll.saturating_sub(step);
        }
        _ => {}
    }
    Ok(())
}

fn detail_scroll_half_page(app: &App) -> u16 {
    (app.logs.detail_view_height / 2).max(1)
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

    let total = match app.request_log.lock() {
        Ok(log) => log.entries().len(),
        Err(_) => 0,
    };

    if total == 0 {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  No requests yet",
                Style::default().fg(t::MUTED),
            ))),
            inner,
        );
        return;
    }

    let selected = app.logs.selected.min(total.saturating_sub(1));
    app.logs.selected = selected;

    let viewport_height = inner.height.saturating_sub(1) as usize;
    let scroll = {
        let mut scroll = app.logs.scroll;
        crate::tui::state::keep_visible(selected, &mut scroll, viewport_height);
        app.logs.scroll = scroll;
        scroll as usize
    };

    // The list paints [scroll, scroll + viewport) and the detail pane the
    // selected row, so clone those rows only, not the whole window.
    let rows = match app.request_log.lock() {
        Ok(log) => log
            .entries()
            .iter()
            .rev()
            .skip(scroll)
            .take(viewport_height)
            .cloned()
            .collect::<Vec<_>>(),
        Err(_) => vec![],
    };

    // Require at least 10 cols for the detail pane; fall back to list-only below that.
    const LIST_WIDTH: u16 = 37;
    if inner.width <= LIST_WIDTH + 10 {
        render_log_list(f, &rows, scroll, selected, inner);
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

    render_log_list(f, &rows, scroll, selected, list_area);

    let detail_block = Block::default()
        .borders(Borders::LEFT)
        .border_style(Style::default().fg(t::MUTED))
        .padding(Padding::new(1, 0, 0, 0));
    let detail_inner = detail_block.inner(detail_area);
    f.render_widget(detail_block, detail_area);
    if let Some(entry) = rows.get(selected.saturating_sub(scroll)) {
        draw_detail(f, entry, detail_inner, &mut app.logs);
    }

    let footer = format!(
        " {} requests  [j/k] navigate  [Shift+J/K] half-page detail  [gg/G] detail top/bottom  [q/Esc] close",
        total
    );
    draw_footer(f, area, &footer);
}

/// Paint the visible page of the log list; `rows` starts at absolute index
/// `first` in newest-first order.
fn render_log_list(
    f: &mut Frame,
    rows: &[RequestLogEntry],
    first: usize,
    selected: usize,
    area: Rect,
) {
    let viewport_height = area.height.saturating_sub(1) as usize;
    let hdr = Style::default().fg(t::MUTED).add_modifier(Modifier::BOLD);
    let mut lines: Vec<Line> = vec![Line::from(vec![
        Span::styled(format!("{:<11}", "Response"), hdr),
        Span::styled(format!("{:<22}", "Provider/Model"), hdr),
        Span::styled("Age", hdr),
    ])];
    for (i, entry) in rows.iter().enumerate().take(viewport_height) {
        let is_selected = first + i == selected;
        let base_status = entry_status_style(entry);
        let (status_style, row_style) = if is_selected {
            let rev = Modifier::REVERSED;
            (
                base_status.add_modifier(rev),
                Style::default().add_modifier(rev),
            )
        } else {
            (base_status, Style::default())
        };
        let (response, pm) = entry_cells(entry, 10, 21);
        lines.push(Line::from(vec![
            Span::styled(format!("{:<11}", response), status_style),
            Span::styled(format!("{:<22}", pm), row_style.fg(t::TEXT)),
            Span::styled(format_time(entry.timestamp), row_style.fg(t::MUTED)),
        ]));
    }
    f.render_widget(Paragraph::new(lines), area);
}

fn draw_detail(f: &mut Frame, entry: &RequestLogEntry, area: Rect, logs: &mut LogsState) {
    logs.detail_view_height = area.height;

    let avail = area.width as usize;
    let text = Style::default().fg(t::TEXT);
    let mut lines: Vec<Line> = Vec::new();
    push_field(
        &mut lines,
        "Status",
        &format!("{} {}", entry.status, fmt_latency(entry.latency_ms)),
        avail,
        entry_status_style(entry),
    );
    push_field(&mut lines, "Provider", &entry.provider, avail, text);
    push_field(&mut lines, "Model", &entry.model, avail, text);
    push_field(
        &mut lines,
        "Tokens",
        &format!("{} in / {} out", entry.input_tokens, entry.output_tokens),
        avail,
        text,
    );
    push_field(
        &mut lines,
        "Stream",
        if entry.is_stream { "yes" } else { "no" },
        avail,
        text,
    );
    push_field(
        &mut lines,
        "Age",
        &format_time(entry.timestamp),
        avail,
        text,
    );

    if let Some(error) = &entry.error {
        lines.push(Line::raw(""));
        push_field(
            &mut lines,
            "Error",
            error,
            avail,
            Style::default().fg(t::ERROR),
        );
    }
    // Upstream's own error text, capped when the row was written.
    if let Some(body) = &entry.error_body {
        lines.push(Line::raw(""));
        push_wrapped_lines(&mut lines, body, avail, Style::default().fg(t::MUTED));
    }

    let max_scroll = lines.len().saturating_sub(area.height as usize) as u16;
    logs.detail_scroll = logs.detail_scroll.min(max_scroll);

    f.render_widget(Paragraph::new(lines).scroll((logs.detail_scroll, 0)), area);
}

/// One `label  value` line: two-tone while it fits, wrapped by the shared
/// wrapper when the pane is too narrow for the layout.
fn push_field<'a>(
    lines: &mut Vec<Line<'a>>,
    label: &'static str,
    value: &str,
    avail: usize,
    value_style: Style,
) {
    let label = format!("{label:<10}");
    let value = sanitize(value);
    let text = format!("{label} {value}");
    if text.width() <= avail {
        lines.push(Line::from(vec![
            Span::styled(label, Style::default().fg(t::MUTED)),
            Span::styled(value.into_owned(), value_style),
        ]));
    } else {
        push_wrapped_lines(lines, &text, avail, Style::default().fg(t::MUTED));
    }
}

fn push_wrapped_lines<'a>(lines: &mut Vec<Line<'a>>, text: &str, avail: usize, style: Style) {
    let width = avail.max(1);
    for raw_line in text.lines() {
        let raw_line = sanitize(raw_line);
        if raw_line.is_empty() {
            lines.push(Line::raw(""));
            continue;
        }
        for chunk in wrap_text(&raw_line, width) {
            lines.push(Line::from(Span::styled(chunk, style)));
        }
    }
}

/// Logged upstream error text is raw. Tabs and stray control characters
/// measure as zero width but still move the cursor, blowing the wrap budget.
fn sanitize(raw: &str) -> Cow<'_, str> {
    if !raw.contains(|c: char| c.is_control()) {
        return Cow::Borrowed(raw);
    }
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '\t' => out.push_str("    "),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    Cow::Owned(out)
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
        let status_style = entry_status_style(entry);
        let (response, provider_model) = entry_cells(entry, 11, 16);
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

fn wrap_text(s: &str, max: usize) -> Vec<String> {
    if max == 0 {
        return vec![s.to_string()];
    }
    let mut chunks = Vec::new();
    let mut remaining = s;
    while !remaining.is_empty() {
        if remaining.width() <= max {
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

/// Status color for a log entry: red on error or HTTP >= 400, green otherwise.
fn entry_status_style(entry: &RequestLogEntry) -> Style {
    if entry.error.is_some() || entry.status >= 400 {
        Style::default().fg(t::ERROR)
    } else {
        Style::default().fg(t::SUCCESS)
    }
}

/// Build the "status latency" and "provider/model" cells, truncated to the
/// given display widths. Shared by the side panel and full-screen log list.
fn entry_cells(entry: &RequestLogEntry, resp_w: usize, pm_w: usize) -> (String, String) {
    (
        truncate_width(
            &format!("{} {}", entry.status, fmt_latency(entry.latency_ms)),
            resp_w,
        ),
        truncate_width(
            &format!("{}/{}", entry.provider, shorten_model_name(&entry.model)),
            pm_w,
        ),
    )
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

#[cfg(test)]
mod tests {
    use super::{push_field, push_wrapped_lines};
    use ratatui::style::Style;
    use unicode_width::UnicodeWidthStr;

    /// Every rendered line must fit `width`; ratatui clips, it does not wrap.
    #[test]
    fn detail_lines_never_exceed_width() {
        let body = format!(
            "\tupstream said:\u{7} rate limit exceeded {}…",
            "more ".repeat(20)
        );

        for width in [10usize, 12, 24, 60, 100] {
            let mut lines = Vec::new();
            push_field(
                &mut lines,
                "Model",
                "deepseek\tv4.1-flash-an-unusually-long-alias",
                width,
                Style::default(),
            );
            push_field(
                &mut lines,
                "Tokens",
                "9876543 in / 2109876 out",
                width,
                Style::default(),
            );
            push_wrapped_lines(&mut lines, &body, width, Style::default());
            for line in &lines {
                let text = line
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>();
                assert!(
                    text.width() <= width,
                    "width={width} got={}: |{text}|",
                    text.width()
                );
            }
        }
    }
}
