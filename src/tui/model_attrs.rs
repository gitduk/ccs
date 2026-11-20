//! Models panel's attribute-columns dialog (`a`).
//!
//! Step one lists the dotted paths found in the selected provider's catalog
//! (`pricing.prompt`, `limit.context`, …). Picking one opens step two, where the
//! paths, a scale and a prefix turn a catalog value into a column rendered on
//! the right of every models row. Columns persist in the DB per provider.

use std::collections::BTreeMap;

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::Frame;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph};
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

use super::models::current_provider;
use super::state::{
    ATTR_FIELD_COUNT, ATTR_LABEL_FIELD, ATTR_PATH1_FIELD, ATTR_PATH2_FIELD, ATTR_PREFIX_FIELD,
    ATTR_SCALE_FIELD, App, AttrEdit, AttrPath, FormField, ListMove, MessageKind, Mode,
    ModelAttrsState, list_move,
};
use super::theme::{self as t};
use super::ui::format::{
    attr_number, cursor_split_spans, fmt_attr_value, pad_width, search_line, truncate_width,
};
use super::ui::layout::centered_fixed;
use crate::catalog::ModelAttr;

/// Rows visible in the path list before it scrolls.
const PATH_ROWS: usize = 12;
/// Share of the terminal the dialogs take; `centered_fixed` takes a percentage.
const LIST_PERCENT: u16 = 72;
const EDIT_PERCENT: u16 = 60;
/// Columns the row marker occupies (`  ▶ ○ `).
const ROW_MARKER_W: usize = 7;
/// Blank columns between the path, count and sample columns.
const COL_GAP: usize = 2;
/// A path column narrower than this is useless; wider than the space left is
/// worse.
const MIN_PATH_W: usize = 24;
/// The sample value keeps at least this much room, or the path column shrinks.
const MIN_SAMPLE_W: usize = 20;

/// Open the dialog for the provider the models panel is showing.
pub(super) fn open(app: &mut App) {
    let Some(provider) = current_provider(app).map(str::to_string) else {
        return;
    };
    app.attrs_dialog = Some(ModelAttrsState {
        paths: discover_paths(app, &provider),
        search_field: FormField::search(),
        search_active: false,
        selected: 0,
        scroll: 0,
        edit: None,
    });
    app.mode = Mode::ModelAttrs;
}

pub(super) fn handle_paste(app: &mut App, text: &str) -> crate::error::Result<()> {
    let Some(state) = app.attrs_dialog.as_mut() else {
        return Ok(());
    };
    match state.edit.as_mut() {
        Some(edit) => edit.fields[edit.focus].insert_str(text),
        None if state.search_active => {
            state.search_field.insert_str(text);
            state.selected = 0;
            state.scroll = 0;
        }
        None => {}
    }
    Ok(())
}

pub(super) fn handle_key(
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> crate::error::Result<()> {
    let ctrl = modifiers.contains(KeyModifiers::CONTROL);

    if app.attrs_dialog.is_none() {
        app.mode = Mode::Models;
        return Ok(());
    }

    if app.attrs_dialog.as_ref().is_some_and(|s| s.edit.is_some()) {
        return handle_edit_key(app, code, ctrl);
    }

    let total = visible_indices(app).len();

    if app.attrs_dialog.as_ref().is_some_and(|s| s.search_active) {
        if ctrl && code == KeyCode::Char('c') {
            app.attrs_dialog = None;
            app.mode = Mode::Models;
            return Ok(());
        }
        if let Some(dir) = list_move(code, ctrl, true) {
            nav(app, total, dir, 1);
            return Ok(());
        }
        match code {
            KeyCode::Esc => set_search_active(app, false),
            KeyCode::Enter => {
                set_search_active(app, false);
                begin_edit(app);
            }
            _ => {
                let mut typed = false;
                if let Some(state) = app.attrs_dialog.as_mut()
                    && let super::input::insert::InsertKeyResult::TextChanged =
                        super::input::insert::handle_field_insert_key(
                            &mut state.search_field,
                            code,
                            ctrl,
                            &mut None,
                        )
                {
                    typed = true;
                }
                if typed {
                    reset_selection(app);
                }
            }
        }
        return Ok(());
    }

    match code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.attrs_dialog = None;
            app.mode = Mode::Models;
        }
        KeyCode::Char('c') if ctrl => {
            app.attrs_dialog = None;
            app.mode = Mode::Models;
        }
        KeyCode::Char('i') => set_search_active(app, true),
        KeyCode::Char('j') | KeyCode::Down => nav(app, total, ListMove::Down, 1),
        KeyCode::Char('k') | KeyCode::Up => nav(app, total, ListMove::Up, 1),
        KeyCode::PageDown => nav(app, total, ListMove::Down, PATH_ROWS as isize),
        KeyCode::PageUp => nav(app, total, ListMove::Up, PATH_ROWS as isize),
        KeyCode::Char('d') if ctrl => nav(app, total, ListMove::Down, PATH_ROWS as isize),
        KeyCode::Char('u') if ctrl => nav(app, total, ListMove::Up, PATH_ROWS as isize),
        KeyCode::Char('G') => select_edge(app, total, true),
        KeyCode::Char('g') => select_edge(app, total, false),
        KeyCode::Char('x') => remove_selected_column(app),
        KeyCode::Enter => begin_edit(app),
        _ => {}
    }
    Ok(())
}

