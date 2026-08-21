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

use super::state::{App, LogsState, MessageKind, Mode};
use super::theme::{self as t};
use super::ui::format::{fmt_latency, format_tokens, shorten_model_name, truncate_width};
use super::ui::layout::{BORDER_INNER_DIVIDER, DASH, TITLE_SIDE, centered_rect};
use crate::metrics::RequestLogEntry;

const TRUNCATED_BODY_MARKER: &str = "…[truncated]";

pub(super) fn handle_key(
    app: &mut App,
    code: KeyCode,
    _modifiers: KeyModifiers,
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

    match code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.mode = Mode::Normal;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if app.logs.selected > 0 {
                app.logs.selected -= 1;
                app.logs.detail_scroll = 0;
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if total > 0 && app.logs.selected < total - 1 {
                app.logs.selected += 1;
                app.logs.detail_scroll = 0;
            }
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
    draw_detail(f, &entries[selected], detail_inner, &mut app.logs);

    let footer = format!(
        " {} requests  [j/k] navigate  [Shift+J/K] half-page detail  [gg/G] detail top/bottom  [q/Esc] close",
        total
    );
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
    let mut lines: Vec<Line> = Vec::new();
    push_summary_line(&mut lines, entry, avail);

    if let Some(req_body) = &entry.request_body {
        push_body_section(&mut lines, req_body, avail);
    }

    if let Some(resp_body) = &entry.response_body {
        push_body_section(&mut lines, resp_body, avail);
    }

    let max_scroll = lines.len().saturating_sub(area.height as usize) as u16;
    logs.detail_scroll = logs.detail_scroll.min(max_scroll);

    f.render_widget(Paragraph::new(lines).scroll((logs.detail_scroll, 0)), area);
}

/// Tokens and stream mode — the fields the list column beside us does not
/// already carry. Two-tone while it fits, wrapped by the shared wrapper when
/// the pane is too narrow for the layout.
fn push_summary_line<'a>(lines: &mut Vec<Line<'a>>, entry: &RequestLogEntry, avail: usize) {
    let label = Style::default().fg(t::MUTED);
    let value = Style::default().fg(t::TEXT);
    let tokens = format!("{} in / {} out", entry.input_tokens, entry.output_tokens);
    let stream = if entry.is_stream { "yes" } else { "no" };
    let summary = format!("Tokens  {tokens}   Stream  {stream}");

    if summary.width() <= avail {
        lines.push(Line::from(vec![
            Span::styled("Tokens  ", label),
            Span::styled(tokens, value),
            Span::styled("   Stream  ", label),
            Span::styled(stream, value),
        ]));
    } else {
        push_wrapped_lines(lines, &summary, 0, avail, label);
    }
}

fn push_body_section<'a>(lines: &mut Vec<Line<'a>>, body: &str, avail: usize) {
    lines.push(Line::raw(""));
    let body = body.strip_suffix(TRUNCATED_BODY_MARKER).unwrap_or(body);

    match parse_log_json(body) {
        Some(value) => push_json_summary(lines, &value, avail),
        None => push_wrapped_lines(lines, body, 0, avail, Style::default().fg(t::MUTED)),
    }
}

fn push_json_summary<'a>(lines: &mut Vec<Line<'a>>, value: &serde_json::Value, avail: usize) {
    let Some(obj) = value.as_object() else {
        push_wrapped_lines(
            lines,
            &compact_json(value),
            2,
            avail,
            Style::default().fg(t::MUTED),
        );
        return;
    };

    push_scalar_field(lines, obj, "model", avail);
    push_scalar_field(lines, obj, "stream", avail);
    push_scalar_field(lines, obj, "max_tokens", avail);
    push_scalar_field(lines, obj, "max_output_tokens", avail);

    if let Some(error) = obj.get("error") {
        push_section_header(lines, "error:", avail, t::ERROR);
        push_json_value(lines, error, 2, avail, true);
    }

    if let Some(messages) = obj.get("messages").and_then(|v| v.as_array()) {
        push_section_header(lines, "messages:", avail, t::TEXT);
        for (idx, message) in messages.iter().enumerate() {
            push_message(lines, idx, message, avail);
        }
    }

    if let Some(content) = obj.get("content") {
        push_section_header(lines, "content:", avail, t::TEXT);
        push_content(lines, content, 2, avail);
    }

    if let Some(tools) = obj.get("tools").and_then(|v| v.as_array()) {
        let names = tools
            .iter()
            .filter_map(|tool| tool.get("name").and_then(|v| v.as_str()))
            .collect::<Vec<_>>()
            .join(", ");
        if !names.is_empty() {
            push_key_value(lines, "tools", &names, avail);
        }
    }

    let rendered = [
        "model",
        "stream",
        "max_tokens",
        "max_output_tokens",
        "error",
        "messages",
        "content",
        "tools",
    ];
    let rest = obj
        .iter()
        .filter(|(key, _)| !rendered.contains(&key.as_str()))
        .collect::<Vec<_>>();
    if !rest.is_empty() {
        push_section_header(lines, "other:", avail, t::MUTED);
        for (key, value) in rest {
            push_key_value(lines, key, &compact_json(value), avail);
        }
    }
}

