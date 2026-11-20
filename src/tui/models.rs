//! Models browser popup.
//!
//! This module owns the searchable models dialog, its navigation, clipboard
//! actions for the currently highlighted model, testing it (`t`), and pinning
//! it as the provider's Test Model (`p`), and the attribute columns drawn on the
//! right of each row (`a` configures them). Test results render after each model
//! name.

use std::time::Instant;

use unicode_width::UnicodeWidthStr;

use super::state::{self, App, ListMove, MessageKind, Mode, list_move};
use super::theme::{self as t};
use super::ui::format::{
    MAX_CELL_W, fmt_latency, fmt_model_cells, max_content_width, search_line, truncate_error_width,
    truncate_width,
};
use super::ui::layout::centered_fixed;
use crate::catalog::CatalogModel;
use crate::tester::TestStatus;
use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph};

/// Blank columns between two value columns.
const CELL_GAP: usize = 2;
/// Model names keep at least this much room before a column is dropped.
const MIN_NAME_W: usize = 16;

/// Viewport assumed before the first frame has reported the real list height.
const FALLBACK_VIEWPORT: usize = 8;

pub(super) fn handle_key(
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> crate::error::Result<()> {
    // Only the names outlive this borrow: every arm below mutates `app`.
    let names: Vec<String> = build_models(app).iter().map(|m| m.name.clone()).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    app.models.selected = app.models.selected.min(refs.len().saturating_sub(1));

    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    let total = refs.len();

    if app.models.search_active {
        if ctrl && code == KeyCode::Char('c') {
            app.mode = Mode::Normal;
            return Ok(());
        }
        if let Some(dir) = list_move(code, ctrl, true) {
            nav(app, total, dir, 1);
            return Ok(());
        }

        match super::input::insert::handle_field_insert_key(
            &mut app.models.search_field,
            code,
            ctrl,
            &mut app.models.pending_key,
        ) {
            super::input::insert::InsertKeyResult::ExitInsert => {
                app.models.search_active = false;
            }
            super::input::insert::InsertKeyResult::TextChanged => {
                app.models.selected = 0;
                app.models.scroll = 0;
            }
            super::input::insert::InsertKeyResult::Consumed
            | super::input::insert::InsertKeyResult::NotHandled => {}
        }
    } else {
        let prev = super::input::insert::consume_pending_key(&mut app.models.pending_key);

        if let Some(dir) = list_move(code, ctrl, false) {
            nav(app, total, dir, 1);
            return Ok(());
        }

        if let Some(pk) = prev {
            match (pk, &code) {
                ('y', KeyCode::Char('y')) => {
                    copy_selected(app, &refs);
                    return Ok(());
                }
                ('g', KeyCode::Char('g')) => {
                    app.models.selected = 0;
                    app.models.scroll = 0;
                    return Ok(());
                }
                _ => {}
            }
        }

        match code {
            KeyCode::Esc | KeyCode::Char('q') => {
                app.mode = Mode::Normal;
            }
            KeyCode::Char('c') if ctrl => {
                app.mode = Mode::Normal;
            }
            KeyCode::Char('i') => {
                app.models.search_active = true;
            }
            KeyCode::Char('t') => {
                if let Some((prov, name)) = selected_model(app, &refs) {
                    super::testing::test_specific_model(app, &prov, name);
                } else if total == 0 {
                    // Empty list — the search box is the only source of a name.
                    // Register it so a row exists and the result stays visible.
                    let name = app.models.search_field.value.trim().to_string();
                    if !name.is_empty()
                        && let Some(prov) = current_provider(app).map(str::to_string)
                    {
                        app.models.search_field.value = name.clone();
                        // Persist too: the DB is the catalog's source at
                        // startup, so a memory-only row would not survive a
                        // restart.
                        let mut models = app.models.catalog.get(&prov).cloned().unwrap_or_default();
                        if !models.iter().any(|m| m.name == name) {
                            models.push(CatalogModel::name_only(&name));
                        }
                        let provider_id = app
                            .config
                            .providers
                            .get(&prov)
                            .map(|p| p.id.clone())
                            .unwrap_or_else(|| prov.clone());
                        app.store_catalog(&provider_id, &prov, models);
                        super::testing::test_specific_model(app, &prov, &name);
                    }
                }
            }
            KeyCode::Char('p') => {
                if let Some((prov, name)) = selected_model(app, &refs) {
                    set_test_model(app, &prov, name);
                }
            }
            KeyCode::Char('a') => super::model_attrs::open(app),

            KeyCode::Char('r') => {
                // Reload the current provider's model catalog from upstream.
                // The fetch is async; when it lands the list updates in place.
                let Some(name) = current_provider(app).map(str::to_string) else {
                    return Ok(());
                };
                if app.models.refresh_inflight {
                    return Ok(());
                }
                if app.spawn_model_fetch(&name) {
                    app.models.refresh_inflight = true;
                    app.set_message(
                        format!("Refreshing model list for '{name}'…"),
                        MessageKind::Info,
                    );
                } else {
                    app.set_message(
                        format!("Cannot refresh '{name}': no base URL or API key"),
                        MessageKind::Error,
                    );
                }
            }
            KeyCode::Char('G') => {
                if total > 0 {
                    let viewport = viewport(app);
                    app.models.selected = total - 1;
                    state::keep_visible(app.models.selected, &mut app.models.scroll, viewport);
                }
            }
            KeyCode::PageDown | KeyCode::Char('d') if ctrl => nav(app, total, ListMove::Down, 10),
            KeyCode::PageUp | KeyCode::Char('u') if ctrl => nav(app, total, ListMove::Up, 10),
            KeyCode::Enter => copy_selected(app, &refs),
            KeyCode::Char('y') => {
                app.models.pending_key = Some(('y', Instant::now()));
            }
            KeyCode::Char('g') => {
                app.models.pending_key = Some(('g', Instant::now()));
            }
            _ => {}
        }
    }

    Ok(())
}

pub(super) fn handle_paste(app: &mut App, text: &str) {
    app.models.search_active = true;
    app.models.search_field.insert_str(text);
    app.models.selected = 0;
    app.models.scroll = 0;
}

pub(super) fn draw_popup(f: &mut Frame, app: &mut App) {
    let prov_name = current_provider(app).unwrap_or("—").to_string();
    let prov_color = t::PRIMARY;

    let content_lines = build_models(app).len().min(u16::MAX as usize) as u16;
    let dialog_height = (content_lines + 4).min(f.area().height * 4 / 5).max(6);
    let area = centered_fixed(80, dialog_height, f.area());
    f.render_widget(Clear, area);
    let mode_tag = if app.models.search_active {
        "[I]"
    } else {
        "[N]"
    };
    let outer_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(t::PRIMARY))
        .title(format!(" {prov_name} — Models  {mode_tag} "))
        .title_style(Style::default().fg(t::TEXT).add_modifier(Modifier::BOLD))
        .padding(Padding::new(1, 1, 0, 0));
    let inner = outer_block.inner(area);
    f.render_widget(outer_block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(inner);

    f.render_widget(
        Paragraph::new(search_line(
            "Search",
            &app.models.search_field,
            app.models.search_active,
        )),
        chunks[0],
    );

    let divider = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(t::MUTED));
    f.render_widget(divider, chunks[1]);

    let list_height = chunks[2].height;
    // `nav` and the `G` jump keep the highlight inside what was really drawn:
    // on a short terminal the dialog shrinks well below the fallback viewport.
    app.models.list_height = list_height;
    let models = build_models(app);
    let mut lines: Vec<Line> = Vec::new();

    if models.is_empty() {
        lines.push(Line::from(Span::styled(
            "  No matches",
            Style::default().fg(t::MUTED),
        )));
    } else {
        let list_width = chunks[2].width as usize;
        let attrs = app
            .models
            .attrs
            .get(&prov_name)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        // Every configured column gets its own width, so `$0` and `$150k` line
        // up instead of shifting the column beside them. Columns past the ones
        // that fit are dropped rather than clipped off the row.
        let cells: Vec<Vec<String>> = if attrs.is_empty() {
            Vec::new()
        } else {
            models
                .iter()
                .map(|model| fmt_model_cells(attrs, model))
                .collect()
        };
        let all_widths = column_widths(&cells, attrs.len());
        let shown = columns_that_fit(&all_widths, list_width);
        let widths = &all_widths[..shown];
        let columns_w: usize =
            widths.iter().sum::<usize>() + CELL_GAP * widths.len().saturating_sub(1);
        for (i, model) in models.iter().enumerate() {
            let is_selected = i == app.models.selected;
            let marker = if is_selected { "  ▶ " } else { "    " };
            let name_style = if is_selected {
                Style::default().fg(prov_color).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(t::TEXT)
            };
            let budget = list_width.saturating_sub(marker.width() + columns_w + 2);
            let name = truncate_width(&model.name, budget.max(8));
            let mut spans = vec![
                Span::styled(marker, Style::default().fg(prov_color)),
                Span::styled(name, name_style),
            ];
            // Error detail stops where the column starts.
            let suffix = model_result_suffix(
                app,
                &prov_name,
                &model.name,
                list_width.saturating_sub(columns_w),
            );
            let mut used: usize = spans.iter().map(Span::width).sum();
            used += suffix.iter().map(Span::width).sum::<usize>();
            spans.extend(suffix);
            if !widths.is_empty() {
                let pad = list_width.saturating_sub(used + columns_w + 1);
                spans.push(Span::raw(" ".repeat(pad.max(1))));
                for (col, width) in widths.iter().enumerate() {
                    if col > 0 {
                        spans.push(Span::raw(" ".repeat(CELL_GAP)));
                    }
                    let cell = cells
                        .get(i)
                        .and_then(|row| row.get(col))
                        .map(|text| truncate_width(text, *width))
                        .unwrap_or_default();
                    // Left-aligned: with a `$` prefix the currency sign is the
                    // column's visible anchor, and `$2` under `$0.15` keeps it.
                    let right = width.saturating_sub(cell.width());
                    // The highlight covers the value columns too; the test
                    // result keeps its own colour so a failure stays visible.
                    let style = if is_selected {
                        Style::default().fg(prov_color)
                    } else {
                        Style::default().fg(t::MUTED)
                    };
                    spans.push(Span::styled(cell, style));
                    spans.push(Span::raw(" ".repeat(right)));
                }
            }
            lines.push(Line::from(spans));
        }
    }

    let max_scroll = (lines.len().min(u16::MAX as usize) as u16).saturating_sub(list_height);
    let scroll = app.models.scroll.min(max_scroll);

    f.render_widget(Paragraph::new(lines).scroll((scroll, 0)), chunks[2]);
}