/// Indices into `state.paths` that survive the filter, in display order.
fn visible_indices(app: &App) -> Vec<usize> {
    let Some(state) = app.attrs_dialog.as_ref() else {
        return vec![];
    };
    let filter = state.search_field.value.to_lowercase();
    state
        .paths
        .iter()
        .enumerate()
        .filter(|(_, path)| super::state::matches_filter(&path.path, &filter))
        .map(|(idx, _)| idx)
        .collect()
}

fn selected_path(app: &App) -> Option<AttrPath> {
    let state = app.attrs_dialog.as_ref()?;
    visible_indices(app)
        .get(state.selected)
        .map(|&idx| state.paths[idx].clone())
}

fn set_search_active(app: &mut App, active: bool) {
    if let Some(state) = app.attrs_dialog.as_mut() {
        state.search_active = active;
    }
}

fn reset_selection(app: &mut App) {
    if let Some(state) = app.attrs_dialog.as_mut() {
        state.selected = 0;
        state.scroll = 0;
    }
}

fn nav(app: &mut App, total: usize, dir: ListMove, rows: isize) {
    if let Some(dialog) = app.attrs_dialog.as_mut() {
        super::state::nav(
            &mut dialog.selected,
            &mut dialog.scroll,
            dir,
            rows,
            total,
            PATH_ROWS,
        );
    }
}

fn select_edge(app: &mut App, total: usize, last: bool) {
    if let Some(dialog) = app.attrs_dialog.as_mut() {
        dialog.selected = if last { total.saturating_sub(1) } else { 0 };
        super::state::keep_visible(dialog.selected, &mut dialog.scroll, PATH_ROWS);
    }
}

/// Every dotted path the provider's catalog carries, plus configured paths the
/// catalog no longer has (so a broken column can still be found and removed).
fn discover_paths(app: &App, provider: &str) -> Vec<AttrPath> {
    let mut found: BTreeMap<String, AttrPath> = BTreeMap::new();
    for model in app.models.catalog.get(provider).into_iter().flatten() {
        for (path, value) in &model.attrs {
            let entry = found.entry(path.clone()).or_insert_with(|| AttrPath {
                path: path.clone(),
                models: 0,
                sample: sample_text(value),
            });
            entry.models += 1;
        }
    }
    for attr in app.models.attrs.get(provider).into_iter().flatten() {
        for path in &attr.paths {
            found.entry(path.clone()).or_insert_with(|| AttrPath {
                path: path.clone(),
                models: 0,
                sample: "(no longer in the catalog)".to_string(),
            });
        }
    }
    found.into_values().collect()
}

fn sample_text(value: &Value) -> String {
    match value {
        Value::String(s) => truncate_width(s, 28),
        other => truncate_width(&other.to_string(), 28),
    }
}

/// Index of the configured column whose first path is `path`.
fn column_index(app: &App, provider: &str, path: &str) -> Option<usize> {
    app.models
        .attrs
        .get(provider)?
        .iter()
        .position(|attr| attr.paths.iter().any(|p| p == path))
}

fn begin_edit(app: &mut App) {
    let Some(provider) = current_provider(app).map(str::to_string) else {
        return;
    };
    let Some(picked) = selected_path(app) else {
        return;
    };
    let path = picked.path.clone();
    let index = column_index(app, &provider, &path);
    let existing = index.and_then(|i| {
        app.models
            .attrs
            .get(&provider)
            .map(|attrs| attrs[i].clone())
    });

    let mut edit = AttrEdit::new(index, existing.as_ref());
    if existing.is_none() {
        fill_defaults(&mut edit, app, &provider, &path);
    }
    if let Some(state) = app.attrs_dialog.as_mut() {
        state.edit = Some(edit);
    }
}

