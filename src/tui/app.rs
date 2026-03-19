use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};


use ratatui::widgets::TableState;

use crate::config::{self, ApiFormat, AppConfig, Provider};
use crate::db::SharedDb;
use crate::error::Result;
use crate::proxy::metrics::{SharedMetrics, TokenMetrics};
use crate::test_provider::TestResult;

// UI constants
const MESSAGE_TIMEOUT_SECS: u64 = 3;

#[derive(Debug, Clone, PartialEq)]
pub enum Mode {
    Normal,
    Editing,
    Confirm,
    Help,
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

#[derive(Debug, Clone)]
pub enum ConfirmAction {
    Delete(String),
    Clear,
    Quit,
}

pub struct App {
    pub config: AppConfig,
    pub mode: Mode,
    pub table_state: TableState,
    pub provider_names: Vec<String>,
    pub form: Option<ProviderForm>,
    pub message: Option<(String, MessageKind, std::time::Instant)>,
    pub confirm_action: Option<ConfirmAction>,
    pub should_quit: bool,
    pub server_status: ServerStatus,
    pub metrics: SharedMetrics,
    pub test_results: HashMap<String, TestResult>,
    pub pending_tests: HashSet<String>,
    pub test_tx: mpsc::Sender<(String, TestResult)>,
    test_rx: mpsc::Receiver<(String, TestResult)>,
    pub db: SharedDb,
    pub bg_proxy_pid: Option<u32>,
    /// Model names per provider, loaded from DB and updated on test.
    pub provider_models: HashMap<String, Vec<String>>,
    /// Shared HTTP client for provider connectivity tests (reuses connection pool).
    pub test_client: reqwest::Client,
}

pub struct ProviderForm {
    pub is_new: bool,
    /// Original name before editing — used to detect renames.
    pub original_name: Option<String>,
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
        self.cursor += c.len_utf8();
    }

