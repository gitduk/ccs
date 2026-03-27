mod dialogs;
mod form;
mod format;
mod layout;
mod main_view;
mod route_editor;
mod stats_panel;

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

    main_view::draw_title_bar(f, app, chunks[0]);
    main_view::draw_main(f, app, chunks[1]);
    main_view::draw_keybindings(f, app, chunks[2]);

    match &app.mode {
        Mode::Editing => form::draw_form(f, app),
        Mode::Confirm => dialogs::draw_confirm(f, app),
        Mode::Help => dialogs::draw_help(f, app),
        Mode::Normal => {}
    }
}
