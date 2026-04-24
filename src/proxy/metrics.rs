use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

static NEXT_REQUEST_LOG_ID: AtomicU64 = AtomicU64::new(1);

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
    /// Cumulative latency of successful requests (ms); divide by requests for avg.
    pub latency_total: u64,
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
    pub id: u64,
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
    /// Push an entry, assigning a unique ID. Returns the assigned ID for
    /// later back-fill lookups.
    pub fn push(&mut self, mut entry: RequestLogEntry) -> u64 {
        let id = NEXT_REQUEST_LOG_ID.fetch_add(1, Ordering::Relaxed);
        entry.id = id;
        if self.entries.len() >= REQUEST_LOG_CAPACITY {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
        id
    }

    /// Find an entry by ID (reverse search — recent entries are near the back).
    pub fn backfill(&mut self, id: u64, input: u64, output: u64, model: Option<&str>) {
        if let Some(entry) = self.entries.iter_mut().rev().find(|e| e.id == id) {
            entry.input_tokens = input;
            entry.output_tokens = output;
            if let Some(m) = model {
                entry.model = m.to_string();
            }
        }
    }

    pub fn entries(&self) -> &VecDeque<RequestLogEntry> {
        &self.entries
    }

    pub fn entries_mut(&mut self) -> &mut VecDeque<RequestLogEntry> {
        &mut self.entries
    }

    /// Replace all in-memory entries, keeping only the newest `REQUEST_LOG_CAPACITY`.
    pub fn replace(&mut self, entries: Vec<RequestLogEntry>) {
        if let Some(max_id) = entries.iter().map(|e| e.id).max() {
            sync_next_request_log_id(max_id.saturating_add(1));
        }
        let mut deque = VecDeque::from(entries);
        while deque.len() > REQUEST_LOG_CAPACITY {
            deque.pop_front();
        }
        self.entries = deque;
    }

    pub fn from_entries(entries: Vec<RequestLogEntry>) -> Self {
        let mut log = Self::default();
        log.replace(entries);
        log
    }
}

pub type SharedRequestLog = Arc<Mutex<RequestLog>>;

pub(crate) fn sync_next_request_log_id(next_id: u64) {
    let mut current = NEXT_REQUEST_LOG_ID.load(Ordering::Relaxed);
    while current < next_id {
        match NEXT_REQUEST_LOG_ID.compare_exchange_weak(
            current,
            next_id,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => break,
            Err(observed) => current = observed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry_with_id(id: u64) -> RequestLogEntry {
        RequestLogEntry {
            id,
            timestamp: SystemTime::UNIX_EPOCH,
            provider: "prov".into(),
            model: String::new(),
            status: 200,
            latency_ms: 0,
            input_tokens: 0,
            output_tokens: 0,
            is_stream: false,
            error: None,
        }
    }

    #[test]
    fn replace_advances_next_id_past_loaded_history() {
        let mut log = RequestLog::default();
        log.replace(vec![entry_with_id(1_000_000)]);

        let next_id = log.push(entry_with_id(0));
        assert!(next_id > 1_000_000);
    }
}
