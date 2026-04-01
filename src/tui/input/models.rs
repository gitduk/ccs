use std::time::Instant;

use crossterm::event::{KeyCode, KeyModifiers};

use super::super::state::{App, MessageKind, Mode};
use super::insert::{InsertKeyResult, consume_pending_key, handle_field_insert_key};

// ── Scroll helpers ────────────────────────────────────────────────────────────

/// Move the highlighted model down by `step`, clamping to `total - 1`.
/// Adjusts `models_scroll` using `line_offsets` so the highlighted row stays visible.
fn nav_down(app: &mut App, total: usize, step: usize, line_offsets: &[usize]) {
    if total == 0 {
        return;
    }
    app.models_selected = (app.models_selected + step).min(total - 1);
    if let Some(&row) = line_offsets.get(app.models_selected) {
        let bottom = app.models_scroll as usize + 8;
        if row >= bottom {
            app.models_scroll = (row + 1).saturating_sub(8) as u16;
        }
    }
}

/// Move the highlighted model up by `step`.
fn nav_up(app: &mut App, step: usize, line_offsets: &[usize]) {
    app.models_selected = app.models_selected.saturating_sub(step);
    if let Some(&row) = line_offsets.get(app.models_selected)
        && row < app.models_scroll as usize
    {
        app.models_scroll = row as u16;
    }
}

// ── Copy helper ───────────────────────────────────────────────────────────────

/// Copy the currently highlighted model name to the clipboard and show a status message.
fn copy_selected(app: &mut App, flat: &[&str]) {
    if let Some(&name) = flat.get(app.models_selected) {
        if super::copy_to_clipboard(name) {
            app.set_message(format!("Copied: {name}"), MessageKind::Success);
        } else {
            app.set_message("Copy failed (wl-copy not found?)", MessageKind::Error);
        }
    }
}

// ── Public entry point ────────────────────────────────────────────────────────

pub(super) fn handle_models_key(
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
    // Flat ordered list of matching model names; built by the caller from the
    // same filter/sort logic used by the renderer so indices stay in sync.
    flat: &[&str],
    // line_offsets[i] = the rendered row index of flat[i] in the list area.
    // Used to keep models_scroll in sync with models_selected.
    line_offsets: &[usize],
) -> crate::error::Result<()> {
    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    let total = flat.len();

    if app.models_insert {
        // ── Insert mode: search box focused ──────────────────────────────────

        // Models-specific keys intercepted before the common handler.
        if ctrl && code == KeyCode::Char('c') {
            app.mode = Mode::Normal;
            return Ok(());
        }
        match code {
            KeyCode::Down => {
                nav_down(app, total, 1, line_offsets);
                return Ok(());
            }
            KeyCode::Up => {
                nav_up(app, 1, line_offsets);
                return Ok(());
            }
            KeyCode::Char('j') if ctrl => {
                nav_down(app, total, 1, line_offsets);
                return Ok(());
            }
            KeyCode::Char('k') if ctrl => {
                nav_up(app, 1, line_offsets);
                return Ok(());
            }
            _ => {}
        }

        // Common Insert-mode editing (Backspace/Ctrl+W/Home/End/jk/Esc/…).
        match handle_field_insert_key(
            &mut app.models_search_field,
            code,
            ctrl,
            &mut app.pending_key,
        ) {
            InsertKeyResult::ExitInsert => {
                app.models_insert = false;
            }
            InsertKeyResult::TextChanged => {
                app.models_selected = 0;
                app.models_scroll = 0;
            }
            InsertKeyResult::Consumed | InsertKeyResult::NotHandled => {}
        }
    } else {
        // ── Normal mode: list navigation ──────────────────────────────────────

        // Consume pending two-key sequence (500 ms timeout).
        let prev = consume_pending_key(&mut app.pending_key);

        if let Some(pk) = prev {
            match (pk, &code) {
                ('y', KeyCode::Char('y')) => {
                    copy_selected(app, flat);
                    return Ok(());
                }
                ('g', KeyCode::Char('g')) => {
                    app.models_selected = 0;
                    app.models_scroll = 0;
                    return Ok(());
                }
                _ => {} // unrecognised combo — prev discarded, current key handled below
            }
        }

        match code {
            KeyCode::Esc | KeyCode::Char('q') => {
                app.mode = Mode::Normal;
            }
            KeyCode::Char('c') if ctrl => {
                app.mode = Mode::Normal;
            }
            KeyCode::Char('i') => {
                app.models_insert = true;
            }
            KeyCode::Down | KeyCode::Char('j') => nav_down(app, total, 1, line_offsets),
            KeyCode::Up | KeyCode::Char('k') => nav_up(app, 1, line_offsets),
            KeyCode::Char('G') => {
                if total > 0 {
                    app.models_selected = total - 1;
                    if let Some(&row) = line_offsets.get(total - 1) {
                        app.models_scroll = (row + 1).saturating_sub(8) as u16;
                    }
                }
            }
            KeyCode::PageDown | KeyCode::Char('d') if ctrl => {
                nav_down(app, total, 10, line_offsets)
            }
            KeyCode::PageUp | KeyCode::Char('u') if ctrl => nav_up(app, 10, line_offsets),
            KeyCode::Enter => copy_selected(app, flat),
            KeyCode::Char('y') => {
                app.pending_key = Some(('y', Instant::now()));
            }
            KeyCode::Char('g') => {
                app.pending_key = Some(('g', Instant::now()));
            }
            _ => {}
        }
    }

    Ok(())
}
