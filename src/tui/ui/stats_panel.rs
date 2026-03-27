use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Padding, Paragraph};
use unicode_width::UnicodeWidthStr;

use super::super::state::App;
use super::super::theme::{self as t};
use super::format::{format_tokens, max_content_width};

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
        .provider_names
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
        .provider_names
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

    // Collect all known model names from test_results (fetched via /v1/models per provider).
    // inactive_models: (model_name, in_selected_provider)
    // Use the cursor-selected provider (not the active one) to determine support.
    let selected_provider: &str = app
        .table_state
        .selected()
        .and_then(|i| app.provider_names.get(i))
        .map(|s| s.as_str())
        .unwrap_or(app.config.current.as_str());
    let used_models: std::collections::HashSet<&str> =
        model_entries.iter().map(|(k, _, _)| k.as_str()).collect();

    // Build known model set from persisted provider_models
    let current_provider_models: std::collections::HashSet<&str> = app
        .provider_models
        .get(selected_provider)
        .map(|names| names.iter().map(|s| s.as_str()).collect())
        .unwrap_or_default();

    let mut all_known: std::collections::HashMap<&str, bool> = std::collections::HashMap::new();
    for (provider_name, names) in &app.provider_models {
        let is_selected = provider_name.as_str() == selected_provider;
        for name in names {
            let entry = all_known.entry(name.as_str()).or_insert(false);
            if is_selected {
                *entry = true;
            }
        }
    }
    let mut inactive_models: Vec<(&str, bool)> = all_known
        .into_iter()
        .filter(|(m, _)| !used_models.contains(m))
        .collect();
    inactive_models.sort_unstable_by(|(a, _), (b, _)| {
        a.bytes()
            .map(|b| b.to_ascii_lowercase())
            .cmp(b.bytes().map(|b| b.to_ascii_lowercase()))
    });

    if model_entries.is_empty() && inactive_models.is_empty() {
        lines.push(Line::from(Span::styled("  No data yet", muted)));
    } else {
        // Cap label width at 30 chars to leave room for bars
        let model_col_width = max_content_width(
            model_entries.iter().map(|(k, _, _)| k.chars().count()),
            10,
            30,
        );
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

        for (model, input, output) in &model_entries {
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

            let model_chars: Vec<char> = model.chars().collect();
            let label = if model_chars.len() > model_col_width {
                let truncated: String = model_chars[..model_col_width.saturating_sub(1)]
                    .iter()
                    .collect();
                format!("{}…", truncated)
            } else {
                format!("{:<width$}", model, width = model_col_width)
            };

            // White if model is supported by cursor-selected provider, muted otherwise
            let in_current = current_provider_models.contains(model.as_str());
            let bar_color = if in_current { t::TEXT } else { t::MUTED };

            let label_color = if in_current { t::TEXT } else { t::MUTED };
            lines.push(Line::from(vec![
                Span::styled(label, Style::default().fg(label_color)),
                Span::raw("  "),
                Span::styled("░".repeat(input_bar), Style::default().fg(bar_color)),
                Span::styled("█".repeat(output_bar), Style::default().fg(bar_color)),
                Span::raw(" ".repeat(empty)),
                Span::styled(
                    format!("  {:>6}", format_tokens(total)),
                    Style::default().fg(label_color),
                ),
            ]));
        }

        // Inactive models: wrap into rows, provider color if in current provider, muted otherwise
        if !inactive_models.is_empty() {
            if !model_entries.is_empty() {
                lines.push(Line::from(""));
            }
            // Grid layout: determine cols from max display width, then compute per-column widths.
            // Use Unicode display width (handles CJK wide chars) rather than byte length.
            let max_name = inactive_models
                .iter()
                .map(|(m, _)| m.width())
                .max()
                .unwrap_or(1);
            let available_width = inner.width as usize;
            let cols = (available_width / (max_name + 2)).max(1);
            // Per-column width = max display width in that column + 2-space gap
            let col_widths: Vec<usize> = (0..cols)
                .map(|c| {
                    inactive_models
                        .iter()
                        .skip(c)
                        .step_by(cols)
                        .map(|(m, _)| m.width())
                        .max()
                        .unwrap_or(0)
                        + 2
                })
                .collect();
            for chunk in inactive_models.chunks(cols) {
                let spans: Vec<Span> = chunk
                    .iter()
                    .enumerate()
                    .map(|(i, (model, in_current))| {
                        let color = if *in_current { t::TEXT } else { t::MUTED };
                        let w = col_widths[i];
                        Span::styled(
                            format!("{:<width$}", model, width = w),
                            Style::default().fg(color),
                        )
                    })
                    .collect();
                lines.push(Line::from(spans));
            }
        }
    }

    f.render_widget(Paragraph::new(lines), inner);
}
