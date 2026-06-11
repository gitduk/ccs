use crate::config::{self, ApiFormat, OpenAiApiVersion, RouteRule};
use crate::error::Result;

use super::{
    API_KEY_FIELD_IDX, App, BASE_URL_FIELD_IDX, ConfirmAction, FALLBACK_FIELD_IDX,
    FORMAT_FIELD_IDX, MessageKind, Mode, NAME_FIELD_IDX, PORT_FIELD_IDX, ProviderForm,
};

/// Parsed and validated fields extracted from a [`ProviderForm`].
/// Produced by [`parse_provider_form`]; consumed by [`App::do_save_form`].
struct ParsedProviderFields {
    name: String,
    base_url: String,
    api_key: String,
    api_format: ApiFormat,
    api_version: Option<OpenAiApiVersion>,
    fallback: bool,
    port: Option<u16>,
    routes: Vec<RouteRule>,
    original_name: Option<String>,
}

impl ParsedProviderFields {
    fn is_new(&self) -> bool {
        self.original_name.is_none()
    }
}

/// Extract, parse, and validate the provider form fields that can be checked
/// without App state (name uniqueness and port collisions are checked by the caller).
/// Returns `Err(message)` with a user-facing error string on the first failure.
fn parse_provider_form(
    form: &ProviderForm,
    known_models: &[String],
) -> std::result::Result<ParsedProviderFields, String> {
    let name = form.fields[NAME_FIELD_IDX].value.trim().to_string();
    let base_url = form.fields[BASE_URL_FIELD_IDX]
        .value
        .trim()
        .trim_end_matches('/')
        .to_string();
    let api_key = form.fields[API_KEY_FIELD_IDX].value.trim().to_string();
    let format_str = form.fields[FORMAT_FIELD_IDX].value.trim().to_string();
    let fallback = form.fields[FALLBACK_FIELD_IDX].value.trim() == "yes";
    let port_raw = form.fields[PORT_FIELD_IDX].value.trim().to_string();
    let original_name = form.original_name.clone();

    let routes: Vec<RouteRule> = form
        .routes
        .iter()
        .filter(|r| r.is_valid(known_models))
        .cloned()
        .collect();

    let port: Option<u16> = if port_raw.is_empty() {
        None
    } else {
        match port_raw.parse::<u16>() {
            Ok(p) => Some(p),
            Err(_) => {
                return Err(format!("Port must be a number (1–65535), got '{port_raw}'"));
            }
        }
    };

    if name.is_empty() {
        return Err("Name cannot be empty".to_string());
    }
    if base_url.is_empty() {
        return Err("Base URL cannot be empty".to_string());
    }
    if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
        return Err("Base URL must start with http:// or https://".to_string());
    }

    let (api_format, api_version) = match format_str.as_str() {
        "openai-chat" => (ApiFormat::OpenAI, Some(OpenAiApiVersion::ChatCompletions)),
        "openai-responses" => (ApiFormat::OpenAI, Some(OpenAiApiVersion::Responses)),
        _ => (ApiFormat::Anthropic, None),
    };

    Ok(ParsedProviderFields {
        name,
        base_url,
        api_key,
        api_format,
        api_version,
        fallback,
        port,
        routes,
        original_name,
    })
}

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

    pub fn save_form_and_close(&mut self) -> Result<()> {
        self.do_save_form(true)
    }

    pub(super) fn do_save_form(&mut self, close: bool) -> Result<()> {
        let Some(form) = &self.form else {
            return Ok(());
        };

        // Look up the known model list for route validation.
        // If not yet loaded we skip the target check (conservative).
        let models_key = form
            .original_name
            .as_deref()
            .unwrap_or_else(|| form.fields[NAME_FIELD_IDX].value.trim());
        let known_models: Vec<String> = self
            .models
            .provider_models
            .get(models_key)
            .cloned()
            .unwrap_or_default();

        let fields = match parse_provider_form(form, &known_models) {
            Ok(f) => f,
            Err(err) => {
                if let Some(f) = self.form.as_mut() {
                    f.error = Some(err);
                }
                return Ok(());
            }
        };
        // `form` borrow ends here (NLL).

        let is_rename =
            !fields.is_new() && fields.original_name.as_deref() != Some(fields.name.as_str());

        if (fields.is_new() || is_rename) && self.config.providers.contains_key(&fields.name) {
            if let Some(f) = self.form.as_mut() {
                f.error = Some(format!("Provider '{}' already exists", fields.name));
            }
            return Ok(());
        }

        let lookup_name = fields.original_name.as_deref().unwrap_or(&fields.name);
        let existing = self.config.providers.get(lookup_name);
        let provider_id = existing
            .map(|p| p.id.clone())
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let model_map = existing.map(|p| p.model_map.clone()).unwrap_or_default();
        let enabled = existing.map(|p| p.enabled).unwrap_or(true);
        // fallback is set via the form field; don't inherit from existing.
        let quota_command = existing.and_then(|p| p.quota_command.clone());
        // Not editable in the form; inherit from the existing provider.
        let inject_thinking_history = existing.map(|p| p.inject_thinking_history).unwrap_or(true);
        let provider = crate::config::Provider {
            id: provider_id.clone(),
            base_url: fields.base_url,
            api_key: fields.api_key,
            api_format: fields.api_format,
            model_map,
            routes: fields.routes.clone(),
            enabled,
            fallback: fields.fallback,
            api_version: fields.api_version,
            inject_thinking_history,
            quota_command,
            port: fields.port,
        };

        // Port collision check — apply the proposed change to a temp config and let
        // validate_ports() enforce all rules centrally, so this path stays in sync.
        if fields.port.is_some() {
            let mut temp = self.config.clone();
            if let Some(old_name) = fields.original_name.as_deref() {
                temp.providers.shift_remove(old_name);
            }
            temp.providers.insert(fields.name.clone(), provider.clone());
            if let Err(e) = temp.validate_ports() {
                if let Some(f) = self.form.as_mut() {
                    f.error = Some(e.to_string());
                }
                return Ok(());
            }
        }

        if is_rename {
            let old_name = fields.original_name.as_deref().unwrap();
            self.config.providers = std::mem::take(&mut self.config.providers)
                .into_iter()
                .map(|(k, v)| {
                    if k == old_name {
                        (fields.name.clone(), provider.clone())
                    } else {
                        (k, v)
                    }
                })
                .collect();

            if self.config.current == old_name {
                self.config.current = fields.name.clone();
            }

            self.db.rename_provider(&provider_id, &fields.name);

            if let Some(models) = self.models.provider_models.remove(old_name) {
                self.models
                    .provider_models
                    .insert(fields.name.clone(), models);
            }
            if let Some(result) = self.tests.results.remove(old_name) {
                self.tests.results.insert(fields.name.clone(), result);
            }
            if self.tests.pending.remove(old_name) {
                self.tests.pending.insert(fields.name.clone());
            }
            if let Some(model) = self.tests.testing_model.remove(old_name) {
                self.tests.testing_model.insert(fields.name.clone(), model);
            }
        } else {
            let is_first = self.config.providers.is_empty();
            self.config.providers.insert(fields.name.clone(), provider);
            if is_first {
                self.config.current = fields.name.clone();
            }
        }

        config::save_config(&self.config)?;
        if let Some(idx) = self.config.providers.get_index_of(&fields.name) {
            self.providers.table_state.select(Some(idx));
        }

        if close {
            self.mode = Mode::Normal;
            self.form = None;
        } else {
            // Keep the form open; if this was a brand-new provider, mark it as
            // an edit from now on so subsequent autosaves don't try to re-insert.
            if let Some(f) = &mut self.form {
                f.routes.retain(|r| r.is_valid(&known_models));
                f.clamp_route_cursor();
                f.original_name = Some(fields.name);
                f.error = None;
            }
        }

        Ok(())
    }

    pub fn confirm(&mut self, action: ConfirmAction) {
        self.confirm_action = Some(action);
        self.mode = Mode::Confirm;
    }

    pub fn clear_metrics(&mut self) {
        self.db.clear_all();
        if let Ok(mut m) = self.metrics.lock() {
            m.last_error.clear();
        }
        // Clear in-process request log (bg proxy mode syncs via reload_metrics_from_db below).
        if self.bg_proxy_pid.is_none()
            && let Ok(mut log) = self.request_log.lock()
        {
            log.replace(vec![]);
        }
        // Reload immediately so the TUI reflects the cleared state right away
        // instead of waiting up to ~1s for the next periodic reload.
        self.reload_metrics_from_db();
        self.set_message("Usage data cleared", MessageKind::Success);
    }

    pub fn clear_current_provider_metrics(&mut self) {
        let Some(name) = self.selected_name().map(|s| s.to_string()) else {
            return;
        };
        let Some(provider_id) = self.config.providers.get(&name).map(|p| p.id.clone()) else {
            return;
        };

        self.db.clear_provider(&provider_id);
        if let Ok(mut m) = self.metrics.lock() {
            m.clear_error(&name);
        }
        self.reload_metrics_from_db();
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
        let id = removed.as_ref().map(|p| p.id.as_str()).unwrap_or(name);
        self.db.clear_provider(id);
        if let Ok(mut m) = self.metrics.lock() {
            m.clear_error(name);
        }
        self.models.provider_models.remove(name);
        self.tests.results.remove(name);
        self.tests.pending.remove(name);
        self.tests.testing_model.remove(name);
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
        if let Some(selected) = self.providers.table_state.selected() {
            let count = self.provider_count();
            if count == 0 {
                self.providers.table_state.select(None);
            } else if selected >= count {
                self.providers.table_state.select(Some(count - 1));
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

    pub fn toggle_provider_fallback(&mut self) -> Result<()> {
        let Some(name) = self.selected_name().map(|s| s.to_string()) else {
            return Ok(());
        };
        if name == self.config.current {
            self.set_message(
                format!("'{name}' is current provider — it always participates in fallback"),
                MessageKind::Info,
            );
            return Ok(());
        }
        if let Some(provider) = self.config.providers.get_mut(&name) {
            // No guard against "last fallback provider": an empty fallback pool is fine —
            // the current provider still handles requests as primary.
            provider.fallback = !provider.fallback;
            let state = if provider.fallback {
                "in fallback"
            } else {
                "out of fallback"
            };
            config::save_config(&self.config)?;
            self.set_message(format!("'{name}' {state}"), MessageKind::Info);
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

    /// Drain background test events: update in-progress model display or
    /// finalise completed results.
    pub fn drain_test_results(&mut self) -> bool {
        use super::TestEvent;
        let events: Vec<_> = self.tests.drain().collect();
        let changed = !events.is_empty();
        for event in events {
            match event {
                TestEvent::ModelSelected { provider, model } => {
                    self.tests.testing_model.insert(provider, model);
                }
                TestEvent::Completed {
                    provider: name,
                    result,
                } => {
                    self.tests.pending.remove(&name);
                    let final_used_model = result.used_model.clone();
                    if final_used_model.is_empty() {
                        self.tests.testing_model.remove(&name);
                    } else {
                        self.tests
                            .testing_model
                            .insert(name.clone(), final_used_model);
                    }
                    let provider_id = self
                        .config
                        .providers
                        .get(&name)
                        .map(|p| p.id.clone())
                        .unwrap_or_else(|| name.clone());
                    if let Some(models) = &result.model_names {
                        self.db.upsert_provider_models(&provider_id, &name, models);
                        self.models
                            .provider_models
                            .insert(name.clone(), models.clone());
                    }
                    // Record the test request in provider stats so it appears in By Provider.
                    let failed = !matches!(result.status, crate::tester::TestStatus::Ok);
                    let model_str =
                        (!result.used_model.is_empty()).then_some(result.used_model.as_str());
                    self.db.persist_stats_async(
                        &provider_id,
                        &name,
                        model_str,
                        crate::repo::StatsDelta {
                            requests: 1,
                            failures: u64::from(failed),
                            ..Default::default()
                        },
                    );
                    // Clear stale error when test passes so Info panel shows clean state.
                    if !failed && let Ok(mut m) = self.metrics.lock() {
                        m.clear_error(&name);
                    }
                    self.tests.results.insert(name, result);
                }
                TestEvent::QuotaCompleted {
                    provider_id,
                    result,
                } => {
                    use super::QuotaStatus;
                    let is_form_provider = self
                        .quota_form
                        .as_ref()
                        .and_then(|form| self.config.providers.get(&form.provider_name))
                        .map(|p| p.id == provider_id)
                        .unwrap_or(false);
                    let status = match &result {
                        Ok(quota_result) => QuotaStatus::Success(quota_result.clone()),
                        Err(e) => QuotaStatus::Error(e.clone()),
                    };
                    self.quota_status.insert(provider_id, status);
                    if let Some(form) = self.quota_form.as_mut()
                        && is_form_provider
                    {
                        form.preview_loading = false;
                        form.preview_scroll = 0;
                        match result {
                            Ok(quota_result) => {
                                form.preview = Some(quota_result.output);
                                form.error = None;
                            }
                            Err(e) => {
                                form.preview = None;
                                form.error = Some(e);
                            }
                        }
                    }
                }
            }
        }
        changed
    }
}
