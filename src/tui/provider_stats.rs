use std::collections::HashMap;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding, Paragraph};
use unicode_width::UnicodeWidthStr;

use super::state::App;
use super::theme::{self as t};
use super::ui::format::{fmt_latency, format_tokens, max_content_width, strip_model_prefix};
use super::ui::layout::{DASH, SUFFIX_GAP, TITLE_SIDE};

pub(super) fn draw_panel(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default().padding(Padding::horizontal(1));

    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.height == 0 {
        return;
    }

    let Ok(m) = app.metrics.lock() else { return };
    let mut provider_rows: Vec<(&str, crate::metrics::ProviderStats)> = app
        .config
        .providers
        .keys()
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

    provider_rows.sort_by(|(_, a), (_, b)| {
        let rate = |s: &crate::metrics::ProviderStats| {
            if s.failures == 0 && s.requests == 0 {
                f64::MAX
            } else if s.requests > 0 {
                s.failures as f64 / s.requests as f64
            } else {
                1.0
            }
        };
        rate(a)
            .partial_cmp(&rate(b))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    model_entries.sort_by_key(|e| std::cmp::Reverse(e.1 + e.2));

    let muted = Style::default().fg(t::MUTED);
    let id_col_width = app
        .config
        .providers
        .keys()
        .map(|s| s.width())
        .max()
        .unwrap_or(8)
        .max(8);

    let make_title_line = |title: &'static str, suffix: &str| -> Line<'static> {
        let mid = (inner.width as usize).saturating_sub(
            TITLE_SIDE
                + 1
                + title.len()
                + 1
                + if suffix.is_empty() {
                    0
                } else {
                    SUFFIX_GAP + suffix.width()
                },
        );
        let mut spans: Vec<Span> = vec![
            Span::styled(DASH.repeat(TITLE_SIDE), muted),
            Span::raw(" "),
            Span::styled(
                title,
                Style::default().fg(t::TEXT).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(DASH.repeat(mid), muted),
        ];
        if !suffix.is_empty() {
            spans.push(Span::raw(" ".repeat(SUFFIX_GAP)));
            spans.push(Span::styled(suffix.to_string(), muted));
        }
        Line::from(spans)
    };

    let mut lines: Vec<Line> = vec![
        Line::from(""),
        make_title_line("By Provider", ""),
        Line::from(""),
    ];
    lines.extend(provider_rows.iter().map(|(name, s)| {
        Line::from(vec![
            Span::styled(
                format!(
                    "{}{}",
                    name,
                    " ".repeat(id_col_width.saturating_sub(name.width()))
                ),
                Style::default().fg(t::TEXT).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  In ", muted),
            Span::styled(
                format!("{:>8}", format_tokens(s.input)),
                Style::default().fg(t::TEXT),
            ),
            Span::styled("  Out ", muted),
            Span::styled(
                format!("{:>8}", format_tokens(s.output)),
                Style::default().fg(t::TEXT),
            ),
            Span::styled("  Avg ", muted),
            {
                let avg = s
                    .latency_total
                    .checked_div(s.requests)
                    .map(fmt_latency)
                    .unwrap_or_else(|| "—".to_string());
                Span::styled(format!("{:>7}", avg), Style::default().fg(t::TEXT))
            },
            Span::styled("  Req ", muted),
            Span::styled(
                format!("{:>5}", s.requests),
                Style::default().fg(t::TEXT).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  Fail ", muted),
            {
                let rate = if s.failures == 0 {
                    0.0
                } else if s.requests > 0 {
                    s.failures as f64 / s.requests as f64
                } else {
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

    lines.push(Line::from(""));
    lines.push(make_title_line("By Model", "░ input  █ output ╌╌"));
    lines.push(Line::from(""));

    if model_entries.is_empty() {
        lines.push(Line::from(Span::styled("  No data yet", muted)));
    } else {
        let suffixes: Vec<String> = model_entries
            .iter()
            .map(|(k, _, _)| strip_model_prefix(k.as_str()).to_string())
            .collect();
        let mut suffix_count: HashMap<String, usize> = HashMap::with_capacity(suffixes.len());
        for s in &suffixes {
            *suffix_count.entry(s.clone()).or_insert(0) += 1;
        }
        let display_names: Vec<String> = model_entries
            .iter()
            .zip(suffixes.iter())
            .map(|((full, _, _), s)| {
                if suffix_count.get(s).copied().unwrap_or(0) > 1 {
                    full.clone()
                } else {
                    s.clone()
                }
            })
            .collect();

        let model_col_width =
            max_content_width(display_names.iter().map(|s| s.chars().count()), 10, 30);
        let value_width = 8usize;
        let bar_area = (inner.width as usize).saturating_sub(model_col_width + 2 + value_width);
        const LINEAR_WEIGHT: f64 = 0.8;

        let max_total = model_entries
            .iter()
            .map(|(_, i, o)| i + o)
            .max()
            .unwrap_or(1);
        let log_max = ((max_total + 1) as f64).ln();

        let lines_so_far = lines.len();
        let available = (inner.height as usize).saturating_sub(lines_so_far + 1);
        let (visible_count, hidden_count) = if model_entries.len() <= available {
            (model_entries.len(), 0)
        } else {
            (
                available.saturating_sub(1),
                model_entries.len() - available.saturating_sub(1),
            )
        };

        for ((_model, input, output), display_name) in model_entries[..visible_count]
            .iter()
            .zip(display_names[..visible_count].iter())
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
                format!("{:<width$}", display_name.as_str(), width = model_col_width)
            };

            lines.push(Line::from(vec![
                Span::styled(label, Style::default().fg(t::TEXT)),
                Span::raw("  "),
                Span::styled("░".repeat(input_bar), Style::default().fg(t::TEXT)),
                Span::styled("█".repeat(output_bar), Style::default().fg(t::TEXT)),
                Span::raw(" ".repeat(empty)),
                Span::styled(
                    format!("  {:>6}", format_tokens(total)),
                    Style::default().fg(t::TEXT),
                ),
            ]));
        }
        if hidden_count > 0 {
            lines.push(Line::from(Span::styled(
                format!("… {} more", hidden_count),
                muted,
            )));
        }
    }

    lines.push(Line::from(""));
    f.render_widget(Paragraph::new(lines), inner);
}
