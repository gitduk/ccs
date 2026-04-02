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
            provider_id   TEXT PRIMARY KEY,
            provider_name TEXT NOT NULL,
            input         INTEGER NOT NULL DEFAULT 0,
            output        INTEGER NOT NULL DEFAULT 0,
            requests      INTEGER NOT NULL DEFAULT 0,
            failures      INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS model_stats (
            provider_id   TEXT NOT NULL,
            provider_name TEXT NOT NULL,
            model_name    TEXT NOT NULL,
            input         INTEGER NOT NULL DEFAULT 0,
            output        INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (provider_id, model_name)
        );",
    )?;
    Ok(())
}

/// Migrate old schema (provider_name as PK, no provider_id column) to new schema.
/// name_to_id maps provider_name → provider UUID from config.
/// Safe to call on an already-migrated DB (no-op if provider_id column exists).
pub fn migrate_schema(db: &SharedDb, name_to_id: &HashMap<String, String>) {
    let Ok(mut conn) = db.lock() else { return };
    if let Err(e) = do_migrate(&mut conn, name_to_id) {
        tracing::warn!("DB schema migration failed: {e}");
    }
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
             provider_id   TEXT PRIMARY KEY,
             provider_name TEXT NOT NULL,
             input         INTEGER NOT NULL DEFAULT 0,
             output        INTEGER NOT NULL DEFAULT 0,
             requests      INTEGER NOT NULL DEFAULT 0,
             failures      INTEGER NOT NULL DEFAULT 0
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
        conn.prepare("SELECT provider_name, input, output, requests, failures FROM provider_stats")
    {
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
                    s.failures = row.4;
                    s.requests = row.3;
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

/// Accumulate token/request/failure deltas for a provider.
pub fn upsert_provider(
    conn: &Connection,
    provider_id: &str,
    provider_name: &str,
    input_delta: u64,
    output_delta: u64,
    req_delta: u64,
    fail_delta: u64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO provider_stats (provider_id, provider_name, input, output, requests, failures)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(provider_id) DO UPDATE SET
             provider_name = excluded.provider_name,
             input    = provider_stats.input    + excluded.input,
             output   = provider_stats.output   + excluded.output,
             failures = provider_stats.failures + excluded.failures,
             requests = provider_stats.requests + excluded.requests",
        params![
            provider_id,
            provider_name,
            input_delta,
            output_delta,
            req_delta,
            fail_delta
        ],
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
    conn: &Connection,
    provider_id: &str,
    provider_name: &str,
    models: &[String],
) -> Result<()> {
    let mut stmt = conn.prepare(
        "INSERT OR IGNORE INTO model_stats (provider_id, provider_name, model_name, input, output)
         VALUES (?1, ?2, ?3, 0, 0)",
    )?;
    for model in models {
        stmt.execute(params![provider_id, provider_name, model])?;
    }
    Ok(())
}

pub fn clear_all(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction()?;
    tx.execute_batch("DELETE FROM provider_stats; DELETE FROM model_stats;")?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_upsert_provider_failure_counting() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();

        let id = "test-id";
        let name = "test-provider";

        // Simulate 5 record_failure calls: each should write requests=1, failures=1
        for _ in 0..5 {
            upsert_provider(&conn, id, name, 0, 0, 1, 1).unwrap();
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
            upsert_provider(&conn, id, name, 0, 0, 1, 1).unwrap();
        }
        // 2 successes (requests=1, failures=0 each)
        for _ in 0..2 {
            upsert_provider(&conn, id, name, 100, 50, 1, 0).unwrap();
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
        upsert_provider(&conn, id, name, 100, 50, 1, 0).unwrap();
        // Clear
        clear_provider(&mut conn, id).unwrap();

        // Now simulate 5 failures on fresh state
        for _ in 0..5 {
            upsert_provider(&conn, id, name, 0, 0, 1, 1).unwrap();
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