/// Prefill the edit step for a fresh column: the picked path, its usual sibling
/// (prompt/completion, input/output), and the unit those fields are priced in.
fn fill_defaults(edit: &mut AttrEdit, app: &App, provider: &str, path: &str) {
    edit.fields[ATTR_LABEL_FIELD].set_value(&suggest_label(path));
    edit.fields[ATTR_PATH1_FIELD].set_value(path);
    let sibling = sibling_path(app, provider, path);
    if let Some(sibling) = &sibling {
        edit.fields[ATTR_PATH2_FIELD].set_value(sibling);
    }
    if is_price_path(path) {
        edit.fields[ATTR_PREFIX_FIELD].set_value("$");
        // Per-token prices are quoted far below any per-million price, but one
        // model's value can mislead — a free model prices at 0 — so the unit is
        // read off the largest value in the whole catalog. The preview shows the
        // result either way.
        let mut paths = vec![path.to_string()];
        paths.extend(sibling);
        let largest = max_numeric(app, provider, &paths).unwrap_or(0.0);
        edit.fields[ATTR_SCALE_FIELD].set_value(if largest < 0.01 { "1000000" } else { "1" });
    }
}

/// Largest numeric value these paths carry across the provider's catalog.
fn max_numeric(app: &App, provider: &str, paths: &[String]) -> Option<f64> {
    app.models
        .catalog
        .get(provider)?
        .iter()
        .flat_map(|model| {
            paths
                .iter()
                .filter_map(|path| model.attr(path))
                .filter_map(attr_number)
        })
        .reduce(f64::max)
}

/// `pricing.prompt` → `pricing`: the segment naming the group the value sits in.
fn suggest_label(path: &str) -> String {
    let mut segments: Vec<&str> = path.split('.').collect();
    if segments.len() > 1 {
        segments.pop();
        segments.pop().unwrap_or(path).to_string()
    } else {
        path.to_string()
    }
}

/// The counterpart field catalogs usually pair a price with, when the catalog
/// actually carries it (`pricing.prompt` → `pricing.completion`).
fn sibling_path(app: &App, provider: &str, path: &str) -> Option<String> {
    const PAIRS: [(&str, &str); 2] = [("prompt", "completion"), ("input", "output")];
    // The paired word sits in any segment: `pricing.prompt` pairs on the last
    // one, `cost.text.input.price` on the one before it.
    let mut segments: Vec<&str> = path.split('.').collect();
    let (position, other) = segments.iter().enumerate().find_map(|(i, segment)| {
        PAIRS.iter().find_map(|(a, b)| {
            if segment == a {
                Some((i, *b))
            } else if segment == b {
                Some((i, *a))
            } else {
                None
            }
        })
    })?;
    segments[position] = other;
    let candidate = segments.join(".");
    let present = app
        .models
        .catalog
        .get(provider)
        .into_iter()
        .flatten()
        .any(|model| model.attr(&candidate).is_some());
    present.then_some(candidate)
}

fn is_price_path(path: &str) -> bool {
    let lower = path.to_lowercase();
    lower.contains("pric") || lower.split('.').any(|s| s == "cost")
}

fn handle_edit_key(app: &mut App, code: KeyCode, ctrl: bool) -> crate::error::Result<()> {
    match code {
        KeyCode::Esc => {
            if let Some(state) = app.attrs_dialog.as_mut() {
                state.edit = None;
            }
            return Ok(());
        }
        KeyCode::Char('c') if ctrl => {
            if let Some(state) = app.attrs_dialog.as_mut() {
                state.edit = None;
            }
            return Ok(());
        }
        KeyCode::Enter => {
            if commit(app)?
                && let Some(state) = app.attrs_dialog.as_mut()
            {
                state.edit = None;
            }
            return Ok(());
        }
        KeyCode::Tab | KeyCode::Down => {
            move_focus(app, 1);
            return Ok(());
        }
        KeyCode::BackTab | KeyCode::Up => {
            move_focus(app, -1);
            return Ok(());
        }
        _ => {}
    }

    let mut back_to_list = false;
    if let Some(edit) = app.attrs_dialog.as_mut().and_then(|s| s.edit.as_mut()) {
        let mut pending = None;
        if let super::input::insert::InsertKeyResult::ExitInsert =
            super::input::insert::handle_field_insert_key(
                &mut edit.fields[edit.focus],
                code,
                ctrl,
                &mut pending,
            )
        {
            back_to_list = true;
        }
    }
    if back_to_list && let Some(state) = app.attrs_dialog.as_mut() {
        state.edit = None;
    }
    Ok(())
}

fn move_focus(app: &mut App, delta: isize) {
    if let Some(edit) = app.attrs_dialog.as_mut().and_then(|s| s.edit.as_mut()) {
        let total = ATTR_FIELD_COUNT as isize;
        edit.focus = ((edit.focus as isize + delta).rem_euclid(total)) as usize;
    }
}

