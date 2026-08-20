use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use super::App;
use super::state::{MessageKind, TestEvent};

const TEST_TASK_TIMEOUT_SECS: u64 = 10;

pub(super) fn test_selected(app: &mut App) {
    let Some(name) = app.selected_name().map(|s| s.to_string()) else {
        return;
    };
    test_provider_by_name(app, &name);
    run_quota_for_name(app, &name);
}

pub(super) fn test_provider_by_name(app: &mut App, name: &str) {
    let Some(provider) = app.config.providers.get(name).cloned() else {
        return;
    };

    let cached = app
        .models
        .provider_models
        .get(name)
        .cloned()
        .filter(|s| !s.is_empty());

    // A pinned Test Model always wins over auto-picking one; otherwise fall
    // back to the fetch-then-test flow if no model list is known yet.
    let (candidates, known_models): (Vec<String>, Option<Vec<String>>) =
        match (provider.test_model.clone(), cached) {
            (Some(model), cached) => (vec![model], cached),
            (None, Some(supported)) => {
                // Pick the best test model:
                // 1. Most-used model from the provider list (by input + output tokens).
                // 2. Random model from the provider list (no usage data yet).
                let best_model: String = app
                    .metrics
                    .lock()
                    .ok()
                    .and_then(|m| {
                        supported
                            .iter()
                            .max_by_key(|model| {
                                m.by_model.get(*model).map_or(0, |s| s.input + s.output)
                            })
                            .filter(|model| m.by_model.contains_key(*model))
                            .map(|s| s.to_string())
                    })
                    .unwrap_or_else(|| pick_next(&supported));

                // Build a fallback candidate list: best_model first, then remaining
                // models (up to MAX_TEST_ATTEMPTS total). Used when best_model
                // errors so a broken model doesn't permanently block health-checking.
                const MAX_TEST_ATTEMPTS: usize = 3;
                let candidates: Vec<String> = std::iter::once(best_model.clone())
                    .chain(supported.iter().filter(|m| **m != best_model).cloned())
                    .take(MAX_TEST_ATTEMPTS)
                    .collect();

                (candidates, Some(supported))
            }
            (None, None) => {
                test_provider_after_add(app, name);
                return;
            }
        };

    let tx = app.tests.tx.clone();
    let name_owned = name.to_string();

    app.tests.pending.insert(name_owned.clone());
    app.set_message(format!("Testing {name}…"), MessageKind::Info);

    let client = app.tests.client.clone();
    tokio::spawn(async move {
        // candidates is always non-empty (a pinned model, or best_model at minimum).
        let mut result = None;
        for model in candidates {
            // Notify TUI which model we're about to test so the display updates
            // in real time when a fallback retry kicks in.
            let _ = tx.send(TestEvent::ModelSelected {
                provider: name_owned.clone(),
                model: model.clone(),
            });
            let r = match tokio::time::timeout(
                Duration::from_secs(TEST_TASK_TIMEOUT_SECS),
                crate::tester::test_latency(
                    &client,
                    &provider,
                    model.clone(),
                    known_models.clone(),
                ),
            )
            .await
            {
                Ok(r) => r,
                Err(_) => crate::tester::TestResult {
                    status: crate::tester::TestStatus::Error("Connection error".to_string()),
                    latency_ms: 0,
                    model_count: known_models.as_ref().map(|v| v.len()),
                    model_names: known_models.clone(),
                    tested_at: std::time::Instant::now(),
                    used_model: model.clone(),
                    tools_supported: None,
                    images_supported: None,
                },
            };
            let done = matches!(
                &r.status,
                // Success or auth failure — no point trying other models.
                crate::tester::TestStatus::Ok | crate::tester::TestStatus::AuthFailed
            );
            result = Some(r);
            if done {
                break;
            }
        }
        // result is guaranteed to be Some because candidates is non-empty.
        if let Some(r) = result {
            let _ = tx.send(TestEvent::Completed {
                provider: name_owned,
                result: r,
            });
        } else {
            // This should never happen, but log if it does.
            tracing::error!("BUG: test task completed with no result");
        }
    });
}

