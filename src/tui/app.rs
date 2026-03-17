use ratatui::widgets::TableState;

use crate::config::{self, ApiFormat, AppConfig, Provider};
use crate::error::Result;

// UI constants
const MESSAGE_TIMEOUT_SECS: u64 = 3;

#[derive(Debug, Clone, PartialEq)]
pub enum Mode {
    Normal,
    Editing,
    Confirm,
    Message,
}

#[derive(Debug, Clone)]
pub enum MessageKind {
    Info,
    Success,
    Error,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ServerStatus {
    Stopped,
    Starting,
    Running,
    Error(String),
}

pub struct App {
    pub config: AppConfig,
    pub mode: Mode,
    pub table_state: TableState,
    pub provider_ids: Vec<String>,
    pub form: Option<ProviderForm>,
    pub message: Option<(String, MessageKind, std::time::Instant)>,
    pub confirm_action: Option<String>,
    pub should_quit: bool,
    pub server_status: ServerStatus,
}

pub struct ProviderForm {
    pub is_new: bool,
    pub fields: Vec<FormField>,
    pub focused: usize,
    pub error: Option<String>,
}

pub struct FormField {
    pub label: &'static str,
    pub value: String,
    pub cursor: usize,
    pub editable: bool,
    pub is_toggle: bool,
}

impl FormField {
    fn text(label: &'static str, value: &str) -> Self {
        Self {
            label,
            value: value.to_string(),
            cursor: value.len(),
            editable: true,
            is_toggle: false,
        }
    }

    fn toggle(label: &'static str, value: &str) -> Self {
        Self {
            label,
            value: value.to_string(),
            cursor: 0,
            editable: true,
            is_toggle: true,
        }
    }

    pub fn insert(&mut self, c: char) {
        if self.is_toggle {
            return;
        }
        self.value.insert(self.cursor, c);
        self.cursor += 1;
    }

    pub fn backspace(&mut self) {
        if self.is_toggle || self.cursor == 0 {
            return;
        }
        self.cursor -= 1;
        self.value.remove(self.cursor);
    }

    pub fn delete(&mut self) {
        if self.is_toggle || self.cursor >= self.value.len() {
            return;
        }
        self.value.remove(self.cursor);
    }

    pub fn move_left(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
        }
    }

    pub fn move_right(&mut self) {
        if self.cursor < self.value.len() {
            self.cursor += 1;
        }
    }

    pub fn delete_word_back(&mut self) {
        if self.is_toggle || self.cursor == 0 {
            return;
        }
        let mut pos = self.cursor;
        // Skip trailing spaces
        while pos > 0 && self.value[..pos].ends_with(' ') {
            pos -= 1;
        }
        // Delete until next space
        while pos > 0 && !self.value[..pos].ends_with(' ') {
            pos -= 1;
        }
        self.value.drain(pos..self.cursor);
        self.cursor = pos;
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.value.len();
    }

    pub fn toggle_value(&mut self) {
        if !self.is_toggle {
            return;
        }
        self.value = if self.value == "anthropic" {
            "openai".to_string()
        } else {
            "anthropic".to_string()
        };
    }
}

impl App {
    pub fn new() -> Result<Self> {
        let config = config::load_config()?;
        let provider_ids: Vec<String> = config.providers.keys().cloned().collect();

        let mut table_state = TableState::default();
        if !provider_ids.is_empty() {
            // Select current provider if it exists
            let idx = provider_ids
                .iter()
                .position(|id| id == &config.current)
                .unwrap_or(0);
            table_state.select(Some(idx));
        }

        Ok(Self {
            config,
            mode: Mode::Normal,
            table_state,
            provider_ids,
            form: None,
            message: None,
            confirm_action: None,
            should_quit: false,
            server_status: ServerStatus::Stopped,
        })
    }

    pub fn refresh_ids(&mut self) {
        self.provider_ids = self.config.providers.keys().cloned().collect();
    }

    pub fn selected_id(&self) -> Option<&str> {
        self.table_state
            .selected()
            .and_then(|i| self.provider_ids.get(i))
            .map(|s| s.as_str())
    }

    pub fn select_next(&mut self) {
        if self.provider_ids.is_empty() {
            return;
        }
        let i = self
            .table_state
            .selected()
            .map(|i| (i + 1) % self.provider_ids.len())
            .unwrap_or(0);
        self.table_state.select(Some(i));
    }

    pub fn select_prev(&mut self) {
        if self.provider_ids.is_empty() {
            return;
        }
        let i = self
            .table_state
            .selected()
            .map(|i| {
                if i == 0 {
                    self.provider_ids.len() - 1
                } else {
                    i - 1
                }
            })
            .unwrap_or(0);
        self.table_state.select(Some(i));
    }

    pub fn switch_to_selected(&mut self) -> Result<()> {
        if let Some(id) = self.selected_id().map(|s| s.to_string()) {
            self.config.current = id.clone();
            config::save_config(&self.config)?;
        }
        Ok(())
    }

    pub fn start_add(&mut self) {
        self.form = Some(ProviderForm {
            is_new: true,
            fields: vec![
                FormField::text("ID", ""),
                FormField::text("Base URL", ""),
                FormField::text("API Key", ""),
                FormField::toggle("Format", "anthropic"),
            ],
            focused: 0,
            error: None,
        });
        self.mode = Mode::Editing;
    }

