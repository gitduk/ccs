use std::sync::Arc;
use std::sync::Mutex;

use crate::config;
use crate::error::Result;
use crate::repo::Repository;

use super::{App, MESSAGE_TIMEOUT_SECS, MessageKind, ModelsState, ProviderList};

impl App {
    pub fn new() -> Result<Self> {
        use ratatui::widgets::TableState;

        let config = config::load_config()?;
        let names: Vec<String> = config.providers.keys().cloned().collect();

        let mut table_state = TableState::default();
        if !names.is_empty() {
            let idx = names
                .iter()
                .position(|name| name == &config.current)
                .unwrap_or(0);
            table_state.select(Some(idx));
        }

        let db = Repository::open(&config.resolve_db_path());
        if let Err(e) = db.migrate(&config.name_to_id_map()) {
            tracing::warn!("DB schema migration failed: {e}");
        }

        let (metrics_data, provider_models) = db.load_all();
        let metrics = Arc::new(Mutex::new(metrics_data));

        let bg_proxy_pid = super::bg_proxy::load_bg_proxy_pid();
        Ok(Self {
            config,
            mode: super::Mode::Normal,
            providers: ProviderList { table_state, names },
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
            pending_key: None,
        })
    }

    pub fn refresh_ids(&mut self) {
        self.providers.names = self.config.providers.keys().cloned().collect();
    }

    pub fn selected_name(&self) -> Option<&str> {
        self.providers
            .table_state
            .selected()
            .and_then(|i| self.providers.names.get(i))
            .map(|s| s.as_str())
    }

    pub fn select_next(&mut self) {
        if self.providers.names.is_empty() {
            return;
        }
        let i = self
            .providers
            .table_state
            .selected()
            .map(|i| (i + 1) % self.providers.names.len())
            .unwrap_or(0);
        self.providers.table_state.select(Some(i));
    }

    pub fn select_prev(&mut self) {
        if self.providers.names.is_empty() {
            return;
        }
        let i = self
            .providers
            .table_state
            .selected()
            .map(|i| {
                if i == 0 {
                    self.providers.names.len() - 1
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
        self.refresh_ids();
        self.providers.table_state.select(Some(idx - 1));
        config::save_config(&self.config)?;
        Ok(())
    }

    pub fn move_provider_down(&mut self) -> Result<()> {
        let Some(idx) = self.providers.table_state.selected() else {
            return Ok(());
        };
        if idx + 1 >= self.providers.names.len() {
            return Ok(());
        }
        self.config.providers.move_index(idx, idx + 1);
        self.refresh_ids();
        self.providers.table_state.select(Some(idx + 1));
        config::save_config(&self.config)?;
        Ok(())
    }

    pub fn toggle_fallback(&mut self) -> Result<()> {
        self.config.fallback = !self.config.fallback;
        config::save_config(&self.config)?;
        Ok(())
    }

    pub fn set_message(&mut self, msg: impl Into<String>, kind: MessageKind) {
        self.message = Some((msg.into(), kind, std::time::Instant::now()));
    }

    /// Clear message if it has expired (after MESSAGE_TIMEOUT_SECS seconds).
    pub fn tick_message(&mut self) {
        if let Some((_, _, created)) = &self.message
            && created.elapsed() > std::time::Duration::from_secs(MESSAGE_TIMEOUT_SECS)
        {
            self.message = None;
        }
    }

    /// Reload configuration from disk.
    pub fn reload_config(&mut self) -> Result<()> {
        match config::load_config() {
            Ok(fresh_config) => {
                self.config = fresh_config;
                self.refresh_ids();

                if let Some(idx) = self
                    .providers
                    .names
                    .iter()
                    .position(|name| name == &self.config.current)
                {
                    self.providers.table_state.select(Some(idx));
                } else if !self.providers.names.is_empty() {
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
