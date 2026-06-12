//! Provider list and detail panels.
//!
//! This module owns the main provider table, its detail panel, and the
//! Normal-mode key handling that acts on the selected provider.

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Padding, Paragraph, Row, Table};
use unicode_width::UnicodeWidthStr;

use crate::config::{ApiFormat, Provider};
use crate::tester::TestStatus;

use super::state::{App, ConfirmAction, MessageKind, Mode, QuotaStatus};
use super::theme::{self as t};
use super::ui::format::{
    api_key_display_len, col_width, config_path_display, fmt_latency, masked_api_key,
    truncate_chars, truncate_error,
};
use super::{ServerHandle, server};

pub(super) fn handle_key(
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
    server_handle: &mut Option<ServerHandle>,
) -> crate::error::Result<()> {
    if app.message.is_some() {
        app.message = None;
    }

    let prev = super::input::insert::consume_pending_key(&mut app.pending_key);

    if let Some(pk) = prev {
        match (pk, &code) {
            ('d', KeyCode::Char('d')) => {
                if let Some(name) = app.selected_name().map(|s| s.to_string()) {
                    app.confirm(ConfirmAction::Delete(name));
                }
                return Ok(());
            }
            ('g', KeyCode::Char('g')) => {
                if !app.config.providers.is_empty() {
                    app.providers.table_state.select(Some(0));
                }
                return Ok(());
            }
            ('y', KeyCode::Char('y')) => {
                if let Some(provider) = app
                    .selected_name()
                    .and_then(|name| app.config.providers.get(name))
                {
                    let url = provider.base_url.clone();
                    if super::input::copy_to_clipboard(&url) {
                        app.set_message("Copied base URL", MessageKind::Success);
                    } else {
                        app.set_message("Copy failed (wl-copy not found?)", MessageKind::Error);
                    }
                }
                return Ok(());
            }
            ('y', KeyCode::Char('c')) => {
                if let Some(name) = app.selected_name().map(|s| s.to_string())
                    && let Some(provider) = app.config.providers.get(&name)
                {
                    let supported = app.models.provider_models.get(&name);
                    let model = supported
                        .filter(|s| !s.is_empty())
                        .and_then(|models| {
                            app.metrics
                                .lock()
                                .ok()
                                .and_then(|m| {
                                    models
                                        .iter()
                                        .max_by_key(|mdl| {
                                            m.by_model.get(*mdl).map_or(0, |s| s.input + s.output)
                                        })
                                        .filter(|mdl| m.by_model.contains_key(*mdl))
                                        .map(|s| s.to_string())
                                })
                                .or_else(|| models.first().map(|s| s.to_string()))
                        })
                        .unwrap_or_default();

                    match build_test_curl(provider, &model) {
                        Ok(cmd) => {
                            if super::input::copy_to_clipboard(&cmd) {
                                app.set_message("Copied curl command", MessageKind::Success);
                            } else {
                                app.set_message(
                                    "Copy failed (wl-copy not found?)",
                                    MessageKind::Error,
                                );
                            }
                        }
                        Err(e) => {
                            app.set_message(format!("Cannot build curl: {e}"), MessageKind::Error);
                        }
                    }
                }
                return Ok(());
            }
            _ => {}
        }
    }

    match code {
        KeyCode::Char('q') | KeyCode::Esc => {
            if app.bg_proxy_pid.is_some() {
                app.should_quit = true;
            } else {
                app.confirm(ConfirmAction::Quit);
            }
        }
        KeyCode::Up | KeyCode::Char('k') => app.select_prev(),
        KeyCode::Down | KeyCode::Char('j') => app.select_next(),
        KeyCode::Char('G') => {
            if !app.config.providers.is_empty() {
                let last = app.provider_count() - 1;
                app.providers.table_state.select(Some(last));
            }
        }
        KeyCode::Char('g') => {
            app.pending_key = Some(('g', std::time::Instant::now()));
        }
        KeyCode::Char('d') => {
            app.pending_key = Some(('d', std::time::Instant::now()));
        }
        KeyCode::Char('y') => {
            app.pending_key = Some(('y', std::time::Instant::now()));
        }
        KeyCode::Char('s') => {
            app.switch_to_selected()?;
            server::sync_proxy_config(app, server_handle);
        }
        KeyCode::Char('a' | 'o') => app.add(),
        KeyCode::Enter | KeyCode::Char('e') => {
            if app.selected_name().is_some() {
                app.start_edit();
            }
        }
        KeyCode::Char('p') => {
            app.toggle_provider_enabled()?;
            server::sync_proxy_config(app, server_handle);
        }
        KeyCode::Char('t') => super::testing::test_selected(app),
        KeyCode::Char('u') => {
            if let Some(name) = app.selected_name().map(|s| s.to_string()) {
                let saved = app
                    .config
                    .providers
                    .get(&name)
                    .and_then(|p| p.quota_command.as_deref());
                app.quota_form = Some(super::state::QuotaForm::new(&name, saved));
                app.mode = Mode::QuotaConfig;
            }
        }
        KeyCode::Char('K') => {
            let _ = app.move_provider_up();
        }
        KeyCode::Char('J') => {
            let _ = app.move_provider_down();
        }
        KeyCode::Char('f') => {
            let _ = app.toggle_provider_fallback();
            server::sync_proxy_config(app, server_handle);
        }
        KeyCode::Char('F') => {
            let _ = app.toggle_fallback();
            server::sync_proxy_config(app, server_handle);
        }
        KeyCode::Char('r') => {
            let _ = app.reload_config();
            server::sync_proxy_config(app, server_handle);
        }
        KeyCode::Char('S') => {
            server::toggle_bg_proxy(app, server_handle);
        }
        KeyCode::Char('c') => app.confirm(ConfirmAction::ClearCurrent),
        KeyCode::Char('C') => app.confirm(ConfirmAction::Clear),
        KeyCode::Char('h' | '?') => {
            app.mode = Mode::Help;
        }
        KeyCode::Char('m') => {
            app.mode = Mode::Models;
            app.models.search_active = false;
            app.models.selected = 0;
            app.models.scroll = 0;
        }
        KeyCode::Char('l') if modifiers.is_empty() => {
            app.mode = Mode::Logs;
            app.logs.selected = 0;
            app.logs.scroll = 0;
        }
        KeyCode::Char('l') if modifiers == KeyModifiers::CONTROL => {
            app.message_log.clear();
        }
        _ => {}
    }
    Ok(())
}