/// Validate the edit step and write the column to the DB and the panel state.
/// `Ok(false)` leaves the edit open with `error` set.
fn commit(app: &mut App) -> crate::error::Result<bool> {
    let Some(provider) = current_provider(app).map(str::to_string) else {
        return Ok(false);
    };
    let Some(edit) = app.attrs_dialog.as_ref().and_then(|s| s.edit.as_ref()) else {
        return Ok(false);
    };

    let label = edit.fields[ATTR_LABEL_FIELD].value.trim().to_string();
    let path1 = edit.fields[ATTR_PATH1_FIELD].value.trim().to_string();
    let path2 = edit.fields[ATTR_PATH2_FIELD].value.trim().to_string();
    let scale_raw = edit.fields[ATTR_SCALE_FIELD].value.trim().to_string();
    let prefix = edit.fields[ATTR_PREFIX_FIELD].value.trim().to_string();

    let error = if label.is_empty() {
        Some("Label cannot be empty".to_string())
    } else if label_taken(app, &provider, &label, edit.index) {
        // Two columns cannot share a label: the DB keys them by it.
        Some(format!("Another column is already labelled '{label}'"))
    } else if path1.is_empty() {
        Some("Path 1 cannot be empty".to_string())
    } else {
        match scale_raw.parse::<f64>() {
            Ok(scale) if scale.is_finite() && scale > 0.0 => {
                let mut paths = vec![path1];
                if !path2.is_empty() {
                    paths.push(path2);
                }
                let attr = ModelAttr {
                    label,
                    paths,
                    scale,
                    prefix,
                };
                return match apply_column(app, &provider, edit.index, attr) {
                    Ok(()) => Ok(true),
                    Err(e) => {
                        set_edit_error(app, Some(e));
                        Ok(false)
                    }
                };
            }
            Ok(_) => Some("Scale must be a finite number greater than 0".to_string()),
            Err(_) => Some(format!(
                "Scale must be a number, e.g. 1000000 — got '{scale_raw}'"
            )),
        }
    };

    set_edit_error(app, error);
    Ok(false)
}

fn set_edit_error(app: &mut App, error: Option<String>) {
    if let Some(state) = app.attrs_dialog.as_mut()
        && let Some(edit) = state.edit.as_mut()
    {
        edit.error = error;
    }
}

/// Is `label` already used by a column other than `editing`?
fn label_taken(app: &App, provider: &str, label: &str, editing: Option<usize>) -> bool {
    app.models
        .attrs
        .get(provider)
        .into_iter()
        .flatten()
        .enumerate()
        .any(|(index, attr)| Some(index) != editing && attr.label == label)
}

/// `Err` carries the DB's refusal: the caller keeps the edit open and shows it.
fn apply_column(
    app: &mut App,
    provider: &str,
    index: Option<usize>,
    attr: ModelAttr,
) -> Result<(), String> {
    let mut columns = app.models.attrs.get(provider).cloned().unwrap_or_default();
    match index {
        Some(i) if i < columns.len() => columns[i] = attr.clone(),
        _ => columns.push(attr.clone()),
    }
    persist_columns(app, provider, columns)?;
    refresh_paths(app, provider);
    app.set_message(
        format!("Column '{}' shown — {}", attr.label, describe_paths(&attr)),
        MessageKind::Success,
    );
    Ok(())
}

/// Write a provider's columns to the DB, and only then to the in-memory mirror:
/// a column the DB refused must not look saved.
fn persist_columns(app: &mut App, provider: &str, columns: Vec<ModelAttr>) -> Result<(), String> {
    let provider_id = app
        .config
        .providers
        .get(provider)
        .map(|p| p.id.clone())
        .unwrap_or_else(|| provider.to_string());
    app.db
        .replace_model_attrs(&provider_id, provider, &columns)
        .map_err(|e| format!("Save failed: {e}"))?;
    app.models.attrs.insert(provider.to_string(), columns);
    Ok(())
}

/// Re-read the path list so a column saved with a typed path has a row to
/// remove it by, without reopening the dialog.
fn refresh_paths(app: &mut App, provider: &str) {
    let paths = discover_paths(app, provider);
    if let Some(dialog) = app.attrs_dialog.as_mut() {
        dialog.selected = dialog.selected.min(paths.len().saturating_sub(1));
        dialog.paths = paths;
    }
}

fn describe_paths(attr: &ModelAttr) -> String {
    attr.paths.join(" / ")
}

/// Drop the column whose first path is the highlighted path.
fn remove_selected_column(app: &mut App) {
    let Some(provider) = current_provider(app).map(str::to_string) else {
        return;
    };
    let Some(path) = selected_path(app).map(|p| p.path) else {
        return;
    };
    let Some(index) = column_index(app, &provider, &path) else {
        return;
    };
    let mut columns = app.models.attrs.get(&provider).cloned().unwrap_or_default();
    let removed = columns.remove(index);
    match persist_columns(app, &provider, columns) {
        Ok(()) => {
            refresh_paths(app, &provider);
            app.set_message(
                format!("Removed column '{}'", removed.label),
                MessageKind::Success,
            );
        }
        Err(e) => app.set_message(e, MessageKind::Error),
    }
}