fn current_test_model(app: &App) -> Option<&str> {
    current_provider(app).and_then(|name| {
        app.config
            .providers
            .get(name)
            .and_then(|p| p.test_model.as_deref())
    })
}

/// Pin the highlighted model as the provider's Test Model and persist it.
/// Pressing `p` again on the pinned model clears it.
fn set_test_model(app: &mut App, provider_name: &str, model: &str) {
    let clearing = current_test_model(app) == Some(model);
    let mut next = app.config.clone();
    if let Some(p) = next.providers.get_mut(provider_name) {
        if clearing {
            p.test_model = None;
        } else {
            p.test_model = Some(model.to_string());
        }
    }
    if let Err(e) = crate::config::save_config(&next) {
        app.set_message(
            format!("Failed to save test model: {e}"),
            MessageKind::Error,
        );
        return;
    }
    app.config = next;
    if clearing {
        app.set_message("Test model cleared".to_string(), MessageKind::Success);
    } else {
        app.set_message(format!("Test model set: {model}"), MessageKind::Success);
    }
}
/// Suffix spans rendered after a model name: a live "Testing…" indicator or
/// the last test result for that model (latency for OK, reason otherwise).
/// `list_width` is the popup list's width; the error detail is truncated to
/// the room left after the model name and the suffix's fixed prefix.
fn model_result_suffix(
    app: &App,
    provider: &str,
    model: &str,
    list_width: usize,
) -> Vec<Span<'static>> {
    // Only show the live indicator while a test is actually running; after
    // completion the row falls through to the stored result below.
    if app.tests.pending.contains(provider)
        && app.tests.testing_model.get(provider).map(String::as_str) == Some(model)
    {
        return vec![Span::styled(
            "  Testing…",
            Style::default().fg(t::MUTED).add_modifier(Modifier::ITALIC),
        )];
    }
    // Row and result keys can differ in case (a typed name vs. the catalog's
    // spelling); look the result up case-insensitively so it still displays.
    let Some(r) = app.tests.results.get(provider).and_then(|m| {
        m.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(model))
            .map(|(_, v)| v)
    }) else {
        return vec![];
    };
    match &r.status {
        TestStatus::Ok => vec![Span::styled(
            format!("  ✓ {}", fmt_latency(r.latency_ms)),
            Style::default().fg(t::SUCCESS),
        )],
        TestStatus::AuthFailed => vec![Span::styled("  ✗ auth", Style::default().fg(t::ERROR))],
        TestStatus::Error(e) => {
            // Room left after the row's indentation, model name and "  ✗ "
            // prefix; any "HTTP 123: " status is covered inside the budget.
            let max = list_width.saturating_sub(model.width() + 8);
            let msg = truncate_error_width(e, max);
            vec![Span::styled(
                format!("  ✗ {msg}"),
                Style::default().fg(t::ERROR),
            )]
        }
    }
}

