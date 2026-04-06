use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph};

use super::super::state::App;
use super::super::theme::{self as t};
use super::layout::centered_rect;

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

    // Build lines: header + entries
    let mut lines: Vec<Line> = Vec::new();

    // Header
    lines.push(Line::from(vec![
        Span::styled(
            format!("{:<8}", "Status"),
            Style::default().fg(t::MUTED).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{:<10}", "Latency"),
            Style::default().fg(t::MUTED).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{:<16}", "Provider"),
            Style::default().fg(t::MUTED).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{:<32}", "Model"),
            Style::default().fg(t::MUTED).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{:<10}", "In"),
            Style::default().fg(t::MUTED).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{:<10}", "Out"),
            Style::default().fg(t::MUTED).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "Time",
            Style::default().fg(t::MUTED).add_modifier(Modifier::BOLD),
        ),
    ]));

    let viewport_height = inner.height.saturating_sub(1) as usize; // minus header
    let selected = app.logs.selected.min(entries.len().saturating_sub(1));

    // Compute scroll to keep selected visible
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

        let status_str = if let Some(err) = &entry.error {
            truncate(err, 7)
        } else {
            format!("{}", entry.status)
        };

        let latency_str = format_latency(entry.latency_ms);
        let provider = truncate(&entry.provider, 15);
        let model = truncate(&entry.model, 31);
        let in_str = format_tokens(entry.input_tokens);
        let out_str = format_tokens(entry.output_tokens);
        let time_str = format_time(entry.timestamp);
        let stream_indicator = if entry.is_stream { "⇄" } else { " " };

        let row_style = if is_selected {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };

        lines.push(Line::from(vec![
            Span::styled(format!("{:<7} ", status_str), status_style),
            Span::styled(stream_indicator, Style::default().fg(t::MUTED)),
            Span::styled(format!("{:<9}", latency_str), row_style),
            Span::styled(format!("{:<16}", provider), row_style.fg(t::TEXT)),
            Span::styled(format!("{:<32}", model), row_style.fg(t::MUTED)),
            Span::styled(format!("{:<10}", in_str), row_style.fg(t::TEXT)),
            Span::styled(format!("{:<10}", out_str), row_style.fg(t::TEXT)),
            Span::styled(time_str, row_style.fg(t::MUTED)),
        ]));
    }

    // Footer with count
    let footer = format!(" {} requests  [j/k] navigate  [q/Esc] close", entries.len());
    draw_footer(f, area, &footer);

    f.render_widget(Paragraph::new(lines), inner);
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

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max.saturating_sub(1)])
    }
}

fn format_latency(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

fn format_tokens(tokens: u64) -> String {
    if tokens == 0 {
        "—".to_string()
    } else if tokens < 1000 {
        format!("{tokens}")
    } else if tokens < 1_000_000 {
        format!("{:.1}k", tokens as f64 / 1000.0)
    } else {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    }
}

fn format_time(ts: std::time::SystemTime) -> String {
    let elapsed = ts.elapsed().unwrap_or_default();
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else {
        format!("{}h ago", secs / 3600)
    }
}