/// Called when no model list is known for the provider (first test or empty list).
/// Fetches the model list first, then picks a random model to run a latency test.
/// Passes the fetched list directly to `test_latency` to avoid a redundant fetch.
pub(super) fn test_provider_after_add(app: &mut App, name: &str) {
    let Some(provider) = app.config.providers.get(name) else {
        return;
    };
    let provider = provider.clone();
    let tx = app.tests.tx.clone();
    let name_owned = name.to_string();

    app.tests.pending.insert(name_owned.clone());
    // Model is unknown until the async fetch completes; leave entry absent so
    // the render falls back to "—".
    app.set_message(format!("Testing {name}…"), MessageKind::Info);

    let client = app.tests.client.clone();
    tokio::spawn(async move {
        // Record the test start time before any I/O so the timestamp reflects
        // when the test was initiated, not when a failure was detected.
        let tested_at = std::time::Instant::now();

        // Step 1: fetch model list.
        let models = crate::tester::fetch_provider_models(&client, &provider).await;

        if models.is_empty() {
            // Provider returned no models — report the error without writing an
            // empty list to DB (model_names: None keeps provider_models untouched).
            let result = crate::tester::TestResult {
                status: crate::tester::TestStatus::Error("No models available".to_string()),
                latency_ms: 0,
                model_count: None,
                model_names: None,
                tested_at,
                used_model: String::new(),
                tools_supported: None,
                images_supported: None,
            };
            let _ = tx.send(TestEvent::Completed {
                provider: name_owned,
                result,
            });
            return;
        }

        // Step 2: pick a random model and run the latency test, passing the
        // already-fetched model list so test_latency skips a redundant fetch.
        let model = pick_next(&models);
        let _ = tx.send(TestEvent::ModelSelected {
            provider: name_owned.clone(),
            model: model.clone(),
        });
        let result = match tokio::time::timeout(
            Duration::from_secs(TEST_TASK_TIMEOUT_SECS),
            crate::tester::test_latency(&client, &provider, model.clone(), Some(models.clone())),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => crate::tester::TestResult {
                status: crate::tester::TestStatus::Error("Connection error".to_string()),
                latency_ms: 0,
                model_count: Some(models.len()),
                model_names: Some(models.clone()),
                tested_at: std::time::Instant::now(),
                used_model: model,
                tools_supported: None,
                images_supported: None,
            },
        };
        let _ = tx.send(TestEvent::Completed {
            provider: name_owned,
            result,
        });
    });
}

/// Run quota commands for all providers that have one configured.
pub(super) fn start_quota_queries(app: &mut App) {
    let names: Vec<String> = app
        .config
        .providers
        .iter()
        .filter(|(_, provider)| provider.quota_command.is_some())
        .map(|(name, _)| name.clone())
        .collect();

    for name in names {
        run_quota_for_name(app, &name);
    }
}

pub(super) fn run_quota_for_name(app: &mut App, name: &str) {
    let Some(provider) = app.config.providers.get(name) else {
        return;
    };
    let Some(command) = &provider.quota_command else {
        return;
    };
    let command = command.clone();
    let provider_id = provider.id.clone();
    let env = super::quota_command::provider_env(name, provider);

    start_quota_query(app, provider_id, command, env);
}

/// `pub(super)`: the quota panel's manual preview dispatches through this too.
pub(super) fn start_quota_query(
    app: &mut App,
    provider_id: String,
    command: String,
    env: Result<super::quota_command::ProviderEnv, String>,
) {
    use super::state::QuotaStatus;

    let tx = app.tests.tx.clone();

    app.quota_status
        .insert(provider_id.clone(), QuotaStatus::Running);

    tokio::spawn(async move {
        let result = match env {
            Ok(env) => super::quota_command::run(&command, &env).await,
            Err(e) => Err(e),
        };
        let _ = tx.send(TestEvent::QuotaCompleted {
            provider_id,
            result,
        });
    });
}

