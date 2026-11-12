//! Global model-route panel (`R`): model → provider/model.
//!
//! A matching request is served exclusively by the named provider. Rows are
//! edited in a draft and written to `config.global_routes` only once they
//! validate, then saved and pushed to a running proxy, so a half-typed rule
//! never reaches the proxy.

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::Frame;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::config::{self, AppConfig, GlobalRoute, RouteBlock};

use super::ServerHandle;
use super::server::sync_proxy_config;
use super::state::{
    App, GlobalRouteEdit, GlobalRouteField, ListMove, Mode, SuggestState, all_models,
    filter_suggestions, list_move, step, suggest_move,
};
use super::theme::{self as t};
use super::ui::format::{SuggestionStyle, cursor_split_spans, render_suggestion_lines};
use super::ui::layout::centered_fixed;

pub(super) fn open(app: &mut App) {
    app.global_routes.error = None;
    app.global_routes.editing = None;
    app.global_routes.pending_key = None;
    app.global_routes.clamp(app.config.global_routes.len());
    app.mode = Mode::GlobalRoutes;
}

pub(super) fn handle_paste(app: &mut App, text: &str) -> crate::error::Result<()> {
    if let Some(edit) = app.global_routes.editing.as_mut() {
        edit.text.insert_str(text.trim());
        sync_draft(edit);
    }
    Ok(())
}

pub(super) fn handle_key(
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
    server: &mut Option<ServerHandle>,
) -> crate::error::Result<()> {
    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    app.global_routes.error = None;

    if app.global_routes.editing.is_some() {
        handle_edit_key(app, code, ctrl, server);
        return Ok(());
    }

    let total = app.config.global_routes.len();

    let pending = super::input::insert::consume_pending_key(&mut app.global_routes.pending_key);
    if pending == Some('d') && code == KeyCode::Char('d') {
        delete_selected(app, server);
        return Ok(());
    }

    if let Some(dir) = list_move(code, ctrl, false) {
        app.global_routes.selected = step(app.global_routes.selected, dir.delta(), total);
        return Ok(());
    }

    match code {
        KeyCode::Esc | KeyCode::Char('q') => app.mode = Mode::Normal,
        KeyCode::Char('c') if ctrl => app.mode = Mode::Normal,
        KeyCode::Char('G') => app.global_routes.selected = total.saturating_sub(1),
        KeyCode::Char('a') | KeyCode::Char('o') if !ctrl => add_rule(app),
        KeyCode::Char('i') | KeyCode::Enter => start_edit(app, GlobalRouteField::Pattern),
        KeyCode::Char('t') => start_edit(app, GlobalRouteField::Target),
        KeyCode::Char(' ') => toggle_selected(app, server),
        KeyCode::Char('d') if !ctrl => {
            app.global_routes.pending_key = Some(('d', std::time::Instant::now()));
        }
        KeyCode::Char('K') => move_selected(app, -1, server),
        KeyCode::Char('J') => move_selected(app, 1, server),
        _ => {}
    }
    Ok(())
}

// ─── Row actions ────────────────────────────────────────────────────────────

fn add_rule(app: &mut App) {
    let provider = app
        .config
        .providers
        .iter()
        .find(|(_, p)| p.enabled)
        .map(|(name, _)| name.clone())
        .unwrap_or_default();
    let draft = GlobalRoute::new(provider, "");
    app.global_routes.selected = app.config.global_routes.len();
    begin_edit(app, GlobalRouteField::Pattern, draft);
}

fn start_edit(app: &mut App, field: GlobalRouteField) {
    let Some(rule) = app.config.global_routes.get(app.global_routes.selected) else {
        return;
    };
    begin_edit(app, field, rule.clone());
}

fn begin_edit(app: &mut App, field: GlobalRouteField, draft: GlobalRoute) {
    let text = field_value(&draft, field);
    app.global_routes.editing = Some(GlobalRouteEdit {
        index: app.global_routes.selected,
        field,
        text: super::state::FormField::text("", &text),
        draft,
        suggest: SuggestState::default(),
    });
}

