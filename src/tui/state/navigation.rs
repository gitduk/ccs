use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use crate::config;
use crate::error::Result;

use super::{App, MESSAGE_TIMEOUT_SECS, MessageKind};

impl App {
    pub fn new() -> Result<Self> {
        use ratatui::widgets::TableState;
        use std::collections::HashSet;
        use std::sync::mpsc;

        let config = config::load_config()?;
        let provider_names: Vec<String> = config.providers.keys().cloned().collect();

        let mut table_state = TableState::default();
        if !provider_names.is_empty() {
            let idx = provider_names
                .iter()
                .position(|name| name == &config.current)
                .unwrap_or(0);
            table_state.select(Some(idx));
        }

        let db = crate::db::open_with_fallback(&config.resolve_db_path());
        crate::db::migrate_schema(&db, &config.name_to_id_map());

        let (metrics, provider_models) = {
            let conn = db.lock().unwrap();
            (
                Arc::new(Mutex::new(crate::db::load_metrics(&conn))),
                crate::db::load_provider_models(&conn),
            )
        };

        let bg_proxy_pid = super::bg_proxy::load_bg_proxy_pid();
        let (test_tx, test_rx) = mpsc::channel();
        Ok(Self {
            config,
            mode: super::Mode::Normal,
            table_state,
            provider_names,
            form: None,
            message: None,
            confirm_action: None,
            should_quit: false,
            server_status: super::ServerStatus::Stopped,
            metrics,
            test_results: HashMap::new(),
            pending_tests: HashSet::new(),
            test_tx,
            test_rx,
            db,
            bg_proxy_pid,
            provider_models,
            test_client: reqwest::Client::new(),
            pending_key: None,
            models_search_field: super::FormField::text("", ""),
            models_insert: true,
            models_selected: 0,
            models_scroll: 0,
        })
    }

    pub fn refresh_ids(&mut self) {
        self.provider_names = self.config.providers.keys().cloned().collect();
    }

    pub fn selected_name(&self) -> Option<&str> {
        self.table_state
            .selected()
            .and_then(|i| self.provider_names.get(i))
            .map(|s| s.as_str())
    }

    pub fn select_next(&mut self) {
        if self.provider_names.is_empty() {
            return;
        }
        let i = self
            .table_state
            .selected()
            .map(|i| (i + 1) % self.provider_names.len())
            .unwrap_or(0);
        self.table_state.select(Some(i));
    }

    pub fn select_prev(&mut self) {
        if self.provider_names.is_empty() {
            return;
        }
        let i = self
            .table_state
            .selected()
            .map(|i| {
                if i == 0 {
                    self.provider_names.len() - 1
                } else {
                    i - 1
                }
            })
            .unwrap_or(0);
        self.table_state.select(Some(i));
    }

    pub fn move_provider_up(&mut self) -> Result<()> {
        let Some(idx) = self.table_state.selected() else {
            return Ok(());
        };
        if idx == 0 {
            return Ok(());
        }
        self.config.providers.move_index(idx, idx - 1);
        self.refresh_ids();
        self.table_state.select(Some(idx - 1));
        config::save_config(&self.config)?;
        Ok(())
    }

    pub fn move_provider_down(&mut self) -> Result<()> {
        let Some(idx) = self.table_state.selected() else {
            return Ok(());
        };
        if idx + 1 >= self.provider_names.len() {
            return Ok(());
        }
        self.config.providers.move_index(idx, idx + 1);
        self.refresh_ids();
        self.table_state.select(Some(idx + 1));
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

    /// Drain completed background test results into test_results.
    pub fn drain_test_results(&mut self) {
        while let Ok((name, result)) = self.test_rx.try_recv() {
            self.pending_tests.remove(&name);
            if let Some(models) = &result.model_names {
                if let Ok(conn) = self.db.lock() {
                    let id = self
                        .config
                        .providers
                        .get(&name)
                        .map(|p| p.id.as_str())
                        .unwrap_or(&name);
                    let _ = crate::db::upsert_provider_models(&conn, id, &name, models);
                }
                self.provider_models.insert(name.clone(), models.clone());
            }
            self.test_results.insert(name, result);
        }
    }

    /// Reload configuration from disk.
    pub fn reload_config(&mut self) -> Result<()> {
        match config::load_config() {
            Ok(fresh_config) => {
                self.config = fresh_config;
                self.refresh_ids();

                if let Some(idx) = self
                    .provider_names
                    .iter()
                    .position(|name| name == &self.config.current)
                {
                    self.table_state.select(Some(idx));
                } else if !self.provider_names.is_empty() {
                    self.table_state.select(Some(0));
                } else {
                    self.table_state.select(None);
                }

                if let Ok(conn) = self.db.lock() {
                    self.provider_models = crate::db::load_provider_models(&conn);
                }

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
