use crate::config::{self, ApiFormat};
use crate::error::Result;

use super::{App, ConfirmAction, MessageKind, Mode, NOTES_FIELD_IDX, ProviderForm};

impl App {
    pub fn add(&mut self) {
        self.form = Some(ProviderForm::new("", None));
        self.mode = Mode::Editing;
    }

    pub fn start_edit(&mut self) {
        let Some(name) = self.selected_name() else {
            return;
        };
        let Some(provider) = self.config.providers.get(name) else {
            return;
        };

        self.form = Some(ProviderForm::new(name, Some(provider)));
        self.mode = Mode::Editing;
    }

    pub fn save_form_in_place(&mut self) -> Result<()> {
        self.do_save_form(false)
    }

    pub(super) fn do_save_form(&mut self, close: bool) -> Result<()> {
        let Some(form) = &self.form else {
            return Ok(());
        };

        let new_name = form.fields[0].value.trim().to_string();
        let base_url = form.fields[1]
            .value
            .trim()
            .trim_end_matches('/')
            .to_string();
        let api_key = form.fields[2].value.trim().to_string();
        let format_str = form.fields[3].value.trim().to_string();
        let notes = form.fields[NOTES_FIELD_IDX]
            .value
            .trim_matches('\n')
            .to_string();
        let is_new = form.original_name.is_none();
        let original_name = form.original_name.clone();
        // Look up the known model list for this provider (used for route validation).
        // If not yet loaded we skip the target check (conservative).
        let models_key = original_name.as_deref().unwrap_or(new_name.as_str());
        let known_models: Vec<String> = self
            .provider_models
            .get(models_key)
            .cloned()
            .unwrap_or_default();
        // Drop invalid routes (empty pattern/target, or target not in known models).
        let routes: Vec<_> = form
            .routes
            .iter()
            .filter(|r| r.is_valid(&known_models))
            .cloned()
            .collect();

        let is_rename = !is_new && original_name.as_deref() != Some(new_name.as_str());

        let validation_error = if new_name.is_empty() {
            Some("Name cannot be empty".to_string())
        } else if base_url.is_empty() {
            Some("Base URL cannot be empty".to_string())
        } else if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
            Some("Base URL must start with http:// or https://".to_string())
        } else if (is_new || is_rename) && self.config.providers.contains_key(&new_name) {
            Some(format!("Provider '{new_name}' already exists"))
        } else {
            None
        };
        if let Some(err) = validation_error {
            if let Some(f) = self.form.as_mut() {
                f.error = Some(err);
            }
            return Ok(());
        }

        let api_format = if format_str == "openai" {
            ApiFormat::OpenAI
        } else {
            ApiFormat::Anthropic
        };

        let lookup_name = original_name.as_deref().unwrap_or(&new_name);
        let existing = self.config.providers.get(lookup_name);
        let provider_id = existing
            .map(|p| p.id.clone())
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let model_map = existing.map(|p| p.model_map.clone()).unwrap_or_default();

        let enabled = existing.map(|p| p.enabled).unwrap_or(true);
        let api_version = existing.and_then(|p| p.api_version.clone());
        let provider = crate::config::Provider {
            id: provider_id.clone(),
            base_url,
            api_key,
            api_format,
            model_map,
            notes: notes.clone(),
            routes,
            enabled,
            api_version,
        };

        if is_rename {
            let old_name = original_name.as_deref().unwrap();
            self.config.providers = std::mem::take(&mut self.config.providers)
                .into_iter()
                .map(|(k, v)| {
                    if k == old_name {
                        (new_name.clone(), provider.clone())
                    } else {
                        (k, v)
                    }
                })
                .collect();

            if self.config.current == old_name {
                self.config.current = new_name.clone();
            }

            if let Ok(conn) = self.db.lock() {
                let _ = crate::db::rename_provider(&conn, &provider_id, &new_name);
            }

            if let Some(models) = self.provider_models.remove(old_name) {
                self.provider_models.insert(new_name.clone(), models);
            }
            if let Some(result) = self.test_results.remove(old_name) {
                self.test_results.insert(new_name.clone(), result);
            }
            if self.pending_tests.remove(old_name) {
                self.pending_tests.insert(new_name.clone());
            }
        } else {
            let is_first = self.config.providers.is_empty();
            self.config.providers.insert(new_name.clone(), provider);
            if is_first {
                self.config.current = new_name.clone();
            }
        }

        config::save_config(&self.config)?;
        self.refresh_ids();
        if let Some(idx) = self.provider_names.iter().position(|s| s == &new_name) {
            self.table_state.select(Some(idx));
        }

        if close {
            self.mode = Mode::Normal;
            self.form = None;
        } else {
            // Keep the form open; if this was a brand-new provider, mark it as
            // an edit from now on so subsequent autosaves don't try to re-insert.
            if let Some(f) = &mut self.form {
                // Mirror the cleanup: remove invalid routes from the live form too.
                f.routes.retain(|r| r.is_valid(&known_models));
                f.clamp_route_cursor();
                f.original_name = Some(new_name);
                f.error = None;
                f.fields[NOTES_FIELD_IDX].cursor =
                    f.fields[NOTES_FIELD_IDX].cursor.min(notes.len());
                f.fields[NOTES_FIELD_IDX].value = notes;
            }
        }