/// Apply the typed field to the draft, then close the suggestion list.
fn sync_draft(edit: &mut GlobalRouteEdit) {
    let value = edit.text.value.clone();
    match edit.field {
        GlobalRouteField::Pattern => edit.draft.rule.pattern = value,
        GlobalRouteField::Target => match value.split_once('/') {
            Some((provider, model)) => {
                edit.draft.provider = provider.to_string();
                edit.draft.rule.target = model.to_string();
            }
            None => {
                // No slash typed yet: keep the provider the field started with
                // instead of eating it, and treat the text as the model name.
                edit.draft.rule.target = value;
            }
        },
    }
    edit.suggest.reset();
}

/// Replace the focused field's text, cursor at the end, then re-sync the draft.
fn set_text(edit: &mut GlobalRouteEdit, text: &str) {
    edit.text = super::state::FormField::text("", text);
    sync_draft(edit);
}

fn handle_edit_key(app: &mut App, code: KeyCode, ctrl: bool, server: &mut Option<ServerHandle>) {
    // Esc closes an open suggestion list before it abandons the row.
    if code == KeyCode::Esc
        && let Some(edit) = app.global_routes.editing.as_mut()
        && edit.suggest.active
    {
        edit.suggest.reset();
        app.global_routes.pending_key = None;
        return;
    }
    if let Some(dir) = suggest_move(code, ctrl) {
        navigate_suggestion(app, dir);
        return;
    }
    match code {
        KeyCode::Enter => {
            if accept_suggestion(app) {
                // Suggestion picked: keep editing so the next field can be filled.
                return;
            }
            commit_edit(app, server);
        }
        KeyCode::Tab | KeyCode::BackTab => {
            // The buffered half of a `jk` belongs to the field it was typed in.
            app.global_routes.pending_key = None;
            switch_field(app);
        }
        KeyCode::Char('c') if ctrl => {
            app.global_routes.editing = None;
            app.global_routes.pending_key = None;
            app.global_routes.clamp(app.config.global_routes.len());
            app.mode = Mode::Normal;
            return;
        }
        _ => {}
    }

    let st = &mut app.global_routes;
    let (Some(edit), pending) = (st.editing.as_mut(), &mut st.pending_key) else {
        return;
    };
    match super::input::insert::handle_field_insert_key(&mut edit.text, code, ctrl, pending) {
        super::input::insert::InsertKeyResult::TextChanged => sync_draft(edit),
        super::input::insert::InsertKeyResult::ExitInsert => {
            app.global_routes.editing = None;
            app.global_routes.clamp(app.config.global_routes.len());
        }
        super::input::insert::InsertKeyResult::Consumed
        | super::input::insert::InsertKeyResult::NotHandled => {}
    }
}

/// Move to the other field, keeping the row as a draft. Two fields, so Tab and
/// Shift-Tab are the same move.
fn switch_field(app: &mut App) {
    let Some(edit) = app.global_routes.editing.as_mut() else {
        return;
    };
    sync_draft(edit);
    let field = edit.field.next();
    edit.field = field;
    let text = field_value(&edit.draft, field);
    set_text(edit, &text);
}

fn navigate_suggestion(app: &mut App, dir: ListMove) {
    let len = match app.global_routes.editing.as_ref() {
        Some(edit) => suggestions(app, edit).len(),
        None => return,
    };
    if len == 0 {
        return;
    }
    if let Some(edit) = app.global_routes.editing.as_mut() {
        match dir {
            ListMove::Down => edit.suggest.advance(len),
            ListMove::Up => edit.suggest.retreat(),
        }
    }
}

/// Fill the focused field from the highlighted suggestion.
fn accept_suggestion(app: &mut App) -> bool {
    let Some(edit) = app.global_routes.editing.as_ref() else {
        return false;
    };
    if !edit.suggest.active {
        return false;
    }
    let picked = suggestions(app, edit).get(edit.suggest.idx).cloned();
    let Some(picked) = picked else {
        return false;
    };
    if let Some(edit) = app.global_routes.editing.as_mut() {
        set_text(edit, &picked);
    }
    true
}

