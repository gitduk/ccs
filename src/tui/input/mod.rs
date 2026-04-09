mod confirm;
pub(super) mod insert;

use crossterm::event::{KeyCode, KeyModifiers};

use super::App;
use super::ServerHandle;
use super::state::Mode;

/// Copy `text` to the system clipboard via `wl-copy`.
/// Returns true on success, false if wl-copy is unavailable or fails.
pub(super) fn copy_to_clipboard(text: &str) -> bool {
    std::process::Command::new("wl-copy")
        .arg(text)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub(super) fn handle_key(
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
    server: &mut Option<ServerHandle>,
) -> crate::error::Result<()> {
    match &app.mode {
        Mode::Normal => super::providers::handle_key(app, code, modifiers, server),
        Mode::Editing => super::editor::handle_key(app, code, modifiers, server),
        Mode::Confirm => confirm::handle_confirm_key(app, code, server),
        Mode::QuotaConfig => super::quota_panel::handle_key(app, code, modifiers),
        Mode::Help => {
            app.mode = Mode::Normal;
            Ok(())
        }
        Mode::Logs => super::logs::handle_key(app, code),
        Mode::Models => super::models::handle_key(app, code, modifiers),
    }
}

pub(super) fn handle_paste(app: &mut App, text: &str) -> crate::error::Result<()> {
    match &app.mode {
        Mode::QuotaConfig => super::quota_panel::handle_paste(app, text),
        Mode::Editing => super::editor::handle_paste(app, text),
        Mode::Models => {
            super::models::handle_paste(app, text);
            Ok(())
        }
        _ => Ok(()), // Ignore paste in other modes
    }
}
