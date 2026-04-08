use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Padding, Paragraph, Row, Table};
use unicode_width::UnicodeWidthStr;

use super::super::state::App;
use super::super::theme::{self as t};
use super::format::fmt_latency;
use super::format::truncate_error;
use super::format::{api_key_display_len, col_width, config_path_display, masked_api_key};
use super::layout::{BORDER_DASHED_TOP, PanelId, ROUTE_LABEL_WIDTH, pack_routes, plan_main_screen};
use super::logs::draw_logs_panel;
use super::stats_panel::draw_stats_panel;
use crate::tester::TestStatus;

/// Build the two title spans embedded in the provider table's top border.
fn make_table_titles(app: &App) -> (Line<'static>, Line<'static>) {
    let left = Line::from(vec![
        Span::styled(
            " Claude Code Switcher",
            Style::default().fg(t::TEXT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  v{} ", env!("CARGO_PKG_VERSION")),
            Style::default().fg(t::MUTED),
        ),
    ]);
    let listen = app.config.listen.to_string();
    let fallback_label = if app.config.fallback {
        "Fallback on"
    } else {
        "Fallback off"
    };
    let right = Line::from(vec![
        Span::styled("╌╌ ", Style::default().fg(t::MUTED)),
        Span::styled(
            listen,
            if app.bg_proxy_pid.is_some() {
                Style::default().fg(t::SUCCESS).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(t::MUTED)
            },
        ),
        Span::styled("  ", Style::default()),
        Span::styled(
            fallback_label,
            if app.config.fallback {
                Style::default().fg(t::SUCCESS).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(t::MUTED)
            },
        ),
        Span::styled(" ╌╌ ", Style::default().fg(t::MUTED)),
    ])
    .right_aligned();
    (left, right)
}

pub(super) fn draw_main(f: &mut Frame, app: &mut App, area: Rect) {
    let plan = plan_main_screen(app, area);

    for placement in &plan.left {
        match placement.id {
            PanelId::Providers => draw_provider_table(f, app, placement.area, !plan.split),
            PanelId::Detail => draw_detail_panel(f, app, placement.area, !plan.split),
            PanelId::Stats => draw_stats_panel(f, app, placement.area, !plan.split),
            PanelId::Logs => {}
        }
    }

    if let Some(logs) = plan.logs {
        draw_logs_panel(f, app, logs.area, plan.top_height);
    }
}

pub(super) fn draw_provider_table(f: &mut Frame, app: &mut App, area: Rect, right_border: bool) {
    let table_borders = if right_border {
        Borders::TOP | Borders::LEFT | Borders::RIGHT
    } else {
        Borders::TOP | Borders::LEFT
    };
    let (title_left, title_right) = make_table_titles(app);
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
                .borders(table_borders)
                .border_set(BORDER_DASHED_TOP)
                .border_style(Style::default().fg(t::MUTED))
                .title_top(title_left)
                .title_top(title_right),
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

    let hdr = Style::default().fg(t::MUTED).add_modifier(Modifier::BOLD);
    let mut header_cells = vec![
        Cell::from("Name").style(hdr),
        Cell::from("Format").style(hdr),
        Cell::from("Base URL").style(hdr),
        Cell::from("API Key").style(hdr),
    ];
    if has_notes {
        header_cells.push(Cell::from("Notes").style(hdr));
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
                .borders(table_borders)
                .border_set(BORDER_DASHED_TOP)
                .border_style(Style::default().fg(t::MUTED))
                .padding(Padding::horizontal(1))
                .title_top(title_left)
                .title_top(title_right),
        )
        .row_highlight_style(Style::default());

    f.render_stateful_widget(table, area, &mut app.providers.table_state);
}

pub(super) fn draw_detail_panel(f: &mut Frame, app: &App, area: Rect, right_border: bool) {
    let border_style = Style::default().fg(t::MUTED);
    let detail_borders = if right_border {
        Borders::LEFT | Borders::RIGHT
    } else {
        Borders::LEFT
    };
    let block = Block::default()
        .borders(detail_borders)
        .border_style(border_style)
        .padding(Padding::horizontal(1));

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

    f.render_widget(Paragraph::new(lines).block(block), area);
}
