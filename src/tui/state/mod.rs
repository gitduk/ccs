use std::collections::{HashMap, HashSet};
use std::sync::mpsc;

/// Index of the Notes field inside `ProviderForm::fields`.
pub(super) const NOTES_FIELD_IDX: usize = 4;

use ratatui::widgets::TableState;

use crate::config::{AppConfig, Provider, RouteRule};
use crate::db::SharedDb;
use crate::proxy::metrics::SharedMetrics;
use crate::tester::TestResult;

// UI constants
pub(super) const MESSAGE_TIMEOUT_SECS: u64 = 3;

mod actions;
mod bg_proxy;
mod filter;
mod navigation;

pub use bg_proxy::{is_process_alive, send_sighup};
pub use filter::filter_suggestions;

#[derive(Debug, Clone, PartialEq)]
pub enum Mode {
    Normal,
    Editing,
    Confirm,
    Help,
    /// Models browser popup: search and browse all available models.
    Models,
}

/// Vim-style sub-mode used inside the provider editor form.
#[derive(Debug, Clone, PartialEq)]
pub enum VimMode {
    /// Navigation / command mode (default on form open).
    Normal,
    /// Text-input mode, entered with `i` / `a`.
    Insert,
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
    ClearCurrent,
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
    pub(super) test_rx: mpsc::Receiver<(String, TestResult)>,
    pub db: SharedDb,
    pub bg_proxy_pid: Option<u32>,
    /// Model names per provider, loaded from DB and updated on test.
    pub provider_models: HashMap<String, Vec<String>>,
    /// Shared HTTP client for provider connectivity tests (reuses connection pool).
    pub test_client: reqwest::Client,
    /// Pending first key of a two-key sequence (`dd`, `gg`) in the normal list view.
    pub pending_key: Option<(char, std::time::Instant)>,
    /// Search field in the Models popup (value + cursor).
    pub models_search_field: FormField,
    /// True = Insert mode (search box focused); false = Normal mode (list navigation).
    pub models_insert: bool,
    /// Index of the highlighted model in the flat filtered list.
    pub models_selected: usize,
    /// Scroll offset (rows) for the models list.
    pub models_scroll: u16,
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

    /// Pending first key of a two-key sequence (`ZZ`, `ZQ`, `dd`) inside the form.
    pub pending_key: Option<(char, std::time::Instant)>,
    pub error: Option<String>,
}

pub struct FormField {
    pub label: &'static str,
    pub value: String,
    pub cursor: usize,
    pub editable: bool,
    pub is_toggle: bool,
    pub is_multiline: bool,
}

// ─── FormField helpers ────────────────────────────────────────────────────────

impl FormField {
    pub(super) fn text(label: &'static str, value: &str) -> Self {
        Self {
            label,
            value: value.to_string(),
            cursor: value.len(),
            editable: true,
            is_toggle: false,
            is_multiline: false,
        }
    }

    pub(super) fn toggle(label: &'static str, value: &str) -> Self {
        Self {
            label,
            value: value.to_string(),
            cursor: 0,
            editable: true,
            is_toggle: true,
            is_multiline: false,
        }
    }

    pub(super) fn multiline(label: &'static str, value: &str) -> Self {
        Self {
            label,
            value: value.to_string(),
            cursor: value.len(),
            editable: true,
            is_toggle: false,
            is_multiline: true,
        }
    }

    pub fn insert_newline(&mut self) {
        self.value.insert(self.cursor, '\n');
        self.cursor += 1;
    }

    /// Delete the line the cursor is on. The adjacent newline is also removed.
    pub fn delete_current_line(&mut self) {
        if self.value.is_empty() {
            return;
        }
        let before = &self.value[..self.cursor];
        let line_start = before.rfind('\n').map(|p| p + 1).unwrap_or(0);
        let line_end = self.value[self.cursor..]
            .find('\n')
            .map(|p| self.cursor + p)
            .unwrap_or(self.value.len());
        // Include the adjacent newline so we don't leave a blank line behind.
        let (remove_start, remove_end) = if line_end < self.value.len() {
            (line_start, line_end + 1) // consume trailing '\n'
        } else if line_start > 0 {
            (line_start - 1, line_end) // consume preceding '\n'
        } else {
            (line_start, line_end) // only line — clear entirely
        };
        self.value.drain(remove_start..remove_end);
        self.cursor = remove_start.min(self.value.len());
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
            .chars()
            .next_back()
            .map(|c| c.len_utf8())
            .unwrap_or(1);
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
        if self.is_toggle || self.cursor == 0 {
            return;
        }
        let mut pos = self.cursor;
        while pos > 0 {
            let c = self.value[..pos]
                .chars()
                .next_back()
                .expect("pos is a valid UTF-8 char boundary");
            if c != ' ' {
                break;
            }
            pos -= c.len_utf8();
        }
        while pos > 0 {
            let c = self.value[..pos]
                .chars()
                .next_back()
                .expect("pos is a valid UTF-8 char boundary");
            if c == ' ' {
                break;
            }
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

// ─── ProviderForm helpers ─────────────────────────────────────────────────────

impl ProviderForm {
    /// Create a new form for adding or editing a provider.
    pub(super) fn new(name: &str, provider: Option<&Provider>) -> Self {
        let (base_url, api_key, format, notes, routes) = match provider {
            Some(p) => (
                p.base_url.as_str(),
                p.api_key.as_str(),
                p.api_format.to_string(),
                p.notes.as_str(),
                p.routes.clone(),
            ),
            None => ("", "", "anthropic".to_string(), "", vec![]),
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
                FormField::toggle("Format", &format),
                FormField::multiline("Notes", notes),
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
    /// Visual order: Name → Base URL → API Key → Format → Routes → Notes → (wrap)
    pub fn focus_next(&mut self) {
        let routes_slot = self.fields.len(); // virtual index for Routes
        let notes_idx = routes_slot - 1; // Notes is always the last field

        let next = if self.focused == notes_idx {
            0 // Notes → Name (wrap)
        } else if self.focused == routes_slot {
            notes_idx // Routes → Notes
        } else if self.focused == notes_idx - 1 {
            routes_slot // Format → Routes
        } else {
            self.focused + 1 // sequential advance
        };

        self.focused = next;
        if next == routes_slot {
            self.reset_route_editing();
            // Routes has its own editing state machine; entering it while
            // VimMode::Insert is active causes a stuck [I] indicator.
            self.vim_mode = VimMode::Normal;
        }
    }

    /// Move focus to the previous editable slot.
    /// Visual order (reverse): Notes → Routes → Format → API Key → Base URL → Name → (wrap)
    pub fn focus_prev(&mut self) {
        let routes_slot = self.fields.len();
        let notes_idx = routes_slot - 1;

        let prev = if self.focused == 0 {
            notes_idx // Name → Notes (wrap)
        } else if self.focused == notes_idx {
            routes_slot // Notes → Routes
        } else if self.focused == routes_slot {
            notes_idx - 1 // Routes → Format
        } else {
            self.focused - 1 // sequential retreat
        };

        self.focused = prev;
        if prev == routes_slot {
            self.reset_route_editing();
            // Same reason as focus_next: reset vim_mode on entering Routes.
            self.vim_mode = VimMode::Normal;
        }
    }
}
