use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Padding, Paragraph, Row, Table, Wrap};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use super::app::{App, ConfirmAction, MessageKind, Mode, VimMode};
use super::theme::{self as t};
use crate::test_provider::TestStatus;

pub fn draw(f: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title bar
            Constraint::Min(0),    // main content
            Constraint::Length(1), // keybindings
        ])
        .split(f.area());

    draw_title_bar(f, app, chunks[0]);
    draw_main(f, app, chunks[1]);
    draw_keybindings(f, app, chunks[2]);

    match &app.mode {
        Mode::Editing => draw_form(f, app),
        Mode::Confirm => draw_confirm(f, app),
        Mode::Help => draw_help(f, app),
        Mode::Normal => {}
    }
}

fn draw_title_bar(f: &mut Frame, app: &App, area: Rect) {
    let fallback_label = if app.config.fallback {
        "Fallback on  "
    } else {
        "Fallback off "
    };
    let listen_label = format!("{}  ", app.config.listen);
    let version = format!("  v{}", env!("CARGO_PKG_VERSION"));
    let title_left = " Claude Code Switcher";
    let left_len = title_left.len() + version.len();
    let right_len = listen_label.len() + fallback_label.len();
    let gap = (area.width as usize).saturating_sub(left_len + right_len);

    let spans: Vec<Span> = vec![
        Span::styled(
            " Claude Code Switcher",
            Style::default().fg(t::TEXT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(version, Style::default().fg(t::MUTED)),
        Span::raw(" ".repeat(gap)),
        Span::styled(
            listen_label,
            if app.bg_proxy_pid.is_some() {
                Style::default().fg(t::SUCCESS).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(t::MUTED)
            },
        ),
        Span::styled(
            fallback_label,
            if app.config.fallback {
                Style::default().fg(t::SUCCESS).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(t::MUTED)
            },
        ),
    ];
    let line = Line::from(spans);
    f.render_widget(Paragraph::new(line), area);
}

fn draw_main(f: &mut Frame, app: &mut App, area: Rect) {
    let table_height = (app.provider_names.len() as u16 + 2)
        .max(3)
        .min(area.height * 2 / 3);
    let detail_height = 3u16;
    // stats: blank + title + N provider rows + bottom border
    let n_providers = app.provider_names.len() as u16;
    let stats_min_height = 3 + n_providers;
    // model: blank + title + N active rows + inactive grid rows + bottom border
    let n_active = app
        .metrics
        .lock()
        .map(|m| {
            m.by_model
                .values()
                .filter(|s| s.input + s.output > 0)
                .count() as u16
        })
        .unwrap_or(0);
    let n_inactive: u16 = {
        let used: std::collections::HashSet<String> = app
            .metrics
            .lock()
            .map(|m| m.by_model.keys().cloned().collect())
            .unwrap_or_default();
        let total_inactive = app
            .provider_models
            .values()
            .flat_map(|v| v.iter())
            .filter(|m| !used.contains(*m))
            .collect::<std::collections::HashSet<_>>()
            .len();
        // Estimate grid rows: inactive models laid out with max_name+2 cell width.
        // Use area.width as an approximation (stats panel has 2-char padding).
        let max_name = app
            .provider_models
            .values()
            .flat_map(|v| v.iter())
            .map(|s| s.width())
            .max()
            .unwrap_or(1);
        let panel_width = (area.width as usize).saturating_sub(4);
        let cols = (panel_width / (max_name + 2)).max(1);
        total_inactive.div_ceil(cols) as u16
    };
    let model_min_height = (3 + n_active + if n_inactive > 0 { n_inactive + 1 } else { 0 }).max(3);

    let leftover = area.height.saturating_sub(table_height + detail_height);
    let show_stats = leftover >= stats_min_height;

    let mut constraints = vec![
        Constraint::Length(table_height),
        Constraint::Length(detail_height),
    ];
    if show_stats {
        constraints.push(Constraint::Min(stats_min_height + model_min_height));
    } else {
        constraints.push(Constraint::Min(0));
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    draw_provider_table(f, app, chunks[0]);
    draw_detail_panel(f, app, chunks[1]);
    if show_stats {
        draw_stats_panel(f, app, chunks[2]);
    }
}

fn draw_provider_table(f: &mut Frame, app: &mut App, area: Rect) {
    if app.provider_names.is_empty() {
        let empty = Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled(
                "  No providers configured",
                Style::default().fg(t::MUTED),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled("  Press ", Style::default().fg(t::MUTED)),
                Span::styled(
                    "a",
                    Style::default().fg(t::WARNING).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    " to add a provider, or edit ",
                    Style::default().fg(t::MUTED),
                ),
                Span::styled(config_path_display(), Style::default().fg(t::PRIMARY)),
            ]),
        ])
        .block(
            Block::default()
                .borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
                .border_style(Style::default().fg(t::MUTED)),
        );
        f.render_widget(empty, area);
        return;
    }

    let url_col = col_width(
        "Base URL",
        app.config.providers.values().map(|p| p.base_url.len()),
    );
    let key_col = col_width(
        "API Key",
        app.config
            .providers
            .values()
            .map(|p| api_key_display_len(&p.api_key)),
    );
    let notes_col = col_width(
        "Notes",
        app.config
            .providers
            .values()
            .map(|p| p.notes.lines().next().unwrap_or("").width()),
    )
    .min(30);
    // Name col = longest name + 2 for the " ◀" indicator + 4 gap
    let max_name_len = app
        .provider_names
        .iter()
        .map(|name| name.len())
        .max()
        .unwrap_or(0)
        .max("Name".len());
    let name_col = (max_name_len + 2 + 4) as u16;

    let header = Row::new(vec![
        Cell::from("Name").style(Style::default().fg(t::TEXT).add_modifier(Modifier::BOLD)),
        Cell::from("Format").style(Style::default().fg(t::TEXT).add_modifier(Modifier::BOLD)),
        Cell::from("Base URL").style(Style::default().fg(t::TEXT).add_modifier(Modifier::BOLD)),
        Cell::from("API Key").style(Style::default().fg(t::TEXT).add_modifier(Modifier::BOLD)),
        Cell::from("Notes").style(Style::default().fg(t::TEXT).add_modifier(Modifier::BOLD)),
    ])
    .height(1);

    let selected = app.table_state.selected();

    let rows: Vec<Row> = app
        .provider_names
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let provider = &app.config.providers[name];
            let is_current = name == &app.config.current;
            let is_selected = selected == Some(i);

            // Cursor triangle shown only on the selected row, colored by that provider.
            let (indicator, indicator_style) = if is_selected {
                (
                    " ◀",
                    Style::default()
                        .fg(t::provider_color(name))
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                ("  ", Style::default())
            };
            // Current active provider: provider color. Others: TEXT name, MUTED details.
            let name_style = if is_current {
                Style::default()
                    .fg(t::provider_color(name))
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(t::TEXT)
            };
            let detail_style = if is_current {
                Style::default().fg(t::provider_color(name))
            } else {
                Style::default().fg(t::MUTED)
            };
            // Pad name to max_name_len so the cursor indicator stays in a fixed column.
            let padded_name = format!("{:<width$}", name.as_str(), width = max_name_len);
            let name_cell = Cell::from(Line::from(vec![
                Span::styled(padded_name, name_style),
                Span::styled(indicator, indicator_style),
            ]));

            let notes_first_line = provider.notes.lines().next().unwrap_or("");
            let notes_text = if notes_first_line.width() > notes_col as usize {
                format!(
                    "{}…",
                    &notes_first_line[..notes_first_line
                        .char_indices()
                        .map(|(i, _)| i)
                        .nth(notes_col.saturating_sub(1) as usize)
                        .unwrap_or(notes_first_line.len())]
                )
            } else {
                notes_first_line.to_string()
            };
            Row::new(vec![
                name_cell,
                Cell::from(Span::styled(provider.api_format.to_string(), detail_style)),
                Cell::from(Span::styled(provider.base_url.as_str(), detail_style)),
                masked_api_key(&provider.api_key),
                Cell::from(Span::styled(notes_text, detail_style)),
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(name_col),
            Constraint::Length(12),
            Constraint::Length(url_col),
            Constraint::Length(key_col),
            Constraint::Length(notes_col),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
            .border_style(Style::default().fg(t::MUTED))
            .padding(Padding::horizontal(1)),
    )
    .row_highlight_style(Style::default());

    f.render_stateful_widget(table, area, &mut app.table_state);
}

fn draw_detail_panel(f: &mut Frame, app: &App, area: Rect) {
    let border_style = Style::default().fg(t::MUTED);
    let block = Block::default()
        .borders(Borders::LEFT | Borders::RIGHT)
        .border_style(border_style)
        .padding(Padding::horizontal(1));

    // Show error toast only when not in editing mode (errors in form are shown inline)
    if app.mode == Mode::Normal {
        if let Some((msg, MessageKind::Error, _)) = &app.message {
            let error_block = Block::default()
                .borders(Borders::LEFT | Borders::RIGHT)
                .border_style(Style::default().fg(t::ERROR))
                .padding(Padding::horizontal(1));
            let lines = vec![
                Line::from(""),
                Line::from(Span::styled(
                    "Error",
                    Style::default().fg(t::ERROR).add_modifier(Modifier::BOLD),
                )),
                Line::from(vec![
                    Span::styled(
                        "✗ ",
                        Style::default().fg(t::ERROR).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(msg.as_str(), Style::default().fg(t::TEXT)),
                ]),
            ];
            f.render_widget(Paragraph::new(lines).block(error_block), area);
            return;
        }
    }

    let label = Style::default().fg(t::MUTED);
    let title_line = Line::from(Span::styled(
        "Info",
        Style::default().fg(t::TEXT).add_modifier(Modifier::BOLD),
    ));

    let Some(name) = app
        .table_state
        .selected()
        .and_then(|i| app.provider_names.get(i))
    else {
        f.render_widget(
            Paragraph::new(vec![Line::from(""), title_line]).block(block),
            area,
        );
        return;
    };

    let mut lines = vec![Line::from(""), title_line];
    if app.pending_tests.contains(name.as_str()) {
        let prev = app.test_results.get(name.as_str());
        let latency_str = prev
            .map(|r| fmt_latency(r.latency_ms))
            .unwrap_or_else(|| "—".to_string());
        let models_str = prev
            .and_then(|r| r.model_count)
            .map(|n| format!("{n} models"))
            .unwrap_or_else(|| "—".to_string());
        lines.push(Line::from(vec![
            Span::styled("Status ", label),
            Span::styled(
                "Testing",
                Style::default().fg(t::MUTED).add_modifier(Modifier::ITALIC),
            ),
            Span::styled("   Latency ", label),
            Span::styled(latency_str, Style::default().fg(t::MUTED)),
            Span::styled("   Models ", label),
            Span::styled(models_str, Style::default().fg(t::MUTED)),
        ]));
    } else if let Some(r) = app.test_results.get(name.as_str()) {
        let (status_str, status_style) = match &r.status {
            TestStatus::Ok => (
                "✓ OK".to_string(),
                Style::default().fg(t::SUCCESS).add_modifier(Modifier::BOLD),
            ),
            TestStatus::AuthFailed => (
                "✗ Auth failed".to_string(),
                Style::default().fg(t::ERROR).add_modifier(Modifier::BOLD),
            ),
            TestStatus::Error(e) => (truncate_error(e), Style::default().fg(t::ERROR)),
        };
        let models_str = match r.model_count {
            Some(n) => Span::styled(format!("{n} models"), Style::default().fg(t::TEXT)),
            None => Span::styled("—", Style::default().fg(t::MUTED)),
        };
        lines.push(Line::from(vec![
            Span::styled("Status ", label),
            Span::styled(status_str, status_style),
            Span::styled("   Latency ", label),
            Span::styled(fmt_latency(r.latency_ms), Style::default().fg(t::TEXT)),
            Span::styled("   Models ", label),
            models_str,
        ]));
    } else {
        lines.push(Line::from(vec![
            Span::styled("Press ", Style::default().fg(t::MUTED)),
            Span::styled(
                "[t]",
                Style::default().fg(t::PRIMARY).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" to test connectivity", Style::default().fg(t::MUTED)),
        ]));
    }

    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_keybindings(f: &mut Frame, app: &App, area: Rect) {
    let bg_label = if app.bg_proxy_pid.is_some() {
        "Stop"
    } else {
        "Server"
    };
    // (key, desc, key_color, desc_color)
    let all_keys: &[(&str, &str, Color, Color)] = &[
        ("j/k", "Nav", t::MUTED, t::MUTED),
        ("s", "Switch", t::PRIMARY, t::MUTED),
        ("a", "Add", t::PRIMARY, t::MUTED),
        ("e", "Edit", t::PRIMARY, t::MUTED),
        ("dd", "Delete", t::WARNING, t::MUTED),
        ("f", "Fallback", t::PRIMARY, t::MUTED),
        ("S", bg_label, t::PRIMARY, t::MUTED),
        ("c", "Clear", t::WARNING, t::MUTED),
        ("q", "Quit", t::WARNING, t::MUTED),
        ("h", "Help", t::MUTED, t::MUTED),
    ];

    let max_width = area.width as usize;
    let mut spans: Vec<Span> = vec![Span::raw(" ")];
    let mut used = 1usize;
    let mut first = true;

    for (key, desc, key_color, desc_color) in all_keys.iter() {
        let sep = if first { 0 } else { 2 };
        // "[k]" (3) + " " (1) + desc
        let item_len = sep + 3 + 1 + desc.len();
        if used + item_len > max_width {
            break;
        }
        if !first {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(
            format!("[{}]", key),
            Style::default().fg(*key_color),
        ));
        spans.push(Span::styled(
            format!(" {}", desc),
            Style::default().fg(*desc_color),
        ));
        used += item_len;
        first = false;
    }

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_stats_panel(f: &mut Frame, app: &App, area: Rect) {
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
            if s.requests == 0 {
                f64::MAX
            } else {
                s.failures as f64 / s.requests as f64
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
        .map(|s| s.len())
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
                format!("{:<width$}", name, width = id_col_width),
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
                let rate = if s.requests > 0 {
                    s.failures as f64 / s.requests as f64
                } else {
                    0.0
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
    lines.push(Line::from(Span::styled(
        "By Model",
        Style::default().fg(t::TEXT).add_modifier(Modifier::BOLD),
    )));
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
    inactive_models.sort_unstable_by_key(|(name, _)| *name);

    if model_entries.is_empty() && inactive_models.is_empty() {
        lines.push(Line::from(Span::styled("  No data yet", muted)));
    } else {
        // Cap label width at 30 chars to leave room for bars
        let model_col_width = model_entries
            .iter()
            .map(|(k, _, _)| k.chars().count())
            .max()
            .unwrap_or(10)
            .min(30);
        let value_width = 8usize; // "  1234.5K"
        let bar_area = (inner.width as usize).saturating_sub(model_col_width + 2 + value_width);
        let max_total = model_entries
            .iter()
            .map(|(_, i, o)| i + o)
            .max()
            .unwrap_or(1);

        for (model, input, output) in &model_entries {
            let total = input + output;
            let total_bar = if bar_area > 0 {
                (total * bar_area as u64 / max_total) as usize
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

fn truncate_error(e: &str) -> String {
    // Strip verbose reqwest prefix: "Connection failed: error sending request for url (...): <cause>"
    let msg = if let Some(pos) = e.rfind(": ") {
        let suffix = &e[pos + 2..];
        // Only use suffix if it's meaningfully shorter and not a URL
        if suffix.len() < e.len() / 2 && !suffix.starts_with("http") {
            suffix
        } else {
            e.split(':').next().unwrap_or(e)
        }
    } else {
        e
    };
    const MAX: usize = 30;
    if msg.chars().count() > MAX {
        let truncated: String = msg.chars().take(MAX).collect();
        format!("{}…", truncated)
    } else {
        msg.to_string()
    }
}

fn fmt_latency(ms: u64) -> String {
    if ms >= 1000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{ms}ms")
    }
}

fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        format!("{n}")
    }
}

/// Column width = max(header length, max content length) + 4 gap.
fn col_width(header: &str, content_lens: impl Iterator<Item = usize>) -> u16 {
    (content_lens.max().unwrap_or(0).max(header.len()) + 4) as u16
}

fn api_key_display_len(key: &str) -> usize {
    if key.is_empty() {
        "(not set)".len()
    } else if key.starts_with('$') {
        key.chars().count()
    } else if key.chars().count() > 8 {
        11 // "abcd···wxyz"
    } else {
        4 // "····"
    }
}

/// Mask a raw API key for display: `abcd···wxyz` (long) or `····` (short).
/// Returns the key unchanged if it is empty or starts with `$` (env-var ref).
fn mask_api_key_str(key: &str) -> Option<String> {
    if key.is_empty() || key.starts_with('$') {
        return None;
    }
    let n = key.chars().count();
    Some(if n > 8 {
        let prefix: String = key.chars().take(4).collect();
        let suffix: String = key.chars().skip(n - 4).collect();
        format!("{prefix}···{suffix}")
    } else {
        "····".to_string()
    })
}

fn masked_api_key(key: &str) -> Cell<'static> {
    match mask_api_key_str(key) {
        Some(masked) => Cell::from(Span::styled(masked, Style::default().fg(t::MUTED))),
        None if key.is_empty() => {
            Cell::from(Span::styled("(not set)", Style::default().fg(t::MUTED)))
        }
        None => Cell::from(Span::styled(
            key.to_string(),
            Style::default().fg(t::WARNING),
        )),
    }
}

fn draw_help(f: &mut Frame, _app: &App) {
    // Section: (heading, &[(key, desc)])
    type Section = (&'static str, &'static [(&'static str, &'static str)]);
    let sections: &[Section] = &[
        (
            "Provider List",
            &[
                ("j / k / ↑↓", "Navigate providers"),
                ("gg / G", "Go to top / bottom"),
                ("s", "Switch to selected provider"),
                ("a / o", "Add new provider"),
                ("e / Enter", "Edit selected provider"),
                ("dd", "Delete selected provider"),
                ("t", "Test provider connectivity"),
                ("K / J", "Move provider up / down"),
                ("f", "Toggle fallback mode"),
                ("r", "Reload config from disk"),
                ("S", "Toggle background proxy"),
                ("c", "Clear usage data"),
                ("q / Esc", "Quit"),
                ("h / ?", "Show this help"),
            ],
        ),
        (
            "Provider Editor  (default: Normal mode)",
            &[
                ("i / a", "Enter Insert mode"),
                ("Esc", "Exit Insert → Normal  |  Normal → cancel"),
                ("j / k", "Navigate fields (Normal)"),
                ("h / l", "Move cursor / toggle field (Normal)"),
                ("Space", "Toggle format field (Normal)"),
                ("ZZ", "Save and close editor"),
                ("ZQ", "Cancel and close editor"),
                ("Ctrl+S", "Save (works in Insert mode too)"),
                ("Tab / S-Tab", "Next / previous field"),
            ],
        ),
        (
            "Route Rules  (inside editor, Routes section)",
            &[
                ("j / k", "Navigate rules"),
                ("n", "New rule (auto-enters Insert)"),
                ("Space", "Toggle rule enabled / disabled"),
                ("i / Enter", "Edit rule pattern (Insert mode)"),
                ("dd", "Delete selected rule"),
                ("K / J", "Move rule up / down (priority)"),
                ("Esc", "Exit Insert → Normal"),
            ],
        ),
    ];

    let key_w = 14usize;
    let content_width: u16 = 62;

    // Count total lines needed.
    let total_lines: u16 = sections
        .iter()
        .map(|(_, entries)| 2 + entries.len() as u16) // heading blank + heading + entries
        .sum::<u16>()
        + 2; // footer
    let dialog_height = total_lines + 4; // borders + padding

    let area = centered_fixed(content_width, dialog_height, f.area());
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(t::PRIMARY))
        .title(" Help ")
        .title_style(Style::default().fg(t::PRIMARY).add_modifier(Modifier::BOLD))
        .padding(Padding::new(2, 2, 1, 1));
    let inner = block.inner(area);
    f.render_widget(block.clone(), area);

    let mut lines: Vec<Line> = Vec::new();

    for (i, (heading, entries)) in sections.iter().enumerate() {
        if i > 0 {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(
            *heading,
            Style::default()
                .fg(t::TEXT)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        )));
        for (key, desc) in *entries {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {:<width$}", key, width = key_w),
                    Style::default().fg(t::PRIMARY).add_modifier(Modifier::BOLD),
                ),
                Span::styled(*desc, Style::default().fg(t::TEXT)),
            ]));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Press any key to close",
        Style::default().fg(t::MUTED),
    )));

    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn draw_form(f: &mut Frame, app: &App) {
    let Some(form) = &app.form else { return };

    let in_routes = form.in_routes();

    // Provider color: derived from the Name field so it updates live as the user types.
    let prov_color = t::provider_color(form.fields[0].value.trim());

    // ── Title: show action + Vim mode tag ────────────────────────────────────
    let vim_tag = match form.vim_mode {
        VimMode::Normal => "[N]",
        VimMode::Insert => "[I]",
    };
    let title = format!(
        " {} Provider  {} ",
        if form.is_new { "Add" } else { "Edit" },
        vim_tag
    );

    // ── Per-field heights ────────────────────────────────────────────────────
    // Multiline field expands when focused; all others are 3 lines tall.
    let field_heights: Vec<u16> = form
        .fields
        .iter()
        .enumerate()
        .map(|(i, field)| {
            if field.is_multiline && i == form.focused {
                let line_count = field.value.chars().filter(|&c| c == '\n').count() + 1;
                (line_count as u16 + 2).max(3)
            } else {
                3
            }
        })
        .collect();
    let fields_total: u16 = field_heights.iter().sum();

    // Routes section: 1 header line + max(1, rule count) item lines + 1 blank separator.
    let routes_items = form.routes.len().max(1) as u16;
    let routes_height = 1 + routes_items + 1;

    let dialog_height = fields_total + routes_height + 3 + 2 + 2; // fields+routes+hint+borders+pad
    let area = centered_fixed(62, dialog_height, f.area());

    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(t::PRIMARY))
        .title(title.as_str())
        .title_style(Style::default().fg(t::PRIMARY).add_modifier(Modifier::BOLD))
        .padding(Padding::new(2, 2, 1, 1));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Layout: one slot per regular field + routes section + hint.
    let field_constraints: Vec<Constraint> = field_heights
        .iter()
        .map(|&h| Constraint::Length(h))
        .chain(std::iter::once(Constraint::Length(routes_height))) // routes
        .chain(std::iter::once(Constraint::Length(3))) // hint
        .collect();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(field_constraints)
        .split(inner);

    // ── Regular fields ───────────────────────────────────────────────────────
    for (i, field) in form.fields.iter().enumerate() {
        let is_focused = i == form.focused;
        // In Normal vim-mode, show cursor only when the field has focus AND
        // we are also in Insert mode (or the field is a toggle).
        let show_cursor =
            is_focused && field.editable && (form.vim_mode == VimMode::Insert || field.is_toggle);

        let label_style = if is_focused {
            Style::default().fg(t::PRIMARY).add_modifier(Modifier::BOLD)
        } else if !field.editable {
            Style::default().fg(t::MUTED)
        } else {
            Style::default().fg(t::TEXT)
        };

        let value_display = if field.is_toggle {
            let selected = Style::default()
                .fg(prov_color)
                .add_modifier(Modifier::REVERSED | Modifier::BOLD);
            let unselected = Style::default().fg(t::MUTED);
            let (left, right) = if field.value == "anthropic" {
                (
                    Span::styled(" anthropic ", selected),
                    Span::styled(" openai ", unselected),
                )
            } else {
                (
                    Span::styled(" anthropic ", unselected),
                    Span::styled(" openai ", selected),
                )
            };
            Line::from(vec![
                Span::styled(format!("{:<10}", field.label), label_style),
                left,
                Span::raw(" "),
                right,
            ])
        } else if field.is_multiline {
            if show_cursor {
                let cursor_pos = field.cursor.min(field.value.len());
                let before_cursor = &field.value[..cursor_pos];
                let cursor_row = before_cursor.chars().filter(|&c| c == '\n').count() as u16;
                let last_nl = before_cursor.rfind('\n').map(|p| p + 1).unwrap_or(0);
                let cursor_col = before_cursor[last_nl..].width() as u16;
                let lines: Vec<Line> = field
                    .value
                    .split('\n')
                    .enumerate()
                    .map(|(row, line)| {
                        if row == cursor_row as usize {
                            let col = cursor_col as usize;
                            let byte_col = line
                                .char_indices()
                                .nth(col)
                                .map(|(b, _)| b)
                                .unwrap_or(line.len());
                            let before = &line[..byte_col];
                            let cursor_char = line[byte_col..].chars().next().unwrap_or(' ');
                            let after_start =
                                byte_col + cursor_char.len_utf8().min(line.len() - byte_col);
                            let after = &line[after_start..];
                            Line::from(vec![
                                Span::raw(before.to_string()),
                                Span::styled(
                                    cursor_char.to_string(),
                                    Style::default()
                                        .fg(prov_color)
                                        .add_modifier(Modifier::REVERSED),
                                ),
                                Span::raw(after.to_string()),
                            ])
                        } else {
                            Line::from(line.to_string())
                        }
                    })
                    .collect();
                let label_line =
                    Line::from(Span::styled(format!("{:<10}", field.label), label_style));
                let mut all_lines = vec![label_line];
                all_lines.extend(lines);
                f.render_widget(Paragraph::new(all_lines), chunks[i]);
                continue;
            } else {
                let first_line = field.value.lines().next().unwrap_or("");
                let label_line =
                    Line::from(Span::styled(format!("{:<10}", field.label), label_style));
                let content_chars: Vec<char> = first_line.chars().collect();
                let max_w = chunks[i].width.saturating_sub(2) as usize;
                let display_str = if content_chars.len() > max_w && max_w > 1 {
                    let truncated: String = content_chars[..max_w - 1].iter().collect();
                    format!("{}\u{2026}", truncated)
                } else {
                    first_line.to_string()
                };
                let content_line =
                    Line::from(Span::styled(display_str, Style::default().fg(t::MUTED)));
                f.render_widget(Paragraph::new(vec![label_line, content_line]), chunks[i]);
                continue;
            }
        } else {
            let display_val = if field.label == "API Key" && !is_focused {
                mask_api_key_str(&field.value).unwrap_or_else(|| field.value.clone())
            } else {
                field.value.clone()
            };

            if show_cursor {
                let cursor_pos = field.cursor.min(display_val.len());
                let before = display_val[..cursor_pos].to_string();
                let cursor_char = display_val[cursor_pos..].chars().next().unwrap_or(' ');
                let after_start =
                    cursor_pos + cursor_char.len_utf8().min(display_val.len() - cursor_pos);
                let after = display_val[after_start..].to_string();
                let (before_span, after_span) = if field.label == "Name" && !display_val.is_empty()
                {
                    let color = t::provider_color(&display_val);
                    (
                        Span::styled(before, Style::default().fg(color)),
                        Span::styled(after, Style::default().fg(color)),
                    )
                } else {
                    (Span::raw(before), Span::raw(after))
                };
                Line::from(vec![
                    Span::styled(format!("{:<10}", field.label), label_style),
                    before_span,
                    Span::styled(
                        cursor_char.to_string(),
                        Style::default()
                            .fg(prov_color)
                            .add_modifier(Modifier::REVERSED),
                    ),
                    after_span,
                ])
            } else {
                let val_style = if field.label == "Name" && !display_val.is_empty() {
                    Style::default().fg(t::provider_color(&display_val))
                } else if !field.editable {
                    Style::default().fg(t::MUTED)
                } else {
                    Style::default()
                };
                Line::from(vec![
                    Span::styled(format!("{:<10}", field.label), label_style),
                    Span::styled(display_val, val_style),
                ])
            }
        };

        f.render_widget(Paragraph::new(value_display), chunks[i]);
    }

    // ── Routes section ───────────────────────────────────────────────────────
    let routes_chunk = chunks[form.fields.len()];
    let routes_label_style = if in_routes {
        Style::default().fg(t::PRIMARY).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(t::TEXT)
    };

    let mut routes_lines: Vec<Line> =
        vec![Line::from(Span::styled("Routes    ", routes_label_style))];

    if form.routes.is_empty() {
        routes_lines.push(Line::from(Span::styled(
            "  (no rules)",
            Style::default().fg(t::MUTED),
        )));
    } else {
        for (i, rule) in form.routes.iter().enumerate() {
            let is_selected = in_routes && i == form.route_cursor;
            let toggle_ch = if rule.enabled { '✓' } else { '✗' };
            let toggle_style = if rule.enabled {
                Style::default().fg(t::SUCCESS)
            } else {
                Style::default().fg(t::MUTED)
            };

            if is_selected && form.route_editing {
                // Insert mode: show cursor within pattern.
                let pat = &rule.pattern;
                let cursor_pos = form.route_pat_cursor.min(pat.len());
                let before = &pat[..cursor_pos];
                let cursor_char = pat[cursor_pos..].chars().next().unwrap_or(' ');
                let after_start = if cursor_pos < pat.len() {
                    cursor_pos + cursor_char.len_utf8()
                } else {
                    cursor_pos
                };
                let after = if after_start <= pat.len() {
                    &pat[after_start..]
                } else {
                    ""
                };
                routes_lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(format!("[{toggle_ch}] "), toggle_style),
                    Span::raw(before.to_string()),
                    Span::styled(
                        cursor_char.to_string(),
                        Style::default()
                            .fg(prov_color)
                            .add_modifier(Modifier::REVERSED),
                    ),
                    Span::raw(after.to_string()),
                ]));
            } else if is_selected {
                // Normal mode: highlight selected rule.
                routes_lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(format!("[{toggle_ch}] "), Style::default().fg(t::PRIMARY)),
                    Span::styled(
                        rule.pattern.as_str(),
                        Style::default().fg(t::PRIMARY).add_modifier(Modifier::BOLD),
                    ),
                ]));
            } else {
                let pat_style = if rule.enabled {
                    Style::default().fg(t::TEXT)
                } else {
                    Style::default().fg(t::MUTED)
                };
                routes_lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(format!("[{toggle_ch}] "), toggle_style),
                    Span::styled(rule.pattern.as_str(), pat_style),
                ]));
            }
        }
    }
    routes_lines.push(Line::from(""));
    f.render_widget(Paragraph::new(routes_lines), routes_chunk);

    // ── Hint bar ─────────────────────────────────────────────────────────────
    let hint_idx = form.fields.len() + 1;
    if hint_idx < chunks.len() {
        let hint_line = if in_routes {
            if form.route_editing {
                // Route Insert mode.
                Line::from(vec![
                    Span::raw("   "),
                    Span::styled("Esc", Style::default().fg(t::WARNING)),
                    Span::styled(" Normal  ", Style::default().fg(t::MUTED)),
                    Span::styled("Ctrl+S", Style::default().fg(t::SUCCESS)),
                    Span::styled(" Save  ", Style::default().fg(t::MUTED)),
                    Span::styled("←/→", Style::default().fg(t::PRIMARY)),
                    Span::styled(" Move cursor", Style::default().fg(t::MUTED)),
                ])
            } else {
                // Route Normal mode.
                Line::from(vec![
                    Span::raw("   "),
                    Span::styled("n", Style::default().fg(t::SUCCESS)),
                    Span::styled(" New  ", Style::default().fg(t::MUTED)),
                    Span::styled("Space", Style::default().fg(t::PRIMARY)),
                    Span::styled(" Toggle  ", Style::default().fg(t::MUTED)),
                    Span::styled("dd", Style::default().fg(t::WARNING)),
                    Span::styled(" Del  ", Style::default().fg(t::MUTED)),
                    Span::styled("i/Enter", Style::default().fg(t::PRIMARY)),
                    Span::styled(" Edit  ", Style::default().fg(t::MUTED)),
                    Span::styled("^S", Style::default().fg(t::SUCCESS)),
                    Span::styled(" Save  ", Style::default().fg(t::MUTED)),
                    Span::styled("q", Style::default().fg(t::WARNING)),
                    Span::styled(" Quit", Style::default().fg(t::MUTED)),
                ])
            }
        } else if form.vim_mode == VimMode::Insert {
            // Field Insert mode.
            let focused_field = &form.fields[form.focused];
            if focused_field.is_multiline {
                Line::from(vec![
                    Span::raw("   "),
                    Span::styled("Esc", Style::default().fg(t::WARNING)),
                    Span::styled("/", Style::default().fg(t::MUTED)),
                    Span::styled("^S", Style::default().fg(t::WARNING)),
                    Span::styled(" Normal  ", Style::default().fg(t::MUTED)),
                    Span::styled("Enter", Style::default().fg(t::PRIMARY)),
                    Span::styled(" Newline  ", Style::default().fg(t::MUTED)),
                    Span::styled("^S", Style::default().fg(t::SUCCESS)),
                    Span::styled("(N) Save", Style::default().fg(t::MUTED)),
                ])
            } else {
                Line::from(vec![
                    Span::raw("   "),
                    Span::styled("Esc", Style::default().fg(t::WARNING)),
                    Span::styled("/", Style::default().fg(t::MUTED)),
                    Span::styled("^S", Style::default().fg(t::WARNING)),
                    Span::styled(" Normal  ", Style::default().fg(t::MUTED)),
                    Span::styled("Tab", Style::default().fg(t::PRIMARY)),
                    Span::styled(" Next field", Style::default().fg(t::MUTED)),
                ])
            }
        } else {
            // Field Normal mode.
            Line::from(vec![
                Span::raw("   "),
                Span::styled("i", Style::default().fg(t::PRIMARY)),
                Span::styled(" Insert  ", Style::default().fg(t::MUTED)),
                Span::styled("j/k", Style::default().fg(t::PRIMARY)),
                Span::styled(" Field  ", Style::default().fg(t::MUTED)),
                Span::styled("^S", Style::default().fg(t::SUCCESS)),
                Span::styled(" Save  ", Style::default().fg(t::MUTED)),
                Span::styled("q", Style::default().fg(t::WARNING)),
                Span::styled(" Quit", Style::default().fg(t::MUTED)),
            ])
        };

        let mut hint_lines = vec![hint_line, Line::from("")];
        if let Some(err) = &form.error {
            hint_lines.push(Line::from(Span::styled(
                format!("✗ {}", err),
                Style::default().fg(t::ERROR),
            )));
        }
        f.render_widget(Paragraph::new(hint_lines), chunks[hint_idx]);
    }
}