    pub fn backspace(&mut self) {
        if self.is_toggle || self.cursor == 0 {
            return;
        }
        let char_len = self.value[..self.cursor]
            .chars().next_back()
            .map(|c| c.len_utf8()).unwrap_or(1);
        self.cursor -= char_len;
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
            let char_len = self.value[..self.cursor]
                .chars().next_back()
                .map(|c| c.len_utf8()).unwrap_or(1);
            self.cursor -= char_len;
        }
    }

    pub fn move_right(&mut self) {
        if self.cursor < self.value.len() {
            let char_len = self.value[self.cursor..]
                .chars().next()
                .map(|c| c.len_utf8()).unwrap_or(1);
            self.cursor += char_len;
        }
    }

    pub fn delete_word_back(&mut self) {
        if self.is_toggle || self.cursor == 0 {
            return;
        }
        let mut pos = self.cursor;
        // Skip trailing spaces (step back by char boundary)
        while pos > 0 {
            // SAFETY: pos is always maintained as a valid UTF-8 char boundary
            // by insert/backspace/move_left; next_back() on a non-empty slice
            // is always Some, but we use expect for a clear panic message.
            let c = self.value[..pos].chars().next_back()
                .expect("pos is a valid UTF-8 char boundary");
            if c != ' ' { break; }
            pos -= c.len_utf8();
        }
        // Delete until next space
        while pos > 0 {
            let c = self.value[..pos].chars().next_back()
                .expect("pos is a valid UTF-8 char boundary");
            if c == ' ' { break; }
            pos -= c.len_utf8();
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

impl ProviderForm {
    /// Move focus to the next editable field (wraps around).
    pub fn focus_next(&mut self) {
        let len = self.fields.len();
        for offset in 1..len {
            let next = (self.focused + offset) % len;
            if self.fields[next].editable {
                self.focused = next;
                break;
            }
        }
    }

    /// Move focus to the previous editable field (wraps around).
    pub fn focus_prev(&mut self) {
        let len = self.fields.len();
        for offset in 1..len {
            let prev = (self.focused + len - offset) % len;
            if self.fields[prev].editable {
                self.focused = prev;
                break;
            }
        }
    }
}

impl App {
    pub fn new() -> Result<Self> {
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

        let bg_proxy_pid = load_bg_proxy_pid();
        let (test_tx, test_rx) = mpsc::channel();
        Ok(Self {
            config,
            mode: Mode::Normal,
            table_state,
            provider_names,
            form: None,
            message: None,
            confirm_action: None,
            should_quit: false,
            server_status: ServerStatus::Stopped,
            metrics,
            test_results: HashMap::new(),
            pending_tests: HashSet::new(),
            test_tx,
            test_rx,
            db,
            bg_proxy_pid,
            provider_models,
            test_client: reqwest::Client::new(),
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

    pub fn switch_to_selected(&mut self) -> Result<()> {
        if let Some(name) = self.selected_name().map(|s| s.to_string()) {
            self.config.current = name.clone();
            config::save_config(&self.config)?;
        }
        Ok(())
    }

    pub fn start_add(&mut self) {
        self.form = Some(ProviderForm {
            is_new: true,
            original_name: None,
            fields: vec![
                FormField::text("Name", ""),
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
        let Some(name) = self.selected_name() else {
            return;
        };
        let Some(provider) = self.config.providers.get(name) else {
            return;
        };

        self.form = Some(ProviderForm {
            is_new: false,
            original_name: Some(name.to_string()),
            fields: vec![
                FormField::text("Name", name),
                FormField::text("Base URL", &provider.base_url),
                FormField::text("API Key", &provider.api_key),
                FormField::toggle("Format", &provider.api_format.to_string()),
            ],
            focused: 0,
            error: None,
        });
        self.mode = Mode::Editing;
    }

    pub fn save_form(&mut self) -> Result<()> {
        let Some(form) = &self.form else {
            return Ok(());
        };

        let new_name = form.fields[0].value.trim().to_string();
        let base_url = form.fields[1].value.trim().trim_end_matches('/').to_string();
        let api_key = form.fields[2].value.trim().to_string();
        let format_str = form.fields[3].value.trim().to_string();
        let is_new = form.is_new;
        let original_name = form.original_name.clone();

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
            self.form.as_mut().unwrap().error = Some(err);
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

        let provider = Provider {
            id: provider_id.clone(),
            base_url,
            api_key,
            api_format,
            model_map,
        };

        if is_rename {
            let old_name = original_name.as_deref().unwrap();
            // Rebuild IndexMap preserving insertion order, swapping the key in one pass.
            self.config.providers = std::mem::take(&mut self.config.providers)
                .into_iter()
                .map(|(k, v)| if k == old_name { (new_name.clone(), provider.clone()) } else { (k, v) })
                .collect();

            if self.config.current == old_name {
                self.config.current = new_name.clone();
            }

            // Update DB: rename provider_name in all rows.
            if let Ok(conn) = self.db.lock() {
                let _ = crate::db::rename_provider(&conn, &provider_id, &new_name);
            }

            // Update in-memory metrics key.
            if let Ok(mut m) = self.metrics.lock() {
                if let Some(stats) = m.by_provider.remove(old_name) {
                    m.by_provider.insert(new_name.clone(), stats);
                }
            }

            // Update provider_models, test_results, pending_tests keys.
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
        if is_new {
            if let Some(idx) = self.provider_names.iter().position(|s| s == &new_name) {
                self.table_state.select(Some(idx));
            }
        }
        self.mode = Mode::Normal;
        self.form = None;

        Ok(())
    }

    pub fn confirm(&mut self, action: ConfirmAction) {
        self.confirm_action = Some(action);
        self.mode = Mode::Confirm;
    }

    pub fn clear_metrics(&mut self) {
        // Acquire both locks together so the DB and in-memory state are cleared
        // atomically from the perspective of any concurrent reader: no other
        // thread holds both locks simultaneously, so lock ordering (db first,
        // then metrics) is deadlock-free.
        let Ok(conn) = self.db.lock() else { return };
        let Ok(mut m) = self.metrics.lock() else { return };
        let _ = crate::db::clear_all(&conn);
        *m = TokenMetrics::new();
        drop(conn);
        drop(m);
        self.provider_models.clear();
        self.set_message("Usage data cleared", MessageKind::Success);
    }

    pub fn confirm_action_execute(&mut self) -> Result<()> {
        match self.confirm_action.take() {
            Some(ConfirmAction::Clear) => {
                self.clear_metrics();
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

    fn do_delete(&mut self, name: &str) -> Result<()> {
        let removed = self.config.providers.shift_remove(name);
        if let Ok(conn) = self.db.lock() {
            let id = removed.as_ref().map(|p| p.id.as_str()).unwrap_or(name);
            let _ = crate::db::delete_provider(&conn, id);
        }
        if let Ok(mut m) = self.metrics.lock() {
            m.by_provider.remove(name);
        }
        self.provider_models.remove(name);
        if self.config.current == name {
            self.config.current = self.config.providers.keys().next().cloned().unwrap_or_default();
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

    pub fn set_message(&mut self, msg: impl Into<String>, kind: MessageKind) {
        self.message = Some((msg.into(), kind, std::time::Instant::now()));
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
        if idx == 0 { return Ok(()); }
        self.config.providers.move_index(idx, idx - 1);
        self.refresh_ids();
        self.table_state.select(Some(idx - 1));
        config::save_config(&self.config)?;
        Ok(())
    }

    pub fn move_provider_down(&mut self) -> Result<()> {
        let Some(idx) = self.table_state.selected() else { return Ok(()); };
        if idx + 1 >= self.provider_names.len() { return Ok(()); }
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

    /// Drain completed background test results into test_results.
    pub fn drain_test_results(&mut self) {
        while let Ok((name, result)) = self.test_rx.try_recv() {
            self.pending_tests.remove(&name);
            if let Some(models) = &result.model_names {
                if let Ok(conn) = self.db.lock() {
                    let id = self.config.providers.get(&name).map(|p| p.id.as_str()).unwrap_or(&name);
                    let _ = crate::db::upsert_provider_models(&conn, id, &name, models);
                }
                self.provider_models.insert(name.clone(), models.clone());
            }
            self.test_results.insert(name, result);
        }
    }

    /// Spawn a detached background `ccs serve` process, writing its PID to ~/.ccs/proxy.pid.
    pub fn spawn_bg_proxy(&mut self) -> Result<()> {
        let exe = std::env::current_exe()?;
        let child = std::process::Command::new(&exe)
            .arg("serve")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        let pid = child.id();
        // Drop handle — child is detached; it will be reparented to init when TUI exits.
        drop(child);
        if let Some(path) = pid_file_path() {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&path, pid.to_string());
        }
        self.bg_proxy_pid = Some(pid);
        Ok(())
    }

    /// Kill the background proxy process and remove the PID file.
    pub fn stop_bg_proxy(&mut self) {
        if let Some(pid) = self.bg_proxy_pid.take() {
            kill_process(pid);
        }
        self.remove_pid_file();
    }

    /// Called when the background proxy is found to have exited on its own.
    /// Clears the in-memory PID and removes the PID file (no kill needed).
    pub fn on_bg_proxy_died(&mut self) {
        self.bg_proxy_pid = None;
        self.remove_pid_file();
    }

    fn remove_pid_file(&self) {
        if let Some(path) = pid_file_path() {
            remove_pid_file_at(&path);
        }
    }

    /// Reload configuration from disk.
    pub fn reload_config(&mut self) -> Result<()> {
        match config::load_config() {
            Ok(fresh_config) => {
                self.config = fresh_config;
                self.refresh_ids();

                // Reselect current provider if it exists
                if let Some(idx) = self.provider_names.iter().position(|name| name == &self.config.current) {
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

// ── Background proxy helpers ──────────────────────────────────────────────────

pub fn pid_file_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".ccs").join("proxy.pid"))
}

/// Read PID file and return the PID if the process is still alive.
pub fn load_bg_proxy_pid() -> Option<u32> {
    let path = pid_file_path()?;
    let content = std::fs::read_to_string(&path).ok()?;
    let pid: u32 = content.trim().parse().ok()?;
    if is_process_alive(pid) {
        Some(pid)
    } else {
        remove_pid_file_at(&path);
        None
    }
}

fn remove_pid_file_at(path: &std::path::Path) {
    if let Err(e) = std::fs::remove_file(path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!("Could not remove PID file {}: {e}", path.display());
        }
    }
}

/// Check whether a process with the given PID is alive AND is a `ccs` process.
pub fn is_process_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        // Verify comm name to guard against PID reuse (comm truncated to 15 chars).
        if std::fs::metadata(format!("/proc/{pid}")).is_err() {
            return false;
        }
        std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .map(|comm| comm.trim().starts_with("ccs"))
            .unwrap_or(false)
    }
    #[cfg(not(target_os = "linux"))]
    {
        // On non-Linux platforms use `kill -0` (no-op signal, just checks existence).
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

/// Send SIGTERM to a process (uses system `kill` command to avoid libc dep).
pub fn kill_process(pid: u32) {
    let _ = std::process::Command::new("kill")
        .arg(pid.to_string())
        .status();
}
