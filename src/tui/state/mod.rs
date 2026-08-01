use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::mpsc;
use std::time::Duration;

// Fixed field order inside `ProviderForm::fields`.
pub(super) const NAME_FIELD_IDX: usize = 0;
pub(super) const BASE_URL_FIELD_IDX: usize = 1;
pub(super) const API_KEY_FIELD_IDX: usize = 2;
/// Pinned model for the Test/Retest action and (on add) format
/// auto-detection, persisted as `Provider::test_model`.
pub(super) const TEST_MODEL_FIELD_IDX: usize = 3;
pub(super) const FALLBACK_FIELD_IDX: usize = 4;
pub(super) const PORT_FIELD_IDX: usize = 5;

use ratatui::widgets::TableState;

use crate::config::{AppConfig, Provider, RouteRule};
use crate::metrics::{SharedMetrics, SharedRequestLog};
use crate::repo::Repository;
use crate::tester::TestResult;

// UI constants
pub(super) const MESSAGE_TIMEOUT_SECS: u64 = 3;

mod actions;
mod bg_proxy;
mod filter;
mod metrics_sync;
mod navigation;

pub use bg_proxy::{is_process_alive, send_sighup};
pub use filter::filter_suggestions;

/// Snapshot of DB-backed metrics loaded off the hot path for non-blocking TUI redraws.
pub(crate) struct MetricsSnapshot {
    pub metrics: crate::metrics::TokenMetrics,
    pub provider_models: std::collections::HashMap<String, Vec<String>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Mode {
    Normal,
    Editing,
    Confirm,
    Help,
    /// Models browser popup: search and browse all available models.
    Models,
    /// Request log viewer: scrollable list of recent proxy requests.
    Logs,
    /// Quota configuration editor for the selected provider.
    QuotaConfig,
}

/// Vim-style sub-mode used inside the provider editor form.
#[derive(Debug, Clone, PartialEq)]
pub enum VimMode {
    /// Navigation / command mode (default on form open).
    Normal,
    /// Text-input mode, entered with `i` / `a`.
    Insert,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MessageKind {
    Info,
    Success,
    Error,
}

#[derive(Debug, Clone)]
pub struct MessageEntry {
    pub text: String,
    pub kind: MessageKind,
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
    ClearCurrent,
    Quit,
}

/// Provider list navigation state (main table).
pub struct ProviderList {
    pub table_state: TableState,
}

/// Background test state: channels, results, and the shared HTTP client.
/// Events sent from background test tasks back to the TUI.
pub(super) enum TestEvent {
    /// The task has selected a model and is about to test it.
    ModelSelected { provider: String, model: String },
    /// The task completed (success, auth failure, or all retries exhausted).
    Completed {
        provider: String,
        result: TestResult,
    },
    /// Quota query completed.
    QuotaCompleted {
        provider_id: String,
        result: Result<QuotaResult, String>,
    },
    /// Model list fetched silently (no test run, no stats recorded).
    ModelsOnly {
        provider: String,
        models: Vec<String>,
    },
    /// API format auto-detection finished for a new provider being added.
    /// Carries every already-validated field needed to finish constructing
    /// and inserting the `Provider`, since it doesn't exist in config yet.
    FormatDetected {
        /// Matched against `ProviderForm::detect_token` to discard stale results.
        token: String,
        name: String,
        base_url: String,
        api_key: String,
        fallback: bool,
        port: Option<u16>,
        routes: Vec<RouteRule>,
        detected: Result<crate::tester::DetectedFormat, String>,
        /// Pinned Test/Retest model, persisted onto the new `Provider`. When
        /// `Some`, detection used it instead of a full `/v1/models` fetch, so
        /// `detected`'s model list is just that one entry and the full
        /// catalog still needs fetching.
        test_model: Option<String>,
    },
}

pub struct TestState {
    pub results: HashMap<String, TestResult>,
    pub pending: HashSet<String>,
    /// Model currently under test for each pending provider.
    pub testing_model: HashMap<String, String>,
    pub tx: mpsc::Sender<TestEvent>,
    rx: mpsc::Receiver<TestEvent>,
    pub client: reqwest::Client,
}

impl TestState {
    pub(super) fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            results: HashMap::new(),
            pending: HashSet::new(),
            testing_model: HashMap::new(),
            tx,
            rx,
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(10))
                .build()
                .expect("Failed to build test client"),
        }
    }

    /// Drain all pending results off the channel in one pass.
    pub(super) fn drain(&mut self) -> impl Iterator<Item = TestEvent> + '_ {
        std::iter::from_fn(|| self.rx.try_recv().ok())
    }
}

