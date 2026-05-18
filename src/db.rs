use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, Result, params};

use crate::metrics::TokenMetrics;

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
pub fn open_with_fallback(path: &str) -> SharedDb {
    open(path).unwrap_or_else(|e| {
        tracing::warn!("Failed to open DB at {path}: {e}; using in-memory fallback");
        let conn = match Connection::open_in_memory() {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("In-memory SQLite unavailable: {e}; cannot continue");
                std::process::exit(1);
            }
        };
        if let Err(e) = init_schema(&conn) {
            tracing::error!("Failed to init in-memory schema: {e}; cannot continue");
            std::process::exit(1);
        }
        Arc::new(Mutex::new(conn))
    })
}

fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS provider_stats (
            provider_id       TEXT PRIMARY KEY,
            provider_name     TEXT NOT NULL,
            input             INTEGER NOT NULL DEFAULT 0,
            output            INTEGER NOT NULL DEFAULT 0,
            requests          INTEGER NOT NULL DEFAULT 0,
            failures          INTEGER NOT NULL DEFAULT 0,
            latency_total  INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS model_stats (
            provider_id   TEXT NOT NULL,
            provider_name TEXT NOT NULL,
            model_name    TEXT NOT NULL,
            input         INTEGER NOT NULL DEFAULT 0,
            output        INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (provider_id, model_name)
        );
        CREATE TABLE IF NOT EXISTS request_log (
            id            INTEGER PRIMARY KEY,
            timestamp_ms  INTEGER NOT NULL,
            provider_name TEXT NOT NULL,
            model         TEXT NOT NULL DEFAULT '',
            status        INTEGER NOT NULL,
            latency_ms    INTEGER NOT NULL DEFAULT 0,
            input_tokens  INTEGER NOT NULL DEFAULT 0,
            output_tokens INTEGER NOT NULL DEFAULT 0,
            is_stream     INTEGER NOT NULL DEFAULT 0,
            error         TEXT,
            request_body  TEXT,
            response_body TEXT
        );",
    )?;
    // Add latency_total column to existing DBs that predate this field.
    conn.execute_batch(
        "ALTER TABLE provider_stats ADD COLUMN latency_total INTEGER NOT NULL DEFAULT 0",
    )
    .ok(); // Ignore error — column already exists on fresh DBs.
    // Migrate older DBs that predate request/response body columns.
    conn.execute_batch("ALTER TABLE request_log ADD COLUMN request_body TEXT")
        .ok();
    conn.execute_batch("ALTER TABLE request_log ADD COLUMN response_body TEXT")
        .ok();
    Ok(())
}

/// Migrate old schema (provider_name as PK, no provider_id column) to new schema.
/// name_to_id maps provider_name → provider UUID from config.
/// Safe to call on an already-migrated DB (no-op if provider_id column exists).
pub(crate) fn migrate_schema(
    db: &SharedDb,
    name_to_id: &HashMap<String, String>,
) -> rusqlite::Result<()> {
    let Ok(mut conn) = db.lock() else {
        return Ok(());
    };
    do_migrate(&mut conn, name_to_id)
}

