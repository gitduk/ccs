use super::state::MessageKind;
use super::App;

pub(super) fn test_selected(app: &mut App) {
    let Some(name) = app.selected_name().map(|s| s.to_string()) else {
        return;
    };
    test_provider_by_name(app, &name);
}

pub(super) fn test_provider_by_name(app: &mut App, name: &str) {
    let Some(provider) = app.config.providers.get(name) else {
        return;
    };
    let provider = provider.clone();
    let tx = app.test_tx.clone();
    let name_owned = name.to_string();

    // Pick the best test model:
    // 1. Most-used model from the provider's supported list (intersection with usage data).
    // 2. Random model from the supported list (no usage intersection).
    // 3. Globally most-used model (supported list empty).
    let best_model: Option<String> = app.metrics.lock().ok().and_then(|m| {
        let supported = app.provider_models.get(name);
        if let Some(supported) = supported.filter(|s| !s.is_empty()) {
            // Cases 1 & 2: provider has a known model list.
            let best = supported
                .iter()
                .filter(|model| m.by_model.contains_key(*model))
                .max_by_key(|model| {
                    m.by_model
                        .get(*model)
                        .map(|s| s.input + s.output)
                        .unwrap_or(0)
                });
            best.or_else(|| {
                let idx = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.subsec_nanos() as usize)
                    .unwrap_or(0);
                supported.get(idx % supported.len())
            })
            .map(|s| s.to_string())
        } else {
            // Case 3: no supported list — use the globally most-used model.
            m.by_model
                .iter()
                .max_by_key(|(_, s)| s.input + s.output)
                .map(|(model, _)| model.clone())
        }
    });

    app.pending_tests.insert(name_owned.clone());
    app.set_message(format!("Testing {name}…"), MessageKind::Info);

    let client = app.test_client.clone();
    tokio::spawn(async move {
        let result = crate::test_provider::test_connectivity(&client, &provider, best_model).await;
        let _ = tx.send((name_owned, result));
    });
}

pub(super) fn start_background_tests(app: &mut App) {
    let names: Vec<String> = app.config.providers.keys().cloned().collect();
    for name in names {
        test_provider_by_name(app, &name);
    }
}
