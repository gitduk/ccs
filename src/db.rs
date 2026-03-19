use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, Result, params};

use crate::proxy::metrics::TokenMetrics;

pub type SharedDb = Arc<Mutex<Connection>>;

pub fn open(path: &str) -> Result<SharedDb> {
    if let Some(parent) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let conn = Connection::open(path)?;
    init_schema(&conn)?;
    Ok(Arc::new(Mutex::new(conn)))
}

/// Open the SQLite DB at `path`, falling back to an in-memory DB on failure.
/// Logs a warning on disk-open failure so callers don't need to repeat this.
pub fn open_with_fallback(path: &str) -> SharedDb {
    open(path).unwrap_or_else(|e| {
        tracing::warn!("Failed to open DB at {path}: {e}; using in-memory fallback");
        let conn = Connection::open_in_memory()
            .expect("in-memory SQLite unavailable — system may be out of file descriptors");
        init_schema(&conn).expect("failed to init in-memory schema");
        Arc::new(Mutex::new(conn))
    })
}

fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS provider_stats (
            provider_name TEXT PRIMARY KEY,
            input         INTEGER NOT NULL DEFAULT 0,
            output        INTEGER NOT NULL DEFAULT 0,
            requests      INTEGER NOT NULL DEFAULT 0,
            failures      INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS model_stats (
            provider_name TEXT NOT NULL,
            model_name    TEXT NOT NULL,
            input         INTEGER NOT NULL DEFAULT 0,
            output        INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (provider_name, model_name)
        );",
    )
}

pub fn load_metrics(conn: &Connection) -> TokenMetrics {
    let mut metrics = TokenMetrics::new();

    if let Ok(mut stmt) = conn.prepare(
        "SELECT provider_name, input, output, requests, failures FROM provider_stats",
    ) {
        match stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, u64>(1)?,
                row.get::<_, u64>(2)?,
                row.get::<_, u64>(3)?,
                row.get::<_, u64>(4)?,
            ))
        }) {
            Ok(rows) => {
                for row in rows.flatten() {
                    let s = metrics.by_provider.entry(row.0).or_default();
                    s.input = row.1;
                    s.output = row.2;
                    s.requests = row.3;
                    s.failures = row.4;
                }
            }
            Err(e) => tracing::warn!("Failed to load provider stats: {e}"),
        }
    }

    // Aggregate usage across all providers per model for the bar chart.
    if let Ok(mut stmt) = conn.prepare(
        "SELECT model_name, SUM(input), SUM(output) FROM model_stats GROUP BY model_name",
    ) {
        match stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, u64>(1)?,
                row.get::<_, u64>(2)?,
            ))
        }) {
            Ok(rows) => {
                for row in rows.flatten() {
                    let s = metrics.by_model.entry(row.0).or_default();
                    s.input = row.1;
                    s.output = row.2;
                }
            }
            Err(e) => tracing::warn!("Failed to load model stats: {e}"),
        }
    }

    metrics
}

/// Accumulate token delta and update request/failure totals for a provider.
/// input/output use += delta to be multi-process safe; requests/failures are
/// written as absolute totals (single writer: the proxy process).
pub fn upsert_provider(
    conn: &Connection,
    name: &str,
    input_delta: u64,
    output_delta: u64,
    requests: u64,
    failures: u64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO provider_stats (provider_name, input, output, requests, failures)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(provider_name) DO UPDATE SET
             input    = provider_stats.input    + excluded.input,
             output   = provider_stats.output   + excluded.output,
             requests = excluded.requests,
             failures = excluded.failures",
        params![name, input_delta, output_delta, requests, failures],
    )?;
    Ok(())
}

/// Accumulate token delta for a specific (provider, model) pair.
pub fn upsert_model(conn: &Connection, provider: &str, model: &str, input_delta: u64, output_delta: u64) -> Result<()> {
    conn.execute(
        "INSERT INTO model_stats (provider_name, model_name, input, output)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(provider_name, model_name) DO UPDATE SET
             input  = model_stats.input  + excluded.input,
             output = model_stats.output + excluded.output",
        params![provider, model, input_delta, output_delta],
    )?;
    Ok(())
}

pub fn delete_provider(conn: &Connection, provider_name: &str) -> Result<()> {
    conn.execute("DELETE FROM provider_stats WHERE provider_name = ?1", [provider_name])?;
    conn.execute("DELETE FROM model_stats    WHERE provider_name = ?1", [provider_name])?;
    Ok(())
}

pub fn load_provider_models(conn: &Connection) -> HashMap<String, Vec<String>> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    if let Ok(mut stmt) = conn.prepare(
        "SELECT provider_name, model_name FROM model_stats ORDER BY provider_name, model_name",
    ) {
        if let Ok(rows) = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        }) {
            for (provider, model) in rows.flatten() {
                map.entry(provider).or_default().push(model);
            }
        }
    }
    map
}

/// Ensure discovered models exist in model_stats (preserves existing usage data).
pub fn upsert_provider_models(conn: &Connection, provider: &str, models: &[String]) -> Result<()> {
    let mut stmt = conn.prepare(
        "INSERT OR IGNORE INTO model_stats (provider_name, model_name, input, output)
         VALUES (?1, ?2, 0, 0)",
    )?;
    for model in models {
        stmt.execute(params![provider, model])?;
    }
    Ok(())
}

pub fn clear_all(conn: &Connection) -> Result<()> {
    conn.execute_batch("BEGIN; DELETE FROM provider_stats; DELETE FROM model_stats; COMMIT;")
}
