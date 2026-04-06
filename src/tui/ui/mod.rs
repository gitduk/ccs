mod dialogs;
mod form;
mod format;
mod layout;
mod logs;
mod route_editor;
mod stats_panel;
mod view;

use ratatui::Frame;

use super::state::{App, Mode};

pub fn draw(f: &mut Frame, app: &mut App) {
    view::draw_main(f, app, f.area());

    match &app.mode {
        Mode::Editing => form::draw_form(f, app),
        Mode::Confirm => dialogs::draw_confirm(f, app),
        Mode::Help => dialogs::draw_help(f, app),
        Mode::Models => dialogs::draw_models(f, app),
        Mode::Logs => logs::draw_logs(f, app),
        Mode::Normal => {}
    }
}