/// Validate the draft and, when it holds, write it to the config.
fn commit_edit(app: &mut App, server: &mut Option<ServerHandle>) {
    let Some(edit) = app.global_routes.editing.as_mut() else {
        return;
    };
    let trimmed = edit.text.value.trim().to_string();
    set_text(edit, &trimmed);
    let index = edit.index;
    let mut draft = edit.draft.clone();
    draft.rule.pattern = draft.rule.pattern.trim().to_string();
    draft.provider = draft.provider.trim().to_string();
    draft.rule.target = draft.rule.target.trim().to_string();

    let is_new = index >= app.config.global_routes.len();
    if let Err(err) = validate(app, &draft, is_new) {
        app.global_routes.error = Some(err);
        return;
    }

    let mut next = app.config.clone();
    if index > next.global_routes.len() {
        app.global_routes.error = Some("Row list changed while editing; reopen the panel".into());
        return;
    }
    if index == next.global_routes.len() {
        next.global_routes.push(draft);
    } else {
        next.global_routes[index] = draft;
    }
    persist(app, server, next);
    if app.global_routes.error.is_none() {
        app.global_routes.editing = None;
    }
}

fn toggle_selected(app: &mut App, server: &mut Option<ServerHandle>) {
    let index = app.global_routes.selected;
    let mut next = app.config.clone();
    let Some(rule) = next.global_routes.get_mut(index) else {
        return;
    };
    rule.rule.enabled = !rule.rule.enabled;
    persist(app, server, next);
}

fn delete_selected(app: &mut App, server: &mut Option<ServerHandle>) {
    let index = app.global_routes.selected;
    if index >= app.config.global_routes.len() {
        return;
    }
    let mut next = app.config.clone();
    next.global_routes.remove(index);
    persist(app, server, next);
    app.global_routes.clamp(app.config.global_routes.len());
}

fn move_selected(app: &mut App, delta: isize, server: &mut Option<ServerHandle>) {
    let index = app.global_routes.selected;
    let target = index as isize + delta;
    if index >= app.config.global_routes.len()
        || target < 0
        || target as usize >= app.config.global_routes.len()
    {
        return;
    }
    let target = target as usize;
    let mut next = app.config.clone();
    next.global_routes.swap(index, target);
    persist(app, server, next);
    app.global_routes.selected = target;
    app.global_routes.clamp(app.config.global_routes.len());
}

/// Save the candidate config, then adopt it and push it to a running proxy.
fn persist(app: &mut App, server: &mut Option<ServerHandle>, next: AppConfig) {
    match config::save_config(&next) {
        Ok(()) => {
            app.config = next;
            sync_proxy_config(app, server);
        }
        Err(e) => app.global_routes.error = Some(format!("Config not saved: {e}")),
    }
}

// ─── Validation & suggestions ───────────────────────────────────────────────

/// Reject a row that could not be served: empty pattern or target,
/// unknown/disabled provider, and a target the provider's own catalog does not
/// list (when that catalog is known).
///
/// The pattern is never checked against a catalog: it matches the model name a
/// client sends, and the upstream only ever sees the target.
///
/// A row that already exists skips the provider check: an existing rule on a
/// since-disabled provider must stay editable, and the panel marks it with ⚠.
fn validate(app: &App, rule: &GlobalRoute, is_new: bool) -> Result<(), String> {
    let provider = rule.provider.as_str();
    if rule.rule.pattern.is_empty() {
        return Err("Pattern cannot be empty".into());
    }
    if provider.is_empty() {
        return Err("Right side must be provider/model".into());
    }
    if is_new {
        match app.config.route_block(provider) {
            Some(RouteBlock::Missing) => return Err(format!("Unknown provider '{provider}'")),
            Some(RouteBlock::Disabled) => {
                return Err(format!("Provider '{provider}' is disabled"));
            }
            None => {}
        }
    }
    if rule.rule.target.is_empty() {
        return Err("Target model cannot be empty".into());
    }

    if let Some(models) = app
        .models
        .provider_models
        .get(provider)
        .filter(|m| !m.is_empty())
        && !models.contains(&rule.rule.target)
    {
        return Err(format!(
            "Target '{}' is not in {provider}'s model list",
            rule.rule.target
        ));
    }
    Ok(())
}

