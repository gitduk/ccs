//! Persistent test results: one JSON file per install (`~/.ccs/ccs.json`),
//! holding the latest `TestResult` per provider name. A plain file instead of
//! the SQLite DB so the store can evolve without a schema migration.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::tester::TestResult;

/// `$CCS_CONFIG_DIR/ccs.json`, or `~/.ccs/ccs.json` when unset.
pub fn store_path() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("CCS_CONFIG_DIR") {
        return Some(PathBuf::from(dir).join("ccs.json"));
    }
    dirs::home_dir().map(|h| h.join(".ccs").join("ccs.json"))
}

/// Load persisted test results; a missing or corrupt file yields an empty map.
pub fn load() -> HashMap<String, TestResult> {
    let Some(path) = store_path() else {
        return HashMap::new();
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return HashMap::new();
    };
    serde_json::from_str(&content).unwrap_or_else(|e| {
        tracing::warn!("Ignoring corrupt test result store {}: {e}", path.display());
        HashMap::new()
    })
}

/// Atomically persist the whole map, reusing the config file's atomic-write
/// pattern so a crash never leaves a partial file.
pub fn save(results: &HashMap<String, TestResult>) {
    let Some(path) = store_path() else {
        return;
    };
    let Ok(content) = serde_json::to_string_pretty(results) else {
        return;
    };
    if let Err(e) = crate::config::write_file_atomic(&path, &content) {
        tracing::warn!("Failed to persist test results to {}: {e}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_support::ConfigDirGuard;
    use crate::tester::{TestResult, TestStatus};

    fn sample_result(used_model: &str) -> TestResult {
        TestResult {
            status: TestStatus::Ok,
            latency_ms: 42,
            model_count: Some(1),
            model_names: Some(vec![used_model.to_string()]),
            tested_at: std::time::Instant::now(),
            used_model: used_model.to_string(),
            tools_supported: Some(true),
            images_supported: None,
        }
    }

    #[test]
    fn save_then_load_round_trips() {
        let _guard = ConfigDirGuard::new();
        let mut map = HashMap::new();
        map.insert("openc".to_string(), sample_result("deepseek-v4-flash"));
        map.insert("vllm".to_string(), sample_result("gpt-4o"));

        save(&map);
        let loaded = load();

        assert_eq!(loaded.len(), 2);
        let r = &loaded["openc"];
        assert!(matches!(r.status, TestStatus::Ok));
        assert_eq!(r.latency_ms, 42);
        assert_eq!(r.used_model, "deepseek-v4-flash");
        assert_eq!(r.tools_supported, Some(true));
        assert_eq!(r.images_supported, None);
    }

    #[test]
    fn load_missing_file_is_empty() {
        let _guard = ConfigDirGuard::new();
        assert!(load().is_empty());
    }

    #[test]
    fn load_corrupt_file_is_empty() {
        let _guard = ConfigDirGuard::new();
        let path = store_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{not json").unwrap();
        assert!(load().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn saved_file_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let _guard = ConfigDirGuard::new();
        let mut map = HashMap::new();
        map.insert("openc".to_string(), sample_result("m"));
        save(&map);

        let mode = std::fs::metadata(store_path().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn save_then_load_preserves_error_status() {
        let _guard = ConfigDirGuard::new();
        let mut map = HashMap::new();
        map.insert(
            "auth".to_string(),
            TestResult {
                status: TestStatus::AuthFailed,
                ..sample_result("m")
            },
        );
        map.insert(
            "err".to_string(),
            TestResult {
                status: TestStatus::Error("HTTP 429".to_string()),
                ..sample_result("m")
            },
        );
        save(&map);
        let loaded = load();

        assert!(matches!(loaded["auth"].status, TestStatus::AuthFailed));
        assert!(matches!(
            loaded["err"].status,
            TestStatus::Error(ref e) if e == "HTTP 429"
        ));
    }
}

