//! Shared list-navigation keymap and index stepping.
//!
//! Every list panel (providers, models, request log, global routes) moves its
//! selection on the same keys. This module is the single definition of that
//! keymap and of the clamp, so the panels cannot drift apart.

use crossterm::event::KeyCode;

/// Direction a key moves a list selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListMove {
    Up,
    Down,
}

impl ListMove {
    /// Signed one-row step, for [`step`].
    pub fn delta(self) -> isize {
        match self {
            Self::Up => -1,
            Self::Down => 1,
        }
    }
}

/// The direction `code` moves a list selection, if it moves one at all.
///
/// `typing` marks a panel whose plain characters go into a text field (the
/// models search box): there only the arrows and `Ctrl+J`/`Ctrl+K` navigate,
/// because a bare `j`/`k` is text.
pub fn list_move(code: KeyCode, ctrl: bool, typing: bool) -> Option<ListMove> {
    match code {
        KeyCode::Down => Some(ListMove::Down),
        KeyCode::Up => Some(ListMove::Up),
        KeyCode::Char('j') if ctrl || !typing => Some(ListMove::Down),
        KeyCode::Char('k') if ctrl || !typing => Some(ListMove::Up),
        _ => None,
    }
}

/// The direction `code` moves a suggestion highlight: the same keymap as
/// [`list_move`] while text is being typed.
pub fn suggest_move(code: KeyCode, ctrl: bool) -> Option<ListMove> {
    list_move(code, ctrl, true)
}

/// Move a selection index by `delta` rows, clamped into `0..len`.
/// An empty list has no rows, so it yields 0.
pub fn step(selected: usize, delta: isize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    (selected as isize + delta).clamp(0, len as isize - 1) as usize
}

/// Scroll `scroll` so a selection that moved or jumped stays inside a
/// `viewport`-row window starting there. Shared by the panels that scroll a
/// `u16` offset (models, request log, attribute columns).
pub fn keep_visible(selected: usize, scroll: &mut u16, viewport: usize) {
    let bottom = *scroll as usize + viewport;
    if selected >= bottom {
        *scroll = (selected + 1).saturating_sub(viewport) as u16;
    } else if selected < *scroll as usize {
        *scroll = selected as u16;
    }
}

/// Move a selection by `rows` in `dir`, clamped to `len`, and keep it visible.
pub fn nav(
    selected: &mut usize,
    scroll: &mut u16,
    dir: ListMove,
    rows: isize,
    len: usize,
    viewport: usize,
) {
    *selected = step(*selected, dir.delta() * rows, len);
    keep_visible(*selected, scroll, viewport);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_clamps_to_both_ends() {
        assert_eq!(step(0, -1, 3), 0);
        assert_eq!(step(2, 1, 3), 2);
        assert_eq!(step(1, 1, 3), 2);
        assert_eq!(step(1, -1, 3), 0);
        assert_eq!(step(1, 5, 3), 2);
    }

    #[test]
    fn keep_visible_scrolls_only_when_the_highlight_leaves_the_window() {
        let mut scroll = 0u16;
        keep_visible(7, &mut scroll, 8);
        assert_eq!(scroll, 0, "still inside the window");

        keep_visible(8, &mut scroll, 8);
        assert_eq!(scroll, 1, "one past the window bottom scrolls by one");

        let mut scroll = 5u16;
        keep_visible(4, &mut scroll, 8);
        assert_eq!(scroll, 4, "above the window top pulls the top down");

        let mut scroll = 0u16;
        keep_visible(99, &mut scroll, 8);
        assert_eq!(scroll, 92);
    }

    #[test]
    fn step_of_an_empty_list_is_zero() {
        assert_eq!(step(4, 1, 0), 0);
    }

    #[test]
    fn arrows_and_jk_navigate_when_no_text_is_being_typed() {
        let none = |code, ctrl| list_move(code, ctrl, false);
        assert_eq!(none(KeyCode::Down, false), Some(ListMove::Down));
        assert_eq!(none(KeyCode::Up, false), Some(ListMove::Up));
        assert_eq!(none(KeyCode::Char('j'), false), Some(ListMove::Down));
        assert_eq!(none(KeyCode::Char('k'), false), Some(ListMove::Up));
        assert_eq!(
            none(KeyCode::Char('j'), true),
            Some(ListMove::Down),
            "Ctrl+J is the terminal's own Down"
        );
        assert_eq!(none(KeyCode::Char('x'), false), None);
    }

    #[test]
    fn only_ctrl_jk_and_arrows_navigate_while_typing() {
        let typing = |code, ctrl| list_move(code, ctrl, true);
        assert_eq!(typing(KeyCode::Char('j'), false), None);
        assert_eq!(typing(KeyCode::Char('k'), false), None);
        assert_eq!(typing(KeyCode::Char('j'), true), Some(ListMove::Down));
        assert_eq!(typing(KeyCode::Char('k'), true), Some(ListMove::Up));
        assert_eq!(typing(KeyCode::Down, false), Some(ListMove::Down));
    }

    #[test]
    fn suggestions_take_arrows_and_ctrl_jk_only() {
        assert_eq!(suggest_move(KeyCode::Down, false), Some(ListMove::Down));
        assert_eq!(suggest_move(KeyCode::Up, false), Some(ListMove::Up));
        assert_eq!(suggest_move(KeyCode::Char('j'), true), Some(ListMove::Down));
        assert_eq!(suggest_move(KeyCode::Char('k'), true), Some(ListMove::Up));
        assert_eq!(
            suggest_move(KeyCode::Char('j'), false),
            None,
            "a bare j types into the field"
        );
    }
}
