use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

#[derive(Debug, Clone)]
pub struct LastRequestError {
    pub status: u16,
    pub model: String,
    /// Route pattern that matched (e.g. `claude-*`), empty if no routing.
    pub pattern: String,
    pub message: String,
}

#[derive(Debug, Default, Clone)]
pub struct ProviderStats {
    pub input: u64,
    pub output: u64,
    pub requests: u64,
    pub failures: u64,
}

#[derive(Debug, Default, Clone)]
pub struct ModelStats {
    pub input: u64,
    pub output: u64,
}

#[derive(Debug, Default, Clone)]
pub struct TokenMetrics {
    pub by_provider: HashMap<String, ProviderStats>,
    pub by_model: HashMap<String, ModelStats>,
    /// Last request error per provider name; cleared on next successful request.
    pub last_error: HashMap<String, LastRequestError>,
}

impl TokenMetrics {
    pub fn record_error(
        &mut self,
        name: &str,
        status: u16,
        model: &str,
        pattern: &str,
        message: &str,
    ) {
        self.last_error.insert(
            name.to_string(),
            LastRequestError {
                status,
                model: model.to_string(),
                pattern: pattern.to_string(),
                message: message.to_string(),
            },
        );
    }

    pub fn clear_error(&mut self, name: &str) {
        self.last_error.remove(name);
    }
}

pub type SharedMetrics = Arc<Mutex<TokenMetrics>>;

// ─── Request Log ─────────────────────────────────────────────────────────────

const REQUEST_LOG_CAPACITY: usize = 200;

#[derive(Debug, Clone)]
pub struct RequestLogEntry {
    pub timestamp: SystemTime,
    pub provider: String,
    pub model: String,
    pub status: u16,
    pub latency_ms: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub is_stream: bool,
    pub error: Option<String>,
}

#[derive(Debug, Default)]
pub struct RequestLog {
    entries: VecDeque<RequestLogEntry>,
}

impl RequestLog {
    pub fn push(&mut self, entry: RequestLogEntry) {
        if self.entries.len() >= REQUEST_LOG_CAPACITY {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
    }

    pub fn entries(&self) -> &VecDeque<RequestLogEntry> {
        &self.entries
    }

    pub fn entries_mut(&mut self) -> &mut VecDeque<RequestLogEntry> {
        &mut self.entries
    }
}

pub type SharedRequestLog = Arc<Mutex<RequestLog>>;