/// The selected provider's models that match the search box, sorted by name.
/// Borrowed: a redraw must not clone every model's attribute map.
fn build_models(app: &App) -> Vec<&CatalogModel> {
    let Some(prov) = current_provider(app) else {
        return vec![];
    };
    let filter = app.models.search_field.value.to_lowercase();
    let mut matched: Vec<&CatalogModel> = app
        .models
        .catalog
        .get(prov)
        .into_iter()
        .flatten()
        .filter(|m| state::matches_filter(&m.name, &filter))
        .collect();
    matched.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    matched
}

/// Width of each value column: the widest cell in that column, so `$0` and
/// `$150k` sharing a column cannot shift the column beside them.
fn column_widths(cells: &[Vec<String>], columns: usize) -> Vec<usize> {
    (0..columns)
        .map(|col| {
            max_content_width(
                cells
                    .iter()
                    .filter_map(|row| row.get(col))
                    .map(|cell| cell.width()),
                0,
                MAX_CELL_W,
            )
        })
        .collect()
}

/// How many of `widths` fit beside the model name; the rest go unrendered.
fn columns_that_fit(widths: &[usize], list_width: usize) -> usize {
    let mut used = 0;
    let mut shown = 0;
    for width in widths {
        let gap = if shown == 0 { 0 } else { CELL_GAP };
        if used + gap + width + MIN_NAME_W > list_width {
            break;
        }
        used += gap + width;
        shown += 1;
    }
    shown
}

