use std::collections::HashMap;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Padding, Paragraph};
use unicode_width::UnicodeWidthStr;

use super::super::state::App;
use super::super::theme::{self as t};
use super::format::{format_tokens, max_content_width, strip_model_prefix};

pub(super) fn draw_stats_panel(f: &mut Frame, app: &App, area: Rect) {
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
    // Collect (name, stats) pairs, then sort by failure rate ascending so the
    // most reliable providers appear at the top.
    let mut provider_rows: Vec<(&str, crate::proxy::metrics::ProviderStats)> = app
        .providers
        .names
        .iter()
        .map(|name| {
            (
                name.as_str(),
                m.by_provider.get(name).cloned().unwrap_or_default(),
            )
        })
        .collect();
    let mut model_entries: Vec<(String, u64, u64)> = m
        .by_model
        .iter()
        .filter(|(_, v)| v.input + v.output > 0)
        .map(|(k, v)| (k.clone(), v.input, v.output))
        .collect();
    drop(m);

    // Sort by failure rate ascending (providers with fewer failures first).
    // Providers with no requests sort to the bottom (rate treated as 1.0).
    provider_rows.sort_by(|(_, a), (_, b)| {
        let rate = |s: &crate::proxy::metrics::ProviderStats| {
            if s.failures == 0 && s.requests == 0 {
                // Never used — sort to bottom.
                f64::MAX
            } else if s.requests > 0 {
                s.failures as f64 / s.requests as f64
            } else {
                // failures > 0 but requests == 0: corrupted data, treat as 100%.
                1.0
            }
        };
        rate(a)
            .partial_cmp(&rate(b))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    model_entries.sort_by(|a, b| (b.1 + b.2).cmp(&(a.1 + a.2)));

    let muted = Style::default().fg(t::MUTED);
    let id_col_width = app
        .providers
        .names
        .iter()
        .map(|s| s.width())
        .max()
        .unwrap_or(8)
        .max(8);

    let dash_line = Line::from(Span::styled(
        "╌".repeat(inner.width as usize),
        Style::default().fg(t::MUTED),
    ));

    let mut lines: Vec<Line> = vec![
        Line::from(""),
        dash_line.clone(),
        Line::from(Span::styled(
            "By Provider",
            Style::default().fg(t::TEXT).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    lines.extend(provider_rows.iter().map(|(name, s)| {
        let color = t::provider_color(name);
        Line::from(vec![
            Span::styled(
                format!(
                    "{}{}",
                    name,
                    " ".repeat(id_col_width.saturating_sub(name.width()))
                ),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  In ", muted),
            Span::styled(
                format!("{:>7}", format_tokens(s.input)),
                Style::default().fg(color),
            ),
            Span::styled("  Out ", muted),
            Span::styled(
                format!("{:>7}", format_tokens(s.output)),
                Style::default().fg(color),
            ),
            Span::styled("  Req ", muted),
            Span::styled(
                format!("{:>4}", s.requests),
                Style::default().fg(t::TEXT).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  Fail ", muted),
            {
                let rate = if s.failures == 0 {
                    0.0
                } else if s.requests > 0 {
                    s.failures as f64 / s.requests as f64
                } else {
                    // failures > 0 but requests == 0: corrupted data.
                    1.0
                };
                let high = rate > 0.5;
                let style = if high {
                    Style::default().fg(t::ERROR).add_modifier(Modifier::BOLD)
                } else if s.failures > 0 {
                    Style::default().fg(t::TEXT)
                } else {
                    Style::default().fg(t::MUTED)
                };
                let text = if high {
                    format!("{:>4} ({:.0}%)", s.failures, rate * 100.0)
                } else {
                    format!("{:>4}", s.failures)
                };
                Span::styled(text, style)
            },
        ])
    }));

    // Model Usage section — horizontal stacked bar chart
    lines.push(Line::from(""));
    lines.push(dash_line);
    {
        let title = "By Model";
        let legend = "░ input  █ output";
        let gap = (inner.width as usize).saturating_sub(title.len() + legend.width());
        lines.push(Line::from(vec![
            Span::styled(
                title,
                Style::default().fg(t::TEXT).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" ".repeat(gap)),
            Span::styled(legend, Style::default().fg(t::MUTED)),
        ]));
    }
    lines.push(Line::from(""));

    if model_entries.is_empty() {
        lines.push(Line::from(Span::styled("  No data yet", muted)));
    } else {
        // Determine display name per model: strip the `org/` prefix unless two
        // different full names share the same suffix (collision), in which case
        // keep the full name to disambiguate.
        let mut suffix_count: HashMap<&str, usize> = HashMap::new();
        for (k, _, _) in &model_entries {
            *suffix_count
                .entry(strip_model_prefix(k.as_str()))
                .or_insert(0) += 1;
        }
        let display_names: Vec<&str> = model_entries
            .iter()
            .map(|(full, _, _)| {
                let s = strip_model_prefix(full.as_str());
                if suffix_count.get(s).copied().unwrap_or(0) > 1 {
                    full.as_str()
                } else {
                    s
                }
            })
            .collect();

        // Cap label width at 30 chars to leave room for bars
        let model_col_width =
            max_content_width(display_names.iter().map(|s| s.chars().count()), 10, 30);
        let value_width = 8usize; // "  1234.5K"
        let bar_area = (inner.width as usize).saturating_sub(model_col_width + 2 + value_width);
        // Mix weight: 0.0 = pure log (small values always visible),
        //             1.0 = pure linear (proportionally accurate).
        const LINEAR_WEIGHT: f64 = 0.8;

        let max_total = model_entries
            .iter()
            .map(|(_, i, o)| i + o)
            .max()
            .unwrap_or(1);
        let log_max = ((max_total + 1) as f64).ln();

        for ((_model, input, output), display_name) in
            model_entries.iter().zip(display_names.iter())
        {
            let total = input + output;
            let total_bar = if bar_area > 0 && total > 0 {
                let log_ratio = ((total + 1) as f64).ln() / log_max;
                let linear_ratio = total as f64 / max_total as f64;
                let ratio = LINEAR_WEIGHT * linear_ratio + (1.0 - LINEAR_WEIGHT) * log_ratio;
                ((ratio * bar_area as f64) as usize).min(bar_area)
            } else {
                0
            };
            let input_bar = if total > 0 {
                total_bar * (*input as usize) / (total as usize)
            } else {
                0
            };
            let output_bar = total_bar.saturating_sub(input_bar);
            let empty = bar_area.saturating_sub(total_bar);

            let model_chars: Vec<char> = display_name.chars().collect();
            let label = if model_chars.len() > model_col_width {
                let truncated: String = model_chars[..model_col_width.saturating_sub(1)]
                    .iter()
                    .collect();
                format!("{}…", truncated)
            } else {
                format!("{:<width$}", display_name, width = model_col_width)
            };

            let label_color = t::TEXT;
            lines.push(Line::from(vec![
                Span::styled(label, Style::default().fg(label_color)),
                Span::raw("  "),
                Span::styled("░".repeat(input_bar), Style::default().fg(t::TEXT)),
                Span::styled("█".repeat(output_bar), Style::default().fg(t::TEXT)),
                Span::raw(" ".repeat(empty)),
                Span::styled(
                    format!("  {:>6}", format_tokens(total)),
                    Style::default().fg(label_color),
                ),
            ]));
        }
    }

    f.render_widget(Paragraph::new(lines), inner);
}