fn draw_confirm(f: &mut Frame, app: &App) {
    let area = centered_rect(40, 20, f.area());
    let area = Rect {
        height: area.height.max(5),
        ..area
    };

    f.render_widget(Clear, area);

    let prompt_line = match &app.confirm_action {
        Some(ConfirmAction::Delete(id)) => Line::from(vec![
            Span::raw("  Delete "),
            Span::styled(
                id.as_str(),
                Style::default().fg(t::ERROR).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" ?"),
        ]),
        Some(ConfirmAction::Clear) => Line::from(vec![
            Span::raw("  "),
            Span::styled("Clear all usage data", Style::default().fg(t::ERROR)),
            Span::raw(" ?"),
        ]),
        Some(ConfirmAction::Quit) => Line::from(vec![
            Span::raw("  "),
            Span::styled("Quit", Style::default().fg(t::ERROR)),
            Span::raw(" ?"),
        ]),
        None => Line::from(""),
    };

    let text = vec![
        Line::from(""),
        prompt_line,
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("y", Style::default().fg(t::SUCCESS)),
            Span::styled(" Yes    ", Style::default().fg(t::MUTED)),
            Span::styled("n", Style::default().fg(t::WARNING)),
            Span::styled(" No", Style::default().fg(t::MUTED)),
        ]),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(t::ERROR))
        .title(" Confirm ")
        .title_style(Style::default().fg(t::ERROR).add_modifier(Modifier::BOLD))
        .padding(Padding::horizontal(1));

    f.render_widget(
        Paragraph::new(text).block(block).wrap(Wrap { trim: false }),
        area,
    );
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

fn centered_fixed(percent_x: u16, height: u16, r: Rect) -> Rect {
    let height = height.min(r.height);
    let v_margin = (r.height - height) / 2;
    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(v_margin),
            Constraint::Length(height),
            Constraint::Min(0),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vert[1])[1]
}

fn config_path_display() -> String {
    crate::config::config_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "~/.ccs/config.json".to_string())
}
