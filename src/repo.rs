use std::collections::HashMap;

use crate::db::{self, SharedDb};
use crate::proxy::metrics::TokenMetrics;

/// Repository wraps the shared DB connection and owns all locking and SQL dispatch.
/// Callers never touch `SharedDb` or `crate::db::*` directly.
#[derive(Clone)]
pub struct Repository(SharedDb);

impl Repository {
    pub fn open(path: &str) -> Self {
        Self(db::open_with_fallback(path))
    }

    pub fn migrate(&self, name_to_id: &HashMap<String, String>) {
        db::migrate_schema(&self.0, name_to_id);
    }

    /// Write provider + model token/request deltas atomically.
    /// Intended to be called from `tokio::task::spawn_blocking`.
    #[allow(clippy::too_many_arguments)]
    pub fn persist_stats(
        &self,
        provider_id: &str,
        provider_name: &str,
        model_name: Option<&str>,
        input: u64,
        output: u64,
        requests: u64,
        failures: u64,
    ) {
        let Ok(mut conn) = self.0.lock() else { return };
        let result = conn.transaction().and_then(|tx| {
            db::upsert_provider(
                &tx,
                provider_id,
                provider_name,
                input,
                output,
                requests,
                failures,
            )?;
            if let Some(model) = model_name {
                db::upsert_model(&tx, provider_id, provider_name, model, input, output)?;
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
            Err(_) => TokenMetrics::default(),
        }
    }

    pub fn load_provider_models(&self) -> HashMap<String, Vec<String>> {
        match self.0.lock() {
            Ok(conn) => db::load_provider_models(&conn),
            Err(_) => HashMap::new(),
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