pub(super) fn draw_table(f: &mut Frame, app: &mut App, area: Rect) {
    if app.config.providers.is_empty() {
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
        .block(Block::default().padding(Padding::new(1, 1, 1, 0)));
        f.render_widget(empty, area);
        return;
    }

    let format_col = col_width(
        "Format",
        app.config
            .providers
            .values()
            .map(|p| p.api_format.to_string().len()),
    );
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
    let port_col = col_width(
        "Port",
        app.config
            .providers
            .values()
            .filter_map(|p| p.port)
            .map(|p| p.to_string().len()),
    );
    let has_fallback_col = app.config.fallback;
    let has_quota = app
        .config
        .providers
        .values()
        .any(|p| p.quota_command.is_some())
        || !app.quota_status.is_empty();
    let has_port = app.config.providers.values().any(|p| p.port.is_some());
    let terminal_width = area.width;
    let compact_quota = terminal_width < 120;
    let quota_col = col_width(
        "Quota",
        app.config
            .providers
            .values()
            .map(|p| match app.quota_status.get(&p.id) {
                None => 0,
                Some(status) => quota_cell_text(status, compact_quota).width(),
            }),
    );
    let max_name_len = app
        .config
        .providers
        .keys()
        .map(|name| name.width())
        .max()
        .unwrap_or(0)
        .max("Name".width());
    let name_col = max_name_len as u16;

    let hdr = Style::default().fg(t::MUTED).add_modifier(Modifier::BOLD);
    let mut header_cells = vec![
        Cell::from("Name").style(hdr),
        Cell::from("Format").style(hdr),
        Cell::from("Base URL").style(hdr),
        Cell::from("API Key").style(hdr),
    ];
    if has_quota {
        header_cells.push(Cell::from("Quota").style(hdr));
    }
    if has_port {
        header_cells.push(Cell::from("Port").style(hdr));
    }
    let header = Row::new(header_cells).height(1);

    let quota_status = &app.quota_status;

    let rows: Vec<Row> = app
        .config
        .providers
        .keys()
        .map(|name| {
            let provider = &app.config.providers[name];
            let is_current = name == &app.config.current;
            let disabled = !provider.enabled;
            let name_style = if disabled {
                Style::default().fg(t::MUTED)
            } else if is_current {
                Style::default().fg(t::PRIMARY).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(t::TEXT)
            };
            let name_style = if has_fallback_col && provider.fallback && !is_current {
                name_style.add_modifier(Modifier::UNDERLINED)
            } else {
                name_style
            };
            let detail_style = if disabled || !is_current {
                Style::default().fg(t::MUTED)
            } else {
                Style::default().fg(t::PRIMARY)
            };
            let name_display_width = name.width();
            let padding = max_name_len.saturating_sub(name_display_width);
            let pad_style = name_style.remove_modifier(Modifier::UNDERLINED);
            let name_cell = Cell::from(Line::from(vec![
                Span::styled(name.as_str().to_string(), name_style),
                Span::styled(" ".repeat(padding), pad_style),
            ]));

            let mut cells = vec![
                name_cell,
                Cell::from(Span::styled(provider.api_format.to_string(), detail_style)),
                Cell::from(Span::styled(provider.base_url.as_str(), detail_style)),
                masked_api_key(&provider.api_key),
            ];
            if has_quota {
                cells.push(render_quota_cell(
                    quota_status,
                    &provider.id,
                    terminal_width,
                ));
            }
            if has_port {
                let port_text = provider.port.map(|p| p.to_string()).unwrap_or_default();
                let port_style = if app.bg_proxy_pid.is_some() && provider.port.is_some() {
                    Style::default().fg(t::SUCCESS)
                } else {
                    Style::default().fg(t::MUTED)
                };
                cells.push(Cell::from(Span::styled(port_text, port_style)));
            }
            Row::new(cells)
        })
        .collect();

    let mut col_constraints = vec![
        Constraint::Length(name_col),
        Constraint::Length(format_col),
        Constraint::Length(url_col),
        Constraint::Length(key_col),
    ];
    if has_quota {
        col_constraints.push(Constraint::Length(quota_col));
    }
    if has_port {
        col_constraints.push(Constraint::Length(port_col));
    }
    let table = Table::new(rows, col_constraints)
        .header(header)
        .column_spacing(COL_GAP)
        .block(Block::default().padding(Padding::new(1, 1, 1, 0)))
        .row_highlight_style(Style::default().bg(t::HIGHLIGHT_BG));

    f.render_stateful_widget(table, area, &mut app.providers.table_state);
}

