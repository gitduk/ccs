//! Request log popup and embedded log side panel.
//!
//! This module renders both the request log viewer and the right-column
//! logs/messages panel on the main screen.

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
use super::ui::format::{fmt_latency, format_tokens, shorten_model_name};
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

fn draw_detail(f: &mut Frame, entry: &RequestLogEntry, area: Rect, logs: &mut LogsState) {
    logs.detail_view_height = area.height;

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

    if let Some(req_body) = &entry.request_body {
        push_body_section(&mut lines, "Request", req_body, area.width);
    }

    if let Some(resp_body) = &entry.response_body {
        push_body_section(&mut lines, "Response", resp_body, area.width);
    }

    let max_scroll = lines.len().saturating_sub(area.height as usize) as u16;
    logs.detail_scroll = logs.detail_scroll.min(max_scroll);

    f.render_widget(Paragraph::new(lines).scroll((logs.detail_scroll, 0)), area);
}

fn push_body_section<'a>(lines: &mut Vec<Line<'a>>, title: &'static str, body: &str, width: u16) {
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        format!("{title}:"),
        Style::default().fg(t::MUTED).add_modifier(Modifier::BOLD),
    )));

    let avail = width.saturating_sub(4) as usize;
    let body = body.strip_suffix(TRUNCATED_BODY_MARKER).unwrap_or(body);

    match parse_log_json(body) {
        Some(value) => push_json_summary(lines, &value, avail),
        None => push_wrapped_lines(lines, body, 2, avail, Style::default().fg(t::MUTED)),
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
        lines.push(Line::from(Span::styled(
            "  error:",
            Style::default().fg(t::ERROR).add_modifier(Modifier::BOLD),
        )));
        push_json_value(lines, error, 4, avail.saturating_sub(2), true);
    }

    if let Some(messages) = obj.get("messages").and_then(|v| v.as_array()) {
        lines.push(Line::from(Span::styled(
            "  messages:",
            Style::default().fg(t::TEXT).add_modifier(Modifier::BOLD),
        )));
        for (idx, message) in messages.iter().enumerate() {
            push_message(lines, idx, message, avail.saturating_sub(2));
        }
    }

    if let Some(content) = obj.get("content") {
        lines.push(Line::from(Span::styled(
            "  content:",
            Style::default().fg(t::TEXT).add_modifier(Modifier::BOLD),
        )));
        push_content(lines, content, 4, avail.saturating_sub(2));
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
        lines.push(Line::from(Span::styled(
            "  other:",
            Style::default().fg(t::MUTED).add_modifier(Modifier::BOLD),
        )));
        for (key, value) in rest {
            push_key_value(lines, key, &compact_json(value), avail);
        }
    }
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
    lines.push(Line::from(Span::styled(
        format!("    #{idx} {role}"),
        Style::default().fg(t::PRIMARY).add_modifier(Modifier::BOLD),
    )));

    if let Some(content) = message.get("content") {
        push_content(lines, content, 6, avail.saturating_sub(4));
    } else {
        push_wrapped_lines(
            lines,
            &compact_json(message),
            6,
            avail.saturating_sub(4),
            Style::default().fg(t::MUTED),
        );
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
                if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                    let kind = part.get("type").and_then(|v| v.as_str()).unwrap_or("text");
                    lines.push(Line::from(Span::styled(
                        format!("{}[{kind}]", " ".repeat(indent)),
                        Style::default().fg(t::MUTED),
                    )));
                    push_wrapped_lines(
                        lines,
                        text,
                        indent + 2,
                        avail.saturating_sub(2),
                        Style::default().fg(t::TEXT),
                    );
                } else {
                    push_json_value(lines, part, indent, avail, false);
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
                lines.push(Line::from(Span::styled(
                    format!("{}{}:", " ".repeat(indent), key),
                    style.add_modifier(Modifier::BOLD),
                )));
                push_json_value(
                    lines,
                    value,
                    indent + 2,
                    avail.saturating_sub(2),
                    error_style,
                );
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
        2,
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
    let prefix = " ".repeat(indent);
    let width = avail.saturating_sub(indent).max(1);
    for raw_line in text.lines() {
        if raw_line.is_empty() {
            lines.push(Line::from(Span::raw(prefix.clone())));
            continue;
        }
        for chunk in wrap_text(raw_line, width) {
            lines.push(Line::from(Span::styled(format!("{prefix}{chunk}"), style)));
        }
    }
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
