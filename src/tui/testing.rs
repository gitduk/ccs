use super::App;
use super::state::MessageKind;

pub(super) fn test_selected(app: &mut App) {
    let Some(name) = app.selected_name().map(|s| s.to_string()) else {
        return;
    };
    test_provider_by_name(app, &name);
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
        .unwrap_or_else(|| pick_random(supported));

    // Clone the known list so test_latency can skip the redundant /v1/models fetch.
    let known_models = supported.clone();

    app.tests.pending.insert(name_owned.clone());
    app.set_message(format!("Testing {name}…"), MessageKind::Info);

    let client = app.tests.client.clone();
    tokio::spawn(async move {
        let result =
            crate::tester::test_latency(&client, &provider, best_model, Some(known_models)).await;
        let _ = tx.send((name_owned, result));
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
            };
            let _ = tx.send((name_owned, result));
            return;
        }

        // Step 2: pick a random model and run the latency test, passing the
        // already-fetched model list so test_latency skips a redundant fetch.
        let model = pick_random(&models);
        let result = crate::tester::test_latency(&client, &provider, model, Some(models)).await;
        let _ = tx.send((name_owned, result));
    });
}

pub(super) fn start_background_tests(app: &mut App) {
    let names: Vec<String> = app.config.providers.keys().cloned().collect();
    for name in names {
        test_provider_by_name(app, &name);
    }
}

/// Pick a model from a non-empty slice using a module-level counter so
/// consecutive calls (e.g. during start_background_tests) cycle through
/// models rather than all landing on the same index.
fn pick_random(items: &[String]) -> String {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static CTR: AtomicUsize = AtomicUsize::new(0);
    let idx = CTR.fetch_add(1, Ordering::Relaxed) % items.len();
    items[idx].clone()
}
