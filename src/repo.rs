use std::collections::HashMap;

use crate::db::{self, SharedDb};
use crate::metrics::{TokenMetrics, sync_next_request_log_id};

/// Trim the request log every this many requests to amortize the SQL cost.
/// Log size stays within `request_log_limit + TRIM_INTERVAL` rows.
const TRIM_INTERVAL: u64 = 50;

/// Repository wraps the shared DB connection and owns all locking and SQL dispatch.
/// Callers never touch `SharedDb` or `crate::db::*` directly.
#[derive(Clone)]
pub struct Repository(SharedDb);

/// Token/request deltas to persist for a single proxy request.
#[derive(Default)]
pub(crate) struct StatsDelta {
    pub input: u64,
    pub output: u64,
    pub requests: u64,
    pub failures: u64,
    pub latency: u64,
}

impl Repository {
    pub fn open(path: &str) -> Self {
        let db = db::open_with_fallback(path);
        if let Ok(conn) = db.lock() {
            let next_id = db::max_request_log_id(&conn).saturating_add(1);
            sync_next_request_log_id(next_id);
        }
        Self(db)
    }

    /// Run DB schema migration. Returns an error if migration SQL fails;
    /// callers decide whether to abort or warn and continue.
    pub fn migrate(&self, name_to_id: &HashMap<String, String>) -> rusqlite::Result<()> {
        db::migrate_schema(&self.0, name_to_id)
    }

    /// Write provider + model token/request deltas atomically.
    /// Intended to be called from `tokio::task::spawn_blocking`.
    pub(crate) fn persist_stats(
        &self,
        provider_id: &str,
        provider_name: &str,
        model_name: Option<&str>,
        delta: StatsDelta,
    ) {
        let mut conn = match self.0.lock() {
            Ok(c) => c,
            Err(_) => {
                tracing::warn!("DB mutex poisoned; skipping persist_stats for {provider_name}");
                return;
            }
        };
        let result = conn.transaction().and_then(|tx| {
            db::upsert_provider(
                &tx,
                &db::ProviderDelta {
                    id: provider_id,
                    name: provider_name,
                    input: delta.input,
                    output: delta.output,
                    requests: delta.requests,
                    failures: delta.failures,
                    latency: delta.latency,
                },
            )?;
            if let Some(model) = model_name {
                db::upsert_model(
                    &tx,
                    provider_id,
                    provider_name,
                    model,
                    delta.input,
                    delta.output,
                )?;
            }
            tx.commit()
        });
        if let Err(e) = result {
            tracing::warn!("Failed to persist stats for {provider_name}: {e}");
        }
    }

    /// Fire-and-forget: spawn a blocking task to persist provider/model deltas.
    /// Owns the arguments so the caller does not need to juggle clones at every
    /// call site. Callable from async contexts.
    pub(crate) fn persist_stats_async(
        &self,
        provider_id: &str,
        provider_name: &str,
        model_name: Option<&str>,
        delta: StatsDelta,
    ) {
        let repo = self.clone();
        let pid = provider_id.to_string();
        let pname = provider_name.to_string();
        let mid = model_name.map(str::to_string);
        tokio::task::spawn_blocking(move || {
            repo.persist_stats(&pid, &pname, mid.as_deref(), delta);
        });
    }

    pub fn load_metrics(&self) -> TokenMetrics {
        match self.0.lock() {
            Ok(conn) => db::load_metrics(&conn),
            Err(_) => {
                tracing::warn!("DB mutex poisoned in load_metrics; returning empty metrics");
                TokenMetrics::default()
            }
        }
    }

    pub fn load_provider_models(&self) -> HashMap<String, Vec<String>> {
        match self.0.lock() {
            Ok(conn) => db::load_provider_models(&conn),
            Err(_) => {
                tracing::warn!("DB mutex poisoned in load_provider_models; returning empty map");
                HashMap::new()
            }
        }
    }

