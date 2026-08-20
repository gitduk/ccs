use crossterm::event::KeyCode;

use crate::tui::App;
use crate::tui::state::Mode;

/// Half-page step is not worth tracking a view height for here — the dialog is
/// a fixed list, so page keys move a flat 10 lines.
const PAGE_STEP: u16 = 10;

/// Scroll keys move the Help dialog; anything else closes it.
/// `u16::MAX` is clamped against the rendered line count in `draw_help`.
pub(super) fn handle_help_key(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Char('j') | KeyCode::Down => {
            app.help_scroll = app.help_scroll.saturating_add(1);
        }
        KeyCode::Char('k') | KeyCode::Up => {
            app.help_scroll = app.help_scroll.saturating_sub(1);
        }
        KeyCode::PageDown => {
            app.help_scroll = app.help_scroll.saturating_add(PAGE_STEP);
        }
        KeyCode::PageUp => {
            app.help_scroll = app.help_scroll.saturating_sub(PAGE_STEP);
        }
        KeyCode::Char('g') | KeyCode::Home => app.help_scroll = 0,
        KeyCode::Char('G') | KeyCode::End => app.help_scroll = u16::MAX,
        _ => app.mode = Mode::Normal,
    }
}