/// Models browser popup state.
pub struct ModelsState {
    /// Model names per provider, loaded from DB and updated on test.
    pub provider_models: HashMap<String, Vec<String>>,
    /// Search field (value + cursor).
    pub search_field: FormField,
    /// True = search box focused (Insert mode); false = list navigation (Normal mode).
    pub search_active: bool,
    /// Index of the highlighted model in the flat filtered list.
    pub selected: usize,
    /// Scroll offset (rows) for the models list.
    pub scroll: u16,
    /// Pending first key of a two-key sequence inside the Models panel.
    pub pending_key: Option<(char, std::time::Instant)>,
}

/// Request log viewer state.
pub struct LogsState {
    /// Index of the highlighted entry (0 = most recent).
    pub selected: usize,
    /// Scroll offset (rows) for the log list.
    pub scroll: u16,
    /// Scroll offset (rows) for the detail panel.
    pub detail_scroll: u16,
    /// Last rendered detail viewport height, used for half-page scrolling.
    pub detail_view_height: u16,
    /// Pending first key of a two-key sequence inside the logs panel.
    pub pending_key: Option<(char, std::time::Instant)>,
}

/// Command execution result shown in the Quota column.
#[derive(Debug, Clone)]
pub struct QuotaResult {
    pub output: String,
}

/// Quota status for a provider.
#[derive(Debug, Clone)]
pub enum QuotaStatus {
    Running,
    Success(QuotaResult),
    Error(String),
}

pub struct App {
    pub config: AppConfig,
    pub mode: Mode,
    /// Whether the terminal pane/window is currently focused.
    pub terminal_focused: bool,
    pub providers: ProviderList,
    pub form: Option<ProviderForm>,
    pub message: Option<(String, MessageKind, std::time::Instant)>,
    pub confirm_action: Option<ConfirmAction>,
    pub should_quit: bool,
    pub server_status: ServerStatus,
    pub metrics: SharedMetrics,
    pub tests: TestState,
    pub db: Repository,
    pub bg_proxy_pid: Option<u32>,
    pub models: ModelsState,
    pub request_log: SharedRequestLog,
    pub logs: LogsState,
    /// Chronological message log shown in the Messages panel (newest last).
    pub message_log: VecDeque<MessageEntry>,
    /// Tracks last-seen error key per provider to avoid duplicate log entries.
    pub seen_provider_errors: HashMap<String, String>,
    /// Pending first key of a two-key sequence in the Normal-mode provider list.
    pub pending_key: Option<(char, std::time::Instant)>,
    /// Quota status for each provider (provider_id -> status).
    pub quota_status: HashMap<String, QuotaStatus>,
    /// Quota configuration form for the selected provider.
    pub quota_form: Option<QuotaForm>,
    /// Line count from the last rendered detail panel, used to size the panel next frame.
    pub detail_line_count: u16,
    pub(super) sysinfo_sampler: crate::tui::sysinfo::SysInfoSampler,
    /// Set when a background task (currently: format auto-detection landing
    /// a new provider) mutated `config` outside the normal input-handling
    /// path, where callers already call `sync_proxy_config` themselves.
    /// Consumed (and cleared) by the main loop after draining test results.
    pub(super) config_needs_sync: bool,
}

// ─── Provider editor form ─────────────────────────────────────────────────────

pub struct ProviderForm {
    /// Original name before editing — `None` means this is a new provider.
    pub original_name: Option<String>,
    pub fields: Vec<FormField>,
    /// Focused slot index: 0..fields.len()-1 = a regular field;
    /// fields.len() = the Routes section.
    pub focused: usize,
    /// Current Vim sub-mode for the form.
    pub vim_mode: VimMode,