/// A `key:` line introducing a nested section of the body summary.
fn push_section_header<'a>(
    lines: &mut Vec<Line<'a>>,
    label: &str,
    avail: usize,
    color: ratatui::style::Color,
) {
    push_wrapped_lines(
        lines,
        label,
        0,
        avail,
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    );
}

fn push_scalar_field<'a>(
    lines: &mut Vec<Line<'a>>,
    obj: &serde_json::Map<String, serde_json::Value>,
    key: &'static str,
    avail: usize,
) {
    if let Some(value) = obj.get(key).filter(|v| !v.is_object() && !v.is_array()) {
        push_key_value(lines, key, &scalar_text(value), avail);
    }
}

/// A content block's one-line header: `[text]`, `[tool_use] read /some/path`.
/// `None` for block types we don't know, which fall back to a raw JSON dump so
/// an upstream addition never silently drops content.
fn block_header(part: &serde_json::Value) -> Option<String> {
    let kind = part.get("type").and_then(|v| v.as_str())?;
    match kind {
        "text" | "thinking" | "tool_result" => Some(format!("[{kind}]")),
        "tool_use" => {
            let name = part.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            Some(match inline_input(part) {
                Some(arg) => format!("[{kind}] {name} {arg}"),
                None => format!("[{kind}] {name}"),
            })
        }
        _ => None,
    }
}

/// Most tool calls take a single scalar argument (`bash` a command, `read` a
/// path); fold it into the header instead of spending two lines on it.
fn inline_input(part: &serde_json::Value) -> Option<String> {
    let input = part.get("input")?.as_object()?;
    if input.len() != 1 {
        return None;
    }
    let value = input.values().next()?;
    if value.is_object() || value.is_array() {
        return None;
    }
    let text = scalar_text(value);
    (!text.contains('\n')).then_some(text)
}

/// The payload under a [`block_header`], minus the bookkeeping fields (`id`,
/// `tool_use_id`, `type`) that only matter for wiring calls to results.
fn push_block_body<'a>(
    lines: &mut Vec<Line<'a>>,
    part: &serde_json::Value,
    indent: usize,
    avail: usize,
) {
    match part
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
    {
        // Both blocks name their payload field after their own type.
        kind @ ("text" | "thinking") => {
            if let Some(text) = part.get(kind).and_then(|v| v.as_str()) {
                push_wrapped_lines(lines, text, indent, avail, Style::default().fg(t::TEXT));
            }
        }
        "tool_result" => {
            if let Some(content) = part.get("content") {
                push_content(lines, content, indent, avail);
            }
        }
        "tool_use" => {
            if let Some(input) = part.get("input").filter(|_| inline_input(part).is_none()) {
                push_json_value(lines, input, indent, avail, false);
            }
        }
        _ => {}
    }
}

