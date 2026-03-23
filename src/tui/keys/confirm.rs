use crossterm::event::KeyCode;

use crate::tui::state::Mode;
use crate::tui::server::sync_proxy_config;
use crate::tui::{App, ServerHandle};

pub(super) fn handle_confirm_key(
    app: &mut App,
    code: KeyCode,
    server: &Option<ServerHandle>,
) -> crate::error::Result<()> {
    match code {
        KeyCode::Char('y') | KeyCode::Enter => {
            app.confirm_action_execute()?;
            sync_proxy_config(app, server);
        }
        _ => {
            app.confirm_action = None;
            app.mode = Mode::Normal;
        }
    }
    Ok(())
}
