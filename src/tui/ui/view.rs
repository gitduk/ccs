use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Padding, Paragraph, Row, Table};
use unicode_width::UnicodeWidthStr;

use super::super::state::{App, MessageKind, Mode};
use super::super::theme::{self as t};
use super::format::fmt_latency;
use super::format::truncate_error;
use super::format::{api_key_display_len, col_width, config_path_display, masked_api_key};
use super::layout::{ROUTE_LABEL_WIDTH, pack_routes};
use super::stats_panel::draw_stats_panel;
use crate::tester::TestStatus;

pub(super) fn draw_title_bar(f: &mut Frame, app: &App, area: Rect) {
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

/// Compute the height (in lines) needed by the detail panel.
///
/// This mirrors the line-building logic in [`draw_detail_panel`] so that
/// `draw_main` can allocate an exact `Constraint::Length`.  When adding new
/// lines to the detail panel, update this function as well.
fn detail_panel_height(app: &App, route_avail: usize) -> u16 {
    // Error-toast early-return path: blank + "Error" title + message = 3
    if app.mode == Mode::Normal
        && let Some((_, MessageKind::Error, _)) = &app.message
    {
        return 3;
    }

    // No provider selected: blank + "Info" title = 2
    if app.providers.names.is_empty() {
        return 2;
    }

    // Base lines present for every provider:
    //   1  blank line
    //   1  "Info" title
    //   1  Status / "Press [t]" line
    let base: u16 = 3;

    // Variable lines that differ per provider — take the max so the panel
    // height stays stable when the user navigates between providers.
    let max_extra: u16 = app
        .config
        .providers
        .iter()
        .map(|(name, p)| {
            // Route lines
            let enabled: Vec<_> = p.routes.iter().filter(|r| r.enabled).collect();
            let route_lines = if enabled.is_empty() {
                0u16
            } else {
                pack_routes(&enabled, route_avail).len() as u16
            };

            // Error line (1 if this provider has a last_error)
            let error_line = app
                .metrics
                .lock()
                .map(|m| u16::from(m.last_error.contains_key(name)))
                .unwrap_or(0);

            route_lines + error_line
        })
        .max()
        .unwrap_or(0);

    base + max_extra
}

pub(super) fn draw_main(f: &mut Frame, app: &mut App, area: Rect) {
    let table_height = (app.providers.names.len() as u16 + 2)
        .max(3)
        .min(area.height * 2 / 3);
    // detail panel: LEFT+RIGHT border (2) + horizontal padding (2) = 4 overhead
    let detail_inner_width = (area.width as usize).saturating_sub(4);
    let route_avail = detail_inner_width.saturating_sub(ROUTE_LABEL_WIDTH);
    let detail_height = detail_panel_height(app, route_avail);
    // stats: blank + title + N provider rows + bottom border
    let n_providers = app.providers.names.len() as u16;
    let stats_min_height = 3 + n_providers;
    // model: blank + title + N active rows + inactive grid rows + bottom border
    // Single lock acquisition; single pass over provider_models — no String clones.
    let (n_active, n_inactive): (u16, u16) = {
        if let Ok(m) = app.metrics.lock() {
            let n_active = m
                .by_model
                .values()
                .filter(|s| s.input + s.output > 0)
                .count() as u16;

            // One pass: count inactive models (unique by name) and find max width.
            let mut inactive: std::collections::HashSet<&str> = std::collections::HashSet::new();
            let mut max_name = 1usize;
            for name in app.models.provider_models.values().flat_map(|v| v.iter()) {
                let w = name.width();
                if w > max_name {
                    max_name = w;
                }
                if !m.by_model.contains_key(name.as_str()) {
                    inactive.insert(name.as_str());
                }
            }

            // Estimate grid rows: inactive models laid out with max_name+2 cell width.
            let panel_width = (area.width as usize).saturating_sub(4);
            let cols = (panel_width / (max_name + 2)).max(1);
            let n_inactive = inactive.len().div_ceil(cols) as u16;
            (n_active, n_inactive)
        } else {
            (0, 0)
        }
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

pub(super) fn draw_provider_table(f: &mut Frame, app: &mut App, area: Rect) {
    if app.providers.names.is_empty() {
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
    let notes_widths: Vec<usize> = app
        .config
        .providers
        .values()
        .map(|p| p.notes.lines().next().unwrap_or("").width())
        .collect();
    let has_notes = notes_widths.iter().any(|&w| w > 0);
    let notes_col = if has_notes {
        col_width("Notes", notes_widths.into_iter()).min(30)
    } else {
        0
    };
    // Name col = longest name + 2 for the " ◀" indicator + 4 gap
    let max_name_len = app
        .providers
        .names
        .iter()
        .map(|name| name.width())
        .max()
        .unwrap_or(0)
        .max("Name".width());
    let name_col = (max_name_len + 2 + 4) as u16;

    let mut header_cells = vec![
        Cell::from("Name").style(Style::default().fg(t::TEXT).add_modifier(Modifier::BOLD)),
        Cell::from("Format").style(Style::default().fg(t::TEXT).add_modifier(Modifier::BOLD)),
        Cell::from("Base URL").style(Style::default().fg(t::TEXT).add_modifier(Modifier::BOLD)),
        Cell::from("API Key").style(Style::default().fg(t::TEXT).add_modifier(Modifier::BOLD)),
    ];
    if has_notes {
        header_cells.push(
            Cell::from("Notes").style(Style::default().fg(t::TEXT).add_modifier(Modifier::BOLD)),
        );
    }
    let header = Row::new(header_cells).height(1);

    let selected = app.providers.table_state.selected();

    let rows: Vec<Row> = app
        .providers
        .names
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
            let disabled = !provider.enabled;
            // Disabled providers are fully muted; current is provider color; others normal.
            let name_style = if disabled {
                Style::default().fg(t::MUTED)
            } else if is_current {
                Style::default()
                    .fg(t::provider_color(name))
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(t::TEXT)
            };
            let detail_style = if disabled || !is_current {
                Style::default().fg(t::MUTED)
            } else {
                Style::default().fg(t::provider_color(name))
            };
            // Pad name to max_name_len display columns so the cursor indicator stays
            // in a fixed column. format! pads by char count, not display width, so we
            // compute the visual width and append spaces manually.
            let name_display_width = name.width();
            let padding = max_name_len.saturating_sub(name_display_width);
            let padded_name = format!("{}{}", name.as_str(), " ".repeat(padding));
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
            let mut cells = vec![
                name_cell,
                Cell::from(Span::styled(provider.api_format.to_string(), detail_style)),
                Cell::from(Span::styled(provider.base_url.as_str(), detail_style)),
                masked_api_key(&provider.api_key),
            ];
            if has_notes {
                cells.push(Cell::from(Span::styled(notes_text, detail_style)));
            }
            Row::new(cells)
        })
        .collect();

    let mut col_constraints = vec![
        Constraint::Length(name_col),
        Constraint::Length(12),
        Constraint::Length(url_col),
        Constraint::Length(key_col),
    ];
    if has_notes {
        col_constraints.push(Constraint::Length(notes_col));
    }
    let table = Table::new(rows, col_constraints)
        .header(header)
        .block(
            Block::default()
                .borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
                .border_style(Style::default().fg(t::MUTED))
                .padding(Padding::horizontal(1)),
        )
        .row_highlight_style(Style::default());

    f.render_stateful_widget(table, area, &mut app.providers.table_state);
}

pub(super) fn draw_detail_panel(f: &mut Frame, app: &App, area: Rect) {
    let border_style = Style::default().fg(t::MUTED);
    let block = Block::default()
        .borders(Borders::LEFT | Borders::RIGHT)
        .border_style(border_style)
        .padding(Padding::horizontal(1));

    // Show error toast only when not in editing mode (errors in form are shown inline)
    if app.mode == Mode::Normal
        && let Some((msg, MessageKind::Error, _)) = &app.message
    {
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

    let label = Style::default().fg(t::MUTED);
    let title_line = Line::from(Span::styled(
        "Info",
        Style::default().fg(t::TEXT).add_modifier(Modifier::BOLD),
    ));

    let Some(name) = app
        .providers
        .table_state
        .selected()
        .and_then(|i| app.providers.names.get(i))
    else {
        f.render_widget(
            Paragraph::new(vec![Line::from(""), title_line]).block(block),
            area,
        );
        return;
    };

    let mut lines = vec![Line::from(""), title_line];
    if app.tests.pending.contains(name.as_str()) {
        let prev = app.tests.results.get(name.as_str());
        let latency_str = prev
            .map(|r| fmt_latency(r.latency_ms))
            .unwrap_or_else(|| "—".to_string());
        let models_str = prev
            .and_then(|r| r.model_count)
            .map(|n| format!("{n} models"))
            .unwrap_or_else(|| "—".to_string());
        let testing_model = app
            .tests
            .testing_model
            .get(name.as_str())
            .map(|m| format!(" ({m})"))
            .unwrap_or_default();
        lines.push(Line::from(vec![
            Span::styled("Status ", label),
            Span::styled(
                format!("Testing{testing_model}"),
                Style::default().fg(t::MUTED).add_modifier(Modifier::ITALIC),
            ),
            Span::styled("   Latency ", label),
            Span::styled(latency_str, Style::default().fg(t::MUTED)),
            Span::styled("   Models ", label),
            Span::styled(models_str, Style::default().fg(t::MUTED)),
        ]));
    } else if let Some(r) = app.tests.results.get(name.as_str()) {
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
        let mut status_spans = vec![
            Span::styled("Status ", label),
            Span::styled(status_str, status_style),
        ];
        if matches!(r.status, TestStatus::Ok) && !r.used_model.is_empty() {
            status_spans.push(Span::styled(
                format!(" ({})", r.used_model),
                Style::default().fg(t::MUTED),
            ));
        }
        status_spans.extend([
            Span::styled("   Latency ", label),
            Span::styled(fmt_latency(r.latency_ms), Style::default().fg(t::TEXT)),
            Span::styled("   Models ", label),
            models_str,
        ]);
        lines.push(Line::from(status_spans));
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

    // Enabled routes for the selected provider.
    let provider = app.config.providers.get(name.as_str());
    let enabled_routes: Vec<_> = provider
        .map(|p| p.routes.iter().filter(|r| r.enabled).collect())
        .unwrap_or_default();
    if !enabled_routes.is_empty() {
        let avail = (area.width as usize).saturating_sub(4 + ROUTE_LABEL_WIDTH);
        for (row_idx, group) in pack_routes(&enabled_routes, avail).into_iter().enumerate() {
            let mut spans: Vec<Span> = vec![if row_idx == 0 {
                Span::styled("Routes ", label)
            } else {
                Span::raw("       ")
            }];
            for (i, route) in group.iter().enumerate() {
                if i > 0 {
                    spans.push(Span::raw("  "));
                }
                spans.push(Span::styled(&route.pattern, Style::default().fg(t::TEXT)));
                spans.push(Span::styled(" → ", Style::default().fg(t::MUTED)));
                spans.push(Span::styled(
                    &route.target,
                    Style::default().fg(t::route_target_color(&route.target)),
                ));
            }
            lines.push(Line::from(spans));
        }
    }

    // Last request error — shown after Routes so it doesn't obscure the route list.
    if let Ok(m) = app.metrics.lock()
        && let Some(err) = m.last_error.get(name.as_str())
    {
        let status_str = if err.status == 0 {
            "Network error".to_string()
        } else {
            format!("HTTP {}", err.status)
        };
        let model_part = if err.model.is_empty() {
            String::new()
        } else {
            format!("{}  ", err.model)
        };
        lines.push(Line::from(vec![
            Span::styled("Error ", Style::default().fg(t::MUTED)),
            Span::styled(
                status_str,
                Style::default().fg(t::ERROR).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(model_part, Style::default().fg(t::WARNING)),
            Span::styled(truncate_error(&err.message), Style::default().fg(t::ERROR)),
        ]));
    }

    f.render_widget(Paragraph::new(lines).block(block), area);
}

pub(super) fn draw_keybindings(f: &mut Frame, app: &App, area: Rect) {
    let bg_label = if app.bg_proxy_pid.is_some() {
        "Stop"
    } else {
        "Server"
    };
    // (key, desc, key_color, desc_color)
    let all_keys: &[(&str, &str, Color, Color)] = &[
        ("a", "Add", t::PRIMARY, t::MUTED),
        ("e", "Edit", t::PRIMARY, t::MUTED),
        ("dd", "Delete", t::WARNING, t::MUTED),
        ("s", "Switch", t::PRIMARY, t::MUTED),
        ("p", "Toggle", t::PRIMARY, t::MUTED),
        ("f", "Fallback", t::PRIMARY, t::MUTED),
        ("S", bg_label, t::PRIMARY, t::MUTED),
        ("c", "Clear", t::WARNING, t::MUTED),
        ("q", "Quit", t::WARNING, t::MUTED),
        ("yc", "Curl", t::MUTED, t::MUTED),
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
