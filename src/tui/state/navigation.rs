use std::sync::Arc;
use std::sync::Mutex;

use crate::config;
use crate::error::Result;
use crate::repo::Repository;

use super::{App, LogsState, MESSAGE_TIMEOUT_SECS, MessageKind, ModelsState, ProviderList};

impl App {
    pub fn new() -> Result<Self> {
        use ratatui::widgets::TableState;

        // The stored order is the user's own; the fold renders enabled-first
        // regardless, so loading must not rewrite it — or the file, on save.
        let config = config::load_config()?;

        let mut table_state = TableState::default();
        if !config.providers.is_empty() {
            let idx = display_rank_of(&config, &config.current);
            table_state.select(Some(idx));
        }

        let db = Repository::open(&config.resolve_db_path());
        if let Err(e) = db.migrate(&config.name_to_id_map()) {
            tracing::warn!("DB schema migration failed: {e}");
        }

        let (metrics_data, provider_models) = db.load_all();
        let metrics = Arc::new(Mutex::new(metrics_data));

        let initial_logs = db.load_recent_request_logs(config.request_log_limit);
        let request_log = Arc::new(Mutex::new(crate::metrics::RequestLog::from_entries(
            initial_logs,
        )));

        let bg_proxy_pid = super::bg_proxy::load_bg_proxy_pid();

        // Restore the last test results; drop entries whose provider no longer
        // exists in config (deleted externally while ccs wasn't running).
        let mut tests = super::TestState::new();
        tests.results = crate::test_store::load();
        tests
            .results
            .retain(|name, _| config.providers.contains_key(name));

        Ok(Self {
            config,
            mode: super::Mode::Normal,
            terminal_focused: true,
            providers: ProviderList {
                table_state,
                expanded: false,
            },
            form: None,
            message: None,
            confirm_action: None,
            should_quit: false,
            server_status: super::ServerStatus::Stopped,
            metrics,
            tests,
            db,
            bg_proxy_pid,
            models: ModelsState {
                provider_models,
                search_field: super::FormField::search(),
                search_active: true,
                selected: 0,
                scroll: 0,
                pending_key: None,
            },
            request_log,
            logs: LogsState {
                selected: 0,
                scroll: 0,
                detail_scroll: 0,
                detail_view_height: 0,
                pending_key: None,
            },
            message_log: std::collections::VecDeque::new(),
            seen_provider_errors: std::collections::HashMap::new(),
            pending_key: None,
            quota_status: std::collections::HashMap::new(),
            quota_form: None,
            quick_form: None,
            help_scroll: 0,
            sysinfo_sampler: crate::tui::sysinfo::SysInfoSampler::new(),
            config_needs_sync: false,
        })
    }

    /// Provider name at the given table row (fold-aware).
    pub fn provider_name_at(&self, idx: usize) -> Option<&str> {
        if self.is_providers_collapsed() && idx >= self.enabled_count() {
            return None; // the fold row ("…") — not a provider
        }
        // Rows are always the enabled block first, then disabled — whether or
        // not the stored order already lines up with that.
        let enabled = self.enabled_names();
        if idx < enabled.len() {
            return enabled.get(idx).copied();
        }
        self.disabled_names().get(idx - enabled.len()).copied()
    }

    /// Enabled provider names, in stored order (the leading fold block).
    fn enabled_names(&self) -> Vec<&str> {
        self.config
            .providers
            .iter()
            .filter(|(_, p)| p.enabled)
            .map(|(n, _)| n.as_str())
            .collect()
    }

    /// Disabled provider names, in stored order (the trailing fold block).
    fn disabled_names(&self) -> Vec<&str> {
        self.config
            .providers
            .iter()
            .filter(|(_, p)| !p.enabled)
            .map(|(n, _)| n.as_str())
            .collect()
    }
    /// Number of enabled providers (the leading block of the folded table).
    pub fn enabled_count(&self) -> usize {
        self.config.providers.values().filter(|p| p.enabled).count()
    }

    /// The providers table folds the trailing disabled block into a single
    /// "…" row until the user moves past it: the fold row itself keeps the
    /// fold, and one more Down expands it. The fold only closes while the
    /// cursor sits at or before the fold row, so a selection placed past it
    /// (e.g. a disabled current provider) stays visible.
    pub fn is_providers_collapsed(&self) -> bool {
        let enabled = self.enabled_count();
        !self.providers.expanded
            && enabled < self.config.providers.len()
            && self
                .providers
                .table_state
                .selected()
                .is_none_or(|s| s <= enabled)
    }