/// Suggestion candidates for the focused field, filtered by what was typed.
fn suggestions(app: &App, edit: &GlobalRouteEdit) -> Vec<String> {
    let filter = edit.text.value.as_str();
    match edit.field {
        GlobalRouteField::Pattern => owned_matches(filter_suggestions(
            &all_models(&app.models.provider_models),
            filter,
        )),
        GlobalRouteField::Target => {
            owned_matches(filter_suggestions(&provider_model_pairs(app), filter))
        }
    }
}

/// Every routable `provider/model` pair across enabled providers.
fn provider_model_pairs(app: &App) -> Vec<String> {
    let mut pairs: Vec<String> = app
        .config
        .providers
        .iter()
        .filter(|(_, p)| p.enabled)
        .flat_map(|(name, _)| {
            app.models
                .provider_models
                .get(name)
                .into_iter()
                .flatten()
                .map(move |model| format!("{name}/{model}"))
        })
        .collect();
    pairs.sort_unstable();
    pairs.dedup();
    pairs
}

/// Owned matches, because the panel keeps strings rather than borrows.
fn owned_matches(matches: Vec<&str>) -> Vec<String> {
    matches.into_iter().map(str::to_string).collect()
}

fn field_value(rule: &GlobalRoute, field: GlobalRouteField) -> String {
    match field {
        GlobalRouteField::Pattern => rule.rule.pattern.clone(),
        GlobalRouteField::Target => format!("{}/{}", rule.provider, rule.rule.target),
    }
}

// ─── Rendering ──────────────────────────────────────────────────────────────

pub(super) fn draw_popup(f: &mut Frame, app: &App) {
    let rules = &app.config.global_routes;
    let mut tail: Vec<Line> = Vec::new();

    if let Some(edit) = &app.global_routes.editing {
        let listed = suggestions(app, edit);
        let listed: Vec<&str> = listed.iter().map(String::as_str).collect();
        tail.extend(render_suggestion_lines(
            &listed,
            &edit.suggest,
            t::PRIMARY,
            SuggestionStyle::Classic,
        ));
    }

    if let Some(err) = &app.global_routes.error {
        tail.push(Line::from(Span::styled(
            format!("✗ {err}"),
            Style::default().fg(t::ERROR),
        )));
    }
    let hint = match &app.global_routes.editing {
        Some(edit) => format!(
            "Editing {} — Enter commit · Tab next field · ↑/↓ pick · Esc: list, then row",
            edit.field.label()
        ),
        None => {
            "a/o new · i pattern · t provider/model · Space on/off · dd delete · J/K move · q close"
                .to_string()
        }
    };
    tail.push(Line::from(Span::styled(
        hint,
        Style::default().fg(t::MUTED),
    )));

    // Saved rows plus the unsaved new row (if the editor is adding one).
    let mut rows: Vec<(usize, &GlobalRoute, Option<&GlobalRouteEdit>)> = rules
        .iter()
        .enumerate()
        .map(|(i, rule)| {
            let edit = app.global_routes.editing.as_ref().filter(|e| e.index == i);
            (i, rule, edit)
        })
        .collect();
    if let Some(edit) = app
        .global_routes
        .editing
        .as_ref()
        .filter(|e| e.index >= rules.len())
    {
        rows.push((rules.len(), &edit.draft, Some(edit)));
    }

    let inner_h =
        (rows.len().max(1) + tail.len()).min(f.area().height.saturating_sub(2) as usize) as u16;
    let area = centered_fixed(90, inner_h + 2, f.area());
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(t::PRIMARY))
        .title(" Global Routes ")
        .title_style(Style::default().fg(t::PRIMARY).add_modifier(Modifier::BOLD));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();
    if rows.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no rules — press a to add one)",
            Style::default().fg(t::MUTED),
        )));
    } else {
        let visible = inner.height.saturating_sub(tail.len() as u16).max(1) as usize;
        let selected = app.global_routes.selected;
        // Keep the selected row inside the window the popup can actually draw.
        let start = selected
            .saturating_sub(visible - 1)
            .min(rows.len().saturating_sub(1));
        for (index, rule, edit) in rows.iter().skip(start).take(visible) {
            lines.push(rule_line(
                *index,
                rule,
                *index == app.global_routes.selected,
                *edit,
                provider_warning(app, &rule.provider),
            ));
        }
    }
    lines.extend(tail);

    f.render_widget(Paragraph::new(lines), inner);
}

