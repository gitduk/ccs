use std::collections::HashMap;

use crate::db::{self, SharedDb};
use crate::proxy::metrics::TokenMetrics;

/// Repository wraps the shared DB connection and owns all locking and SQL dispatch.
/// Callers never touch `SharedDb` or `crate::db::*` directly.
#[derive(Clone)]
pub struct Repository(SharedDb);

/// Token/request deltas to persist for a single proxy request.
#[derive(Clone, Default)]
pub struct StatsDelta {
    pub input: u64,
    pub output: u64,
    pub requests: u64,
    pub failures: u64,
}

impl Repository {
    pub fn open(path: &str) -> Self {
        Self(db::open_with_fallback(path))
    }

    /// Run DB schema migration. Returns an error if migration SQL fails;
    /// callers decide whether to abort or warn and continue.
    pub fn migrate(&self, name_to_id: &HashMap<String, String>) -> rusqlite::Result<()> {
        db::migrate_schema(&self.0, name_to_id)
    }

    /// Write provider + model token/request deltas atomically.
    /// Intended to be called from `tokio::task::spawn_blocking`.
    pub fn persist_stats(
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
                provider_id,
                provider_name,
                delta.input,
                delta.output,
                delta.requests,
                delta.failures,
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

    pub fn upsert_provider_models(
        &self,
        provider_id: &str,
        provider_name: &str,
        models: &[String],
    ) {
        if let Ok(conn) = self.0.lock()
            && let Err(e) = db::upsert_provider_models(&conn, provider_id, provider_name, models)
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
}
