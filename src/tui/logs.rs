//! Request log popup and embedded log side panel.
//!
//! This module renders both the request log viewer and the right-column
//! logs/messages panel on the main screen.

use std::borrow::Cow;
use std::time::Instant;

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::state::{App, LogsState, MessageKind, Mode, keep_visible, list_move, step};
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
        app.logs.error_scroll = 0;
        return Ok(());
    }

    if let Some(dir) = list_move(code, modifiers.contains(KeyModifiers::CONTROL), false) {
        let next = step(app.logs.selected, dir.delta(), total);
        if next != app.logs.selected {
            app.logs.selected = next;
            app.logs.error_scroll = 0;
        }
        return Ok(());
    }

    match code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.mode = Mode::Normal;
        }
        KeyCode::Char('G') | KeyCode::End => {
            app.logs.error_scroll = u16::MAX;
        }
        KeyCode::Char('g') => {
            app.logs.pending_key = Some(('g', Instant::now()));
        }
        KeyCode::Home => {
            app.logs.error_scroll = 0;
        }
        KeyCode::Char('J') => {
            let step = error_scroll_half_page(app);
            app.logs.error_scroll = app.logs.error_scroll.saturating_add(step);
        }
        KeyCode::Char('K') => {
            let step = error_scroll_half_page(app);
            app.logs.error_scroll = app.logs.error_scroll.saturating_sub(step);
        }
        _ => {}
    }
    Ok(())
}

fn error_scroll_half_page(app: &App) -> u16 {
    (app.logs.error_view_height / 2).max(1)
}

pub(super) fn draw_popup(f: &mut Frame, app: &mut App) {
    let area = popup_area(f.area());
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

    // The error section takes height from the table, so the selected row is
    // read before the viewport is known.
    let selected_entry = match app.request_log.lock() {
        Ok(log) => log.entries().iter().rev().nth(selected).cloned(),
        Err(_) => None,
    };
    let error_lines = selected_entry
        .as_ref()
        .map(|entry| error_section_lines(entry, inner.width as usize))
        .unwrap_or_default();
    let error_height = error_section_height(error_lines.len(), inner.height);
    let table_area = Rect {
        height: inner.height - error_height,
        ..inner
    };

    let viewport = table_area.height.saturating_sub(1) as usize;
    let scroll = {
        let mut scroll = app.logs.scroll;
        keep_visible(selected, &mut scroll, viewport);
        app.logs.scroll = scroll;
        scroll as usize
    };

    // The table paints [scroll, scroll + viewport) and the error section the
    // selected row only, so clone those rows, not the whole window.
    let rows = match app.request_log.lock() {
        Ok(log) => log
            .entries()
            .iter()
            .rev()
            .skip(scroll)
            .take(viewport)
            .cloned()
            .collect::<Vec<_>>(),
        Err(_) => vec![],
    };
    f.render_widget(
        Paragraph::new(log_table_lines(
            &rows,
            scroll,
            selected,
            table_area.width as usize,
        )),
        table_area,
    );

    if error_height > 0 {
        let error_area = Rect {
            y: table_area.y + table_area.height,
            height: error_height,
            ..inner
        };
        draw_error_section(f, error_lines, error_area, &mut app.logs);
    }

    let footer = format!(
        " {} requests  [j/k] navigate  [Shift+J/K] error text  [gg/G] error top/bottom  [q/Esc] close",
        total
    );
    draw_footer(f, area, &footer);
}

/// Past this width the table only gains empty space between provider/model and
/// the numeric columns, so the popup stops growing.
const POPUP_MAX_WIDTH: u16 = 110;

/// Centered popup, narrowed to [`POPUP_MAX_WIDTH`].
fn popup_area(area: Rect) -> Rect {
    let centered = centered_rect(90, 80, area);
    let width = centered.width.min(POPUP_MAX_WIDTH);
    Rect {
        x: centered.x + (centered.width - width) / 2,
        width,
        ..centered
    }
}

/// Widths of the columns that always fit; provider/model takes the rest.
const COL_RESPONSE: usize = 11;
const COL_TOKENS: usize = 16;
/// Token sub-columns: the prompt count right-aligned, so the slashes of
/// consecutive rows line up, the completion count left-aligned.
const TOKENS_IN_WIDTH: usize = 6;
const TOKENS_OUT_WIDTH: usize = 6;
const COL_STREAM: usize = 7;
const COL_AGE: usize = 4;
/// Popup widths below which an optional column would squeeze provider/model.
const TOKENS_MIN_WIDTH: usize = 58;
const STREAM_MIN_WIDTH: usize = 72;

