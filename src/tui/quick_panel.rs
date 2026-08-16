//! Shared single-field quick-edit popup, opened from the provider table with
//! `o` (Port) or `T` (Test Model). Both panels share one form shape and only
//! differ in prefill source, commit behavior, and the dialog title.

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::Frame;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph};

use crate::config;

use super::App;
use super::ServerHandle;
use super::server::sync_proxy_config;
use super::state::{Mode, QuickForm, QuickFormKind, provider_model_suggestions};
use super::theme::{self as t};
use super::ui::format::{SuggestionStyle, cursor_split_spans, render_suggestion_lines};
use super::ui::layout::centered_fixed;

pub(super) fn open(app: &mut App, kind: QuickFormKind) {
    let Some(name) = app.selected_name().map(|s| s.to_string()) else {
        return;
    };
    let current: Option<String> = match kind {
        QuickFormKind::Port => app
            .config
            .providers
            .get(&name)
            .and_then(|p| p.port)
            .map(|p| p.to_string()),
        QuickFormKind::TestModel => app
            .config
            .providers
            .get(&name)
            .and_then(|p| p.test_model.clone()),
    };
    app.quick_form = Some(QuickForm::new(kind, &name, current.as_deref()));
    app.mode = Mode::QuickInput;
}

pub(super) fn handle_paste(app: &mut App, text: &str) -> crate::error::Result<()> {
    if let Some(form) = app.quick_form.as_mut() {
        form.field.insert_str(text);
    }
    Ok(())
}

pub(super) fn handle_key(
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
    server: &mut Option<ServerHandle>,
) -> crate::error::Result<()> {
    if app.quick_form.is_none() {
        app.mode = Mode::Normal;
        return Ok(());
    }

    let ctrl = modifiers.contains(KeyModifiers::CONTROL);

    // Two-level Esc: a highlighted suggestion is cleared first; a second Esc
    // closes the form — mirrors the Routes target field.
    if code == KeyCode::Esc {
        if app
            .quick_form
            .as_ref()
            .is_some_and(|f| f.kind == QuickFormKind::TestModel && f.suggest.active)
        {
            app.quick_form.as_mut().expect("checked above").suggest.reset();
        } else {
            app.quick_form = None;
            app.mode = Mode::Normal;
        }
        return Ok(());
    }

    // Enter: pick the highlighted suggestion (Test Model) and stay open so the
    // value can be edited before a second Enter commits.
    if code == KeyCode::Enter {
        let pick: Option<(String, String, usize)> = {
            let form = app.quick_form.as_ref().expect("checked above");
            (form.kind == QuickFormKind::TestModel && form.suggest.active).then(|| {
                (
                    form.provider_name.clone(),
                    form.field.value.clone(),
                    form.suggest.idx,
                )
            })
        };
        if let Some((name, filter, idx)) = pick {
            let picked = test_model_suggestions(app, &name, &filter)
                .get(idx)
                .map(|s| s.to_string());
            match picked {
                Some(model) => {
                    let form = app.quick_form.as_mut().expect("checked above");
                    form.field.value = model;
                    form.field.cursor = form.field.value.len();
                    form.suggest.reset();
                    return Ok(());
                }
                // No matching suggestion (e.g. an empty list after Down) — fall
                // through to the normal commit path instead of swallowing Enter.
                None => {
                    app.quick_form
                        .as_mut()
                        .expect("checked above")
                        .suggest
                        .reset();
                }
            }
        }
        if commit(app)? {
            sync_proxy_config(app, server);
            app.quick_form = None;
            app.mode = Mode::Normal;
        }
        return Ok(());
    }

    // Suggestion navigation (Test Model only): Down/Up or Ctrl+J/Ctrl+K.
    let is_nav = matches!(code, KeyCode::Down | KeyCode::Up)
        || (ctrl && matches!(code, KeyCode::Char('j') | KeyCode::Char('k')));
    if is_nav {
        let data: Option<(String, String, bool)> = {
            let form = app.quick_form.as_ref().expect("checked above");
            (form.kind == QuickFormKind::TestModel).then(|| {
                (
                    form.provider_name.clone(),
                    form.field.value.clone(),
                    matches!(code, KeyCode::Up | KeyCode::Char('k')),
                )
            })
        };
        if let Some((name, filter, up)) = data {
            let len = test_model_suggestions(app, &name, &filter).len();
            let form = app.quick_form.as_mut().expect("checked above");
            if up {
                form.suggest.retreat();
            } else {
                form.suggest.advance(len);
            }
            return Ok(());
        }
        // Port form: fall through to the insert handler (no-op there).
    }

    let (mut close, mut reset_suggest) = (false, false);
    {
        let form = app.quick_form.as_mut().expect("checked above");
        match super::input::insert::handle_field_insert_key(
            &mut form.field,
            code,
            ctrl,
            &mut form.pending_key,
        ) {
            super::input::insert::InsertKeyResult::ExitInsert => close = true,
            super::input::insert::InsertKeyResult::TextChanged
                if form.kind == QuickFormKind::TestModel =>
            {
                reset_suggest = true
            }
            _ => {}
        }
    }
    if reset_suggest {
        app.quick_form
            .as_mut()
            .expect("checked above")
            .suggest
            .reset();
    }
    if close {
        app.quick_form = None;
        app.mode = Mode::Normal;
    }
    Ok(())
}