pub(super) fn draw_popup(f: &mut Frame, app: &App) {
    let Some(state) = app.attrs_dialog.as_ref() else {
        return;
    };
    let provider = current_provider(app).unwrap_or("—");
    match &state.edit {
        Some(edit) => draw_edit(f, app, provider, edit),
        None => draw_list(f, app, provider, state),
    }
}

fn draw_list(f: &mut Frame, app: &App, provider: &str, state: &ModelAttrsState) {
    let visible = visible_indices(app);
    let window = visible.len().min(PATH_ROWS);
    let height = (window + 5) as u16;
    let area = centered_fixed(LIST_PERCENT, height, f.area());
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(t::PRIMARY))
        .title(format!(
            " {provider} — Attribute columns  {} ",
            if state.search_active { "[I]" } else { "[N]" }
        ))
        .title_style(Style::default().fg(t::TEXT).add_modifier(Modifier::BOLD))
        .padding(Padding::new(1, 1, 0, 0));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines = Vec::new();
    lines.push(search_line(
        "Filter",
        &state.search_field,
        state.search_active,
    ));
    lines.push(Line::from(vec![
        Span::styled("Enter", Style::default().fg(t::PRIMARY)),
        Span::styled(" configure  ", Style::default().fg(t::MUTED)),
        Span::styled("x", Style::default().fg(t::WARNING)),
        Span::styled(" remove  ", Style::default().fg(t::MUTED)),
        Span::styled("i", Style::default().fg(t::PRIMARY)),
        Span::styled(" filter  ", Style::default().fg(t::MUTED)),
        Span::styled("Esc", Style::default().fg(t::WARNING)),
        Span::styled(" back", Style::default().fg(t::MUTED)),
    ]));
    lines.push(Line::from(Span::styled(
        "─".repeat(inner.width as usize),
        Style::default().fg(t::MUTED),
    )));

    if visible.is_empty() {
        let hint = if state.search_field.value.is_empty() {
            "  No attributes in this catalog — refresh it with `r`"
        } else {
            "  No matches"
        };
        lines.push(Line::from(Span::styled(
            hint,
            Style::default().fg(t::MUTED),
        )));
    }

    // Columns are sized from the content so a deep path list uses the width the
    // dialog has instead of stacking everything at the left edge.
    let inner_w = inner.width as usize;
    let count_w = visible
        .iter()
        .map(|&idx| count_text(state.paths[idx].models).width())
        .max()
        .unwrap_or(0)
        .max("models".len());
    let fixed = ROW_MARKER_W + COL_GAP + count_w + COL_GAP;
    let path_w = visible
        .iter()
        .map(|&idx| state.paths[idx].path.width())
        .max()
        .unwrap_or(0)
        .min(inner_w.saturating_sub(fixed + MIN_SAMPLE_W))
        .max(MIN_PATH_W)
        .min(inner_w.saturating_sub(fixed));
    let sample_w = inner_w.saturating_sub(fixed + path_w).max(1);

    let scroll = state.scroll as usize;
    for (row, &idx) in visible.iter().enumerate().skip(scroll).take(PATH_ROWS) {
        let path = &state.paths[idx];
        let enabled = column_index(app, provider, &path.path).is_some();
        let is_selected = row == state.selected;
        let marker = if enabled { "●" } else { "○" };
        let style = if is_selected {
            Style::default().fg(t::PRIMARY).add_modifier(Modifier::BOLD)
        } else if enabled {
            Style::default().fg(t::SUCCESS)
        } else {
            Style::default().fg(t::TEXT)
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("{} {marker} ", if is_selected { "  ▶" } else { "    " }),
                style,
            ),
            Span::styled(
                format!(
                    "{}{}",
                    pad_width(&truncate_width(&path.path, path_w), path_w),
                    " ".repeat(COL_GAP)
                ),
                style,
            ),
            Span::styled(
                format!(
                    "{:>count_w$}{}",
                    count_text(path.models),
                    " ".repeat(COL_GAP)
                ),
                Style::default().fg(t::MUTED),
            ),
            Span::styled(
                truncate_width(&path.sample, sample_w),
                Style::default().fg(t::MUTED),
            ),
        ]));
    }

    f.render_widget(Paragraph::new(lines), inner);
}

fn count_text(models: usize) -> String {
    if models == 0 {
        "gone".to_string()
    } else {
        format!("{models} models")
    }
}

