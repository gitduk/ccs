use crossterm::event::KeyCode;

use super::super::state::Mode;
use crate::tui::App;

pub(super) fn handle_logs_key(
    app: &mut App,
    code: KeyCode,
    total: usize,
) -> crate::error::Result<()> {
    match code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.mode = Mode::Normal;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if app.logs.selected > 0 {
                app.logs.selected -= 1;
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if total > 0 && app.logs.selected < total - 1 {
                app.logs.selected += 1;
            }
        }
        KeyCode::Char('G') => {
            if total > 0 {
                app.logs.selected = total - 1;
            }
        }
        KeyCode::Char('g') => {
            // Single 'g' goes to top (gg would need pending_key; keep it simple)
            app.logs.selected = 0;
        }
        _ => {}
    }
    Ok(())
}