fn do_migrate(conn: &mut Connection, name_to_id: &HashMap<String, String>) -> Result<()> {
    // Check if provider_id column already exists — if so, nothing to do.
    let already_migrated: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('provider_stats') WHERE name = 'provider_id'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0)
        > 0;

    if already_migrated {
        return Ok(());
    }

    // Begin an exclusive transaction so the entire migration is atomic.
    // If any step fails, the transaction is rolled back automatically when
    // `tx` is dropped, leaving the original data intact.
    let tx = conn.transaction()?;

    // Read old data before recreating tables.
    let provider_rows: Vec<(String, u64, u64, u64, u64)> = {
        let mut stmt = tx.prepare(
            "SELECT provider_name, input, output, requests, failures FROM provider_stats",
        )?;

        stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, u64>(1)?,
                row.get::<_, u64>(2)?,
                row.get::<_, u64>(3)?,
                row.get::<_, u64>(4)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .collect()
    };

    let model_rows: Vec<(String, String, u64, u64)> = {
        let mut stmt =
            tx.prepare("SELECT provider_name, model_name, input, output FROM model_stats")?;

        stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, u64>(2)?,
                row.get::<_, u64>(3)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .collect()
    };

    // Recreate tables with new schema.
    tx.execute_batch(
        "DROP TABLE IF EXISTS provider_stats;
         DROP TABLE IF EXISTS model_stats;
         CREATE TABLE provider_stats (
             provider_id       TEXT PRIMARY KEY,
             provider_name     TEXT NOT NULL,
             input             INTEGER NOT NULL DEFAULT 0,
             output            INTEGER NOT NULL DEFAULT 0,
             requests          INTEGER NOT NULL DEFAULT 0,
             failures          INTEGER NOT NULL DEFAULT 0,
             latency_total  INTEGER NOT NULL DEFAULT 0
         );
         CREATE TABLE model_stats (
             provider_id   TEXT NOT NULL,
             provider_name TEXT NOT NULL,
             model_name    TEXT NOT NULL,
             input         INTEGER NOT NULL DEFAULT 0,
             output        INTEGER NOT NULL DEFAULT 0,
             PRIMARY KEY (provider_id, model_name)
         );",
    )?;

    // Re-insert with UUIDs from config; orphaned rows get a fresh UUID.
    let mut id_cache: HashMap<String, String> = name_to_id.clone();

    for (name, input, output, requests, failures) in &provider_rows {
        let id = id_cache
            .entry(name.clone())
            .or_insert_with(|| uuid::Uuid::new_v4().to_string())
            .clone();
        tx.execute(
            "INSERT INTO provider_stats (provider_id, provider_name, input, output, requests, failures)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, name, input, output, requests, failures],
        )?;
    }

    for (name, model, input, output) in &model_rows {
        let id = id_cache
            .entry(name.clone())
            .or_insert_with(|| uuid::Uuid::new_v4().to_string())
            .clone();
        tx.execute(
            "INSERT INTO model_stats (provider_id, provider_name, model_name, input, output)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, name, model, input, output],
        )?;
    }

    tx.commit()?;
    tracing::info!("DB schema migrated to provider_id primary key");
    Ok(())
}

pub fn load_metrics(conn: &Connection) -> TokenMetrics {
    let mut metrics = TokenMetrics::default();

    if let Ok(mut stmt) =
        conn.prepare("SELECT provider_name, input, output, requests, failures, latency_total FROM provider_stats")
    {
        match stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, u64>(1)?,
                row.get::<_, u64>(2)?,
                row.get::<_, u64>(3)?,
                row.get::<_, u64>(4)?,
                row.get::<_, u64>(5)?,
            ))
        }) {
            Ok(rows) => {
                for row in rows.flatten() {
                    let s = metrics.by_provider.entry(row.0).or_default();
                    s.input = row.1;
                    s.output = row.2;
                    s.failures = row.4;
                    s.requests = row.3;
                    s.latency_total = row.5;
                }
            }
            Err(e) => tracing::warn!("Failed to load provider stats: {e}"),
        }
    }

    if let Ok(mut stmt) = conn
        .prepare("SELECT model_name, SUM(input), SUM(output) FROM model_stats GROUP BY model_name")
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

/// Deltas passed to `upsert_provider`.
pub(crate) struct ProviderDelta<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub input: u64,
    pub output: u64,
    pub requests: u64,
    pub failures: u64,
    pub latency: u64,
}

/// Accumulate token/request/failure deltas for a provider.
pub fn upsert_provider(conn: &Connection, d: &ProviderDelta<'_>) -> Result<()> {
    conn.execute(
        "INSERT INTO provider_stats (provider_id, provider_name, input, output, requests, failures, latency_total)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(provider_id) DO UPDATE SET
             provider_name    = excluded.provider_name,
             input            = provider_stats.input    + excluded.input,
             output           = provider_stats.output   + excluded.output,
             failures         = provider_stats.failures + excluded.failures,
             requests         = provider_stats.requests + excluded.requests,
             latency_total = provider_stats.latency_total + excluded.latency_total",
        params![d.id, d.name, d.input, d.output, d.requests, d.failures, d.latency],
    )?;
    Ok(())
}

/// Accumulate token delta for a specific (provider, model) pair.
pub fn upsert_model(
    conn: &Connection,
    provider_id: &str,
    provider_name: &str,
    model: &str,
    input_delta: u64,
    output_delta: u64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO model_stats (provider_id, provider_name, model_name, input, output)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(provider_id, model_name) DO UPDATE SET
             provider_name = excluded.provider_name,
             input  = model_stats.input  + excluded.input,
             output = model_stats.output + excluded.output",
        params![provider_id, provider_name, model, input_delta, output_delta],
    )?;
    Ok(())
}