fn draw_edit(f: &mut Frame, app: &App, provider: &str, edit: &AttrEdit) {
    let height = (ATTR_FIELD_COUNT + 6) as u16;
    let area = centered_fixed(EDIT_PERCENT, height, f.area());
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(t::PRIMARY))
        .title(format!(" {provider} — Attribute column "))
        .title_style(Style::default().fg(t::TEXT).add_modifier(Modifier::BOLD))
        .padding(Padding::new(1, 1, 0, 0));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();
    for (idx, field) in edit.fields.iter().enumerate() {
        let focused = idx == edit.focus;
        let label_style = if focused {
            Style::default().fg(t::PRIMARY).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(t::MUTED)
        };
        let mut spans = vec![Span::styled(format!("{:>8}  ", field.label), label_style)];
        if focused {
            spans.extend(cursor_split_spans(&field.value, field.cursor, t::PRIMARY));
        } else {
            spans.push(Span::raw(field.value.clone()));
        }
        lines.push(Line::from(spans));
    }

    lines.push(Line::from(vec![
        Span::styled("  Preview  ", Style::default().fg(t::MUTED)),
        Span::styled(
            preview(app, provider, edit),
            Style::default().fg(t::SUCCESS),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled("Enter", Style::default().fg(t::PRIMARY)),
        Span::styled(" save  ", Style::default().fg(t::MUTED)),
        Span::styled("Tab", Style::default().fg(t::PRIMARY)),
        Span::styled(" next field  ", Style::default().fg(t::MUTED)),
        Span::styled("Esc", Style::default().fg(t::WARNING)),
        Span::styled(" back", Style::default().fg(t::MUTED)),
    ]));
    if let Some(err) = &edit.error {
        lines.push(Line::from(Span::styled(
            format!("  ✗ {err}"),
            Style::default().fg(t::ERROR),
        )));
    }

    f.render_widget(Paragraph::new(lines), inner);
}