const COL_GAP: u16 = 4;
const MODEL_COUNT_WIDTH: usize = 3;
const MODEL_NAME_TRUNCATE: usize = 40;
const INFO_FIELD_SEP: &str = "   ";

pub(super) fn draw_detail_panel(f: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::default().padding(Padding::horizontal(1));

    let label = Style::default().fg(t::MUTED);
    let title_line = Line::from(Span::styled(
        "Info",
        Style::default().fg(t::TEXT).add_modifier(Modifier::BOLD),
    ));

    let Some(name) = app
        .providers
        .table_state
        .selected()
        .and_then(|i| app.provider_name_at(i))
    else {
        f.render_widget(
            Paragraph::new(vec![Line::from(""), title_line]).block(block),
            area,
        );
        return;
    };

    let mut lines = vec![Line::from(""), title_line];
    if app.tests.pending.contains(name) {
        let prev = app.tests.results.get(name);
        let latency_str = prev
            .map(|r| fmt_latency(r.latency_ms))
            .unwrap_or_else(|| "—".to_string());
        let models_str = prev
            .and_then(|r| r.model_count)
            .map(|n| format!("{n:<width$}", width = MODEL_COUNT_WIDTH))
            .unwrap_or_else(|| format!("{:<width$}", "—", width = MODEL_COUNT_WIDTH));
        let testing_model = app
            .tests
            .testing_model
            .get(name)
            .map(|m| format!(" ({})", truncate_chars(m, MODEL_NAME_TRUNCATE)))
            .unwrap_or_default();
        let status_text = format!("Testing{testing_model}");
        let left_w = status_text.as_str().width();
        let models_pad = left_w.saturating_sub(MODEL_COUNT_WIDTH);
        lines.push(Line::from(vec![
            Span::styled("Status ", label),
            Span::styled(
                status_text,
                Style::default().fg(t::MUTED).add_modifier(Modifier::ITALIC),
            ),
            Span::styled(format!("{INFO_FIELD_SEP}Latency "), label),
            Span::styled(latency_str, Style::default().fg(t::MUTED)),
        ]));
        let mut pending_metrics = vec![
            Span::styled("Models ", label),
            Span::styled(models_str, Style::default().fg(t::MUTED)),
            Span::raw(" ".repeat(models_pad)),
        ];
        let muted = Style::default().fg(t::MUTED);
        pending_metrics.extend([
            Span::styled(format!("{INFO_FIELD_SEP}Tools   "), label),
            capability_span(prev.and_then(|r| r.tools_supported), muted, muted),
            Span::styled(format!("{INFO_FIELD_SEP}Images "), label),
            capability_span(prev.and_then(|r| r.images_supported), muted, muted),
        ]);
        lines.push(Line::from(pending_metrics));
    } else if let Some(r) = app.tests.results.get(name) {
        let (status_str, status_style) = match &r.status {
            TestStatus::Ok => (
                "OK".to_string(),
                Style::default().fg(t::SUCCESS).add_modifier(Modifier::BOLD),
            ),
            TestStatus::AuthFailed => (
                "Auth failed".to_string(),
                Style::default().fg(t::ERROR).add_modifier(Modifier::BOLD),
            ),
            TestStatus::Error(e) => (truncate_error(e), Style::default().fg(t::ERROR)),
        };
        let models_str = match r.model_count {
            Some(n) => Span::styled(
                format!("{n:<width$}", width = MODEL_COUNT_WIDTH),
                Style::default().fg(t::TEXT),
            ),
            None => Span::styled(
                format!("{:<width$}", "—", width = MODEL_COUNT_WIDTH),
                Style::default().fg(t::MUTED),
            ),
        };
        // Line 1: Status + Latency
        let status_str_w = status_str.as_str().width();
        let model_display = if !r.used_model.is_empty() {
            format!(" ({})", truncate_chars(&r.used_model, MODEL_NAME_TRUNCATE))
        } else {
            String::new()
        };
        let left_w = status_str_w + model_display.as_str().width();
        let mut status_spans = vec![
            Span::styled("Status ", label),
            Span::styled(status_str, status_style),
        ];
        if !model_display.is_empty() {
            status_spans.push(Span::styled(model_display, Style::default().fg(t::MUTED)));
        }
        status_spans.extend([
            Span::styled(format!("{INFO_FIELD_SEP}Latency "), label),
            Span::styled(fmt_latency(r.latency_ms), Style::default().fg(t::TEXT)),
        ]);
        lines.push(Line::from(status_spans));
        // Line 2: Models + Tools  (left side padded to match line 1)
        let models_pad = left_w.saturating_sub(MODEL_COUNT_WIDTH);
        let mut metrics_spans = vec![
            Span::styled("Models ", label),
            models_str,
            Span::raw(" ".repeat(models_pad)),
        ];
        metrics_spans.extend([
            Span::styled(format!("{INFO_FIELD_SEP}Tools   "), label),
            capability_span(
                r.tools_supported,
                Style::default().fg(t::SUCCESS),
                Style::default().fg(t::ERROR),
            ),
            Span::styled(format!("{INFO_FIELD_SEP}Images "), label),
            capability_span(
                r.images_supported,
                Style::default().fg(t::SUCCESS),
                Style::default().fg(t::ERROR),
            ),
        ]);
        lines.push(Line::from(metrics_spans));
    } else {
        lines.push(Line::from(vec![
            Span::styled("Press ", Style::default().fg(t::MUTED)),
            Span::styled(
                "[t]",
                Style::default().fg(t::PRIMARY).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" to test provider", Style::default().fg(t::MUTED)),
        ]));
    }

    app.detail_line_count = super::ui::format::DETAIL_HEIGHT;
    f.render_widget(Paragraph::new(lines).block(block), area);
}