/// Rename a provider: updates provider_name in all rows with the given provider_id.
pub fn rename_provider(conn: &Connection, provider_id: &str, new_name: &str) -> Result<()> {
    conn.execute(
        "UPDATE provider_stats SET provider_name = ?1 WHERE provider_id = ?2",
        params![new_name, provider_id],
    )?;
    conn.execute(
        "UPDATE model_stats SET provider_name = ?1 WHERE provider_id = ?2",
        params![new_name, provider_id],
    )?;
    Ok(())
}

pub fn load_provider_models(conn: &Connection) -> HashMap<String, Vec<String>> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    if let Ok(mut stmt) = conn.prepare(
        "SELECT provider_name, model_name FROM model_stats ORDER BY provider_name, model_name",
    ) && let Ok(rows) = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    }) {
        for (provider, model) in rows.flatten() {
            map.entry(provider).or_default().push(model);
        }
    }
    map
}

/// Ensure discovered models exist in model_stats (preserves existing usage data).
pub fn upsert_provider_models(
    conn: &mut Connection,
    provider_id: &str,
    provider_name: &str,
    models: &[String],
) -> Result<()> {
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare(
            "INSERT OR IGNORE INTO model_stats (provider_id, provider_name, model_name, input, output)
             VALUES (?1, ?2, ?3, 0, 0)",
        )?;
        for model in models {
            stmt.execute(params![provider_id, provider_name, model])?;
        }
    }
    tx.commit()
}

pub fn clear_all(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction()?;
    tx.execute_batch(
        "DELETE FROM provider_stats; DELETE FROM model_stats; DELETE FROM request_log;",
    )?;
    tx.commit()
}

pub fn clear_provider(conn: &mut Connection, provider_id: &str) -> Result<()> {
    let tx = conn.transaction()?;
    tx.execute(
        "DELETE FROM provider_stats WHERE provider_id = ?1",
        [provider_id],
    )?;
    tx.execute(
        "DELETE FROM model_stats WHERE provider_id = ?1",
        [provider_id],
    )?;
    tx.commit()
}

// ─── Request Log ─────────────────────────────────────────────────────────────

/// Insert a request log entry. Uses the entry's `id` as the primary key so that
/// in-process and bg-proxy entries share the same ID space within a session.
pub fn insert_request_log(
    conn: &Connection,
    entry: &crate::metrics::RequestLogEntry,
) -> Result<()> {
    let ts = entry
        .timestamp
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    conn.execute(
        "INSERT OR IGNORE INTO request_log
             (id, timestamp_ms, provider_name, model, status, latency_ms,
              input_tokens, output_tokens, is_stream, error, request_body, response_body)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            entry.id,
            ts,
            entry.provider,
            entry.model,
            entry.status,
            entry.latency_ms,
            entry.input_tokens,
            entry.output_tokens,
            entry.is_stream as i64,
            entry.error.as_deref(),
            entry.request_body.as_deref(),
            entry.response_body.as_deref(),
        ],
    )?;
    Ok(())
}

/// Update token counts and model name for a streaming request once the stream ends.
pub fn update_request_log_tokens(
    conn: &Connection,
    id: u64,
    input: u64,
    output: u64,
    model: Option<&str>,
) -> Result<()> {
    // Always update tokens; update model only when provided.
    conn.execute(
        "UPDATE request_log SET input_tokens = ?1, output_tokens = ?2,
         model = COALESCE(?3, model) WHERE id = ?4",
        params![input, output, model, id],
    )?;
    Ok(())
}

/// Load the most recent `limit` entries, ordered oldest-first (for TUI display).
pub fn load_recent_request_logs(
    conn: &Connection,
    limit: usize,
) -> Vec<crate::metrics::RequestLogEntry> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT id, timestamp_ms, provider_name, model, status, latency_ms,
                input_tokens, output_tokens, is_stream, error, request_body, response_body
         FROM request_log
         ORDER BY id DESC
         LIMIT ?1",
    ) else {
        return vec![];
    };

    let rows = stmt.query_map([limit as i64], |row| {
        let ts_ms: i64 = row.get(1)?;
        let is_stream: i64 = row.get(8)?;
        let error: Option<String> = row.get(9)?;
        let request_body: Option<String> = row.get(10)?;
        let response_body: Option<String> = row.get(11)?;
        Ok(crate::metrics::RequestLogEntry {
            id: row.get(0)?,
            timestamp: std::time::SystemTime::UNIX_EPOCH
                + std::time::Duration::from_millis(ts_ms as u64),
            provider: row.get(2)?,
            model: row.get(3)?,
            status: row.get(4)?,
            latency_ms: row.get(5)?,
            input_tokens: row.get(6)?,
            output_tokens: row.get(7)?,
            is_stream: is_stream != 0,
            error,
            request_body,
            response_body,
        })
    });

    match rows {
        Ok(iter) => {
            let mut v: Vec<_> = iter.flatten().collect();
            v.reverse(); // oldest-first for TUI
            v
        }
        Err(e) => {
            tracing::warn!("Failed to load request logs: {e}");
            vec![]
        }
    }
}

