use std::sync::{Arc, Mutex};

use rusqlite::{Connection, Result, params};

use crate::proxy::metrics::{ModelStats, ProviderStats, TokenMetrics};

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
        Arc::new(Mutex::new(
            Connection::open_in_memory()
                .expect("in-memory SQLite unavailable — system may be out of file descriptors"),
        ))
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
            model_name TEXT PRIMARY KEY,
            input      INTEGER NOT NULL DEFAULT 0,
            output     INTEGER NOT NULL DEFAULT 0
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

    if let Ok(mut stmt) =
        conn.prepare("SELECT model_name, input, output FROM model_stats")
    {
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

pub fn upsert_provider(conn: &Connection, name: &str, s: &ProviderStats) -> Result<()> {
    conn.execute(
        "INSERT INTO provider_stats (provider_name, input, output, requests, failures)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(provider_name) DO UPDATE SET
             input = excluded.input, output = excluded.output,
             requests = excluded.requests, failures = excluded.failures",
        params![name, s.input, s.output, s.requests, s.failures],
    )?;
    Ok(())
}

pub fn upsert_model(conn: &Connection, name: &str, s: &ModelStats) -> Result<()> {
    conn.execute(
        "INSERT INTO model_stats (model_name, input, output)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(model_name) DO UPDATE SET
             input = excluded.input, output = excluded.output",
        params![name, s.input, s.output],
    )?;
    Ok(())
}

pub fn delete_provider(conn: &Connection, provider_name: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM provider_stats WHERE provider_name = ?1",
        [provider_name],
    )?;
    Ok(())
}

pub fn clear_all(conn: &Connection) -> Result<()> {
    conn.execute_batch("BEGIN; DELETE FROM provider_stats; DELETE FROM model_stats; COMMIT;")
}
