use std::time::{Duration, Instant};

use crossterm::event::KeyCode;

use crate::tui::state::FormField;

const JK_TIMEOUT: Duration = Duration::from_millis(500);

/// Shared timeout for all two-key sequences (jk, gg, dd, yy…).
pub const PENDING_KEY_TIMEOUT: Duration = JK_TIMEOUT;

/// Consume the pending two-key buffer and return the buffered char if it is
/// still within the timeout window. Works for any two-key sequence (jk, dd, yy, gg…).
///
/// Pass `&mut form.pending_key` or `&mut app.pending_key` depending on context.
pub fn consume_pending_key(pending: &mut Option<(char, Instant)>) -> Option<char> {
    pending
        .take()
        .and_then(|(k, t)| (t.elapsed() < PENDING_KEY_TIMEOUT).then_some(k))
}

/// Result returned by [`handle_field_insert_key`] to tell the caller what happened.
pub enum InsertKeyResult {
    /// Key was handled; field content did not change (e.g. cursor movement).
    Consumed,
    /// Key was handled and field content changed → caller should trigger side-effects
    /// (save / sync / scroll-reset, etc.).
    TextChanged,
    /// "jk" sequence or Esc detected → caller should exit Insert mode.
    /// Note: when triggered by "jk", the buffered 'j' is NOT written to the field.
    ExitInsert,
    /// Key was not handled here → caller should handle it (Down/Up/Enter/Tab/Ctrl+C…).
    NotHandled,
}

/// Handle a keypress for a single-line or multi-line [`FormField`] in Insert mode.
///
/// Covers the common editing shortcuts shared across all Insert-mode contexts:
/// - `jk` two-key escape sequence → [`InsertKeyResult::ExitInsert`]
/// - `Esc` → [`InsertKeyResult::ExitInsert`]
/// - `Backspace` / `Ctrl+H` → backspace
/// - `Ctrl+W` → delete word backward
/// - `Delete` → delete forward
/// - `←` / `→` → move cursor
/// - `Home` / `Ctrl+A` → jump to start
/// - `End` / `Ctrl+E` → jump to end
/// - `Char(c)` (non-ctrl) → insert character
///
/// Caller-specific keys (Down/Up/Enter/Tab/Ctrl+J/Ctrl+K/Ctrl+C) are returned as
/// [`InsertKeyResult::NotHandled`] so each context can handle them independently.
///
/// # `pending_key`
/// Pass `&mut form.pending_key` (routes / editing) or `&mut app.models.pending_key` (models search).
/// The function reads and clears it to detect the `jk` escape sequence.
pub fn handle_field_insert_key(
    field: &mut FormField,
    code: KeyCode,
    ctrl: bool,
    pending_key: &mut Option<(char, Instant)>,
) -> InsertKeyResult {
    use InsertKeyResult::{Consumed, ExitInsert, NotHandled, TextChanged};

    // ── 1. Consume pending 'j' (first half of "jk" escape sequence) ──────────
    let prev_j = pending_key
        .take()
        .and_then(|(k, t)| (k == 'j' && t.elapsed() < JK_TIMEOUT).then_some(()));

    if prev_j.is_some() {
        if code == KeyCode::Char('k') && !ctrl {
            // "jk" complete → exit Insert; 'j' is discarded (not written to field).
            return ExitInsert;
        }
        if code == KeyCode::Esc && !ctrl {
            // Esc cancels both the pending 'j' and Insert mode; 'j' is discarded.
            return ExitInsert;
        }
        // Not "jk" or Esc — flush the buffered 'j' into the field and fall through
        // to handle the current key normally.
        field.insert('j');
    }
    let flushed_j = prev_j.is_some();

    // ── 2. Esc → exit signal ──────────────────────────────────────────────────
    if code == KeyCode::Esc && !ctrl {
        return ExitInsert;
    }

    // ── 3. Common text-editing shortcuts ──────────────────────────────────────
    let result = match code {
        KeyCode::Backspace => {
            field.backspace();
            TextChanged
        }
        KeyCode::Char('h') if ctrl => {
            field.backspace(); // Ctrl+H = BS (terminal convention)
            TextChanged
        }
        KeyCode::Char('w') if ctrl => {
            field.delete_word_back();
            TextChanged
        }
        KeyCode::Delete => {
            field.delete();
            TextChanged
        }
        KeyCode::Left => {
            field.move_left();
            Consumed
        }
        KeyCode::Right => {
            field.move_right();
            Consumed
        }
        KeyCode::Home => {
            field.home();
            Consumed
        }
        KeyCode::End => {
            field.end();
            Consumed
        }
        KeyCode::Char('a') if ctrl => {
            field.home(); // Ctrl+A = home (Readline)
            Consumed
        }
        KeyCode::Char('e') if ctrl => {
            field.end(); // Ctrl+E = end (Readline)
            Consumed
        }
        // Buffer 'j' — may be first key of "jk" escape sequence.
        KeyCode::Char('j') if !ctrl => {
            *pending_key = Some(('j', Instant::now()));
            Consumed
        }
        // Plain character input.
        KeyCode::Char(c) if !ctrl => {
            field.insert(c);
            TextChanged
        }
        // Everything else (Down/Up/Enter/Tab/Ctrl+J/Ctrl+K/Ctrl+C…) → caller handles.
        _ => NotHandled,
    };

    // ── 4. If 'j' was flushed, any result other than ExitInsert should be
    //       upgraded to TextChanged so callers trigger their side-effects for 'j'.
    //       (ExitInsert callers already handle saves themselves.)
    if flushed_j && !matches!(result, ExitInsert) {
        TextChanged
    } else {
        result
    }
}
