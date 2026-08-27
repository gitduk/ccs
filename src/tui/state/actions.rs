use crate::config::{self, RouteRule};
use crate::error::Result;

use super::{
    API_KEY_FIELD_IDX, App, BASE_URL_FIELD_IDX, ConfirmAction, MessageKind, Mode, NAME_FIELD_IDX,
    ProviderForm,
};

/// Parsed and validated fields extracted from a [`ProviderForm`].
/// Produced by [`parse_provider_form`]; consumed by [`App::do_save_form`].
///
/// Omits `api_format`/`api_version`/`fallback`/`port` — those live outside this form.
struct ParsedProviderFields {
    name: String,
    base_url: String,
    api_key: String,
    routes: Vec<RouteRule>,
    original_name: Option<String>,
}

impl ParsedProviderFields {
    fn is_new(&self) -> bool {
        self.original_name.is_none()
    }
}

/// Extract, parse, and validate the provider form fields that can be checked
/// without App state (name uniqueness is checked by the caller).
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
    let original_name = form.original_name.clone();

    let routes: Vec<RouteRule> = form
        .routes
        .iter()
        .filter(|r| r.is_valid(known_models))
        .cloned()
        .collect();

    if name.is_empty() {
        return Err("Name cannot be empty".to_string());
    }
    if base_url.is_empty() {
        return Err("Base URL cannot be empty".to_string());
    }
    if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
        return Err("Base URL must start with http:// or https://".to_string());
    }

    Ok(ParsedProviderFields {
        name,
        base_url,
        api_key,
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

    /// Save the pending add without format auto-detection, using the format
    /// the user picked explicitly (`a` Anthropic / `o` OpenAI in the form).
    /// Only reachable after detection failed; field validation is unchanged.
    pub fn save_form_manual_format(
        &mut self,
        format: crate::config::ApiFormat,
    ) -> Result<()> {
        if let Some(f) = self.form.as_mut() {
            f.manual_format = Some(format);
        }
        self.do_save_form(true)
    }

    pub(super) fn do_save_form(&mut self, close: bool) -> Result<()> {
        let Some(form) = &self.form else {
            return Ok(());
        };

        if form.detect_token.is_some() {
            // Detection already in flight for this add; ignore repeat save attempts.
            return Ok(());
        }

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

        // One-shot: consume the explicit a/o format choice (if any) on this
        // save attempt, so a later `q` re-runs detection instead of a stale
        // manual choice left behind by a validation-error or IO-error attempt.
        let manual_format = self.form.as_mut().and_then(|f| f.manual_format.take());

        let fields = match parse_provider_form(self.form.as_ref().unwrap(), &known_models) {
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
        // Not editable in the form (see `f`/`F`/`o` shortcuts); inherit unchanged.
        let fallback = existing.map(|p| p.fallback).unwrap_or(false);
        let port = existing.and_then(|p| p.port);
        let quota_command = existing.and_then(|p| p.quota_command.clone());
        // Not editable in the form; inherit from the existing provider.
        let inject_thinking_history = existing.map(|p| p.inject_thinking_history).unwrap_or(true);
        // Not editable in the form; inherit from the existing provider.
        let strict_thinking_history = existing
            .map(|p| p.strict_thinking_history)
            .unwrap_or(false);
        // A new provider has no format yet (resolved below by auto-detection),
        // so this placeholder is only ever seen if fields.is_new() — editing
        // an existing provider always carries its real, already-known format.
        let existing_format = existing.map(|p| p.api_format.clone());
        let existing_version = existing.and_then(|p| p.api_version.clone());
        let mut provider = crate::config::Provider {
            id: provider_id.clone(),
            base_url: fields.base_url.clone(),
            api_key: fields.api_key.clone(),
            api_format: existing_format.unwrap_or(crate::config::ApiFormat::Anthropic),
            model_map,
            routes: fields.routes.clone(),
            enabled,
            fallback,
            api_version: existing_version,
            inject_thinking_history,
            strict_thinking_history,
            quota_command,
            port,
            test_model: existing.and_then(|p| p.test_model.clone()),
        };

        if fields.is_new() {
            // The user may have picked a format manually after detection
            // failed — insert immediately instead of re-detecting.
            if let Some(format) = manual_format {
                provider.api_format = format;
                return self.finish_new_provider_save(fields.name.clone(), provider);
            }

            // Format unknown yet — detect async, finish the insert in drain_test_results.
            let token = uuid::Uuid::new_v4().to_string();
            if let Some(f) = self.form.as_mut() {
                f.detect_token = Some(token.clone());
                f.detect_failed = false;
                f.error = None;
            }
            self.set_message(
                format!("Detecting API format for '{}'…", fields.name),
                MessageKind::Info,
            );

            let tx = self.tests.tx.clone();
            let client = self.tests.client.clone();
            let name = fields.name.clone();
            let base_url = fields.base_url.clone();
            let api_key = fields.api_key.clone();
            let routes = fields.routes.clone();
            tokio::spawn(async move {
                let detected = match config::resolve_api_key_str(&api_key) {
                    Ok(key) => crate::tester::detect_api_format(&client, &base_url, &key, None)
                        .await
                        .ok_or_else(|| {
                            "Could not detect API format - fix Base URL / API Key and retry, or press a/o in the form to save manually"
                                .to_string()
                        }),
                    Err(e) => Err(format!("API key resolution failed: {e}")),
                };
                let _ = tx.send(super::TestEvent::FormatDetected {
                    token,
                    name,
                    base_url,
                    api_key,
                    fallback,
                    port,
                    routes,
                    detected,
                });
            });

            return Ok(());
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
            crate::test_store::save(&self.tests.results);
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
            self.spawn_model_fetch(&fields.name);
        }

        self.config.sort_providers_by_enabled();
        config::save_config(&self.config)?;
        if self.config.providers.contains_key(&fields.name) {
            let idx = super::navigation::display_rank_of(&self.config, &fields.name);
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

    /// Insert a brand-new provider (fully formed, format resolved) into the
    /// in-memory config, persist it, and close the editing form. On a save
    /// failure the insert is rolled back and the form stays open with the error.
    fn finish_new_provider_save(
        &mut self,
        name: String,
        provider: crate::config::Provider,
    ) -> Result<()> {
        let is_first = self.config.providers.is_empty();
        self.config.providers.insert(name.clone(), provider);
        if is_first {
            self.config.current = name.clone();
        }
        self.spawn_model_fetch(&name);
        self.config.sort_providers_by_enabled();
        if let Err(e) = config::save_config(&self.config) {
            // Roll back so the table never shows a provider that didn't hit
            // disk; keep the form open with the error so the user can retry.
            self.config.providers.shift_remove(&name);
            if let Some(f) = self.form.as_mut() {
                f.error = Some(format!("Save failed: {e}"));
            }
            return Ok(());
        }
        if self.config.providers.contains_key(&name) {
            let idx = super::navigation::display_rank_of(&self.config, &name);
            self.providers.table_state.select(Some(idx));
        }
        self.set_message(format!("Added '{name}'"), MessageKind::Success);
        self.form = None;
        self.mode = Mode::Normal;
        self.config_needs_sync = true;
        Ok(())
    }

    /// Fetch a provider's model list in the background so routes can be
    /// configured immediately. Skips providers with no URL or key.
    fn spawn_model_fetch(&self, name: &str) {
        let Some(p) = self.config.providers.get(name).cloned() else {
            return;
        };
        if p.base_url.is_empty() || p.api_key.is_empty() {
            return;
        }
        let tx = self.tests.tx.clone();
        let client = self.tests.client.clone();
        let provider_name = name.to_string();
        tokio::spawn(async move {
            let models = crate::tester::fetch_provider_models(&client, &p).await;
            if !models.is_empty() {
                let _ = tx.send(super::TestEvent::ModelsOnly {
                    provider: provider_name,
                    models,
                });
            }
        });
    }

    /// Store a freshly discovered catalog in the DB and its in-memory mirror.
    /// An empty list means the fetch came back with nothing — keep the last
    /// known catalog rather than blanking the provider's model list.
    /// Returns whether anything was written.
    pub(crate) fn store_provider_models(
        &mut self,
        provider_id: &str,
        name: &str,
        models: Vec<String>,
    ) -> bool {
        if models.is_empty() {
            return false;
        }
        self.db.replace_provider_models(provider_id, name, &models);
        self.models.provider_models.insert(name.to_string(), models);
        true
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
        self.db.delete_provider(id);
        if let Ok(mut m) = self.metrics.lock() {
            m.clear_error(name);
        }
        self.models.provider_models.remove(name);
        self.tests.results.remove(name);
        crate::test_store::save(&self.tests.results);
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
            let count = self.table_row_count();
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
        let Some(name) = self.selected_name().map(|s| s.to_string()) else {
            return Ok(());
        };
        if !self.config.providers.contains_key(&name) {
            return Ok(());
        }
        let idx_before = super::navigation::display_rank_of(&self.config, &name);
        let Some(provider) = self.config.providers.get_mut(&name) else {
            return Ok(());
        };
        provider.enabled = !provider.enabled;
        let now_enabled = provider.enabled;

        // If we just disabled the current provider, advance to the next enabled one.
        let no_enabled_left = if !now_enabled && self.config.current == name {
            match self
                .config
                .providers
                .iter()
                .find(|(k, v)| *k != &name && v.enabled)
            {
                Some((k, _)) => {
                    self.config.current = k.clone();
                    false
                }
                None => true,
            }
        } else {
            false
        };

        // Keep the table in the fold layout: enabled first, disabled folded at the end.
        self.config.sort_providers_by_enabled();
        let enabled_count = self.enabled_count();
        if now_enabled {
            // Cursor follows the (re-enabled) provider at the end of the enabled block.
            if self.config.providers.contains_key(&name) {
                let idx = super::navigation::display_rank_of(&self.config, &name);
                self.providers.table_state.select(Some(idx));
            }
        } else {
            // Cursor lands where the disabled provider was: the next enabled row,
            // or the last enabled row, which folds the disabled block back up.
            self.providers
                .table_state
                .select(Some(idx_before.min(enabled_count.saturating_sub(1))));
        }

        config::save_config(&self.config)?;
        if no_enabled_left {
            self.set_message(
                format!("'{name}' disabled — no enabled providers remain"),
                MessageKind::Error,
            );
        } else {
            let state = if now_enabled { "enabled" } else { "disabled" };
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
                    if let Some(mut models) = result.model_names.clone() {
                        // The refreshed catalog may omit the just-tested model
                        // (e.g. a search-box name): keep it so the row and its
                        // result stay visible instead of vanishing on completion.
                        if !result.used_model.is_empty()
                            && !models
                                .iter()
                                .any(|m| m.eq_ignore_ascii_case(&result.used_model))
                        {
                            models.push(result.used_model.clone());
                        }
                        self.store_provider_models(&provider_id, &name, models);
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
                    // Clear stale error when test passes so the Models panel
                    // shows a clean per-model result.
                    if !failed && let Ok(mut m) = self.metrics.lock() {
                        m.clear_error(&name);
                    }
                    // Store per-model so the Models panel can show the result
                    // next to the model name. used_model is empty only when the
                    // test failed before selecting a model (no per-model entry).
                    if !result.used_model.is_empty() {
                        self.tests
                            .results
                            .entry(name.clone())
                            .or_default()
                            .insert(result.used_model.clone(), result);
                        crate::test_store::save(&self.tests.results);
                    }
                }
                TestEvent::ModelsOnly {
                    provider: name,
                    models,
                } => {
                    let provider_id = self
                        .config
                        .providers
                        .get(&name)
                        .map(|p| p.id.clone())
                        .unwrap_or_else(|| name.clone());
                    self.store_provider_models(&provider_id, &name, models);
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
                TestEvent::FormatDetected {
                    token,
                    name,
                    base_url,
                    api_key,
                    fallback,
                    port,
                    routes,
                    detected,
                } => {
                    let still_pending = self
                        .form
                        .as_ref()
                        .is_some_and(|f| f.detect_token.as_deref() == Some(token.as_str()));
                    if !still_pending {
                        // Add was cancelled or the form moved on; drop the result.
                        continue;
                    }
                    match detected {
                        Ok(d) => {
                            let provider_id = uuid::Uuid::new_v4().to_string();
                            let provider = crate::config::Provider {
                                id: provider_id.clone(),
                                base_url,
                                api_key,
                                api_format: d.api_format,
                                model_map: Default::default(),
                                routes,
                                enabled: true,
                                fallback,
                                api_version: d.api_version,
                                inject_thinking_history: true,
                                strict_thinking_history: false,
                                quota_command: None,
                                port,
                                test_model: None,
                            };
                            let is_first = self.config.providers.is_empty();
                            self.config.providers.insert(name.clone(), provider);
                            if is_first {
                                self.config.current = name.clone();
                            }
                            self.config.sort_providers_by_enabled();
                            let wrote_models =
                                self.store_provider_models(&provider_id, &name, d.models);
                            match config::save_config(&self.config) {
                                Ok(()) => {
                                    if self.config.providers.contains_key(&name) {
                                        let idx = super::navigation::display_rank_of(&self.config, &name);
                                        self.providers.table_state.select(Some(idx));
                                    }
                                    self.set_message(
                                        format!("Added '{name}'"),
                                        MessageKind::Success,
                                    );
                                    self.form = None;
                                    self.mode = Mode::Normal;
                                    self.config_needs_sync = true;
                                }
                                Err(e) => {
                                    self.config.providers.shift_remove(&name);
                                    // DB row orphaned (harmless); drop the in-memory copy validation reads.
                                    if wrote_models {
                                        self.models.provider_models.remove(&name);
                                    }
                                    if let Some(f) = self.form.as_mut() {
                                        f.detect_token = None;
                                        f.error = Some(format!("Save failed: {e}"));
                                    }
                                }
                            }
                        }
                        Err(msg) => {
                            if let Some(f) = self.form.as_mut() {
                                f.detect_token = None;
                                f.detect_failed = true;
                                f.error = Some(msg);
                            }
                        }
                    }
                }
            }
        }
        changed
    }
}