fn push_message<'a>(
    lines: &mut Vec<Line<'a>>,
    idx: usize,
    message: &serde_json::Value,
    avail: usize,
) {
    let role = message
        .get("role")
        .and_then(|v| v.as_str())
        .unwrap_or("message");
    let head = Style::default().fg(t::PRIMARY).add_modifier(Modifier::BOLD);

    // A lone block folds onto the role line, saving a line and an indent level
    // on the tool_result messages that dominate a transcript.
    let lone =
        message
            .get("content")
            .and_then(|v| v.as_array())
            .and_then(|parts| match &parts[..] {
                [part] => block_header(part).map(|header| (header, part)),
                _ => None,
            });
    if let Some((header, part)) = lone {
        push_wrapped_lines(lines, &format!("#{idx} {role} {header}"), 2, avail, head);
        push_block_body(lines, part, 4, avail);
        return;
    }

    push_wrapped_lines(lines, &format!("#{idx} {role}"), 2, avail, head);
    match message.get("content") {
        Some(content) => push_content(lines, content, 4, avail),
        None => push_wrapped_lines(
            lines,
            &compact_json(message),
            4,
            avail,
            Style::default().fg(t::MUTED),
        ),
    }
}

fn push_content<'a>(
    lines: &mut Vec<Line<'a>>,
    content: &serde_json::Value,
    indent: usize,
    avail: usize,
) {
    match content {
        serde_json::Value::String(text) => {
            push_wrapped_lines(lines, text, indent, avail, Style::default().fg(t::TEXT));
        }
        serde_json::Value::Array(parts) => {
            for part in parts {
                match block_header(part) {
                    Some(header) => {
                        push_wrapped_lines(
                            lines,
                            &header,
                            indent,
                            avail,
                            Style::default().fg(t::MUTED),
                        );
                        push_block_body(lines, part, indent + 2, avail);
                    }
                    None => push_json_value(lines, part, indent, avail, false),
                }
            }
        }
        _ => push_json_value(lines, content, indent, avail, false),
    }
}

fn push_json_value<'a>(
    lines: &mut Vec<Line<'a>>,
    value: &serde_json::Value,
    indent: usize,
    avail: usize,
    error_style: bool,
) {
    let style = if error_style {
        Style::default().fg(t::ERROR)
    } else {
        Style::default().fg(t::MUTED)
    };
    if let Some(obj) = value.as_object() {
        for (key, value) in obj {
            if value.is_object() || value.is_array() {
                push_wrapped_lines(
                    lines,
                    &format!("{key}:"),
                    indent,
                    avail,
                    style.add_modifier(Modifier::BOLD),
                );
                push_json_value(lines, value, indent + 2, avail, error_style);
            } else {
                push_wrapped_lines(
                    lines,
                    &format!("{key}: {}", scalar_text(value)),
                    indent,
                    avail,
                    style,
                );
            }
        }
    } else {
        push_wrapped_lines(lines, &compact_json(value), indent, avail, style);
    }
}

fn push_key_value<'a>(lines: &mut Vec<Line<'a>>, key: &str, value: &str, avail: usize) {
    push_wrapped_lines(
        lines,
        &format!("{key}: {value}"),
        0,
        avail,
        Style::default().fg(t::MUTED),
    );
}

fn push_wrapped_lines<'a>(
    lines: &mut Vec<Line<'a>>,
    text: &str,
    indent: usize,
    avail: usize,
    style: Style,
) {
    let indent = indent.min(avail.saturating_sub(1));
    let prefix = " ".repeat(indent);
    let width = avail.saturating_sub(indent).max(1);
    for raw_line in text.lines() {
        let raw_line = sanitize(raw_line);
        if raw_line.is_empty() {
            lines.push(Line::from(Span::raw(prefix.clone())));
            continue;
        }
        for chunk in wrap_text(&raw_line, width) {
            lines.push(Line::from(Span::styled(format!("{prefix}{chunk}"), style)));
        }
    }
}

/// Logged bodies carry raw source text. Tabs and stray control characters
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