/// Lines of the full-width request table; `rows` starts at absolute index
/// `first` in newest-first order.
fn log_table_lines(
    rows: &[RequestLogEntry],
    first: usize,
    selected: usize,
    width: usize,
) -> Vec<Line<'static>> {
    let tokens_width = if width >= TOKENS_MIN_WIDTH {
        COL_TOKENS
    } else {
        0
    };
    let stream_width = if width >= STREAM_MIN_WIDTH {
        COL_STREAM
    } else {
        0
    };
    let widths = [
        COL_RESPONSE,
        width.saturating_sub(COL_RESPONSE + tokens_width + stream_width + COL_AGE),
        tokens_width,
        stream_width,
        COL_AGE,
    ];
    let header = Style::default().fg(t::MUTED).add_modifier(Modifier::BOLD);
    let mut lines = vec![table_line(
        ["Response", "Provider/Model", "Tokens", "Stream", "Age"],
        widths,
        [header; 5],
    )];
    for (i, entry) in rows.iter().enumerate() {
        let is_selected = first + i == selected;
        let row = if is_selected {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        let mut status = entry_status_style(entry);
        if is_selected {
            status = status.add_modifier(Modifier::REVERSED);
        }
        let response = format!("{} {}", entry.status, fmt_latency(entry.latency_ms));
        let provider_model = format!("{}/{}", entry.provider, entry.model);
        let tokens = tokens_cell(entry);
        lines.push(table_line(
            [
                &response,
                &provider_model,
                &tokens,
                if entry.is_stream { "yes" } else { "no" },
                &format_time(entry.timestamp),
            ],
            widths,
            [
                status,
                row.fg(t::TEXT),
                row.fg(t::MUTED),
                row.fg(t::MUTED),
                row.fg(t::MUTED),
            ],
        ));
    }
    lines
}

fn tokens_cell(entry: &RequestLogEntry) -> String {
    if entry.input_tokens == 0 && entry.output_tokens == 0 {
        return "—".to_string();
    }
    format!(
        "{:>TOKENS_IN_WIDTH$} / {:<TOKENS_OUT_WIDTH$}",
        format_tokens(entry.input_tokens),
        format_tokens(entry.output_tokens),
    )
}

/// One table line: each cell is truncated to its column and padded to fill it;
/// a zero-width column is dropped.
fn table_line(cells: [&str; 5], widths: [usize; 5], styles: [Style; 5]) -> Line<'static> {
    let mut spans = Vec::with_capacity(5);
    for ((cell, width), style) in cells.iter().zip(widths).zip(styles) {
        if width == 0 {
            continue;
        }
        // One column of gap keeps neighbouring truncation markers apart.
        let text = truncate_width(cell, width.saturating_sub(1));
        spans.push(Span::styled(format!("{text:<width$}"), style));
    }
    Line::from(spans)
}

fn error_section_lines(entry: &RequestLogEntry, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(error) = &entry.error {
        push_wrapped_lines(&mut lines, error, width, Style::default().fg(t::ERROR));
    }
    // Upstream's own error text, capped when the row was written.
    if let Some(body) = &entry.error_body {
        push_wrapped_lines(&mut lines, body, width, Style::default().fg(t::MUTED));
    }
    lines
}

/// Height the error section takes from the table, top border included; zero
/// when the selected row carries no error text.
fn error_section_height(needed: usize, inner_height: u16) -> u16 {
    if needed == 0 {
        return 0;
    }
    let cap = (inner_height as usize / 3).max(3);
    let body = needed.min(cap) as u16;
    // The table keeps its header and two rows.
    (body + 1).min(inner_height.saturating_sub(3))
}

fn draw_error_section(f: &mut Frame, lines: Vec<Line<'static>>, area: Rect, logs: &mut LogsState) {
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(t::MUTED))
        .title(" Error ")
        .title_style(Style::default().fg(t::ERROR))
        .padding(Padding::new(1, 1, 0, 0));
    let inner = block.inner(area);
    f.render_widget(block, area);

    logs.error_view_height = inner.height;
    let max_scroll = lines.len().saturating_sub(inner.height as usize) as u16;
    logs.error_scroll = logs.error_scroll.min(max_scroll);
    f.render_widget(Paragraph::new(lines).scroll((logs.error_scroll, 0)), inner);
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
    use super::{error_section_lines, log_table_lines};
    use crate::metrics::{RequestLogEntry, entry_with_id};
    use ratatui::text::Line;
    use unicode_width::UnicodeWidthStr;

    fn failing_entry() -> RequestLogEntry {
        RequestLogEntry {
            error: Some("upstream 429".into()),
            error_body: Some(
                format!(
                    "\tupstream said:\u{7} rate limit exceeded {}…",
                    "more ".repeat(20)
                )
                .into(),
            ),
            model: "deepseek\tv4.1-flash-an-unusually-long-alias".into(),
            input_tokens: 220_828,
            output_tokens: 134,
            ..entry_with_id(1)
        }
    }

    fn text_of(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// Every rendered line must fit `width`; ratatui clips, it does not wrap.
    #[test]
    fn error_lines_never_exceed_width() {
        let entry = failing_entry();

        for width in [10usize, 12, 24, 60, 100] {
            for line in error_section_lines(&entry, width) {
                let text = text_of(&line);
                assert!(
                    text.width() <= width,
                    "width={width} got={}: |{text}|",
                    text.width()
                );
            }
        }
    }

    /// A table line pads to the popup width, so rows stay column-aligned and
    /// nothing spills past the frame.
    #[test]
    fn table_lines_fill_the_popup_width() {
        let rows = [
            failing_entry(),
            RequestLogEntry {
                is_stream: true,
                model: "claude-sonnet-4-6".into(),
                ..entry_with_id(2)
            },
        ];

        for width in [40usize, 62, 76, 120, 171] {
            for line in log_table_lines(&rows, 0, 1, width) {
                let text = text_of(&line);
                assert_eq!(text.width(), width, "width={width}: |{text}|");
            }
        }
    }
}