    /// Load both metrics and provider models in a single lock acquisition.
    pub fn load_all(&self) -> (TokenMetrics, HashMap<String, Vec<String>>) {
        match self.0.lock() {
            Ok(conn) => (db::load_metrics(&conn), db::load_provider_models(&conn)),
            Err(_) => {
                tracing::warn!("DB mutex poisoned in load_all; returning empty state");
                (TokenMetrics::default(), HashMap::new())
            }
        }
    }

    pub fn upsert_provider_models(
        &self,
        provider_id: &str,
        provider_name: &str,
        models: &[String],
    ) {
        if let Ok(mut conn) = self.0.lock()
            && let Err(e) =
                db::upsert_provider_models(&mut conn, provider_id, provider_name, models)
        {
            tracing::warn!("Failed to upsert provider models for {provider_name}: {e}");
        }
    }

    pub fn rename_provider(&self, provider_id: &str, new_name: &str) {
        if let Ok(conn) = self.0.lock()
            && let Err(e) = db::rename_provider(&conn, provider_id, new_name)
        {
            tracing::warn!("Failed to rename provider {provider_id}: {e}");
        }
    }

    pub fn clear_all(&self) {
        if let Ok(mut conn) = self.0.lock()
            && let Err(e) = db::clear_all(&mut conn)
        {
            tracing::warn!("Failed to clear all stats: {e}");
        }
    }

    pub fn clear_provider(&self, provider_id: &str) {
        if let Ok(mut conn) = self.0.lock()
            && let Err(e) = db::clear_provider(&mut conn, provider_id)
        {
            tracing::warn!("Failed to clear stats for provider {provider_id}: {e}");
        }
    }

    // ─── Request Log ─────────────────────────────────────────────────────────

    /// Fire-and-forget: persist a request log entry, then trim to `keep` rows.
    /// `keep` should be at least as large as the TUI display limit; a minimum of
    /// 200 is enforced so a small `request_log_limit` never truncates history too aggressively.
    pub(crate) fn persist_request_log_async(
        &self,
        entry: crate::metrics::RequestLogEntry,
        keep: usize,
    ) {
        let repo = self.clone();
        let keep = keep.max(200);
        tokio::task::spawn_blocking(move || {
            if let Ok(conn) = repo.0.lock() {
                if let Err(e) = db::insert_request_log(&conn, &entry) {
                    tracing::warn!(
                        provider = %entry.provider,
                        model = %entry.model,
                        "Failed to persist request log entry {}: {e}", entry.id
                    );
                }
                // Amortize trim cost: run every 50 requests rather than on every insert.
                if entry.id.is_multiple_of(TRIM_INTERVAL) {
                    db::trim_request_log(&conn, keep).ok();
                }
            }
        });
    }

    /// Fire-and-forget: back-fill token counts for a streaming request.
    pub(crate) fn update_request_log_tokens_async(
        &self,
        id: u64,
        input: u64,
        output: u64,
        model: Option<String>,
        keep: usize,
    ) {
        let repo = self.clone();
        let keep = keep.max(200);
        tokio::task::spawn_blocking(move || {
            if let Ok(conn) = repo.0.lock() {
                if let Err(e) =
                    db::update_request_log_tokens(&conn, id, input, output, model.as_deref())
                {
                    tracing::warn!("Failed to update request log tokens for id {id}: {e}");
                }
                // Trim only on every 50th request to amortize the cost; the insert
                // path already trimmed once, so log size stays within ~keep+50 rows.
                if id.is_multiple_of(TRIM_INTERVAL) {
                    db::trim_request_log(&conn, keep).ok();
                }
            }
        });
    }

    /// Load the most recent `limit` request log entries (oldest-first).
    pub fn load_recent_request_logs(&self, limit: usize) -> Vec<crate::metrics::RequestLogEntry> {
        match self.0.lock() {
            Ok(conn) => db::load_recent_request_logs(&conn, limit),
            Err(_) => {
                tracing::warn!("DB mutex poisoned in load_recent_request_logs");
                vec![]
            }
        }
    }
}