/// Models matching the Test Model field's filter, for the current provider.
fn test_model_suggestions<'a>(app: &'a App, provider_name: &str, filter: &str) -> Vec<&'a str> {
    provider_model_suggestions(&app.models.provider_models, provider_name, filter)
}

fn commit(app: &mut App) -> crate::error::Result<bool> {
    match app.quick_form.as_ref().map(|f| f.kind) {
        Some(QuickFormKind::Port) => commit_port(app),
        Some(QuickFormKind::TestModel) => commit_test_model(app),
        None => Ok(false),
    }
}

/// `Ok(true)` closes the form; `Ok(false)` leaves it open with `form.error` set.
fn commit_port(app: &mut App) -> crate::error::Result<bool> {
    let Some(form) = app.quick_form.as_ref() else {
        return Ok(false);
    };
    let name = form.provider_name.clone();
    let raw = form.field.value.trim().to_string();

    let port: Option<u16> = if raw.is_empty() {
        None
    } else {
        match raw.parse() {
            Ok(p) => Some(p),
            Err(_) => {
                if let Some(f) = app.quick_form.as_mut() {
                    f.error = Some(format!("Port must be a number (1–65535), got '{raw}'"));
                }
                return Ok(false);
            }
        }
    };

    // Validate and persist a temp copy so a bad port or a failed save never
    // touches live state; apply to `app.config` only after the file is written.
    let mut next = app.config.clone();
    if let Some(p) = next.providers.get_mut(&name) {
        p.port = port;
    }
    if let Err(e) = next.validate_ports() {
        if let Some(f) = app.quick_form.as_mut() {
            f.error = Some(e.to_string());
        }
        return Ok(false);
    }
    if let Err(e) = config::save_config(&next) {
        if let Some(f) = app.quick_form.as_mut() {
            f.error = Some(e.to_string());
        }
        return Ok(false);
    }
    app.config = next;
    Ok(true)
}

fn commit_test_model(app: &mut App) -> crate::error::Result<bool> {
    let Some(form) = app.quick_form.as_ref() else {
        return Ok(false);
    };
    let name = form.provider_name.clone();
    let raw = form.field.value.trim().to_string();
    let test_model = (!raw.is_empty()).then_some(raw);

    let mut next = app.config.clone();
    if let Some(p) = next.providers.get_mut(&name) {
        p.test_model = test_model;
    }
    if let Err(e) = config::save_config(&next) {
        if let Some(f) = app.quick_form.as_mut() {
            f.error = Some(e.to_string());
        }
        return Ok(false);
    }
    app.config = next;
    Ok(true)
}