    pub fn start_edit(&mut self) {
        let Some(id) = self.selected_id() else {
            return;
        };
        let Some(provider) = self.config.providers.get(id) else {
            return;
        };

        let mut id_field = FormField::text("ID", id);
        id_field.editable = false;

        self.form = Some(ProviderForm {
            is_new: false,
            fields: vec![
                id_field,
                FormField::text("Base URL", &provider.base_url),
                FormField::text("API Key", &provider.api_key),
                FormField::toggle("Format", &provider.api_format.to_string()),
            ],
            focused: 1,
            error: None,
        });
        self.mode = Mode::Editing;
    }

    pub fn save_form(&mut self) -> Result<()> {
        let Some(form) = &self.form else {
            return Ok(());
        };

        let id = form.fields[0].value.trim().to_string();
        let base_url = form.fields[1].value.trim().trim_end_matches('/').to_string();
        let api_key = form.fields[2].value.trim().to_string();
        let format_str = form.fields[3].value.trim().to_string();
        let is_new = form.is_new;

        let validation_error = if id.is_empty() {
            Some("ID cannot be empty".to_string())
        } else if base_url.is_empty() {
            Some("Base URL cannot be empty".to_string())
        } else if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
            Some("Base URL must start with http:// or https://".to_string())
        } else if is_new && self.config.providers.contains_key(&id) {
            Some(format!("Provider '{id}' already exists"))
        } else {
            None
        };
        if let Some(err) = validation_error {
            self.form.as_mut().unwrap().error = Some(err);
            return Ok(());
        }

        let api_format = if format_str == "openai" {
            ApiFormat::OpenAI
        } else {
            ApiFormat::Anthropic
        };

        let provider = Provider {
            base_url,
            api_key,
            api_format,
            model_map: if is_new {
                std::collections::HashMap::new()
            } else {
                self.config
                    .providers
                    .get(&id)
                    .map(|p| p.model_map.clone())
                    .unwrap_or_default()
            },
        };

        let is_first = self.config.providers.is_empty();
        self.config.providers.insert(id.clone(), provider);
        if is_first {
            self.config.current = id.clone();
        }

        config::save_config(&self.config)?;
        self.refresh_ids();
        self.mode = Mode::Normal;
        self.form = None;


        Ok(())
    }

    pub fn confirm_delete(&mut self) {
        if let Some(id) = self.selected_id() {
            self.confirm_action = Some(id.to_string());
            self.mode = Mode::Confirm;
        }
    }

    pub fn delete_confirmed(&mut self) -> Result<()> {
        if let Some(id) = self.confirm_action.take() {
            self.config.providers.shift_remove(&id);
            if self.config.current == id {
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

            // Fix selection
            if let Some(selected) = self.table_state.selected() {
                if selected >= self.provider_ids.len() && !self.provider_ids.is_empty() {
                    self.table_state.select(Some(self.provider_ids.len() - 1));
                } else if self.provider_ids.is_empty() {
                    self.table_state.select(None);
                }
            }

            self.set_message(format!("Deleted '{id}'"), MessageKind::Success);
        }
        self.mode = Mode::Normal;
        Ok(())
    }

    pub fn set_message(&mut self, msg: impl Into<String>, kind: MessageKind) {
        self.message = Some((msg.into(), kind, std::time::Instant::now()));
    }

    pub fn show_message(&mut self, msg: String, kind: MessageKind) {
        self.set_message(msg, kind);
        self.mode = Mode::Message;
    }

    /// Clear message if it has expired (after 3 seconds).
    pub fn tick_message(&mut self) {
        if let Some((_, _, created)) = &self.message {
            if created.elapsed() > std::time::Duration::from_secs(MESSAGE_TIMEOUT_SECS) {
                self.message = None;
            }
        }
    }

    pub fn move_provider_up(&mut self) -> Result<()> {
        let Some(idx) = self.table_state.selected() else { return Ok(()); };
        // Can't move past index 0, and index 1 can't move up past pinned "anthropic"
        if idx <= 1 { return Ok(()); }
        self.config.providers.move_index(idx, idx - 1);
        self.refresh_ids();
        self.table_state.select(Some(idx - 1));
        config::save_config(&self.config)?;
        Ok(())
    }

    pub fn move_provider_down(&mut self) -> Result<()> {
        let Some(idx) = self.table_state.selected() else { return Ok(()); };
        if idx + 1 >= self.provider_ids.len() { return Ok(()); }
        // "anthropic" is pinned at position 0 and cannot move
        if self.provider_ids.get(idx).map(|s| s == "anthropic").unwrap_or(false) {
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

    /// Reload configuration from disk.
    pub fn reload_config(&mut self) -> Result<()> {
        match config::load_config() {
            Ok(fresh_config) => {
                self.config = fresh_config;
                self.refresh_ids();

                // Reselect current provider if it exists
                if let Some(idx) = self.provider_ids.iter().position(|id| id == &self.config.current) {
                    self.table_state.select(Some(idx));
                } else if !self.provider_ids.is_empty() {
                    self.table_state.select(Some(0));
                } else {
                    self.table_state.select(None);
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