/// Render a capability probe result (Tools / Images): yes / no / not probed.
fn capability_span(v: Option<bool>, yes: Style, no: Style) -> Span<'static> {
    match v {
        Some(true) => Span::styled("yes", yes),
        Some(false) => Span::styled("no", no),
        None => Span::styled("—", Style::default().fg(t::MUTED)),
    }
}

fn build_test_curl(provider: &Provider, model: &str) -> Result<String, String> {
    let api_key = provider.resolve_api_key().map_err(|e| e.to_string())?;
    let (url, body) = provider.chat_url_and_body(model);

    let mut cmd = format!("curl -s -X POST '{url}' \\\n  -H 'Content-Type: application/json' \\\n");

    match provider.api_format {
        ApiFormat::Anthropic => {
            cmd.push_str(&format!(
                "  -H 'x-api-key: {api_key}' \\\n  -H 'anthropic-version: 2023-06-01' \\\n"
            ));
        }
        ApiFormat::OpenAI => {
            cmd.push_str(&format!("  -H 'Authorization: Bearer {api_key}' \\\n"));
        }
    }

    cmd.push_str(&format!("  -d '{body}'"));
    Ok(cmd)
}

/// Quota cell text, shared by the column-width pass and `render_quota_cell`
/// so the measured width always matches what is rendered.
fn quota_cell_text(status: &QuotaStatus, compact: bool) -> String {
    let max = if compact { 10 } else { 18 };
    match status {
        QuotaStatus::Running => "-".to_string(),
        QuotaStatus::Success(result) => {
            truncate_chars(&super::quota_command::cell_text(result), max)
        }
        QuotaStatus::Error(msg) => truncate_chars(msg, max),
    }
}