    // ── Route rules ──
    /// Working copy of the provider's route rules.
    pub routes: Vec<RouteRule>,
    /// Which route is highlighted when the Routes section has focus.
    pub route_cursor: usize,
    /// True while a route's pattern field is being edited (Insert sub-mode).
    pub route_editing: bool,
    /// FormField tracking the currently edited route pattern.
    pub route_pat_field: FormField,
    /// True while editing the target field; false = editing pattern field.
    pub route_edit_target: bool,
    /// FormField tracking the currently edited route target.
    pub route_tgt_field: FormField,
    /// True when keyboard navigation focus is inside the suggestion list.
    pub route_suggest_active: bool,
    /// Currently highlighted index inside the filtered suggestion list (global, not viewport-relative).
    pub route_suggest_idx: usize,
    /// First visible suggestion index; keeps the highlighted item within the 8-row viewport.
    pub route_suggest_scroll: usize,

    /// Pending first key of a two-key sequence (`dd`, `gg`, `yy`) inside the form.
    pub pending_key: Option<(char, std::time::Instant)>,
    pub error: Option<String>,
    /// Set while API format auto-detection runs in the background for a new
    /// provider; matched against `TestEvent::FormatDetected`'s own token so a
    /// stale result from a cancelled add can't land on a later one.
    pub detect_token: Option<String>,
}

// ─── Quota configuration form ─────────────────────────────────────────────────

pub struct QuotaForm {
    /// Provider name being configured.
    pub provider_name: String,
    /// Raw curl command editor field.
    pub curl_field: FormField,
    /// Current Vim sub-mode for the form.
    pub vim_mode: VimMode,
    /// Pending first key of a two-key sequence.
    pub pending_key: Option<(char, std::time::Instant)>,
    /// Last error from parsing/sending the preview request.
    pub error: Option<String>,
    /// Last response preview text (status + body snippet).
    pub preview: Option<String>,
    /// True while sending preview request.
    pub preview_loading: bool,
    /// Vertical scroll offset for response preview.
    pub preview_scroll: u16,
}

pub struct FormField {
    pub label: &'static str,
    pub value: String,
    pub cursor: usize,
    pub editable: bool,
    /// Non-empty for toggle fields. Lists the values the toggle cycles through (first = left, second = right).
    pub toggle_options: &'static [&'static str],
}

// ─── FormField helpers ────────────────────────────────────────────────────────

impl FormField {
    /// Construct a plain search/filter field with no special flags set.
    pub(super) fn search() -> Self {
        Self {
            label: "",
            value: String::new(),
            cursor: 0,
            editable: true,
            toggle_options: &[],
        }
    }

    pub(super) fn text(label: &'static str, value: &str) -> Self {
        Self {
            label,
            value: value.to_string(),
            cursor: value.len(),
            editable: true,
            toggle_options: &[],
        }
    }

    pub fn is_toggle(&self) -> bool {
        !self.toggle_options.is_empty()
    }