/// Remove old entries, keeping only the most recent `keep` rows.
pub fn trim_request_log(conn: &Connection, keep: usize) -> Result<()> {
    // Inner query walks the PK B-tree to find the cutoff id; outer DELETE uses a
    // PK range scan — both index-only, no full table scan. If fewer than `keep`
    // rows exist the inner query returns NULL and the DELETE is a no-op.
    conn.execute(
        "DELETE FROM request_log WHERE id < (
             SELECT id FROM request_log ORDER BY id DESC LIMIT 1 OFFSET ?1
         )",
        [(keep as i64) - 1],
    )?;
    Ok(())
}

pub fn max_request_log_id(conn: &Connection) -> u64 {
    conn.query_row("SELECT COALESCE(MAX(id), 0) FROM request_log", [], |row| {
        row.get(0)
    })
    .unwrap_or(0)
}

mod tests {
    #[allow(unused_imports)]
    use super::*;

    #[test]
    fn test_upsert_provider_failure_counting() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();

        let id = "test-id";
        let name = "test-provider";

        // Simulate 5 record_failure calls: each should write requests=1, failures=1
        for _ in 0..5 {
            upsert_provider(
                &conn,
                &ProviderDelta {
                    id,
                    name,
                    input: 0,
                    output: 0,
                    requests: 1,
                    failures: 1,
                    latency: 0,
                },
            )
            .unwrap();
        }

        let (req, fail): (u64, u64) = conn
            .query_row(
                "SELECT requests, failures FROM provider_stats WHERE provider_id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();

        assert_eq!(req, 5, "requests should be 5 after 5 failure writes");
        assert_eq!(fail, 5, "failures should be 5 after 5 failure writes");
    }

    #[test]
    fn test_upsert_provider_mixed() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();

        let id = "test-id";
        let name = "test-provider";

        // 3 failures (requests=1, failures=1 each)
        for _ in 0..3 {
            upsert_provider(
                &conn,
                &ProviderDelta {
                    id,
                    name,
                    input: 0,
                    output: 0,
                    requests: 1,
                    failures: 1,
                    latency: 0,
                },
            )
            .unwrap();
        }
        // 2 successes (requests=1, failures=0 each)
        for _ in 0..2 {
            upsert_provider(
                &conn,
                &ProviderDelta {
                    id,
                    name,
                    input: 100,
                    output: 50,
                    requests: 1,
                    failures: 0,
                    latency: 0,
                },
            )
            .unwrap();
        }

        let (req, fail, input, output): (u64, u64, u64, u64) = conn
            .query_row(
                "SELECT requests, failures, input, output FROM provider_stats WHERE provider_id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();

        assert_eq!(req, 5, "requests should be 3 failures + 2 successes = 5");
        assert_eq!(fail, 3, "failures should be 3");
        assert_eq!(input, 200, "input should be 2 * 100 = 200");
        assert_eq!(output, 100, "output should be 2 * 50 = 100");
    }

    #[test]
    fn test_upsert_after_clear() {
        let mut conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();

        let id = "test-id";
        let name = "test-provider";

        // Accumulate some data
        upsert_provider(
            &conn,
            &ProviderDelta {
                id,
                name,
                input: 100,
                output: 50,
                requests: 1,
                failures: 0,
                latency: 0,
            },
        )
        .unwrap();
        // Clear
        clear_provider(&mut conn, id).unwrap();

        // Now simulate 5 failures on fresh state
        for _ in 0..5 {
            upsert_provider(
                &conn,
                &ProviderDelta {
                    id,
                    name,
                    input: 0,
                    output: 0,
                    requests: 1,
                    failures: 1,
                    latency: 0,
                },
            )
            .unwrap();
        }

        let (req, fail): (u64, u64) = conn
            .query_row(
                "SELECT requests, failures FROM provider_stats WHERE provider_id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();

        assert_eq!(
            req, 5,
            "requests should be 5 after clear + 5 failure writes"
        );
        assert_eq!(
            fail, 5,
            "failures should be 5 after clear + 5 failure writes"
        );
    }
}