        Ok(())
    }

    pub fn confirm(&mut self, action: ConfirmAction) {
        self.confirm_action = Some(action);
        self.mode = Mode::Confirm;
    }

    pub fn clear_metrics(&mut self) {
        if let Ok(mut conn) = self.db.lock() {
            let _ = crate::db::clear_all(&mut conn);
        }
        if let Ok(mut m) = self.metrics.lock() {
            m.last_error.clear();
        }
        // Reload immediately so the TUI reflects the cleared state right away
        // instead of waiting up to ~1s for the next periodic reload.
        crate::tui::event_loop::reload_metrics_from_db(self);
        self.set_message("Usage data cleared", MessageKind::Success);
    }

    pub fn clear_current_provider_metrics(&mut self) {
        let Some(name) = self.selected_name().map(|s| s.to_string()) else {
            return;
        };
        let Some(provider_id) = self.config.providers.get(&name).map(|p| p.id.clone()) else {
            return;
        };

        if let Ok(mut conn) = self.db.lock() {
            let _ = crate::db::clear_provider(&mut conn, &provider_id);
        }
        if let Ok(mut m) = self.metrics.lock() {
            m.clear_error(&name);
        }
        crate::tui::event_loop::reload_metrics_from_db(self);
        self.set_message(
            format!("Usage data cleared for '{name}'"),
            MessageKind::Success,
        );
    }

    pub fn confirm_action_execute(&mut self) -> Result<()> {
        match self.confirm_action.take() {
            Some(ConfirmAction::Clear) => {
                self.clear_metrics();
            }
            Some(ConfirmAction::ClearCurrent) => {
                self.clear_current_provider_metrics();
            }
            Some(ConfirmAction::Quit) => {
                self.should_quit = true;
            }
            Some(ConfirmAction::Delete(name)) => {
                self.do_delete(&name)?;
            }
            None => {}
        }
        self.mode = Mode::Normal;
        Ok(())
    }

    pub(super) fn do_delete(&mut self, name: &str) -> Result<()> {
        let removed = self.config.providers.shift_remove(name);
        if let Ok(mut conn) = self.db.lock() {
            let id = removed.as_ref().map(|p| p.id.as_str()).unwrap_or(name);
            let _ = crate::db::clear_provider(&mut conn, id);
        }
        if let Ok(mut m) = self.metrics.lock() {
            m.clear_error(name);
        }
        self.provider_models.remove(name);
        if self.config.current == name {
            self.config.current = self
                .config
                .providers
                .keys()
                .next()
                .cloned()
                .unwrap_or_default();
        }
        config::save_config(&self.config)?;
        self.refresh_ids();
        if let Some(selected) = self.table_state.selected() {
            if selected >= self.provider_names.len() && !self.provider_names.is_empty() {
                self.table_state.select(Some(self.provider_names.len() - 1));
            } else if self.provider_names.is_empty() {
                self.table_state.select(None);
            }
        }
        self.set_message(format!("Deleted '{name}'"), MessageKind::Success);
        Ok(())
    }

    pub fn switch_to_selected(&mut self) -> Result<()> {
        if let Some(name) = self.selected_name().map(|s| s.to_string()) {
            let is_enabled = self
                .config
                .providers
                .get(&name)
                .map(|p| p.enabled)
                .unwrap_or(false);
            if !is_enabled {
                self.set_message(
                    format!("'{name}' is disabled — enable it first with 'p'"),
                    MessageKind::Error,
                );
                return Ok(());
            }
            self.config.current = name.clone();
            config::save_config(&self.config)?;
        }
        Ok(())
    }

    pub fn toggle_provider_enabled(&mut self) -> Result<()> {
        if let Some(name) = self.selected_name().map(|s| s.to_string())
            && let Some(provider) = self.config.providers.get_mut(&name)
        {
            provider.enabled = !provider.enabled;
            let now_enabled = provider.enabled;
            let state = if now_enabled { "enabled" } else { "disabled" };

            // If we just disabled the current provider, advance to the next enabled one.
            if !now_enabled && self.config.current == name {
                let next = self
                    .config
                    .providers
                    .iter()
                    .find(|(k, v)| *k != &name && v.enabled)
                    .map(|(k, _)| k.clone());
                match next {
                    Some(next_name) => self.config.current = next_name,
                    None => {
                        config::save_config(&self.config)?;
                        self.set_message(
                            format!("'{name}' disabled — no enabled providers remain"),
                            MessageKind::Error,
                        );
                        return Ok(());
                    }
                }
            }

            config::save_config(&self.config)?;
            self.set_message(format!("'{name}' {state}"), MessageKind::Info);
        }
        Ok(())
    }
}