/// Pick a model from a non-empty slice using a module-level counter so
/// consecutive calls cycle through
/// models rather than all landing on the same index.
fn pick_next(items: &[String]) -> String {
    static CTR: AtomicUsize = AtomicUsize::new(0);
    let idx = CTR.fetch_add(1, Ordering::Relaxed) % items.len();
    items[idx].clone()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::config::{ApiFormat, AppConfig, Provider};
    use crate::tui::state::{
        App, FormField, LogsState, Mode, ModelsState, ProviderList, ServerStatus, TestState,
    };
    use indexmap::IndexMap;
    use ratatui::widgets::TableState;

    use crate::config::test_support::ConfigDirGuard;

    /// `pub(crate)`: reused by `quick_panel`'s tests, not just this module's.
    pub(crate) fn provider(id: &str) -> Provider {
        Provider {
            id: id.to_string(),
            base_url: "http://127.0.0.1:9".to_string(),
            api_key: "sk-test".to_string(),
            api_format: ApiFormat::Anthropic,
            model_map: Default::default(),
            routes: vec![],
            enabled: true,
            fallback: true,
            api_version: None,
            inject_thinking_history: true,
            quota_command: None,
            port: None,
            test_model: None,
        }
    }

    /// `pub(crate)`: reused by `quick_panel`'s tests, not just this module's.
    pub(crate) fn app_with_current(current: &str) -> App {
        let path = format!("/tmp/ccs-startup-test-{}.db", uuid::Uuid::new_v4());
        let mut providers = IndexMap::new();
        providers.insert("first".to_string(), provider("first-id"));
        providers.insert("second".to_string(), provider("second-id"));

        let db = crate::repo::Repository::open(&path);
        let mut table_state = TableState::default();
        table_state.select(Some(0));

        let mut app = App {
            config: AppConfig {
                current: current.to_string(),
                listen: "127.0.0.1:0".to_string(),
                providers,
                fallback: false,
                db_path: Some(path),
                request_log_limit: 100,
            },
            mode: Mode::Normal,
            terminal_focused: true,
            providers: ProviderList { table_state },
            form: None,
            message: None,
            confirm_action: None,
            should_quit: false,
            server_status: ServerStatus::Stopped,
            metrics: std::sync::Arc::new(std::sync::Mutex::new(Default::default())),
            tests: TestState::new(),
            db,
            bg_proxy_pid: None,
            models: ModelsState {
                provider_models: std::collections::HashMap::from([
                    ("first".to_string(), vec!["first-model".to_string()]),
                    ("second".to_string(), vec!["second-model".to_string()]),
                ]),
                search_field: FormField::search(),
                search_active: true,
                selected: 0,
                scroll: 0,
                pending_key: None,
            },
            request_log: std::sync::Arc::new(std::sync::Mutex::new(
                crate::metrics::RequestLog::default(),
            )),
            logs: LogsState {
                selected: 0,
                scroll: 0,
                detail_scroll: 0,
                detail_view_height: 0,
                pending_key: None,
            },
            message_log: std::collections::VecDeque::new(),
            seen_provider_errors: std::collections::HashMap::new(),
            pending_key: None,
            quota_status: std::collections::HashMap::new(),
            quota_form: None,
            quick_form: None,
            detail_line_count: 0,
            help_scroll: 0,
            sysinfo_sampler: crate::tui::sysinfo::SysInfoSampler::new(),
            config_needs_sync: false,
        };
        app.tests.client = reqwest::Client::builder()
            .timeout(Duration::from_millis(10))
            .connect_timeout(Duration::from_millis(10))
            .build()
            .unwrap();
        app
    }

    /// A pinned Test Model must win over the normal best-model auto-pick,
    /// even when a cached model list (with other candidates) already exists.
    #[tokio::test]
    async fn pinned_test_model_overrides_auto_selection() {
        let _guard = ConfigDirGuard::new();
        let mut app = app_with_current("first");
        app.config.providers.get_mut("first").unwrap().test_model =
            Some("pinned-model".to_string());
        app.models.provider_models.insert(
            "first".to_string(),
            vec!["other-a".to_string(), "other-b".to_string()],
        );

        test_provider_by_name(&mut app, "first");

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while app.tests.pending.contains("first") {
            if tokio::time::Instant::now() > deadline {
                panic!("pinned-model test never completed");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
            app.drain_test_results();
        }

        assert_eq!(
            app.tests.testing_model.get("first"),
            Some(&"pinned-model".to_string())
        );
    }

    /// Server that answers `/v1/messages` (Anthropic format) plus a small
    /// model list — reuses tester.rs's scenario server instead of a second
    /// axum test harness.
    async fn spawn_anthropic_only_server() -> String {
        use crate::tester::tests::{Scenario, spawn_scenario_server};
        spawn_scenario_server(Scenario {
            messages_ok: true,
            models_ok: true,
            ..Default::default()
        })
        .await
    }

    /// End-to-end: saving a brand-new provider through `App` spawns format
    /// detection in the background (form stays open, `detect_token` set),
    /// and once the result lands it finishes the insert and closes the form —
    /// exercising the full `save_form_and_close` -> `drain_test_results`
    /// hand-off, not just `detect_api_format` in isolation.
    #[tokio::test]
    async fn add_provider_detects_format_and_completes_save() {
        use crate::tui::state::{API_KEY_FIELD_IDX, BASE_URL_FIELD_IDX, NAME_FIELD_IDX};

        let _guard = ConfigDirGuard::new();
        let mut app = app_with_current("first");
        app.tests.client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .unwrap();

        let base_url = spawn_anthropic_only_server().await;

        app.add();
        {
            let form = app.form.as_mut().unwrap();
            form.fields[NAME_FIELD_IDX].value = "brand-new".to_string();
            form.fields[BASE_URL_FIELD_IDX].value = base_url;
            form.fields[API_KEY_FIELD_IDX].value = "sk-test".to_string();
        }

        app.save_form_and_close().unwrap();
        assert!(
            app.form.as_ref().is_some_and(|f| f.detect_token.is_some()),
            "form should stay open while detection runs"
        );
        assert!(!app.config.providers.contains_key("brand-new"));

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !app.config.providers.contains_key("brand-new") {
            if tokio::time::Instant::now() > deadline {
                panic!("format detection never completed");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
            app.drain_test_results();
        }

        assert!(
            app.form.is_none(),
            "form should close once detection succeeds"
        );
        let saved = &app.config.providers["brand-new"];
        assert_eq!(saved.api_format, ApiFormat::Anthropic);
        assert_eq!(saved.api_version, None);
        // Fallback isn't in the form (toggled via f/F on the main table instead).
        assert!(!saved.fallback);
    }

    /// If the user cancels the add (form closed) before detection resolves,
    /// the async result must be discarded instead of resurrecting the form
    /// or inserting a provider nobody asked for anymore.
    #[tokio::test]
    async fn cancelling_add_discards_late_format_detection_result() {
        use crate::tui::state::{API_KEY_FIELD_IDX, BASE_URL_FIELD_IDX, NAME_FIELD_IDX};

        let _guard = ConfigDirGuard::new();
        let mut app = app_with_current("first");
        app.tests.client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .unwrap();

        let base_url = spawn_anthropic_only_server().await;

        app.add();
        {
            let form = app.form.as_mut().unwrap();
            form.fields[NAME_FIELD_IDX].value = "abandoned".to_string();
            form.fields[BASE_URL_FIELD_IDX].value = base_url;
            form.fields[API_KEY_FIELD_IDX].value = "sk-test".to_string();
        }
        app.save_form_and_close().unwrap();
        assert!(app.form.as_ref().is_some_and(|f| f.detect_token.is_some()));

        // User bails out of the add entirely before detection reports back.
        app.form = None;

        // Give the background task time to finish and send its result, then
        // drain repeatedly — it must be discarded, not resurrect the form.
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            app.drain_test_results();
        }

        assert!(app.form.is_none());
        assert!(!app.config.providers.contains_key("abandoned"));
    }
}