pub(super) fn current_provider(app: &App) -> Option<&str> {
    app.providers
        .table_state
        .selected()
        .and_then(|i| app.provider_name_at(i))
}

/// Provider name and the highlighted model name, if both exist.
fn selected_model<'a>(app: &App, refs: &'a [&str]) -> Option<(String, &'a str)> {
    let prov = current_provider(app)?.to_string();
    let model = *refs.get(app.models.selected)?;
    Some((prov, model))
}

/// Rows the models list draws; the draw stores the real height each frame.
fn viewport(app: &App) -> usize {
    match app.models.list_height {
        0 => FALLBACK_VIEWPORT,
        height => height as usize,
    }
}

/// Move the models selection `rows` rows, keeping it inside the viewport.
fn nav(app: &mut App, total: usize, dir: ListMove, rows: isize) {
    let viewport = viewport(app);
    state::nav(
        &mut app.models.selected,
        &mut app.models.scroll,
        dir,
        rows,
        total,
        viewport,
    );
}

fn copy_selected(app: &mut App, flat: &[&str]) {
    if let Some(&name) = flat.get(app.models.selected) {
        if super::input::copy_to_clipboard(name) {
            app.set_message(format!("Copied: {name}"), MessageKind::Success);
        } else {
            app.set_message("Copy failed (wl-copy not found?)", MessageKind::Error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::testing::tests::app_with_current;

    fn app_with_no_models(search: &str) -> App {
        let mut app = app_with_current("first");
        app.models.catalog.insert("first".to_string(), vec![]);
        app.models.search_field.value = search.to_string();
        app.models.search_active = false;
        app
    }

    #[tokio::test]
    async fn t_with_empty_list_tests_search_value() {
        let mut app = app_with_no_models("deepseek-v4-flash");

        handle_key(&mut app, KeyCode::Char('t'), KeyModifiers::NONE).unwrap();

        assert!(app.tests.pending.contains("first"));
        assert_eq!(
            app.tests.testing_model.get("first").map(String::as_str),
            Some("deepseek-v4-flash")
        );
        // The name is registered so the panel has a row to render the result.
        assert_eq!(
            app.models.catalog.get("first").map(Vec::as_slice),
            Some(&[CatalogModel::name_only("deepseek-v4-flash")][..])
        );
        // Registered in the DB too: the next start reads the catalog from there.
        assert_eq!(
            app.db.load_catalog().get("first").map(Vec::as_slice),
            Some(&[CatalogModel::name_only("deepseek-v4-flash")][..])
        );
    }

    #[tokio::test]
    async fn plain_j_types_into_the_search_box_instead_of_moving_the_selection() {
        let mut app = app_with_no_models("");
        app.models.search_active = true;

        // `j` is buffered as the first half of the `jk` exit sequence, so it
        // lands in the field only once the next key arrives.
        handle_key(&mut app, KeyCode::Char('j'), KeyModifiers::NONE).unwrap();
        handle_key(&mut app, KeyCode::Char('x'), KeyModifiers::NONE).unwrap();

        assert_eq!(app.models.search_field.value, "jx");
        assert_eq!(app.models.selected, 0);
    }

    #[tokio::test]
    async fn t_with_empty_list_and_blank_search_does_nothing() {
        let mut app = app_with_no_models("   ");

        handle_key(&mut app, KeyCode::Char('t'), KeyModifiers::NONE).unwrap();

        assert!(!app.tests.pending.contains("first"));
        assert!(app.models.catalog.get("first").unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn completed_test_keeps_row_when_fresh_catalog_omits_it() {
        use crate::config::test_support::ConfigDirGuard;
        use axum::response::IntoResponse;
        use axum::routing::{get, post};
        use axum::{Json, Router};

        async fn chat() -> impl IntoResponse {
            Json(serde_json::json!({"content": [{"type": "text", "text": "pong"}]}))
        }
        async fn catalog() -> impl IntoResponse {
            Json(serde_json::json!({"data": [{"id": "other-model"}]}))
        }

        let addr = crate::tester::tests::spawn_test_server(
            Router::new()
                .route("/v1/messages", post(chat))
                .route("/v1/models", get(catalog)),
        )
        .await;

        let _guard = ConfigDirGuard::new();
        let mut app = app_with_current("first");
        app.mode = Mode::Models;
        app.models.search_active = false;
        app.models.selected = 0;
        app.config.providers.get_mut("first").unwrap().base_url = format!("http://{addr}/v1");

        handle_key(&mut app, KeyCode::Char('t'), KeyModifiers::NONE).unwrap();

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while !app.drain_test_results() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(!app.tests.pending.contains("first"), "test never completed");

        // The refreshed catalog omits the tested model; the row must survive so
        // the result stays visible (regression: it used to vanish on completion).
        assert!(build_models(&app).iter().any(|m| m.name == "first-model"));
        assert!(
            app.tests
                .results
                .get("first")
                .is_some_and(|m| m.contains_key("first-model"))
        );
        assert!(
            app.tests
                .results
                .get("first")
                .is_some_and(|m| m.contains_key("first-model"))
        );
    }

    #[test]
    fn r_without_credentials_reports_error_without_starting_refresh() {
        let mut app = app_with_current("first");
        app.models.search_active = false;
        app.config
            .providers
            .get_mut("first")
            .unwrap()
            .api_key
            .clear();

        handle_key(&mut app, KeyCode::Char('r'), KeyModifiers::NONE).unwrap();

        assert!(!app.models.refresh_inflight);
        let (msg, kind, _) = app.message.as_ref().expect("error message shown");
        assert!(matches!(kind, MessageKind::Error), "{msg}");
        assert!(msg.contains("Cannot refresh 'first'"), "{msg}");
    }

    #[test]
    fn r_is_ignored_while_a_refresh_is_in_flight() {
        let mut app = app_with_current("first");
        app.models.search_active = false;
        app.models.refresh_inflight = true;

        handle_key(&mut app, KeyCode::Char('r'), KeyModifiers::NONE).unwrap();

        // The in-flight marker survives and no misleading message is shown.
        assert!(app.models.refresh_inflight);
        assert!(app.message.is_none());
    }
}

#[cfg(test)]
mod column_layout {
    use super::*;

    /// Each configured column is as wide as its own widest value. Collapsing
    /// them into one width (or one joined string) is what made two price
    /// columns ragged.
    #[test]
    fn every_column_takes_its_own_widest_cell() {
        let cells = vec![
            vec!["$0".to_string(), "$0".to_string()],
            vec!["$0.15".to_string(), "$0.6".to_string()],
            vec!["$150k".to_string(), "$2M".to_string()],
        ];

        assert_eq!(column_widths(&cells, 2), vec![5, 4]);
    }

    /// On a short terminal the dialog draws fewer rows than the fallback
    /// viewport; navigation must follow what is really on screen.
    #[test]
    fn navigation_keeps_the_highlight_inside_the_drawn_rows() {
        use crate::config::test_support::ConfigDirGuard;
        use crate::tui::testing::tests::app_with_current;

        let _guard = ConfigDirGuard::new();
        let mut app = app_with_current("first");
        let catalog: Vec<CatalogModel> = (0..20)
            .map(|i| CatalogModel::name_only(&format!("m{i:02}")))
            .collect();
        app.models.catalog.insert("first".to_string(), catalog);
        app.models.search_active = false;
        app.models.list_height = 3;

        for _ in 0..6 {
            nav(&mut app, 20, ListMove::Down, 1);
        }

        assert_eq!(app.models.selected, 6);
        let top = app.models.scroll as usize;
        assert!(
            (top..top + 3).contains(&app.models.selected),
            "highlight {} outside the drawn rows {top}..{}",
            app.models.selected,
            top + 3
        );
    }

    /// The draw is what tells navigation how many rows it actually has.
    #[test]
    fn the_draw_records_the_real_list_height() {
        use crate::config::test_support::ConfigDirGuard;
        use crate::tui::testing::tests::app_with_current;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let _guard = ConfigDirGuard::new();
        let mut app = app_with_current("first");
        let catalog: Vec<CatalogModel> = (0..20)
            .map(|i| CatalogModel::name_only(&format!("m{i:02}")))
            .collect();
        app.models.catalog.insert("first".to_string(), catalog);
        app.models.search_active = false;

        let mut terminal = Terminal::new(TestBackend::new(60, 14)).unwrap();
        terminal.draw(|f| draw_popup(f, &mut app)).unwrap();

        let height = app.models.list_height as usize;
        assert!(
            height > 0 && height < FALLBACK_VIEWPORT,
            "a 14-row terminal draws {height} rows, not the {FALLBACK_VIEWPORT}-row fallback"
        );
    }

    /// Columns past what fits are dropped, never clipped off the row.
    #[test]
    fn only_the_columns_that_fit_are_shown() {
        assert_eq!(columns_that_fit(&[6, 6, 6], 60), 3);
        assert_eq!(columns_that_fit(&[6, 6, 6], 16 + 6 + 2 + 6), 2);
        assert_eq!(columns_that_fit(&[40], 30), 0, "not even one fits");
        assert_eq!(columns_that_fit(&[], 30), 0);
    }

    #[test]
    fn a_column_with_no_values_anywhere_is_empty() {
        let cells = vec![vec!["$1".to_string(), "-".to_string()]];

        assert_eq!(column_widths(&cells, 2), vec![2, 1]);
    }

    /// The highlight has to reach the value columns: only the name turning
    /// primary leaves a selected row looking half-selected.
    #[test]
    fn the_selected_row_highlights_its_value_columns() {
        use crate::catalog::{CatalogModel, ModelAttr};
        use crate::config::test_support::ConfigDirGuard;
        use crate::tui::testing::tests::app_with_current;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use serde_json::json;

        let _guard = ConfigDirGuard::new();
        let mut app = app_with_current("first");
        let model = |name: &str, price: f64| {
            let mut m = CatalogModel::name_only(name);
            m.attrs.insert("pric".into(), json!(price));
            m
        };
        app.models.catalog.insert(
            "first".to_string(),
            vec![model("a-first", 0.15), model("z-second", 2.0)],
        );
        app.models.attrs.insert(
            "first".to_string(),
            vec![ModelAttr {
                label: "price".into(),
                paths: vec!["pric".into()],
                scale: 1.0,
                prefix: "$".into(),
            }],
        );
        app.mode = Mode::Models;
        app.models.search_active = false;
        app.models.selected = 0;

        let mut terminal = Terminal::new(TestBackend::new(80, 10)).unwrap();
        terminal.draw(|f| draw_popup(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        let dollar = |needle: &str| {
            let y = (0..buf.area.height)
                .find(|&y| {
                    let line: String = (0..buf.area.width)
                        .map(|x| buf[(x, y)].symbol().to_string())
                        .collect();
                    line.contains(needle)
                })
                .expect("row rendered");
            let x = (0..buf.area.width)
                .find(|&x| buf[(x, y)].symbol() == "$")
                .expect("value column rendered");
            buf[(x, y)].style().fg
        };

        assert_eq!(dollar("a-first"), Some(t::PRIMARY), "selected row");
        assert_eq!(dollar("z-second"), Some(t::MUTED), "other rows");
    }
}
