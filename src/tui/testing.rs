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
    if !app.config.providers.contains_key(name) {
        return;
    }

    // If no model list is known yet, fall back to the fetch-then-test flow.
    let supported = app.models.provider_models.get(name);
    let Some(supported) = supported.filter(|s| !s.is_empty()) else {
        test_provider_after_add(app, name);
        return;
    };

    let provider = app.config.providers[name].clone();
    let tx = app.tests.tx.clone();
    let name_owned = name.to_string();

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
                .max_by_key(|model| m.by_model.get(*model).map_or(0, |s| s.input + s.output))
                .filter(|model| m.by_model.contains_key(*model))
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| pick_next(supported));

    // Build a fallback candidate list: best_model first, then remaining models
    // (up to MAX_TEST_ATTEMPTS total). Used when best_model returns an Error so
    // a broken model doesn't permanently block health-checking the provider.
    const MAX_TEST_ATTEMPTS: usize = 3;
    let candidates: Vec<String> = std::iter::once(best_model.clone())
        .chain(
            supported
                .iter()
                .filter(|m| m.as_str() != best_model)
                .cloned(),
        )
        .take(MAX_TEST_ATTEMPTS)
        .collect();

    // Clone the known list so test_latency can skip the redundant /v1/models fetch.
    let known_models = supported.clone();

    app.tests.pending.insert(name_owned.clone());
    app.set_message(format!("Testing {name}…"), MessageKind::Info);

    let client = app.tests.client.clone();
    tokio::spawn(async move {
        // candidates always contains best_model, so the loop runs at least once.
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
                    Some(known_models.clone()),
                ),
            )
            .await
            {
                Ok(r) => r,
                Err(_) => crate::tester::TestResult {
                    status: crate::tester::TestStatus::Error("Connection error".to_string()),
                    latency_ms: 0,
                    model_count: Some(known_models.len()),
                    model_names: Some(known_models.clone()),
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
        // result is guaranteed to be Some because candidates is non-empty (always contains best_model).
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

pub(super) fn start_background_tests(app: &mut App) {
    let name = app.config.current.clone();
    test_provider_by_name(app, &name);
    start_quota_queries(app);
}

/// Run quota commands for all providers that have one configured.
pub(super) fn start_quota_queries(app: &mut App) {
    let jobs: Vec<(String, String)> = app
        .config
        .providers
        .values()
        .filter_map(|provider| {
            provider
                .quota_command
                .as_ref()
                .map(|command| (provider.id.clone(), command.clone()))
        })
        .collect();

    for (provider_id, command) in jobs {
        start_quota_query(app, provider_id, command);
    }
}

pub(super) fn run_quota_for_name(app: &mut App, name: &str) {
    let Some(provider) = app.config.providers.get(name) else {
        return;
    };
    let Some(command) = &provider.quota_command else {
        return;
    };

    start_quota_query(app, provider.id.clone(), command.clone());
}

fn start_quota_query(app: &mut App, provider_id: String, command: String) {
    use super::state::QuotaStatus;

    let tx = app.tests.tx.clone();

    app.quota_status
        .insert(provider_id.clone(), QuotaStatus::Running);

    tokio::spawn(async move {
        let result = super::quota_command::run(&command).await;
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
mod tests {
    use super::*;
    use crate::config::{ApiFormat, AppConfig, Provider};
    use crate::tui::state::{
        App, FormField, LogsState, Mode, ModelsState, ProviderList, ServerStatus, TestState,
    };
    use indexmap::IndexMap;
    use ratatui::widgets::TableState;

    fn provider(id: &str) -> Provider {
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
        }
    }

    fn app_with_current(current: &str) -> App {
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
            detail_line_count: 0,
            sysinfo_sampler: crate::tui::sysinfo::SysInfoSampler::new(),
        };
        app.tests.client = reqwest::Client::builder()
            .timeout(Duration::from_millis(10))
            .connect_timeout(Duration::from_millis(10))
            .build()
            .unwrap();
        app
    }

    #[tokio::test]
    async fn startup_tests_only_current_provider() {
        let mut app = app_with_current("second");

        start_background_tests(&mut app);

        assert!(app.tests.pending.contains("second"));
        assert!(!app.tests.pending.contains("first"));
    }
}