/// What the column would look like, rendered against a model that carries the
/// first path — empty when no model does.
fn preview(app: &App, provider: &str, edit: &AttrEdit) -> String {
    let path1 = edit.fields[ATTR_PATH1_FIELD].value.trim();
    let path2 = edit.fields[ATTR_PATH2_FIELD].value.trim();
    let Ok(scale) = edit.fields[ATTR_SCALE_FIELD].value.trim().parse::<f64>() else {
        return "(scale is not a number)".to_string();
    };
    let attr = ModelAttr {
        label: String::new(),
        paths: if path2.is_empty() {
            vec![path1.to_string()]
        } else {
            vec![path1.to_string(), path2.to_string()]
        },
        scale,
        prefix: edit.fields[ATTR_PREFIX_FIELD].value.trim().to_string(),
    };
    app.models
        .catalog
        .get(provider)
        .into_iter()
        .flatten()
        .find_map(|model| fmt_attr_value(&attr, model))
        .unwrap_or_else(|| "(no model carries this path)".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::CatalogModel;
    use crate::config::test_support::ConfigDirGuard;
    use crate::tui::testing::tests::app_with_current;

    fn app_with_priced_catalog() -> App {
        let mut app = app_with_current("first");
        let mut priced = CatalogModel::name_only("m-priced");
        priced
            .attrs
            .insert("pricing.prompt".into(), Value::String("0.000002".into()));
        priced.attrs.insert(
            "pricing.completion".into(),
            Value::String("0.000006".into()),
        );
        priced
            .attrs
            .insert("context_length".into(), Value::from(200000));
        let mut bare = CatalogModel::name_only("m-bare");
        bare.attrs
            .insert("context_length".into(), Value::from(32000));
        app.models
            .catalog
            .insert("first".to_string(), vec![priced, bare]);
        app
    }

    fn path_names(state: &ModelAttrsState) -> Vec<&str> {
        state.paths.iter().map(|p| p.path.as_str()).collect()
    }

    #[test]
    fn open_lists_every_catalog_path_with_its_model_count() {
        let _guard = ConfigDirGuard::new();
        let mut app = app_with_priced_catalog();

        open(&mut app);

        let state = app.attrs_dialog.as_ref().unwrap();
        assert_eq!(
            path_names(state),
            vec!["context_length", "pricing.completion", "pricing.prompt"]
        );
        let context = state
            .paths
            .iter()
            .find(|p| p.path == "context_length")
            .unwrap();
        assert_eq!(context.models, 2, "both models carry it");
        assert_eq!(app.mode, Mode::ModelAttrs);
    }

    #[test]
    fn open_keeps_a_configured_path_the_catalog_dropped() {
        let _guard = ConfigDirGuard::new();
        let mut app = app_with_priced_catalog();
        app.models.attrs.insert(
            "first".to_string(),
            vec![ModelAttr {
                label: "gone".into(),
                paths: vec!["cost.text.input.price".into()],
                scale: 1.0,
                prefix: "$".into(),
            }],
        );

        open(&mut app);

        let gone = app
            .attrs_dialog
            .as_ref()
            .unwrap()
            .paths
            .iter()
            .find(|p| p.path == "cost.text.input.price")
            .expect("a configured path must stay removable");
        assert_eq!(gone.models, 0);
    }

    #[test]
    fn picking_a_prompt_price_prefills_its_sibling_scale_and_currency() {
        let _guard = ConfigDirGuard::new();
        let mut app = app_with_priced_catalog();
        open(&mut app);
        let prompt_idx = app
            .attrs_dialog
            .as_ref()
            .unwrap()
            .paths
            .iter()
            .position(|p| p.path == "pricing.prompt")
            .unwrap();
        app.attrs_dialog.as_mut().unwrap().selected = prompt_idx;

        begin_edit(&mut app);

        let edit = app.attrs_dialog.as_ref().unwrap().edit.as_ref().unwrap();
        assert_eq!(edit.fields[ATTR_LABEL_FIELD].value, "pricing");
        assert_eq!(edit.fields[ATTR_PATH1_FIELD].value, "pricing.prompt");
        assert_eq!(edit.fields[ATTR_PATH2_FIELD].value, "pricing.completion");
        assert_eq!(edit.fields[ATTR_SCALE_FIELD].value, "1000000");
        assert_eq!(edit.fields[ATTR_PREFIX_FIELD].value, "$");
    }

    /// A free model sorting first must not read as "the catalog quotes tokens":
    /// the unit comes from the largest price in the catalog, not one sample.
    #[test]
    fn a_free_model_s_first_place_does_not_flip_the_price_unit() {
        let _guard = ConfigDirGuard::new();
        let mut app = app_with_current("first");
        let mut free = CatalogModel::name_only("a-free");
        free.attrs
            .insert("cost.text.input.price".into(), Value::from(0.0));
        free.attrs
            .insert("cost.text.output.price".into(), Value::from(0.0));
        let mut paid = CatalogModel::name_only("z-paid");
        paid.attrs
            .insert("cost.text.input.price".into(), Value::from(0.15));
        paid.attrs
            .insert("cost.text.output.price".into(), Value::from(0.47));
        app.models
            .catalog
            .insert("first".to_string(), vec![free, paid]);
        open(&mut app);
        let idx = app
            .attrs_dialog
            .as_ref()
            .unwrap()
            .paths
            .iter()
            .position(|p| p.path == "cost.text.input.price")
            .unwrap();
        app.attrs_dialog.as_mut().unwrap().selected = idx;

        begin_edit(&mut app);

        let edit = app.attrs_dialog.as_ref().unwrap().edit.as_ref().unwrap();
        assert_eq!(edit.fields[ATTR_SCALE_FIELD].value, "1");
        assert_eq!(
            edit.fields[ATTR_PATH2_FIELD].value,
            "cost.text.output.price"
        );
    }

    #[test]
    fn saving_stores_the_column_in_the_db_and_the_panel() {
        let _guard = ConfigDirGuard::new();
        let mut app = app_with_priced_catalog();
        open(&mut app);
        let prompt_idx = app
            .attrs_dialog
            .as_ref()
            .unwrap()
            .paths
            .iter()
            .position(|p| p.path == "pricing.prompt")
            .unwrap();
        app.attrs_dialog.as_mut().unwrap().selected = prompt_idx;
        begin_edit(&mut app);

        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE).unwrap();

        let stored = &app.models.attrs["first"];
        assert_eq!(stored.len(), 1);
        assert_eq!(
            stored[0].paths,
            vec!["pricing.prompt", "pricing.completion"]
        );
        assert!(
            app.attrs_dialog.as_ref().unwrap().edit.is_none(),
            "saving returns to the path list"
        );
        assert_eq!(app.db.load_model_attrs().get("first"), Some(stored));
        assert_eq!(app.mode, Mode::ModelAttrs, "the dialog stays open");
    }

    #[test]
    fn a_non_numeric_scale_keeps_the_edit_open_with_its_error() {
        let _guard = ConfigDirGuard::new();
        let mut app = app_with_priced_catalog();
        open(&mut app);
        begin_edit(&mut app);
        app.attrs_dialog
            .as_mut()
            .unwrap()
            .edit
            .as_mut()
            .unwrap()
            .fields[ATTR_SCALE_FIELD]
            .set_value("per-token");

        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE).unwrap();

        let edit = app.attrs_dialog.as_ref().unwrap().edit.as_ref().unwrap();
        assert!(edit.error.is_some());
        assert!(!app.models.attrs.contains_key("first"));
        assert!(app.db.load_model_attrs().is_empty());
    }

    #[test]
    fn a_label_already_in_use_is_refused_before_the_write() {
        let _guard = ConfigDirGuard::new();
        let mut app = app_with_priced_catalog();
        app.models.attrs.insert(
            "first".to_string(),
            vec![ModelAttr {
                label: "price".into(),
                paths: vec!["pricing.prompt".into()],
                scale: 1e6,
                prefix: "$".into(),
            }],
        );
        open(&mut app);
        let completion = app
            .attrs_dialog
            .as_ref()
            .unwrap()
            .paths
            .iter()
            .position(|p| p.path == "pricing.completion")
            .unwrap();
        app.attrs_dialog.as_mut().unwrap().selected = completion;
        begin_edit(&mut app);
        app.attrs_dialog
            .as_mut()
            .unwrap()
            .edit
            .as_mut()
            .unwrap()
            .fields[ATTR_LABEL_FIELD]
            .set_value("price");

        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE).unwrap();

        let edit = app.attrs_dialog.as_ref().unwrap().edit.as_ref().unwrap();
        assert!(
            edit.error
                .as_deref()
                .unwrap_or("")
                .contains("already labelled")
        );
        assert_eq!(app.models.attrs["first"].len(), 1, "nothing written");
    }

    #[test]
    fn a_non_finite_scale_is_refused() {
        let _guard = ConfigDirGuard::new();
        let mut app = app_with_priced_catalog();
        open(&mut app);
        begin_edit(&mut app);
        app.attrs_dialog
            .as_mut()
            .unwrap()
            .edit
            .as_mut()
            .unwrap()
            .fields[ATTR_SCALE_FIELD]
            .set_value("inf");

        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE).unwrap();

        assert!(app.attrs_dialog.as_ref().unwrap().edit.is_some());
        assert!(!app.models.attrs.contains_key("first"));
    }

    #[test]
    fn x_removes_the_configured_column_for_the_highlighted_path() {
        let _guard = ConfigDirGuard::new();
        let mut app = app_with_priced_catalog();
        open(&mut app);
        let context_idx = app
            .attrs_dialog
            .as_ref()
            .unwrap()
            .paths
            .iter()
            .position(|p| p.path == "context_length")
            .unwrap();
        app.attrs_dialog.as_mut().unwrap().selected = context_idx;
        begin_edit(&mut app);
        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE).unwrap();
        assert_eq!(app.models.attrs["first"].len(), 1);

        handle_key(&mut app, KeyCode::Char('x'), KeyModifiers::NONE).unwrap();

        assert!(app.models.attrs["first"].is_empty());
        assert!(app.db.load_model_attrs().values().all(|v| v.is_empty()));
    }

    fn type_filter(app: &mut App, text: &str) {
        for ch in text.chars() {
            handle_key(app, KeyCode::Char(ch), KeyModifiers::NONE).unwrap();
        }
    }

    #[test]
    fn typing_in_the_filter_narrows_the_list() {
        let _guard = ConfigDirGuard::new();
        let mut app = app_with_priced_catalog();
        open(&mut app);
        assert_eq!(visible_indices(&app).len(), 3);

        handle_key(&mut app, KeyCode::Char('i'), KeyModifiers::NONE).unwrap();
        type_filter(&mut app, "prompt");

        let visible = visible_indices(&app);
        assert_eq!(visible.len(), 1);
        assert_eq!(
            app.attrs_dialog.as_ref().unwrap().paths[visible[0]].path,
            "pricing.prompt"
        );

        // Esc drops focus but keeps the filter, so the list stays narrowed.
        handle_key(&mut app, KeyCode::Esc, KeyModifiers::NONE).unwrap();
        assert!(!app.attrs_dialog.as_ref().unwrap().search_active);
        assert_eq!(visible_indices(&app).len(), 1);
    }

    #[test]
    fn enter_while_filtering_configures_the_matching_path() {
        let _guard = ConfigDirGuard::new();
        let mut app = app_with_priced_catalog();
        open(&mut app);
        handle_key(&mut app, KeyCode::Char('i'), KeyModifiers::NONE).unwrap();
        type_filter(&mut app, "completion");

        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE).unwrap();

        let edit = app.attrs_dialog.as_ref().unwrap().edit.as_ref().unwrap();
        assert_eq!(edit.fields[ATTR_PATH1_FIELD].value, "pricing.completion");
        assert_eq!(edit.fields[ATTR_PATH2_FIELD].value, "pricing.prompt");
    }

    #[test]
    fn a_filter_matching_nothing_leaves_no_row_to_configure() {
        let _guard = ConfigDirGuard::new();
        let mut app = app_with_priced_catalog();
        open(&mut app);
        handle_key(&mut app, KeyCode::Char('i'), KeyModifiers::NONE).unwrap();
        type_filter(&mut app, "zzz");

        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE).unwrap();

        assert!(visible_indices(&app).is_empty());
        assert!(app.attrs_dialog.as_ref().unwrap().edit.is_none());
    }

    #[test]
    fn esc_leaves_the_dialog_without_touching_columns() {
        let _guard = ConfigDirGuard::new();
        let mut app = app_with_priced_catalog();
        open(&mut app);

        handle_key(&mut app, KeyCode::Esc, KeyModifiers::NONE).unwrap();

        assert!(app.attrs_dialog.is_none());
        assert_eq!(app.mode, Mode::Models);
        assert!(app.models.attrs.is_empty());
    }
}
