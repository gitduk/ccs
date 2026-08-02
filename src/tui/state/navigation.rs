use std::sync::Arc;
use std::sync::Mutex;

use crate::config;
use crate::error::Result;
use crate::repo::Repository;

use super::{App, LogsState, MESSAGE_TIMEOUT_SECS, MessageKind, ModelsState, ProviderList};

impl App {
    pub fn new() -> Result<Self> {
        use ratatui::widgets::TableState;

        let config = config::load_config()?;

        let mut table_state = TableState::default();
        if !config.providers.is_empty() {
            let idx = config.providers.get_index_of(&config.current).unwrap_or(0);
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

        let detail_line_count = crate::tui::ui::format::DETAIL_HEIGHT;

        Ok(Self {
            config,
            mode: super::Mode::Normal,
            terminal_focused: true,
            providers: ProviderList { table_state },
            form: None,
            message: None,
            confirm_action: None,
            should_quit: false,
            server_status: super::ServerStatus::Stopped,
            metrics,
            tests: super::TestState::new(),
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
            port_form: None,
            detail_line_count,
            sysinfo_sampler: crate::tui::sysinfo::SysInfoSampler::new(),
            config_needs_sync: false,
        })
    }

    /// Provider name at the given table row (config order).
    pub fn provider_name_at(&self, idx: usize) -> Option<&str> {
        self.config
            .providers
            .get_index(idx)
            .map(|(name, _)| name.as_str())
    }

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
        let i = self
            .providers
            .table_state
            .selected()
            .map(|i| (i + 1) % self.provider_count())
            .unwrap_or(0);
        self.providers.table_state.select(Some(i));
    }

    pub fn select_prev(&mut self) {
        if self.config.providers.is_empty() {
            return;
        }
        let i = self
            .providers
            .table_state
            .selected()
            .map(|i| {
                if i == 0 {
                    self.provider_count() - 1
                } else {
                    i - 1
                }
            })
            .unwrap_or(0);
        self.providers.table_state.select(Some(i));
    }

    pub fn move_provider_up(&mut self) -> Result<()> {
        let Some(idx) = self.providers.table_state.selected() else {
            return Ok(());
        };
        if idx == 0 {
            return Ok(());
        }
        self.config.providers.move_index(idx, idx - 1);
        self.providers.table_state.select(Some(idx - 1));
        config::save_config(&self.config)?;
        Ok(())
    }

    pub fn move_provider_down(&mut self) -> Result<()> {
        let Some(idx) = self.providers.table_state.selected() else {
            return Ok(());
        };
        if idx + 1 >= self.provider_count() {
            return Ok(());
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
                self.config = fresh_config;

                if let Some(idx) = self.config.providers.get_index_of(&self.config.current) {
                    self.providers.table_state.select(Some(idx));
                } else if !self.config.providers.is_empty() {
                    self.providers.table_state.select(Some(0));
                } else {
                    self.providers.table_state.select(None);
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