fn parse_log_json(s: &str) -> Option<serde_json::Value> {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(s) {
        return Some(v);
    }

    // Request/response bodies are intentionally truncated before storage. Close
    // the common dangling string/object/array shape so the detail view can still
    // present the readable prefix as structured JSON.
    let mut in_string = false;
    let mut escape = false;
    let mut brace_depth: i32 = 0;
    let mut bracket_depth: i32 = 0;
    for ch in s.chars() {
        if escape {
            escape = false;
            continue;
        }
        if in_string {
            match ch {
                '\\' => escape = true,
                '"' => in_string = false,
                _ => {}
            }
        } else {
            match ch {
                '"' => in_string = true,
                '{' => brace_depth += 1,
                '}' => brace_depth -= 1,
                '[' => bracket_depth += 1,
                ']' => bracket_depth -= 1,
                _ => {}
            }
        }
    }
    let mut repaired = s.to_string();
    if in_string {
        repaired.push('"');
    }
    for _ in 0..bracket_depth.max(0) {
        repaired.push(']');
    }
    for _ in 0..brace_depth.max(0) {
        repaired.push('}');
    }
    if repaired.len() > s.len()
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&repaired)
    {
        return Some(v);
    }
    None
}

fn scalar_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Bool(v) => v.to_string(),
        serde_json::Value::Number(v) => v.to_string(),
        _ => compact_json(value),
    }
}

fn compact_json(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::{push_body_section, push_message, push_summary_line};
    use serde_json::json;
    use unicode_width::UnicodeWidthStr;

    /// Every rendered line must fit `width`; ratatui clips, it does not wrap.
    #[test]
    fn body_lines_never_exceed_width() {
        let body = json!({
            "model": "m",
            "error": {"a_rather_long_key_name": {"nested": 1}},
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "\tabc 中文 def\tghi jkl mno pqr stu"},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "content": "line one\n\tline two indented"},
                ]},
            ],
        })
        .to_string();

        for width in [10usize, 12, 24, 60, 100] {
            let mut lines = Vec::new();
            // The summary line is the one place still built span-by-span.
            push_summary_line(
                &mut lines,
                &crate::metrics::RequestLogEntry {
                    input_tokens: 9_876_543,
                    output_tokens: 2_109_876,
                    is_stream: true,
                    ..crate::metrics::entry_with_id(1)
                },
                width,
            );
            push_body_section(&mut lines, &body, width);
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

    fn render(message: serde_json::Value, avail: usize) -> Vec<String> {
        let mut lines = Vec::new();
        push_message(&mut lines, 2, &message, avail);
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn lone_block_folds_onto_the_role_line() {
        assert_eq!(
            render(
                json!({"role": "user", "content": [{
                    "type": "tool_result", "tool_use_id": "toolu_1", "content": "ok",
                }]}),
                60,
            ),
            ["  #2 user [tool_result]", "    ok"],
        );
    }

    #[test]
    fn single_scalar_tool_input_folds_into_the_header() {
        assert_eq!(
            render(
                json!({"role": "assistant", "content": [{
                    "type": "tool_use", "id": "toolu_1", "name": "read",
                    "input": {"path": "/tmp/x.md"},
                }]}),
                60,
            ),
            ["  #2 assistant [tool_use] read /tmp/x.md"],
        );
    }

    #[test]
    fn multi_arg_and_multiline_inputs_stay_broken_out() {
        assert_eq!(
            render(
                json!({"role": "assistant", "content": [{
                    "type": "tool_use", "name": "edit",
                    "input": {"path": "/tmp/x", "text": "y"},
                }]}),
                60,
            ),
            [
                "  #2 assistant [tool_use] edit",
                "    path: /tmp/x",
                "    text: y",
            ],
        );
        assert_eq!(
            render(
                json!({"role": "assistant", "content": [{
                    "type": "tool_use", "name": "bash", "input": {"command": "a\nb"},
                }]}),
                60,
            ),
            ["  #2 assistant [tool_use] bash", "    command: a", "    b",],
        );
    }

    #[test]
    fn several_blocks_keep_their_own_header_lines() {
        assert_eq!(
            render(
                json!({"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "hmm"},
                    {"type": "tool_use", "name": "read", "input": {"path": "/tmp/x"}},
                ]}),
                60,
            ),
            [
                "  #2 assistant",
                "    [thinking]",
                "      hmm",
                "    [tool_use] read /tmp/x",
            ],
        );
    }

    #[test]
    fn unknown_block_types_fall_back_to_a_raw_dump() {
        assert_eq!(
            render(
                json!({"role": "user", "content": [{"type": "image", "source": "s"}]}),
                60,
            ),
            ["  #2 user", "    source: s", "    type: image"],
        );
    }
}
