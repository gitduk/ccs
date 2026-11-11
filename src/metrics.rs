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

#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// JSON body sent to the upstream provider. `Arc<str>` so the in-memory
    /// log, DB persistence, and per-frame rendering share one allocation.
    pub request_body: Option<Arc<str>>,
    /// JSON body returned by the upstream provider (error or success).
    pub response_body: Option<Arc<str>>,
}

/// Token/model values of one stored request, read back after a stream patched
/// its usage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogUsage {
    pub id: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub model: String,
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

    /// Id of the newest entry held, or 0 when the log is empty.
    pub fn newest_id(&self) -> u64 {
        self.entries.back().map(|e| e.id).unwrap_or(0)
    }

    /// Merge rows in by id, keeping the newest `keep`. Returns true when any row
    /// was added.
    ///
    /// Ids are assigned before the row is written, so a lower id can reach the
    /// database after a higher one; inserting in order (instead of appending)
    /// keeps a late row from being skipped forever.
    pub fn merge(&mut self, mut rows: Vec<RequestLogEntry>, keep: usize) -> bool {
        rows.sort_unstable_by_key(|e| e.id);
        let mut added = false;
        for row in rows {
            if let Err(pos) = self.entries.binary_search_by(|e| e.id.cmp(&row.id)) {
                self.entries.insert(pos, row);
                added = true;
            }
        }
        if !added {
            return false;
        }
        self.sync_id_past_newest();
        self.trim_to(keep);
        true
    }

    /// Patch token/model values of entries already held: a streamed request is
    /// stored with zero usage and patched once the stream ends. Returns true when
    /// any value changed.
    pub fn apply_usage(&mut self, usage: &[LogUsage]) -> bool {
        let mut changed = false;
        for u in usage {
            let Ok(pos) = self.entries.binary_search_by(|e| e.id.cmp(&u.id)) else {
                continue;
            };
            let entry = &mut self.entries[pos];
            if entry.input_tokens == u.input_tokens
                && entry.output_tokens == u.output_tokens
                && entry.model == u.model
            {
                continue;
            }
            entry.input_tokens = u.input_tokens;
            entry.output_tokens = u.output_tokens;
            entry.model.clone_from(&u.model);
            changed = true;
        }
        changed
    }

    /// Replace all in-memory entries with `entries`, keeping the newest `keep`.
    pub fn replace(&mut self, entries: Vec<RequestLogEntry>, keep: usize) {
        self.entries.clear();
        self.entries.extend(entries);
        self.sync_id_past_newest();
        self.trim_to(keep);
    }

    fn sync_id_past_newest(&mut self) {
        if let Some(max_id) = self.entries.back().map(|e| e.id) {
            sync_next_request_log_id(max_id.saturating_add(1));
        }
    }

    fn trim_to(&mut self, keep: usize) {
        while self.entries.len() > keep.max(1) {
            self.entries.pop_front();
        }
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

/// Minimal entry for tests; set only the fields the test is about.
#[cfg(test)]
pub(crate) fn entry_with_id(id: u64) -> RequestLogEntry {
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
        request_body: None,
        response_body: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backfill_updates_to_final_response_model() {
        let mut log = RequestLog::default();
        let id = log.push(RequestLogEntry {
            model: "sonnet-4-6".into(),
            ..entry_with_id(0)
        });

        log.backfill(id, 1234, 5678, Some("gpt-5.4"));

        let entry = log.entries().iter().find(|e| e.id == id).unwrap();
        assert_eq!(entry.model, "gpt-5.4");
        assert_eq!(entry.input_tokens, 1234);
        assert_eq!(entry.output_tokens, 5678);
    }

    #[test]
    fn replace_advances_next_id_past_loaded_history() {
        let mut log = RequestLog::default();
        log.replace(vec![entry_with_id(1_000_000)], 200);

        let next_id = log.push(entry_with_id(0));
        assert!(next_id > 1_000_000);
    }

    #[test]
    fn merge_keeps_a_row_that_reaches_the_db_after_a_higher_id() {
        let mut log = RequestLog::default();
        log.merge(vec![entry_with_id(11)], 10); // id 10 is still being written

        assert!(
            log.merge(vec![entry_with_id(10)], 10),
            "the late row must still be merged"
        );

        let ids: Vec<u64> = log.entries().iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![10, 11], "inserted in id order, not appended");
        assert_eq!(log.newest_id(), 11);
    }

    #[test]
    fn apply_usage_patches_tokens_written_after_the_row_was_logged() {
        let mut log = RequestLog::default();
        log.merge(vec![entry_with_id(7), entry_with_id(8)], 10);
        let usage = vec![LogUsage {
            id: 8,
            input_tokens: 120,
            output_tokens: 45,
            model: "served-model".into(),
        }];

        assert!(log.apply_usage(&usage));
        let entry = log.entries().iter().find(|e| e.id == 8).unwrap();
        assert_eq!(entry.input_tokens, 120);
        assert_eq!(entry.output_tokens, 45);
        assert_eq!(entry.model, "served-model");

        assert!(
            !log.apply_usage(&usage),
            "an unchanged patch redraws nothing"
        );
    }

    #[test]
    fn merge_skips_rows_already_held_and_keeps_the_newest() {
        let mut log = RequestLog::default();
        log.replace(vec![entry_with_id(10), entry_with_id(11)], 10);

        assert!(!log.merge(vec![entry_with_id(11)], 10), "nothing new");
        assert!(log.merge(vec![entry_with_id(12), entry_with_id(13)], 10));

        let ids: Vec<u64> = log.entries().iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![10, 11, 12, 13]);

        log.merge(vec![entry_with_id(14)], 2);
        let ids: Vec<u64> = log.entries().iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![13, 14], "trimmed to the newest rows");
    }
}
