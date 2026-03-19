use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Padding, Paragraph, Row, Table, Wrap};
use ratatui::Frame;

use super::app::{App, ConfirmAction, MessageKind, Mode};
use crate::test_provider::TestStatus;
use super::theme::{self as t};

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
    let fallback_label = if app.config.fallback { "Fallback on  " } else { "Fallback off " };
    let listen_label = format!("{}  ", app.config.listen);
    let version = format!("  v{}", env!("CARGO_PKG_VERSION"));
    let title_left = " CCS  Claude Code Switcher";
    let left_len = title_left.len() + version.len();
    let right_len = listen_label.len() + fallback_label.len();
    let gap = (area.width as usize).saturating_sub(left_len + right_len);

    let spans: Vec<Span> = vec![
        Span::styled(" CCS ", Style::default().fg(Color::Black).bg(t::SUCCESS)),
        Span::raw(" "),
        Span::styled(
            "Claude Code Switcher",
            Style::default().fg(t::SUCCESS).add_modifier(Modifier::BOLD),
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
    let table_height = (app.provider_names.len() as u16 + 2).max(3).min(area.height * 2 / 3);
    let detail_height = 3u16;
    // stats: blank + title + N provider rows + bottom border
    let n_providers = app.provider_names.len() as u16;
    let stats_min_height = 3 + n_providers;
    // model: blank + title + N model rows + bottom border
    let n_models = app.metrics.lock().map(|m| m.by_model.len() as u16).unwrap_or(0);
    let model_min_height = (3 + n_models).max(3);

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
                Span::styled("a", Style::default().fg(t::WARNING).add_modifier(Modifier::BOLD)),
                Span::styled(" to add a provider, or edit ", Style::default().fg(t::MUTED)),
                Span::styled(
                    config_path_display(),
                    Style::default().fg(t::PRIMARY),
                ),
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

    let url_col = col_width("Base URL", app.config.providers.values().map(|p| p.base_url.len()));
    let key_col = col_width("API Key",  app.config.providers.values().map(|p| api_key_display_len(&p.api_key)));
    // Name col = longest name + 2 for the " ◀" indicator + 4 gap
    let max_name_len = app.provider_names.iter().map(|name| name.len()).max().unwrap_or(0).max("Name".len());
    let name_col = (max_name_len + 2 + 4) as u16;

    let header = Row::new(vec![
        Cell::from("Name").style(Style::default().fg(t::MUTED).add_modifier(Modifier::BOLD)),
        Cell::from("Format").style(Style::default().fg(t::MUTED).add_modifier(Modifier::BOLD)),
        Cell::from("Base URL").style(Style::default().fg(t::MUTED).add_modifier(Modifier::BOLD)),
        Cell::from("API Key").style(Style::default().fg(t::MUTED).add_modifier(Modifier::BOLD)),
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

            // Cursor triangle shown only on the selected row.
            let (indicator, indicator_style) = if is_selected {
                (" ◀", Style::default().fg(t::SUCCESS).add_modifier(Modifier::BOLD))
            } else {
                ("  ", Style::default())
            };
            // Current provider name is green+bold regardless of cursor position.
            let name_style = if is_current {
                Style::default().fg(t::SUCCESS).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(t::TEXT)
            };
            // Pad name to max_name_len so the cursor indicator stays in a fixed column.
            let padded_name = format!("{:<width$}", name.as_str(), width = max_name_len);
            let name_cell = Cell::from(Line::from(vec![
                Span::styled(padded_name, name_style),
                Span::styled(indicator, indicator_style),
            ]));

            let api_key_cell = masked_api_key(&provider.api_key);

            Row::new(vec![
                name_cell,
                Cell::from(Span::styled(provider.api_format.to_string(), Style::default().fg(t::MUTED))),
                Cell::from(Span::styled(provider.base_url.as_str(), Style::default().fg(t::MUTED))),
                api_key_cell,
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
                Line::from(Span::styled("Error", Style::default().fg(t::ERROR).add_modifier(Modifier::BOLD))),
                Line::from(vec![
                    Span::styled("✗ ", Style::default().fg(t::ERROR).add_modifier(Modifier::BOLD)),
                    Span::styled(msg.as_str(), Style::default().fg(t::TEXT)),
                ]),
            ];
            f.render_widget(Paragraph::new(lines).block(error_block), area);
            return;
        }
    }

    let label = Style::default().fg(t::MUTED);
    let title_line = Line::from(Span::styled("Info", Style::default().fg(t::MUTED).add_modifier(Modifier::BOLD)));

    let Some(name) = app
        .table_state
        .selected()
        .and_then(|i| app.provider_names.get(i))
    else {
        f.render_widget(Paragraph::new(vec![Line::from(""), title_line]).block(block), area);
        return;
    };

    let mut lines = vec![Line::from(""), title_line];
    if app.pending_tests.contains(name.as_str()) {
        let prev = app.test_results.get(name.as_str());
        let latency_str = prev.map(|r| fmt_latency(r.latency_ms)).unwrap_or_else(|| "—".to_string());
        let models_str = prev.and_then(|r| r.model_count).map(|n| format!("{n} models")).unwrap_or_else(|| "—".to_string());
        lines.push(Line::from(vec![
            Span::styled("Status ", label),
            Span::styled("Testing", Style::default().fg(t::MUTED).add_modifier(Modifier::ITALIC)),
            Span::styled("   Latency ", label),
            Span::styled(latency_str, Style::default().fg(t::MUTED)),
            Span::styled("   Models ", label),
            Span::styled(models_str, Style::default().fg(t::MUTED)),
        ]));
    } else if let Some(r) = app.test_results.get(name.as_str()) {
        let (status_str, status_style) = match &r.status {
            TestStatus::Ok => ("✓ OK".to_string(), Style::default().fg(t::SUCCESS).add_modifier(Modifier::BOLD)),
            TestStatus::AuthFailed => ("✗ Auth failed".to_string(), Style::default().fg(t::ERROR).add_modifier(Modifier::BOLD)),
            TestStatus::Error(e) => (truncate_error(e), Style::default().fg(t::ERROR)),
        };
        let models_str = match r.model_count {
            Some(n) => Span::styled(format!("{n} models"), Style::default().fg(t::TEXT)),
            None    => Span::styled("—", Style::default().fg(t::MUTED)),
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
            Span::styled("[t]", Style::default().fg(t::PRIMARY).add_modifier(Modifier::BOLD)),
            Span::styled(" to test connectivity", Style::default().fg(t::MUTED)),
        ]));
    }

    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_keybindings(f: &mut Frame, app: &App, area: Rect) {
    let bg_label = if app.bg_proxy_pid.is_some() { "Stop" } else { "Server" };
    // (key, desc, key_color, desc_color)
    let all_keys: &[(&str, &str, Color, Color)] = &[
        ("s", "Switch",   t::SUCCESS, t::MUTED),
        ("a", "Add",      t::SUCCESS, t::MUTED),
        ("e", "Edit",     t::SUCCESS, t::MUTED),
        ("f", "Fallback", t::SUCCESS, t::MUTED),
        ("c", "Clear",    t::SUCCESS, t::MUTED),
        ("q", "Quit",     t::SUCCESS, t::MUTED),
        ("S", bg_label,   t::SUCCESS, t::MUTED),
        ("h", "Help",     t::MUTED,   t::MUTED),
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
        spans.push(Span::styled(format!("[{}]", key), Style::default().fg(*key_color)));
        spans.push(Span::styled(format!(" {}", desc), Style::default().fg(*desc_color)));
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
        .map(|name| (name.as_str(), m.by_provider.get(name).cloned().unwrap_or_default()))
        .collect();
    let mut model_entries: Vec<(String, u64, u64)> = m
        .by_model
        .iter()
        .map(|(k, v)| (k.clone(), v.input, v.output))
        .collect();
    drop(m);

    // Sort by failure rate ascending (providers with fewer failures first).
    // Providers with no requests sort to the bottom (rate treated as 1.0).
    provider_rows.sort_by(|(_, a), (_, b)| {
        let rate = |s: &crate::proxy::metrics::ProviderStats| {
            if s.requests == 0 { f64::MAX } else { s.failures as f64 / s.requests as f64 }
        };
        rate(a).partial_cmp(&rate(b)).unwrap_or(std::cmp::Ordering::Equal)
    });
    model_entries.sort_by(|a, b| (b.1 + b.2).cmp(&(a.1 + a.2)));

    let muted = Style::default().fg(t::MUTED);
    let id_col_width = app.provider_names.iter().map(|s| s.len()).max().unwrap_or(8).max(8);

    let dash_line = Line::from(Span::styled("╌".repeat(inner.width as usize), Style::default().fg(t::MUTED)));

    let mut lines: Vec<Line> = vec![
        Line::from(""),
        dash_line.clone(),
        Line::from(Span::styled("Token Usage", Style::default().fg(t::MUTED).add_modifier(Modifier::BOLD))),
        Line::from(""),
    ];
    lines.extend(provider_rows.iter().map(|(name, s)| {
        let color = t::provider_color(name);
        Line::from(vec![
            Span::styled(
                format!("{:<width$}", name, width = id_col_width),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  In ",  muted),
            Span::styled(format!("{:>7}", format_tokens(s.input)),  Style::default().fg(color)),
            Span::styled("  Out ", muted),
            Span::styled(format!("{:>7}", format_tokens(s.output)), Style::default().fg(color)),
            Span::styled("  Req ", muted),
            Span::styled(format!("{:>4}", s.requests), Style::default().fg(t::TEXT).add_modifier(Modifier::BOLD)),
            Span::styled("  Fail ", muted),
            {
                let rate = if s.requests > 0 { s.failures as f64 / s.requests as f64 } else { 0.0 };
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
    lines.push(Line::from(Span::styled("By Model", Style::default().fg(t::MUTED).add_modifier(Modifier::BOLD))));
    lines.push(Line::from(""));

    // Collect all known model names from test_results (fetched via /v1/models per provider).
    // inactive_models: (model_name, in_current_provider)
    let current_provider = app.config.current.as_str();
    let used_models: std::collections::HashSet<&str> =
        model_entries.iter().map(|(k, _, _)| k.as_str()).collect();

    // Build set of model names per provider from test results
    let current_provider_models: std::collections::HashSet<&str> = app
        .test_results
        .get(current_provider)
        .and_then(|r| r.model_names.as_ref())
        .map(|names| names.iter().map(|s| s.as_str()).collect())
        .unwrap_or_default();

    let mut all_known: std::collections::HashMap<&str, bool> = std::collections::HashMap::new();
    for (provider_name, result) in &app.test_results {
        if let Some(names) = &result.model_names {
            let is_current = provider_name == current_provider;
            for name in names {
                let entry = all_known.entry(name.as_str()).or_insert(false);
                if is_current { *entry = true; }
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
        let model_col_width = model_entries.iter().map(|(k, _, _)| k.chars().count()).max().unwrap_or(10).min(30);
        let value_width = 8usize; // "  1234.5K"
        let bar_area = (inner.width as usize).saturating_sub(model_col_width + 2 + value_width);
        let max_total = model_entries.iter().map(|(_, i, o)| i + o).max().unwrap_or(1);

        for (model, input, output) in &model_entries {
            let total = input + output;
            let total_bar = if bar_area > 0 { (total * bar_area as u64 / max_total) as usize } else { 0 };
            let input_bar = if total > 0 { total_bar * (*input as usize) / (total as usize) } else { 0 };
            let output_bar = total_bar.saturating_sub(input_bar);
            let empty = bar_area.saturating_sub(total_bar);

            let model_chars: Vec<char> = model.chars().collect();
            let label = if model_chars.len() > model_col_width {
                let truncated: String = model_chars[..model_col_width.saturating_sub(1)].iter().collect();
                format!("{}…", truncated)
            } else {
                format!("{:<width$}", model, width = model_col_width)
            };

            // Highlight bar if model is available in current provider
            let in_current = current_provider_models.contains(model.as_str());
            let bar_color = if in_current { t::SUCCESS } else { t::MUTED };

            lines.push(Line::from(vec![
                Span::styled(label, Style::default().fg(t::TEXT)),
                Span::raw("  "),
                Span::styled("░".repeat(input_bar),  Style::default().fg(bar_color)),
                Span::styled("█".repeat(output_bar), Style::default().fg(bar_color)),
                Span::raw(" ".repeat(empty)),
                Span::styled(format!("  {:>6}", format_tokens(total)), Style::default().fg(t::TEXT)),
            ]));
        }

        // Inactive models: wrap into rows, green if in current provider, muted otherwise
        if !inactive_models.is_empty() {
            if !model_entries.is_empty() {
                lines.push(Line::from(""));
            }
            // Grid layout: fixed cell width = longest name + 2-space gap
            let cell_width = inactive_models.iter().map(|(m, _)| m.len()).max().unwrap_or(1) + 2;
            let available_width = inner.width as usize;
            let cols = (available_width / cell_width).max(1);
            for chunk in inactive_models.chunks(cols) {
                let spans: Vec<Span> = chunk
                    .iter()
                    .map(|(model, in_current)| {
                        let color = if *in_current { t::SUCCESS } else { t::MUTED };
                        Span::styled(
                            format!("{:<width$}", model, width = cell_width),
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
        4  // "····"
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
    if key.is_empty() {
        Cell::from(Span::styled("(not set)", Style::default().fg(t::MUTED)))
    } else if key.starts_with('$') {
        Cell::from(Span::styled(key.to_string(), Style::default().fg(t::WARNING)))
    } else {
        let masked = mask_api_key_str(key).unwrap_or_default();
        Cell::from(Span::styled(masked, Style::default().fg(t::MUTED)))
    }
}

fn draw_help(f: &mut Frame, _app: &App) {
    let entries: &[(&str, &str)] = &[
        ("s", "Switch to selected provider"),
        ("a", "Add new provider"),
        ("e", "Edit selected provider"),
        ("d", "Delete selected provider"),
        ("t", "Test provider connectivity"),
        ("J / K", "Move provider down / up"),
        ("j / k", "Select next / previous"),
        ("↑ / ↓", "Select next / previous"),
        ("f", "Toggle fallback mode"),
        ("r", "Reload config from disk"),
        ("S", "Toggle background proxy (safe to quit TUI)"),
        ("q / Esc", "Quit"),
        ("h / ?", "Show this help"),
    ];

    let width: u16 = 50;
    let height: u16 = entries.len() as u16 + 4; // entries + border + title + footer
    let area = centered_fixed(
        (width * 100 / f.area().width.max(1)).min(80),
        height,
        f.area(),
    );

    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(t::PRIMARY))
        .title(" Help ")
        .title_style(Style::default().fg(t::PRIMARY).add_modifier(Modifier::BOLD))
        .padding(Padding::new(2, 2, 1, 1));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line> = entries
        .iter()
        .map(|(key, desc)| {
            Line::from(vec![
                Span::styled(format!("{:<10}", key), Style::default().fg(t::PRIMARY).add_modifier(Modifier::BOLD)),
                Span::styled(*desc, Style::default().fg(t::TEXT)),
            ])
        })
        .collect();

    // Footer hint
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("Press any key to close", Style::default().fg(t::MUTED)),
    ]));

    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_form(f: &mut Frame, app: &App) {
    let Some(form) = &app.form else { return };

    let title = if form.is_new {
        " Add Provider "
    } else {
        " Edit Provider "
    };

    // Fixed height: fields*2 + hint(3) + borders(2) + padding(2)
    let dialog_height = (form.fields.len() as u16) * 2 + 3 + 2 + 2;
    let area = centered_fixed(60, dialog_height, f.area());

    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(t::PRIMARY))
        .title(title)
        .title_style(Style::default().fg(t::PRIMARY).add_modifier(Modifier::BOLD))
        .padding(Padding::new(2, 2, 1, 1));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let field_constraints: Vec<Constraint> = form
        .fields
        .iter()
        .map(|_| Constraint::Length(2))
        .chain(std::iter::once(Constraint::Length(3)))
        .collect();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(field_constraints)
        .split(inner);

    for (i, field) in form.fields.iter().enumerate() {
        let is_focused = i == form.focused;
        let label_style = if is_focused {
            Style::default().fg(t::PRIMARY).add_modifier(Modifier::BOLD)
        } else if !field.editable {
            Style::default().fg(t::MUTED)
        } else {
            Style::default().fg(t::TEXT)
        };

        let value_display = if field.is_toggle {
            let (left, right) = if field.value == "anthropic" {
                (
                    Span::styled(" anthropic ", Style::default().fg(t::MUTED).add_modifier(Modifier::REVERSED)),
                    Span::styled(" openai ", Style::default().fg(t::MUTED)),
                )
            } else {
                (
                    Span::styled(" anthropic ", Style::default().fg(t::MUTED)),
                    Span::styled(" openai ", Style::default().fg(t::MUTED).add_modifier(Modifier::REVERSED)),
                )
            };
            Line::from(vec![
                Span::styled(format!("{:<10}", field.label), label_style),
                left,
                Span::raw(" "),
                right,
            ])
        } else {
            let display_val = if field.label == "API Key" && !is_focused {
                mask_api_key_str(&field.value).unwrap_or_else(|| field.value.clone())
            } else {
                field.value.clone()
            };

            if is_focused && field.editable {
                let cursor_pos = field.cursor.min(display_val.len());
                let before = display_val[..cursor_pos].to_string();
                let cursor_char = display_val[cursor_pos..].chars().next().unwrap_or(' ');
                let after_start =
                    cursor_pos + cursor_char.len_utf8().min(display_val.len() - cursor_pos);
                let after = display_val[after_start..].to_string();
                Line::from(vec![
                    Span::styled(format!("{:<10}", field.label), label_style),
                    Span::raw(before),
                    Span::styled(
                        cursor_char.to_string(),
                        Style::default().add_modifier(Modifier::REVERSED),
                    ),
                    Span::raw(after),
                ])
            } else {
                let val_style = if !field.editable {
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

    let hint_idx = form.fields.len();
    if hint_idx < chunks.len() {
        let mut hint_lines = vec![
            Line::from(vec![
                Span::raw("          "),
                Span::styled("Enter", Style::default().fg(t::SUCCESS)),
                Span::styled(" Save  ", Style::default().fg(t::MUTED)),
                Span::styled("Esc", Style::default().fg(t::WARNING)),
                Span::styled(" Cancel", Style::default().fg(t::MUTED)),
            ]),
            Line::from(""),
        ];
        if let Some(err) = &form.error {
            hint_lines.push(
                Line::from(Span::styled(format!("✗ {}", err), Style::default().fg(t::ERROR))),
            );
        }
        f.render_widget(Paragraph::new(hint_lines), chunks[hint_idx]);
    }
}

fn draw_confirm(f: &mut Frame, app: &App) {
    let area = centered_rect(40, 20, f.area());
    let area = Rect { height: area.height.max(5), ..area };

    f.render_widget(Clear, area);

    let prompt_line = match &app.confirm_action {
        Some(ConfirmAction::Delete(id)) => Line::from(vec![
            Span::raw("  Delete "),
            Span::styled(id.as_str(), Style::default().fg(t::ERROR).add_modifier(Modifier::BOLD)),
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

    f.render_widget(Paragraph::new(text).block(block).wrap(Wrap { trim: false }), area);
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
