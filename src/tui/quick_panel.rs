//! Shared single-field quick-edit popup, opened from the provider table with
//! `o` (Port). The Port form is the only remaining kind.

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

    if code == KeyCode::Esc {
        app.quick_form = None;
        app.mode = Mode::Normal;
        return Ok(());
    }

    if code == KeyCode::Enter {
        if commit(app)? {
            sync_proxy_config(app, server);
            app.quick_form = None;
            app.mode = Mode::Normal;
        }
        return Ok(());
    }

    let mut close = false;
    {
        let form = app.quick_form.as_mut().expect("checked above");
        if let super::input::insert::InsertKeyResult::ExitInsert =
            super::input::insert::handle_field_insert_key(
                &mut form.field,
                code,
                ctrl,
                &mut form.pending_key,
            )
        {
            close = true;
        }
    }
    if close {
        app.quick_form = None;
        app.mode = Mode::Normal;
    }
    Ok(())
}

fn commit(app: &mut App) -> crate::error::Result<bool> {
    match app.quick_form.as_ref().map(|f| f.kind) {
        Some(QuickFormKind::Port) => commit_port(app),
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

pub(super) fn draw_popup(f: &mut Frame, app: &App) {
    let Some(form) = &app.quick_form else {
        return;
    };

    let title = format!(" Port — {} ", form.provider_name);

    // Content below the input line, in render order: error, hint.
    let mut tail: Vec<Line> = Vec::new();
    if let Some(err) = &form.error {
        tail.push(Line::from(Span::styled(
            format!("✗ {err}"),
            Style::default().fg(t::ERROR),
        )));
    }
    tail.push(Line::from(vec![
        Span::styled("Enter", Style::default().fg(t::PRIMARY)),
        Span::styled(" Save  ", Style::default().fg(t::MUTED)),
        Span::styled("Esc", Style::default().fg(t::WARNING)),
        Span::styled(" Cancel", Style::default().fg(t::MUTED)),
    ]));

    // Content fills the dialog top-to-bottom: input line, then the hint row.
    // Only the screen height clips the tail.
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
}