    /// Construct a toggle field that cycles through the given options.
    pub(super) fn toggle(
        label: &'static str,
        value: &str,
        options: &'static [&'static str],
    ) -> Self {
        debug_assert!(
            options.len() >= 2,
            "toggle fields require at least two options"
        );
        Self {
            label,
            value: value.to_string(),
            cursor: 0,
            editable: true,
            toggle_options: options,
        }
    }

    pub fn insert_newline(&mut self) {
        self.value.insert(self.cursor, '\n');
        self.cursor += 1;
    }

    /// Move cursor up one line within a multiline field.
    /// Returns false if already on first line (caller should focus prev field).
    pub fn move_up(&mut self) -> bool {
        let before = &self.value[..self.cursor];
        let line_start = before.rfind('\n').map(|p| p + 1).unwrap_or(0);
        if line_start == 0 {
            return false; // already on first line
        }
        let col = self.cursor - line_start;
        let prev_line_end = line_start - 1;
        let prev_line_start = self.value[..prev_line_end]
            .rfind('\n')
            .map(|p| p + 1)
            .unwrap_or(0);
        let prev_line_len = prev_line_end - prev_line_start;
        self.cursor = prev_line_start + col.min(prev_line_len);
        true
    }

    /// Move cursor down one line within a multiline field.
    /// Returns false if already on last line (caller should focus next field).
    pub fn move_down(&mut self) -> bool {
        let next_nl = self.value[self.cursor..].find('\n');
        let Some(rel) = next_nl else {
            return false;
        };
        let next_line_start = self.cursor + rel + 1;
        let before = &self.value[..self.cursor];
        let line_start = before.rfind('\n').map(|p| p + 1).unwrap_or(0);
        let col = self.cursor - line_start;
        let next_line_end = self.value[next_line_start..]
            .find('\n')
            .map(|p| next_line_start + p)
            .unwrap_or(self.value.len());
        let next_line_len = next_line_end - next_line_start;
        self.cursor = next_line_start + col.min(next_line_len);
        true
    }

    pub fn insert(&mut self, c: char) {
        if self.is_toggle() {
            return;
        }
        self.value.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    /// Insert pasted text at the cursor, advancing the cursor past it.
    pub fn insert_str(&mut self, text: &str) {
        if self.is_toggle() {
            return;
        }
        self.value.insert_str(self.cursor, text);
        self.cursor += text.len();
    }

    pub fn backspace(&mut self) {
        if self.is_toggle() || self.cursor == 0 {
            return;
        }
        let char_len = self.value[..self.cursor]
            .chars()
            .next_back()
            .map(|c| c.len_utf8())
            .unwrap_or(1);
        self.cursor -= char_len;
        self.value.remove(self.cursor);
    }

    pub fn delete(&mut self) {
        if self.is_toggle() || self.cursor >= self.value.len() {
            return;
        }
        self.value.remove(self.cursor);
    }

    pub fn move_left(&mut self) {
        if self.cursor > 0 {
            let char_len = self.value[..self.cursor]
                .chars()
                .next_back()
                .map(|c| c.len_utf8())
                .unwrap_or(1);
            self.cursor -= char_len;
        }
    }

    pub fn move_right(&mut self) {
        if self.cursor < self.value.len() {
            let char_len = self.value[self.cursor..]
                .chars()
                .next()
                .map(|c| c.len_utf8())
                .unwrap_or(1);
            self.cursor += char_len;
        }
    }

    pub fn delete_word_back(&mut self) {
        if self.is_toggle() || self.cursor == 0 {
            return;
        }
        let mut pos = self.cursor;
        // Skip trailing whitespace
        while pos > 0 {
            if let Some(c) = self.value[..pos].chars().next_back() {
                if !c.is_whitespace() {
                    break;
                }
                pos -= c.len_utf8();
            } else {
                // Invalid UTF-8 boundary; bail out
                return;
            }
        }
        // Delete word characters
        while pos > 0 {
            if let Some(c) = self.value[..pos].chars().next_back() {
                if c.is_whitespace() {
                    break;
                }
                pos -= c.len_utf8();
            } else {
                // Invalid UTF-8 boundary; bail out
                return;
            }
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

    pub fn move_next(&mut self) {
        if self.toggle_options.len() < 2 {
            return;
        }
        let current = self
            .toggle_options
            .iter()
            .position(|&o| o == self.value)
            .unwrap_or(0);
        let next = (current + 1) % self.toggle_options.len();
        self.value = self.toggle_options[next].to_string();
    }

    pub fn move_prev(&mut self) {
        if self.toggle_options.len() < 2 {
            return;
        }
        let current = self
            .toggle_options
            .iter()
            .position(|&o| o == self.value)
            .unwrap_or(0);
        let prev = if current == 0 {
            self.toggle_options.len() - 1
        } else {
            current - 1
        };
        self.value = self.toggle_options[prev].to_string();
    }
}

// ─── ProviderForm helpers ─────────────────────────────────────────────────────

impl ProviderForm {
    /// Create a new form for adding or editing a provider.
    ///
    /// API format is not user-editable here: for a new provider it's
    /// resolved by auto-detection on save (see `TestEvent::FormatDetected`);
    /// for an existing one it's fixed and simply carried over unchanged.
    pub(super) fn new(name: &str, provider: Option<&Provider>) -> Self {
        let (base_url, api_key, test_model, fallback, port_str, routes) = match provider {
            Some(p) => {
                let fb = if p.fallback { "yes" } else { "no" };
                let port = p.port.map(|p| p.to_string()).unwrap_or_default();
                (
                    p.base_url.as_str(),
                    p.api_key.as_str(),
                    p.test_model.as_deref().unwrap_or(""),
                    fb,
                    port,
                    p.routes.clone(),
                )
            }
            None => ("", "", "", "no", String::new(), vec![]),
        };
        Self {
            original_name: if name.is_empty() {
                None
            } else {
                Some(name.to_string())
            },
            fields: vec![
                FormField::text("Name", name),
                FormField::text("Base URL", base_url),
                FormField::text("API Key", api_key),
                FormField::text("Test Model", test_model),
                FormField::toggle("Fallback", fallback, &["yes", "no"]),
                FormField::text("Port", &port_str),
            ],
            focused: 0,
            vim_mode: VimMode::Normal,
            routes,
            route_cursor: 0,
            route_editing: false,
            route_pat_field: FormField::text("", ""),
            route_edit_target: false,
            route_tgt_field: FormField::text("", ""),
            route_suggest_active: false,
            route_suggest_idx: 0,
            route_suggest_scroll: 0,
            pending_key: None,
            error: None,
            detect_token: None,
        }
    }

    /// Returns true when the Routes section currently has focus.
    pub fn in_routes(&self) -> bool {
        self.focused == self.fields.len()
    }

    /// Reset route editing state (exit Insert mode in routes section).
    pub fn reset_route_editing(&mut self) {
        self.route_editing = false;
        self.route_edit_target = false;
        self.route_suggest_active = false;
        self.route_suggest_idx = 0;
        self.route_suggest_scroll = 0;
    }

    /// Clamp route_cursor to valid range after routes have been modified.
    pub fn clamp_route_cursor(&mut self) {
        if self.routes.is_empty() {
            self.route_cursor = 0;
        } else if self.route_cursor >= self.routes.len() {
            self.route_cursor = self.routes.len() - 1;
        }
    }

    /// Move focus to the next editable slot.
    /// Visual order: Name → Base URL → API Key → Format → Fallback → Port → Routes → (wrap)
    pub fn focus_next(&mut self) {
        let routes_slot = self.fields.len(); // virtual index for Routes (past last regular field)

        let next = if self.focused == routes_slot {
            0 // Routes → Name (wrap)
        } else {
            self.focused + 1 // sequential: Port(5) → Routes(6) naturally
        };

        self.focused = next;
        if next == routes_slot {
            self.reset_route_editing();
            self.vim_mode = VimMode::Normal;
        } else if self.fields[next].is_toggle() {
            // Toggle fields have no text cursor; Insert mode makes no sense here.
            self.vim_mode = VimMode::Normal;
        }
    }

    /// Move focus to the previous editable slot.
    /// Visual order (reverse): Routes → Port → Fallback → Format → API Key → Base URL → Name → (wrap)
    pub fn focus_prev(&mut self) {
        let routes_slot = self.fields.len();

        let prev = if self.focused == 0 {
            routes_slot // Name → Routes (wrap)
        } else {
            self.focused - 1 // sequential: Routes(6) → Port(5) → ...
        };

        self.focused = prev;
        if prev == routes_slot {
            self.reset_route_editing();
            self.vim_mode = VimMode::Normal;
        } else if prev < self.fields.len() && self.fields[prev].is_toggle() {
            self.vim_mode = VimMode::Normal;
        }
    }
}

// ─── QuotaForm helpers ────────────────────────────────────────────────────────

impl QuotaForm {
    /// Create a new quota preview form for a provider.
    pub(super) fn new(provider_name: &str, saved_curl: Option<&str>) -> Self {
        Self {
            provider_name: provider_name.to_string(),
            curl_field: FormField::text("cURL", saved_curl.unwrap_or("")),
            vim_mode: VimMode::Normal,
            pending_key: None,
            error: None,
            preview: None,
            preview_loading: false,
            preview_scroll: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use indexmap::IndexMap;

    use super::{App, ProviderForm, TestEvent};
    use crate::config::test_support::ConfigDirGuard;
    use crate::config::{ApiFormat, AppConfig, Provider};
    use crate::tester::{TestResult, TestStatus};

    #[test]
    fn new_provider_form_defaults_fallback_to_no() {
        let form = ProviderForm::new("", None);
        assert_eq!(form.fields[super::FALLBACK_FIELD_IDX].value, "no");
    }

    #[tokio::test]
    async fn completed_test_event_updates_displayed_model_to_used_model() {
        let path = format!("/tmp/ccs-state-test-{}.db", uuid::Uuid::new_v4());
        let mut providers = IndexMap::new();
        providers.insert(
            "vllm".to_string(),
            Provider {
                id: "vllm-id".into(),
                base_url: "http://127.0.0.1:1".into(),
                api_key: "test-key".into(),
                api_format: ApiFormat::OpenAI,
                model_map: Default::default(),
                routes: vec![],
                enabled: true,
                fallback: true,
                api_version: None,
                inject_thinking_history: true,
                quota_command: None,
                port: None,
                test_model: None,
            },
        );

        let db = crate::repo::Repository::open(&path);
        let mut app = App {
            config: AppConfig {
                current: "vllm".into(),
                listen: "127.0.0.1:0".into(),
                providers,
                fallback: false,
                db_path: Some(path),
                request_log_limit: 100,
            },
            mode: super::Mode::Normal,
            terminal_focused: true,
            providers: super::ProviderList {
                table_state: {
                    let mut s = ratatui::widgets::TableState::default();
                    s.select(Some(0));
                    s
                },
            },
            form: None,
            message: None,
            confirm_action: None,
            should_quit: false,
            server_status: super::ServerStatus::Stopped,
            metrics: std::sync::Arc::new(std::sync::Mutex::new(Default::default())),
            tests: super::TestState::new(),
            db,
            bg_proxy_pid: None,
            models: super::ModelsState {
                provider_models: std::collections::HashMap::new(),
                search_field: super::FormField::search(),
                search_active: true,
                selected: 0,
                scroll: 0,
                pending_key: None,
            },
            request_log: std::sync::Arc::new(std::sync::Mutex::new(
                crate::metrics::RequestLog::default(),
            )),
            logs: super::LogsState {
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
            detail_line_count: 6,
            sysinfo_sampler: crate::tui::sysinfo::SysInfoSampler::new(),
            config_needs_sync: false,
        };

        app.tests.pending.insert("vllm".into());
        app.tests
            .testing_model
            .insert("vllm".into(), "gemma-4-31b-it".into());
        app.tests
            .tx
            .send(TestEvent::Completed {
                provider: "vllm".into(),
                result: TestResult {
                    status: TestStatus::Error("HTTP 502".into()),
                    latency_ms: 100,
                    model_count: Some(1),
                    model_names: Some(vec!["gemma-4-31b-it".into()]),
                    tested_at: Instant::now(),
                    used_model: "sonnet-4-6".into(),
                    tools_supported: None,
                    images_supported: None,
                },
            })
            .unwrap();

        assert!(app.drain_test_results());

        assert!(!app.tests.pending.contains("vllm"));
        assert_eq!(
            app.tests.results.get("vllm").unwrap().used_model,
            "sonnet-4-6"
        );
        assert_eq!(
            app.tests.testing_model.get("vllm").map(String::as_str),
            Some("sonnet-4-6")
        );
    }

    #[tokio::test]
    async fn drain_test_results_returns_false_when_empty() {
        let _guard = ConfigDirGuard::new();
        let path = format!("/tmp/ccs-state-empty-drain-{}.db", uuid::Uuid::new_v4());
        let mut app = App::new().unwrap();
        app.config.db_path = Some(path);
        assert!(!app.drain_test_results());
    }

    #[tokio::test]
    async fn tick_message_reports_when_message_expires() {
        let _guard = ConfigDirGuard::new();
        let mut app = App::new().unwrap();
        app.message = Some((
            "old".into(),
            super::MessageKind::Info,
            Instant::now() - Duration::from_secs(super::MESSAGE_TIMEOUT_SECS + 1),
        ));
        assert!(app.tick_message());
        assert!(app.message.is_none());
    }

    #[tokio::test]
    async fn tick_message_reports_false_for_fresh_message() {
        let _guard = ConfigDirGuard::new();
        let mut app = App::new().unwrap();
        app.set_message("fresh", super::MessageKind::Info);
        assert!(!app.tick_message());
        assert!(app.message.is_some());
    }
}
