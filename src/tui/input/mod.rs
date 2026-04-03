mod confirm;
mod editor;
mod insert;
mod models;
mod normal;
mod routes;

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
        Mode::Normal => normal::handle_normal_key(app, code, server),
        Mode::Editing => editor::handle_editing_key(app, code, modifiers, server),
        Mode::Confirm => confirm::handle_confirm_key(app, code, server),
        Mode::Help => {
            app.mode = Mode::Normal;
            Ok(())
        }
        Mode::Models => {
            // Build the flat filtered list + per-model line offsets.
            // SYNC: filter/sort/grouping logic must match draw_models in ui/dialogs.rs.
            let filter = app.models.search_field.value.to_lowercase();
            let mut providers: Vec<&String> = app.models.provider_models.keys().collect();
            providers.sort_unstable();

            // Clone strings so `flat` has no outstanding borrow on `app.models.provider_models`,
            // allowing `handle_models_key` to take `&mut App` in the same scope.
            let mut flat_owned: Vec<String> = Vec::new();
            // line_offsets[i] = the row index in the rendered list that flat_owned[i] occupies.
            // This mirrors the exact line layout produced by draw_models (blank line +
            // provider heading before each group) so scroll stays in sync with selection.
            let mut line_offsets: Vec<usize> = Vec::new();
            let mut line: usize = 0;
            let mut first_group = true;

            for prov in &providers {
                let models = app
                    .models
                    .provider_models
                    .get(*prov)
                    .map(|v| v.as_slice())
                    .unwrap_or(&[]);
                let mut matched: Vec<String> = models
                    .iter()
                    .filter(|m| filter.is_empty() || m.to_lowercase().contains(&filter))
                    .cloned()
                    .collect();
                if matched.is_empty() {
                    continue;
                }
                matched.sort_unstable();
                if !first_group {
                    line += 1; // blank line between groups
                }
                first_group = false;
                line += 1; // provider heading
                for m in matched {
                    line_offsets.push(line);
                    line += 1;
                    flat_owned.push(m);
                }
            }

            let flat: Vec<&str> = flat_owned.iter().map(|s| s.as_str()).collect();
            // Clamp selection in case provider_models shrank since last keypress.
            app.models.selected = app.models.selected.min(flat.len().saturating_sub(1));
            models::handle_models_key(app, code, modifiers, &flat, &line_offsets)
        }
    }
}