pub(super) fn draw_popup(f: &mut Frame, app: &App) {
    let Some(form) = &app.quick_form else {
        return;
    };

    let title = match form.kind {
        QuickFormKind::Port => format!(" Port — {} ", form.provider_name),
        QuickFormKind::TestModel => format!(" Test Model — {} ", form.provider_name),
    };

    let mut suggestions: Vec<&str> = match form.kind {
        QuickFormKind::TestModel => {
            test_model_suggestions(app, &form.provider_name, &form.field.value)
        }
        QuickFormKind::Port => vec![],
    };
    // A single suggestion that exactly duplicates the input is noise — hide it.
    if suggestions.len() == 1 && suggestions[0].eq_ignore_ascii_case(&form.field.value) {
        suggestions.clear();
    }

    // Content below the input line, in render order: error, suggestions, hint.
    let mut tail: Vec<Line> = Vec::new();
    if let Some(err) = &form.error {
        tail.push(Line::from(Span::styled(
            format!("✗ {err}"),
            Style::default().fg(t::ERROR),
        )));
    }
    if !suggestions.is_empty() {
        tail.extend(render_suggestion_lines(
            &suggestions,
            &form.suggest,
            t::PRIMARY,
            SuggestionStyle::Compact,
        ));
    }
    tail.push(Line::from(vec![
        Span::styled("Enter", Style::default().fg(t::PRIMARY)),
        Span::styled(" Save  ", Style::default().fg(t::MUTED)),
        Span::styled("Esc", Style::default().fg(t::WARNING)),
        Span::styled(" Cancel", Style::default().fg(t::MUTED)),
    ]));

    // Content fills the dialog top-to-bottom: input line, then suggestions
    // and the hint row. Only the screen height clips the tail.
    let t = tail.len();
    let inner_h = (1 + t).min(f.area().height.saturating_sub(2) as usize);
    let height = (inner_h + 2) as u16;

    let area = centered_fixed(44, height, f.area());
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(t::PRIMARY))
        .title(title.as_str())
        .title_style(Style::default().fg(t::PRIMARY).add_modifier(Modifier::BOLD))
        .padding(Padding::new(1, 1, 0, 0));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines = vec![Line::from(cursor_split_spans(
        &form.field.value,
        form.field.cursor,
        t::PRIMARY,
    ))];
    lines.extend(tail);

    f.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_support::ConfigDirGuard;
    use crate::tui::testing::tests::app_with_current;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn open_prefills_field_with_existing_port() {
        let _guard = ConfigDirGuard::new();
        let mut app = app_with_current("first");
        app.config.providers.get_mut("first").unwrap().port = Some(8080);

        open(&mut app, QuickFormKind::Port);

        assert_eq!(app.mode, Mode::QuickInput);
        assert_eq!(app.quick_form.unwrap().field.value, "8080");
    }

    #[test]
    fn commit_rejects_non_numeric_input() {
        let _guard = ConfigDirGuard::new();
        let mut app = app_with_current("first");
        open(&mut app, QuickFormKind::Port);
        app.quick_form.as_mut().unwrap().field.value = "not-a-port".to_string();

        let mut server = None;
        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE, &mut server).unwrap();

        assert_eq!(app.mode, Mode::QuickInput, "form stays open on error");
        assert!(app.quick_form.unwrap().error.is_some());
        assert_eq!(app.config.providers["first"].port, None);
    }

    #[test]
    fn commit_rejects_port_already_claimed_by_another_provider() {
        let _guard = ConfigDirGuard::new();
        let mut app = app_with_current("first");
        app.config.providers.get_mut("second").unwrap().port = Some(9000);
        open(&mut app, QuickFormKind::Port);
        app.quick_form.as_mut().unwrap().field.value = "9000".to_string();

        let mut server = None;
        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE, &mut server).unwrap();

        assert_eq!(app.mode, Mode::QuickInput, "form stays open on collision");
        assert!(app.quick_form.unwrap().error.is_some());
        assert_eq!(app.config.providers["first"].port, None);
    }

    #[test]
    fn commit_saves_a_valid_port_and_closes() {
        let _guard = ConfigDirGuard::new();
        let mut app = app_with_current("first");
        open(&mut app, QuickFormKind::Port);
        app.quick_form.as_mut().unwrap().field.value = "9001".to_string();

        let mut server = None;
        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE, &mut server).unwrap();

        assert_eq!(app.mode, Mode::Normal);
        assert!(app.quick_form.is_none());
        assert_eq!(app.config.providers["first"].port, Some(9001));
    }

    #[test]
    fn commit_with_empty_input_clears_an_existing_port() {
        let _guard = ConfigDirGuard::new();
        let mut app = app_with_current("first");
        app.config.providers.get_mut("first").unwrap().port = Some(8080);
        open(&mut app, QuickFormKind::Port);
        app.quick_form.as_mut().unwrap().field.value.clear();
        app.quick_form.as_mut().unwrap().field.cursor = 0;

        let mut server = None;
        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE, &mut server).unwrap();

        assert_eq!(app.config.providers["first"].port, None);
    }

    #[test]
    fn esc_cancels_without_saving() {
        let _guard = ConfigDirGuard::new();
        let mut app = app_with_current("first");
        open(&mut app, QuickFormKind::Port);
        app.quick_form.as_mut().unwrap().field.value = "9002".to_string();

        let mut server = None;
        handle_key(&mut app, KeyCode::Esc, KeyModifiers::NONE, &mut server).unwrap();

        assert_eq!(app.mode, Mode::Normal);
        assert!(app.quick_form.is_none());
        assert_eq!(app.config.providers["first"].port, None);
    }

    #[test]
    fn open_prefills_field_with_existing_test_model() {
        let _guard = ConfigDirGuard::new();
        let mut app = app_with_current("first");
        app.config.providers.get_mut("first").unwrap().test_model =
            Some("gemma-4-31b-it".to_string());

        open(&mut app, QuickFormKind::TestModel);

        assert_eq!(app.mode, Mode::QuickInput);
        assert_eq!(
            app.quick_form.unwrap().field.value,
            "gemma-4-31b-it"
        );
    }

    #[test]
    fn commit_saves_a_valid_test_model_and_closes() {
        let _guard = ConfigDirGuard::new();
        let mut app = app_with_current("first");
        open(&mut app, QuickFormKind::TestModel);
        app.quick_form.as_mut().unwrap().field.value = "sonnet-4-6".to_string();

        let mut server = None;
        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE, &mut server).unwrap();

        assert_eq!(app.mode, Mode::Normal);
        assert!(app.quick_form.is_none());
        assert_eq!(
            app.config.providers["first"].test_model.as_deref(),
            Some("sonnet-4-6")
        );
    }

    #[test]
    fn commit_with_empty_input_clears_an_existing_test_model() {
        let _guard = ConfigDirGuard::new();
        let mut app = app_with_current("first");
        app.config.providers.get_mut("first").unwrap().test_model =
            Some("gemma-4-31b-it".to_string());
        open(&mut app, QuickFormKind::TestModel);
        app.quick_form.as_mut().unwrap().field.value.clear();
        app.quick_form.as_mut().unwrap().field.cursor = 0;

        let mut server = None;
        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE, &mut server).unwrap();

        assert_eq!(app.config.providers["first"].test_model, None);
    }

    #[test]
    fn esc_cancels_without_saving_test_model() {
        let _guard = ConfigDirGuard::new();
        let mut app = app_with_current("first");
        open(&mut app, QuickFormKind::TestModel);
        app.quick_form.as_mut().unwrap().field.value = "sonnet-4-6".to_string();

        let mut server = None;
        handle_key(&mut app, KeyCode::Esc, KeyModifiers::NONE, &mut server).unwrap();

        assert_eq!(app.mode, Mode::Normal);
        assert!(app.quick_form.is_none());
        assert_eq!(app.config.providers["first"].test_model, None);
    }

    #[test]
    fn test_model_down_navigates_and_enter_applies_selection() {
        let _guard = ConfigDirGuard::new();
        let mut app = app_with_current("first");
        app.models.provider_models.insert(
            "first".to_string(),
            vec!["alpha-1".to_string(), "beta-2".to_string()],
        );
        open(&mut app, QuickFormKind::TestModel);

        let mut server = None;
        handle_key(&mut app, KeyCode::Down, KeyModifiers::NONE, &mut server).unwrap();
        handle_key(&mut app, KeyCode::Down, KeyModifiers::NONE, &mut server).unwrap();
        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE, &mut server).unwrap();

        assert_eq!(
            app.mode,
            Mode::QuickInput,
            "picking a suggestion keeps the form open"
        );
        assert_eq!(app.quick_form.as_ref().unwrap().field.value, "beta-2");
    }

    #[test]
    fn enter_with_empty_suggestions_still_commits() {
        let _guard = ConfigDirGuard::new();
        let mut app = app_with_current("first");
        // No cached model list → suggestions are always empty.
        open(&mut app, QuickFormKind::TestModel);
        app.quick_form.as_mut().unwrap().field.value = "my-model".to_string();
        app.quick_form.as_mut().unwrap().field.cursor = 8;

        let mut server = None;
        // A real Down press activates the (empty) suggestion list.
        handle_key(&mut app, KeyCode::Down, KeyModifiers::NONE, &mut server).unwrap();
        assert!(app.quick_form.as_ref().unwrap().suggest.active);

        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE, &mut server).unwrap();

        // Enter must not be swallowed by the empty suggestion list.
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.quick_form.is_none());
        assert_eq!(
            app.config.providers["first"].test_model.as_deref(),
            Some("my-model")
        );
    }

    #[test]
    fn test_model_typing_resets_suggest_highlight() {
        let _guard = ConfigDirGuard::new();
        let mut app = app_with_current("first");
        app.models.provider_models.insert(
            "first".to_string(),
            vec!["alpha-1".to_string(), "beta-2".to_string()],
        );
        open(&mut app, QuickFormKind::TestModel);

        let mut server = None;
        handle_key(&mut app, KeyCode::Down, KeyModifiers::NONE, &mut server).unwrap();
        assert!(app.quick_form.as_ref().unwrap().suggest.active);

        handle_key(&mut app, KeyCode::Char('x'), KeyModifiers::NONE, &mut server).unwrap();
        assert!(!app.quick_form.as_ref().unwrap().suggest.active);
    }

    #[test]
    fn test_model_esc_clears_highlight_before_closing() {
        let _guard = ConfigDirGuard::new();
        let mut app = app_with_current("first");
        app.models.provider_models.insert(
            "first".to_string(),
            vec!["alpha-1".to_string(), "beta-2".to_string()],
        );
        open(&mut app, QuickFormKind::TestModel);

        let mut server = None;
        handle_key(&mut app, KeyCode::Down, KeyModifiers::NONE, &mut server).unwrap();
        assert!(app.quick_form.as_ref().unwrap().suggest.active);

        handle_key(&mut app, KeyCode::Esc, KeyModifiers::NONE, &mut server).unwrap();
        assert_eq!(app.mode, Mode::QuickInput);
        assert!(!app.quick_form.as_ref().unwrap().suggest.active);

        handle_key(&mut app, KeyCode::Esc, KeyModifiers::NONE, &mut server).unwrap();
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.quick_form.is_none());
    }

    #[test]
    fn test_model_hides_single_duplicate_suggestion() {
        let _guard = ConfigDirGuard::new();
        let mut app = app_with_current("first");
        app.models.provider_models.insert(
            "first".to_string(),
            vec!["deepseek-v4-flash".to_string()],
        );
        open(&mut app, QuickFormKind::TestModel);
        app.quick_form.as_mut().unwrap().field.value = "deepseek-v4-flash".to_string();
        app.quick_form.as_mut().unwrap().field.cursor = 17;

        let backend = TestBackend::new(60, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| draw_popup(f, &app))
            .unwrap();

        let buffer = terminal.backend().buffer();
        let text: String = buffer.content().iter().map(|c| c.symbol()).collect();
        // Input matches the only suggestion exactly: the list is hidden, so the
        // model name appears once (input line) instead of twice.
        assert_eq!(text.matches("deepseek-v4-flash").count(), 1);
    }

    #[test]
    fn test_model_shows_suggestion_when_it_differs_from_input() {
        let _guard = ConfigDirGuard::new();
        let mut app = app_with_current("first");
        app.models.provider_models.insert(
            "first".to_string(),
            vec!["deepseek-v4-flash".to_string()],
        );
        open(&mut app, QuickFormKind::TestModel);
        app.quick_form.as_mut().unwrap().field.value = "deepseek".to_string();
        app.quick_form.as_mut().unwrap().field.cursor = 7;

        let backend = TestBackend::new(60, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| draw_popup(f, &app))
            .unwrap();

        let buffer = terminal.backend().buffer();
        let text: String = buffer.content().iter().map(|c| c.symbol()).collect();
        // Prefix input still differs from the suggestion — the list stays:
        // "deepseek" appears once in the input line and once as the
        // suggestion's prefix.
        assert_eq!(text.matches("deepseek").count(), 2);
    }

    #[test]
    fn test_model_dialog_has_no_vertical_padding() {
        let _guard = ConfigDirGuard::new();
        let mut app = app_with_current("first");
        app.models.provider_models.insert(
            "first".to_string(),
            vec!["deepseek-v4-flash".to_string(), "deepseek-v4-pro".to_string()],
        );
        open(&mut app, QuickFormKind::TestModel);
        app.quick_form.as_mut().unwrap().field.value = "x".to_string();
        app.quick_form.as_mut().unwrap().field.cursor = 1;

        let backend = TestBackend::new(60, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| draw_popup(f, &app))
            .unwrap();

        let buffer = terminal.backend().buffer();
        let content = buffer.content();
        let width = 60usize;
        let y_of = |ch: &str| {
            content
                .iter()
                .enumerate()
                .find(|(_, c)| c.symbol() == ch)
                .map(|(i, _)| i / width)
                .unwrap()
        };
        let y_top = y_of("┌");
        let y_bot = y_of("└");
        // "x" appears only in the input line; "E" only at the hint row
        // ("Enter Save Esc Cancel").
        let y_input = y_of("x");
        let y_hint = y_of("E");

        // No padding: the input line is the first inner row and the hint row
        // is the last inner row.
        assert_eq!(y_input - y_top, 1, "input line must hug the top border");
        assert_eq!(y_bot - y_hint, 1, "hint row must hug the bottom border");
    }
}
