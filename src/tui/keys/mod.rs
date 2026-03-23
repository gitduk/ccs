mod confirm;
mod editing;
mod normal;
mod routes;

use crossterm::event::{KeyCode, KeyModifiers};

use super::state::Mode;
use super::App;
use super::ServerHandle;

pub(super) fn handle_key(
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
    server: &mut Option<ServerHandle>,
) -> crate::error::Result<()> {
    match &app.mode {
        Mode::Normal => normal::handle_normal_key(app, code, server),
        Mode::Editing => editing::handle_editing_key(app, code, modifiers, server),
        Mode::Confirm => confirm::handle_confirm_key(app, code, server),
        Mode::Help => {
            app.mode = Mode::Normal;
            Ok(())
        }
    }
}