/// Why a row would never be served, if it would not be.
fn provider_warning(app: &App, provider: &str) -> Option<&'static str> {
    match app.config.route_block(provider) {
        Some(RouteBlock::Missing) => Some("  ⚠ unknown provider"),
        Some(RouteBlock::Disabled) => Some("  ⚠ provider disabled"),
        None => None,
    }
}

fn rule_line(
    index: usize,
    rule: &GlobalRoute,
    selected: bool,
    editing: Option<&GlobalRouteEdit>,
    warning: Option<&'static str>,
) -> Line<'static> {
    // The row being edited drops the selection background so the cursor block
    // stands out on its own.
    let base = if editing.is_some() {
        Style::default().fg(t::TEXT).add_modifier(Modifier::BOLD)
    } else if selected {
        Style::default()
            .fg(t::HIGHLIGHT_FG)
            .bg(t::HIGHLIGHT_BG)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(t::TEXT)
    };
    let cursor_color = t::PRIMARY;
    let toggle = if rule.rule.enabled { '✓' } else { ' ' };

    let mut spans: Vec<Span<'static>> = vec![
        Span::styled(format!("{toggle} "), base),
        Span::styled(format!("{:>2} ", index + 1), Style::default().fg(t::MUTED)),
    ];

    // Each side renders from the field being typed into, or from the row.
    match editing.filter(|e| e.field == GlobalRouteField::Pattern) {
        Some(e) => spans.extend(cursor_split_spans(
            &e.text.value,
            e.text.cursor,
            cursor_color,
        )),
        None => spans.push(Span::styled(rule.rule.pattern.clone(), base)),
    }
    spans.push(Span::styled(" -> ", base));
    match editing.filter(|e| e.field == GlobalRouteField::Target) {
        Some(e) => spans.extend(cursor_split_spans(
            &e.text.value,
            e.text.cursor,
            cursor_color,
        )),
        None => spans.push(Span::styled(
            format!("{}/{}", rule.provider, rule.rule.target),
            base,
        )),
    }

    if !rule.rule.enabled {
        spans.push(Span::styled("  (off)", Style::default().fg(t::MUTED)));
    }
    if let Some(warning) = warning {
        spans.push(Span::styled(warning, Style::default().fg(t::WARNING)));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_support::ConfigDirGuard;
    use crate::tui::testing::tests::app_with_current;

    fn open_panel() -> (ConfigDirGuard, App) {
        let guard = ConfigDirGuard::new();
        let mut app = app_with_current("first");
        open(&mut app);
        (guard, app)
    }

    fn key(app: &mut App, code: KeyCode) {
        key_mod(app, code, KeyModifiers::NONE);
    }

    fn key_mod(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
        let mut server = None;
        handle_key(app, code, modifiers, &mut server).unwrap();
    }

    /// Fill the focused field by typing it, character by character.
    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            key(app, KeyCode::Char(c));
        }
    }

    /// Overwrite the focused field's text (used to reach invalid values).
    fn set_text(app: &mut App, text: &str) {
        let edit = app.global_routes.editing.as_mut().unwrap();
        edit.text.value = text.to_string();
        edit.text.cursor = text.len();
    }

    /// Render the panel once into a fresh 100×20 buffer.
    fn drawn_buffer(app: &App) -> ratatui::buffer::Buffer {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|f| draw_popup(f, app)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn drawn_text(app: &App) -> String {
        drawn_buffer(app)
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    fn editing_draft(app: &App) -> &GlobalRoute {
        &app.global_routes.editing.as_ref().unwrap().draft
    }

    #[test]
    fn add_rule_starts_editing_the_pattern_for_the_first_enabled_provider() {
        let (_guard, mut app) = open_panel();

        key(&mut app, KeyCode::Char('a'));

        let edit = app.global_routes.editing.as_ref().unwrap();
        assert_eq!(edit.field, GlobalRouteField::Pattern);
        assert_eq!(edit.draft.provider, "first");
        assert_eq!(edit.index, 0);
    }

    #[test]
    fn pattern_typing_syncs_into_the_draft() {
        let (_guard, mut app) = open_panel();
        key(&mut app, KeyCode::Char('a'));

        type_text(&mut app, "first-model");

        assert_eq!(editing_draft(&app).rule.pattern, "first-model");
    }

    #[test]
    fn enter_saves_a_valid_row_and_leaves_insert_mode() {
        let (_guard, mut app) = open_panel();
        key(&mut app, KeyCode::Char('a'));
        type_text(&mut app, "first-model");
        key(&mut app, KeyCode::Tab);
        // The right-hand side is prefilled `first/`; finish the model name.
        type_text(&mut app, "first-model");

        key(&mut app, KeyCode::Enter);

        assert!(app.global_routes.editing.is_none());
        assert!(app.global_routes.error.is_none());
        assert_eq!(app.config.global_routes.len(), 1);
        let saved = app.config.global_routes[0].clone();
        assert_eq!(saved.rule.pattern, "first-model");
        assert_eq!(saved.provider, "first");
        assert_eq!(saved.rule.target, "first-model");

        // The row must survive a reload: it was written to the config file.
        let reloaded = crate::config::load_config().unwrap();
        assert_eq!(reloaded.global_routes.len(), 1);
    }

    #[test]
    fn commit_rejects_an_unknown_provider() {
        let (_guard, mut app) = open_panel();
        key(&mut app, KeyCode::Char('a'));
        type_text(&mut app, "first-model");
        key(&mut app, KeyCode::Tab);
        set_text(&mut app, "nope/first-model");

        key(&mut app, KeyCode::Enter);

        assert!(app.global_routes.editing.is_some(), "stays in insert mode");
        assert!(
            app.global_routes
                .error
                .unwrap()
                .contains("Unknown provider")
        );
        assert!(app.config.global_routes.is_empty());
    }

    #[test]
    fn commit_rejects_a_target_outside_the_provider_catalog() {
        let (_guard, mut app) = open_panel();
        key(&mut app, KeyCode::Char('a'));
        type_text(&mut app, "first-model");
        key(&mut app, KeyCode::Tab);
        set_text(&mut app, "first/second-model");

        key(&mut app, KeyCode::Enter);

        assert!(app.global_routes.editing.is_some());
        assert!(app.global_routes.error.unwrap().contains("model list"));
        assert!(app.config.global_routes.is_empty());
    }

    #[test]
    fn a_pattern_no_model_carries_is_saved_as_written() {
        let (_guard, mut app) = open_panel();
        key(&mut app, KeyCode::Char('a'));
        type_text(&mut app, "client-alias");
        key(&mut app, KeyCode::Tab);
        set_text(&mut app, "first/first-model");

        key(&mut app, KeyCode::Enter);

        assert!(
            app.global_routes.editing.is_none(),
            "{:?}",
            app.global_routes.error
        );
        assert_eq!(app.config.global_routes[0].rule.pattern, "client-alias");
    }

    #[test]
    fn wildcard_patterns_are_accepted() {
        let (_guard, mut app) = open_panel();
        key(&mut app, KeyCode::Char('a'));
        type_text(&mut app, "first-*");
        key(&mut app, KeyCode::Tab);
        type_text(&mut app, "first-model");

        key(&mut app, KeyCode::Enter);

        assert_eq!(app.config.global_routes.len(), 1);
        assert_eq!(app.config.global_routes[0].rule.pattern, "first-*");
    }

    #[test]
    fn commit_trims_surrounding_whitespace_from_every_field() {
        let (_guard, mut app) = open_panel();
        key(&mut app, KeyCode::Char('a'));
        // Margin whitespace would otherwise never match a stored model name.
        type_text(&mut app, " first-model ");
        key(&mut app, KeyCode::Tab);
        set_text(&mut app, " first / first-model ");

        key(&mut app, KeyCode::Enter);

        let saved = &app.config.global_routes[0];
        assert_eq!(saved.rule.pattern, "first-model");
        assert_eq!(saved.provider, "first");
        assert_eq!(saved.rule.target, "first-model");
    }

    #[test]
    fn cancelling_a_new_row_pulls_the_selection_back_into_range() {
        let (_guard, mut app) = open_panel();
        app.config.global_routes =
            vec![GlobalRoute::for_test("first", "first-model", "first-model")];

        key(&mut app, KeyCode::Char('a'));
        assert_eq!(app.global_routes.selected, 1, "the new row past the end");
        key(&mut app, KeyCode::Esc);

        assert!(app.global_routes.editing.is_none());
        assert_eq!(app.global_routes.selected, 0);
    }

    #[test]
    fn a_buffered_j_does_not_leak_into_the_next_field() {
        let (_guard, mut app) = open_panel();
        key(&mut app, KeyCode::Char('a'));

        key(&mut app, KeyCode::Char('j')); // buffered half of a `jk` exit
        key(&mut app, KeyCode::Tab);

        let edit = app.global_routes.editing.as_ref().unwrap();
        assert_eq!(
            edit.text.value, "first/",
            "the j belongs to the pattern field"
        );
        assert_eq!(edit.draft.rule.pattern, "");
    }

    #[test]
    fn typing_only_a_model_keeps_the_provider_side() {
        let (_guard, mut app) = open_panel();
        key(&mut app, KeyCode::Char('a'));
        type_text(&mut app, "first-model");
        key(&mut app, KeyCode::Tab);
        set_text(&mut app, "first-model");

        key(&mut app, KeyCode::Enter);
        assert!(
            app.global_routes.error.is_none(),
            "{:?}",
            app.global_routes.error
        );
        assert_eq!(app.config.global_routes.len(), 1);
        assert_eq!(app.config.global_routes[0].provider, "first");
        assert_eq!(app.config.global_routes[0].rule.target, "first-model");
    }

    #[test]
    fn validate_only_guards_the_provider_of_a_new_row() {
        let (_guard, mut app) = open_panel();
        app.config.providers.get_mut("second").unwrap().enabled = false;
        let rule = GlobalRoute::for_test("second", "second-model", "second-model");

        assert!(
            validate(&app, &rule, false).is_ok(),
            "an existing row on a disabled provider must stay editable"
        );
        assert!(validate(&app, &rule, true).is_err());
    }

    #[test]
    fn rows_pointing_at_a_missing_or_disabled_provider_are_marked() {
        let (_guard, mut app) = open_panel();
        app.config.global_routes = vec![
            GlobalRoute::for_test("ghost", "m1", "m1"),
            GlobalRoute::for_test("second", "m2", "m2"),
        ];
        app.config.providers.get_mut("second").unwrap().enabled = false;

        let text = drawn_text(&app);

        assert!(text.contains("unknown provider"), "{text}");
        assert!(text.contains("provider disabled"), "{text}");
    }

    #[test]
    fn esc_discards_the_draft() {
        let (_guard, mut app) = open_panel();
        key(&mut app, KeyCode::Char('a'));
        type_text(&mut app, "first-model");

        key(&mut app, KeyCode::Esc);

        assert!(app.global_routes.editing.is_none());
        assert!(app.config.global_routes.is_empty());
    }

    #[test]
    fn space_toggles_and_dd_removes_the_selected_row() {
        let (_guard, mut app) = open_panel();
        app.config.global_routes =
            vec![GlobalRoute::for_test("first", "first-model", "first-model")];

        key(&mut app, KeyCode::Char(' '));
        assert!(!app.config.global_routes[0].rule.enabled);

        key(&mut app, KeyCode::Char('d'));
        key(&mut app, KeyCode::Char('d'));
        assert!(app.config.global_routes.is_empty());
    }

    #[test]
    fn draw_popup_shows_a_new_row_before_it_is_committed() {
        let (_guard, mut app) = open_panel();
        key(&mut app, KeyCode::Char('a'));
        type_text(&mut app, "first-mo");

        let text = drawn_text(&app);

        assert!(
            text.contains("first-mo"),
            "the unsaved row must be visible: {text}"
        );
        assert!(!text.contains("(no rules"));
    }

    #[test]
    fn draw_popup_renders_a_row_and_the_edited_field() {
        let (_guard, mut app) = open_panel();
        app.config.global_routes =
            vec![GlobalRoute::for_test("first", "first-model", "first-model")];
        key(&mut app, KeyCode::Char('i'));

        let text = drawn_text(&app);

        assert!(text.contains("Global Routes"));
        assert!(text.contains("first-model"));
        assert!(text.contains("Editing pattern"));
    }

    #[test]
    fn target_suggestions_list_provider_slash_model_pairs() {
        let (_guard, mut app) = open_panel();
        key(&mut app, KeyCode::Char('a'));
        type_text(&mut app, "first-model");
        key(&mut app, KeyCode::Tab);

        let edit = app.global_routes.editing.as_ref().unwrap();
        // Prefilled with the default provider, so only its pairs match.
        assert_eq!(suggestions(&app, edit), vec!["first/first-model"]);

        set_text(&mut app, "");
        let edit = app.global_routes.editing.as_ref().unwrap();
        assert_eq!(
            suggestions(&app, edit),
            vec!["first/first-model", "second/second-model"],
            "one flat list across providers"
        );
    }

    #[test]
    fn ctrl_j_and_ctrl_k_move_inside_the_suggestion_list() {
        let (_guard, mut app) = open_panel();
        key(&mut app, KeyCode::Char('a'));
        type_text(&mut app, "first-model");
        key(&mut app, KeyCode::Tab);
        set_text(&mut app, "");

        key_mod(&mut app, KeyCode::Char('j'), KeyModifiers::CONTROL);
        {
            let edit = app.global_routes.editing.as_ref().unwrap();
            assert!(edit.suggest.active);
            assert_eq!(edit.suggest.idx, 0);
            assert!(edit.text.value.is_empty(), "ctrl+j must not type a `j`");
        }
        key_mod(&mut app, KeyCode::Char('j'), KeyModifiers::CONTROL);
        assert_eq!(app.global_routes.editing.as_ref().unwrap().suggest.idx, 1);
        key_mod(&mut app, KeyCode::Char('k'), KeyModifiers::CONTROL);
        assert_eq!(app.global_routes.editing.as_ref().unwrap().suggest.idx, 0);
    }

    #[test]
    fn ctrl_j_in_the_row_list_moves_the_selection() {
        let (_guard, mut app) = open_panel();
        app.config.global_routes = vec![
            GlobalRoute::for_test("first", "first-model", "first-model"),
            GlobalRoute::for_test("first", "second-model", "second-model"),
        ];

        key_mod(&mut app, KeyCode::Char('j'), KeyModifiers::CONTROL);

        assert_eq!(app.global_routes.selected, 1);
    }

    #[test]
    fn picking_a_provider_model_pair_fills_both_sides() {
        let (_guard, mut app) = open_panel();
        key(&mut app, KeyCode::Char('a'));
        type_text(&mut app, "second-model");
        key(&mut app, KeyCode::Tab);
        set_text(&mut app, "");

        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Enter); // accept the highlighted pair

        let draft = editing_draft(&app);
        assert_eq!(draft.provider, "first");
        assert_eq!(draft.rule.target, "first-model");

        key(&mut app, KeyCode::Enter); // commit the row
        assert_eq!(app.config.global_routes.len(), 1);
        assert_eq!(app.config.global_routes[0].provider, "first");
        assert_eq!(app.config.global_routes[0].rule.target, "first-model");
    }

    #[test]
    fn the_row_being_edited_drops_the_selection_background() {
        let (_guard, mut app) = open_panel();
        app.config.global_routes =
            vec![GlobalRoute::for_test("first", "first-model", "first-model")];

        let selected = drawn_buffer(&app);
        assert!(
            selected.content().iter().any(|c| c.bg == t::HIGHLIGHT_BG),
            "a selected, idle row is highlighted"
        );

        key(&mut app, KeyCode::Char('i'));
        let editing = drawn_buffer(&app);
        assert!(
            !editing.content().iter().any(|c| c.bg == t::HIGHLIGHT_BG),
            "the edited row must not paint a background"
        );
    }

    #[test]
    fn pattern_suggestions_come_from_every_provider_catalog() {
        let (_guard, mut app) = open_panel();
        key(&mut app, KeyCode::Char('a'));

        let edit = app.global_routes.editing.as_ref().unwrap();
        let listed = suggestions(&app, edit);

        assert_eq!(listed, vec!["first-model", "second-model"]);
    }
}
