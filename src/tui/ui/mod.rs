mod dialogs;
mod form;
mod format;
mod layout;
mod route_editor;
mod stats_panel;
mod view;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};

use super::state::{App, Mode};

pub fn draw(f: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title bar
            Constraint::Min(0),    // main content
            Constraint::Length(1), // keybindings
        ])
        .split(f.area());

    view::draw_title_bar(f, app, chunks[0]);
    view::draw_main(f, app, chunks[1]);
    view::draw_keybindings(f, app, chunks[2]);

    match &app.mode {
        Mode::Editing => form::draw_form(f, app),
        Mode::Confirm => dialogs::draw_confirm(f, app),
        Mode::Help => dialogs::draw_help(f, app),
        Mode::Models => dialogs::draw_models(f, app),
        Mode::Normal => {}
    }
}