    /// Number of rows actually shown in the providers table (fold-aware).
    pub fn table_row_count(&self) -> usize {
        let enabled = self.enabled_count();
        let total = self.config.providers.len();
        if self.is_providers_collapsed() {
            enabled + usize::from(enabled < total)
        } else {
            total
        }
    }

    /// Total providers in config (including hidden disabled rows). Use
    /// [`App::table_row_count`] for the number of rows actually shown.
    pub fn provider_count(&self) -> usize {
        self.config.providers.len()
    }

    pub fn selected_name(&self) -> Option<&str> {
        self.providers
            .table_state
            .selected()
            .and_then(|i| self.provider_name_at(i))
    }

    pub fn select_next(&mut self) {
        if self.config.providers.is_empty() {
            return;
        }
        let Some(i) = self.providers.table_state.selected() else {
            self.providers.table_state.select(Some(0));
            return;
        };
        let enabled = self.enabled_count();
        // Down on the fold row expands into the disabled block instead of
        // wrapping; the cursor lands on the first disabled row.
        if self.is_providers_collapsed() && i == enabled {
            self.providers.expanded = true;
            return;
        }
        let next = (i + 1) % self.table_row_count();
        self.providers.table_state.select(Some(next));
        // Wrapping back onto the enabled block folds the disabled block again.
        if next < enabled {
            self.providers.expanded = false;
        }
    }

    pub fn select_prev(&mut self) {
        if self.config.providers.is_empty() {
            return;
        }
        let count = self.table_row_count();
        let i = self
            .providers
            .table_state
            .selected()
            .map(|i| if i == 0 { count - 1 } else { i - 1 })
            .unwrap_or(0);
        self.providers.table_state.select(Some(i));
        // Moving back up onto the enabled block folds the disabled block again.
        if i < self.enabled_count() {
            self.providers.expanded = false;
        }
    }
    /// Select a row directly (not via Up/Down) and fold the disabled block;
    /// the fold reopens on its own if the cursor is past it.
    pub(super) fn select_row(&mut self, idx: usize) {
        self.providers.expanded = false;
        self.providers.table_state.select(Some(idx));
    }


    pub fn move_provider_up(&mut self) -> Result<()> {
        let Some(idx) = self.providers.table_state.selected() else {
            return Ok(());
        };
        // Line the stored order up with the display (enabled-first) before
        // reordering, so the row index is also the provider's index.
        self.config.sort_providers_by_enabled();
        if idx == 0 || idx == self.enabled_count() {
            return Ok(()); // top of list, or first disabled row — can't cross the fold boundary
        }
        self.config.providers.move_index(idx, idx - 1);
        self.providers.table_state.select(Some(idx - 1));
        config::save_config(&self.config)?;
        Ok(())
    }

    pub fn move_provider_down(&mut self) -> Result<()> {
        // Line the stored order up with the display (enabled-first) before
        // reordering, so the row index is also the provider's index.
        self.config.sort_providers_by_enabled();
        let Some(idx) = self.providers.table_state.selected() else {
            return Ok(());
        };
        if idx + 1 >= self.provider_count() || idx + 1 == self.enabled_count() {
            return Ok(()); // bottom of list, or last enabled row — can't cross the fold boundary
        }
        self.config.providers.move_index(idx, idx + 1);
        self.providers.table_state.select(Some(idx + 1));
        config::save_config(&self.config)?;
        Ok(())
    }

    pub fn toggle_fallback(&mut self) -> Result<()> {
        self.config.fallback = !self.config.fallback;
        config::save_config(&self.config)?;
        let state = if self.config.fallback { "on" } else { "off" };
        self.set_message(format!("Fallback mode {state}"), super::MessageKind::Info);
        Ok(())
    }

    pub fn push_message_log(&mut self, text: String, kind: MessageKind) {
        const MAX_MESSAGES: usize = 100;
        self.message_log
            .push_back(super::MessageEntry { text, kind });
        if self.message_log.len() > MAX_MESSAGES {
            self.message_log.pop_front();
        }
    }

    pub fn set_message(&mut self, msg: impl Into<String>, kind: MessageKind) {
        let msg = msg.into();
        self.push_message_log(msg.clone(), kind.clone());
        self.message = Some((msg, kind, std::time::Instant::now()));
    }

