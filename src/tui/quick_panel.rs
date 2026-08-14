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
use super::state::{Mode, QuickForm, QuickFormKind};
use super::theme::{self as t};
use super::ui::format::cursor_split_spans;
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

    match code {
        KeyCode::Esc => {
            app.quick_form = None;
            app.mode = Mode::Normal;
            return Ok(());
        }
        KeyCode::Enter => {
            if commit(app)? {
                sync_proxy_config(app, server);
                app.quick_form = None;
                app.mode = Mode::Normal;
            }
            return Ok(());
        }
        _ => {}
    }

    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    let form = app.quick_form.as_mut().expect("checked above");
    if let super::input::insert::InsertKeyResult::ExitInsert =
        super::input::insert::handle_field_insert_key(
            &mut form.field,
            code,
            ctrl,
            &mut form.pending_key,
        )
    {
        app.quick_form = None;
        app.mode = Mode::Normal;
    }
    Ok(())
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
    let height = if form.error.is_some() { 6 } else { 5 };
    let area = centered_fixed(36, height, f.area());
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(t::PRIMARY))
        .title(title.as_str())
        .title_style(Style::default().fg(t::PRIMARY).add_modifier(Modifier::BOLD))
        .padding(Padding::new(1, 1, 0, 0));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines = vec![
        Line::from(cursor_split_spans(
            &form.field.value,
            form.field.cursor,
            t::PRIMARY,
        )),
        Line::from(""),
    ];

    if let Some(err) = &form.error {
        lines.push(Line::from(Span::styled(
            format!("✗ {err}"),
            Style::default().fg(t::ERROR),
        )));
    }

    lines.push(Line::from(vec![
        Span::styled("Enter", Style::default().fg(t::PRIMARY)),
        Span::styled(" Save  ", Style::default().fg(t::MUTED)),
        Span::styled("Esc", Style::default().fg(t::WARNING)),
        Span::styled(" Cancel", Style::default().fg(t::MUTED)),
    ]));

    f.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_support::ConfigDirGuard;
    use crate::tui::testing::tests::app_with_current;

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
}