fn render_quota_cell(
    quota_status: &std::collections::HashMap<String, QuotaStatus>,
    provider_id: &str,
    available_width: u16,
) -> Cell<'static> {
    let compact = available_width < 120;

    match quota_status.get(provider_id) {
        None => Cell::from(""),
        Some(status) => {
            let color = match status {
                QuotaStatus::Running => t::MUTED,
                QuotaStatus::Success(_) => t::SUCCESS,
                QuotaStatus::Error(_) => t::ERROR,
            };
            Cell::from(quota_cell_text(status, compact)).style(Style::default().fg(color))
        }
    }
}

#[cfg(test)]
mod tests {
    use indexmap::IndexMap;
    use ratatui::{Terminal, backend::TestBackend, layout::Rect, widgets::TableState};

    use super::draw_table;
    use crate::config::{ApiFormat, AppConfig, Provider};
    use crate::tui::state::{App, LogsState, ModelsState, ProviderList, TestState};

    #[test]
    fn port_cell_is_success_color_when_bg_proxy_is_running() {
        let path = format!("/tmp/ccs-provider-port-test-{}.db", uuid::Uuid::new_v4());
        let mut providers = IndexMap::new();
        providers.insert(
            "ds".to_string(),
            Provider {
                id: "ds-id".into(),
                base_url: "https://api.deepseek.com/anthropic".into(),
                api_key: "sk-test".into(),
                api_format: ApiFormat::Anthropic,
                model_map: Default::default(),
                routes: vec![],
                enabled: true,
                fallback: false,
                api_version: None,
                inject_thinking_history: true,
                quota_command: None,
                port: Some(8003),
            },
        );

        let db = crate::repo::Repository::open(&path);
        let mut table_state = TableState::default();
        table_state.select(Some(0));
        let mut app = App {
            config: AppConfig {
                current: "ds".into(),
                listen: "127.0.0.1:0".into(),
                providers,
                fallback: false,
                db_path: Some(path),
                request_log_limit: 100,
            },
            mode: crate::tui::state::Mode::Normal,
            terminal_focused: true,
            providers: ProviderList { table_state },
            form: None,
            message: None,
            confirm_action: None,
            should_quit: false,
            server_status: crate::tui::state::ServerStatus::Stopped,
            metrics: std::sync::Arc::new(std::sync::Mutex::new(Default::default())),
            tests: TestState::new(),
            db,
            bg_proxy_pid: Some(123),
            models: ModelsState {
                provider_models: std::collections::HashMap::new(),
                search_field: crate::tui::state::FormField::search(),
                search_active: true,
                selected: 0,
                scroll: 0,
                pending_key: None,
            },
            request_log: std::sync::Arc::new(std::sync::Mutex::new(
                crate::metrics::RequestLog::default(),
            )),
            logs: LogsState {
                selected: 0,
                scroll: 0,
                detail_scroll: 0,
                detail_view_height: 0,
                pending_key: None,
            },
            message_log: std::collections::VecDeque::new(),
            seen_provider_errors: std::collections::HashMap::new(),
            pending_key: None,
            quota_status: std::collections::HashMap::new(),
            quota_form: None,
            detail_line_count: 0,
            sysinfo_sampler: crate::tui::sysinfo::SysInfoSampler::new(),
        };

        let backend = TestBackend::new(100, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| draw_table(f, &mut app, Rect::new(0, 0, 100, 6)))
            .unwrap();

        let buffer = terminal.backend().buffer();
        let port_cell = buffer
            .content()
            .iter()
            .find(|cell| cell.symbol() == "8")
            .unwrap();

        assert_eq!(port_cell.fg, super::t::SUCCESS);
    }
}