    /// Clear message if it has expired (after MESSAGE_TIMEOUT_SECS seconds).
    pub fn tick_message(&mut self) -> bool {
        if let Some((_, _, created)) = &self.message
            && created.elapsed() > std::time::Duration::from_secs(MESSAGE_TIMEOUT_SECS)
        {
            self.message = None;
            return true;
        }
        false
    }

    /// Reload configuration from disk.
    pub fn reload_config(&mut self) -> Result<()> {
        match config::load_config() {
            Ok(fresh_config) => {
                // Reload means "take the file as it is" — no reordering, so a
                // hand-edit to the provider order is not undone on refresh.
                self.config = fresh_config;

                if self.config.providers.contains_key(&self.config.current) {
                    let idx = display_rank_of(&self.config, &self.config.current);
                    self.select_row(idx);
                } else if !self.config.providers.is_empty() {
                    self.select_row(0);
                } else {
                    self.providers.table_state.select(None);
                    self.providers.expanded = false;
                }

                self.models.provider_models = self.db.load_provider_models();

                self.set_message("Configuration reloaded", MessageKind::Success);
                Ok(())
            }
            Err(e) => {
                self.set_message(format!("Failed to reload config: {e}"), MessageKind::Error);
                Err(e)
            }
        }
    }
}

/// Display rank of a provider in the fold layout — the row it occupies in the
/// rendered table: enabled providers first (in stored order), disabled ones
/// after. Differs from the stored index only while the stored order is not
/// already enabled-first; the fold renders enabled-first regardless.
pub(super) fn display_rank_of(config: &crate::config::AppConfig, name: &str) -> usize {
    let enabled = config
        .providers
        .iter()
        .filter(|(_, p)| p.enabled)
        .map(|(n, _)| n.as_str())
        .collect::<Vec<_>>();
    if let Some(r) = enabled.iter().position(|n| *n == name) {
        return r;
    }
    config
        .providers
        .iter()
        .filter(|(_, p)| !p.enabled)
        .position(|(n, _)| n == name)
        .map_or(0, |r| enabled.len() + r)
}

#[cfg(test)]
mod tests {
    use super::display_rank_of;
    use crate::config::test_support::ConfigDirGuard;
    use crate::tui::testing::tests::provider;
    use indexmap::IndexMap;

    fn unsorted_config() -> crate::config::AppConfig {
        // Disabled provider first — as if the file was hand-ordered.
        let mut a = provider("id-a");
        a.enabled = false;
        let mut providers = IndexMap::new();
        providers.insert("a".to_string(), a);
        providers.insert("b".to_string(), provider("id-b"));
        crate::config::AppConfig {
            current: "b".into(),
            listen: "127.0.0.1:7896".into(),
            providers,
            fallback: false,
            db_path: None,
            request_log_limit: 100,
        }
    }

    #[test]
    fn new_preserves_stored_order_and_selects_enabled_current() {
        let _guard = ConfigDirGuard::new();
        let cfg = unsorted_config();
        crate::config::save_config(&cfg).unwrap();

        let app = super::super::App::new().unwrap();
        let order: Vec<&str> = app.config.providers.keys().map(|k| k.as_str()).collect();
        assert_eq!(order, vec!["a", "b"]); // stored order untouched on load
        assert!(app.is_providers_collapsed());
        assert_eq!(app.selected_name(), Some("b"));
        assert_eq!(app.table_row_count(), 2);
    }

    #[test]
    fn disabled_current_past_fold_row_keeps_selection_visible() {
        let _guard = ConfigDirGuard::new();
        // Hand-edited config where the current provider is disabled and is not
        // the first in the disabled block, so its row sits past the fold row.
        let mut cfg = unsorted_config();
        cfg.providers.insert("c".to_string(), provider("id-c"));
        cfg.providers.get_mut("c").unwrap().enabled = false;
        cfg.current = "c".into();
        crate::config::save_config(&cfg).unwrap();

        let app = super::super::App::new().unwrap();
        // The fold must not close over the cursor: it would point past the
        // last rendered row and leave the selection invisible.
        assert!(!app.is_providers_collapsed());
        assert_eq!(app.table_row_count(), 3);
        assert_eq!(app.selected_name(), Some("c"));
    }

    #[test]
    fn display_rank_places_enabled_first() {
        let cfg = unsorted_config();
        assert_eq!(display_rank_of(&cfg, "b"), 0);
        assert_eq!(display_rank_of(&cfg, "a"), 1);
    }
}
