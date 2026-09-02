use std::time::Duration;

use super::App;
use super::state::{MessageKind, TestEvent};

const TEST_TASK_TIMEOUT_SECS: u64 = 10;

/// Test a single explicit model for the provider (from the Models panel).
/// No retries: the user asked for this model specifically.
pub(super) fn test_specific_model(app: &mut App, provider_name: &str, model: &str) {
    let Some(provider) = app.config.providers.get(provider_name).cloned() else {
        return;
    };
    let tx = app.tests.tx.clone();
    let name = provider_name.to_string();
    let model = model.to_string();

    app.tests.pending.insert(name.clone());
    app.tests.testing_model.insert(name.clone(), model.clone());
    app.set_message(format!("Testing {model}…"), MessageKind::Info);

    let client = app.tests.client.clone();
    tokio::spawn(async move {
        let result = match tokio::time::timeout(
            Duration::from_secs(TEST_TASK_TIMEOUT_SECS),
            crate::tester::test_latency(&client, &provider, model.clone(), None),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => crate::tester::TestResult {
                status: crate::tester::TestStatus::Error("Connection error".to_string()),
                latency_ms: 0,
                model_count: None,
                model_names: None,
                tested_at: std::time::Instant::now(),
                used_model: model.clone(),
                tools_supported: None,
                images_supported: None,
            },
        };
        let _ = tx.send(TestEvent::Completed {
            provider: name,
            result,
        });
    });
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
            strict_thinking_history: false,
            quota_command: None,
            port: None,
            test_model: None,
            max_tokens_cap: None,
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
                db_path: Some(path),
                request_log_limit: 100,
            },
            mode: Mode::Normal,
            terminal_focused: true,
            providers: ProviderList {
                table_state,
                expanded: false,
            },
            form: None,
            message: None,
            confirm_action: None,
            should_quit: false,
            server_status: ServerStatus::Stopped,
            metrics: std::sync::Arc::new(std::sync::Mutex::new(Default::default())),
            tests: TestState::new(),
            db,
            bg_proxy_pid: None,
            bg_proxy_stale_version: None,
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

    /// If format detection fails (nothing answers on the endpoint), the form
    /// stays open with an error, and the add can still be completed by
    /// picking a format manually — the `a`/`o` form keys call this.
    #[tokio::test]
    async fn manual_format_saves_provider_after_detection_fails() {
        use crate::tui::state::{API_KEY_FIELD_IDX, BASE_URL_FIELD_IDX, NAME_FIELD_IDX};

        let _guard = ConfigDirGuard::new();
        let mut app = app_with_current("first");
        app.tests.client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .unwrap();

        // Everything 404s/400s: the model-list fetch fails, so detection
        // has no model to probe with and reports failure.
        let base_url = crate::tester::tests::spawn_scenario_server(Default::default()).await;

        app.add();
        {
            let form = app.form.as_mut().unwrap();
            form.fields[NAME_FIELD_IDX].value = "manual".to_string();
            form.fields[BASE_URL_FIELD_IDX].value = base_url;
            form.fields[API_KEY_FIELD_IDX].value = "sk-test".to_string();
        }
        app.save_form_and_close().unwrap();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while app
            .form
            .as_ref()
            .is_none_or(|f| f.detect_token.is_some() || f.error.is_none())
        {
            if tokio::time::Instant::now() > deadline {
                panic!("detection never reported back");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
            app.drain_test_results();
        }

        assert!(app.form.as_ref().is_some_and(|f| f.detect_failed));
        assert!(!app.config.providers.contains_key("manual"));

        app.save_form_manual_format(ApiFormat::Anthropic).unwrap();
        assert!(app.form.is_none(), "manual save should close the form");
        let saved = &app.config.providers["manual"];
        assert_eq!(saved.api_format, ApiFormat::Anthropic);
        assert_eq!(saved.api_version, None);
    }

    /// The Format field shows the provider's effective choice and switching
    /// it to an OpenAI variant persists `api_format` + `api_version`.
    #[tokio::test]
    async fn editing_provider_can_change_api_format_and_version() {
        use crate::tui::state::FORMAT_FIELD_IDX;

        let _guard = ConfigDirGuard::new();
        let mut app = app_with_current("first");
        app.start_edit();

        let form = app.form.as_ref().unwrap();
        assert_eq!(form.fields.len(), 4, "editor has a single Format field");
        assert_eq!(form.fields[FORMAT_FIELD_IDX].label, "Format");
        // Anthropic provider pre-fills "anthropic".
        assert_eq!(form.fields[FORMAT_FIELD_IDX].value, "anthropic");

        let form = app.form.as_mut().unwrap();
        form.fields[FORMAT_FIELD_IDX].value = "chat_completions".to_string();

        app.save_form_and_close().unwrap();
        assert!(app.form.is_none());

        let saved = &app.config.providers["first"];
        assert_eq!(saved.api_format, ApiFormat::OpenAI);
        assert_eq!(
            saved.api_version,
            Some(crate::config::OpenAiApiVersion::ChatCompletions)
        );
    }

    /// Editing an OpenAI provider pre-fills its Format field with the
    /// configured API version, and saving untouched preserves both fields.
    #[tokio::test]
    async fn editing_openai_provider_preserves_format() {
        use crate::tui::state::FORMAT_FIELD_IDX;

        let _guard = ConfigDirGuard::new();
        let mut app = app_with_current("first");
        {
            let p = app.config.providers.get_mut("first").unwrap();
            p.api_format = ApiFormat::OpenAI;
            p.api_version = Some(crate::config::OpenAiApiVersion::Responses);
        }

        app.start_edit();
        let form = app.form.as_ref().unwrap();
        assert_eq!(form.fields.len(), 4, "single Format field for OpenAI too");
        assert_eq!(form.fields[FORMAT_FIELD_IDX].value, "responses");

        app.save_form_and_close().unwrap();
        let saved = &app.config.providers["first"];
        assert_eq!(saved.api_format, ApiFormat::OpenAI);
        assert_eq!(
            saved.api_version,
            Some(crate::config::OpenAiApiVersion::Responses)
        );
    }

    /// A new provider saved with an explicit Format field skips auto-detection.
    #[tokio::test]
    async fn adding_provider_with_explicit_format_skips_detection() {
        use crate::tui::state::{
            API_KEY_FIELD_IDX, BASE_URL_FIELD_IDX, FORMAT_FIELD_IDX, NAME_FIELD_IDX,
        };

        let _guard = ConfigDirGuard::new();
        let mut app = app_with_current("first");

        app.add();
        {
            let form = app.form.as_mut().unwrap();
            form.fields[NAME_FIELD_IDX].value = "explicit".to_string();
            form.fields[BASE_URL_FIELD_IDX].value = "http://127.0.0.1:9".to_string();
            form.fields[API_KEY_FIELD_IDX].value = "sk-test".to_string();
            form.fields[FORMAT_FIELD_IDX].value = "anthropic".to_string();
        }

        app.save_form_and_close().unwrap();
        assert!(app.form.is_none(), "explicit format should close the form");
        let saved = &app.config.providers["explicit"];
        assert_eq!(saved.api_format, ApiFormat::Anthropic);
    }

    /// Focusing the Format field opens the three-choice dropdown; `↓` then
    /// `Enter` selects `chat_completions` and saving persists it as OpenAI.
    #[tokio::test]
    async fn format_dropdown_selects_choice() {
        use crate::tui::state::FORMAT_FIELD_IDX;
        use crossterm::event::{KeyCode, KeyModifiers};

        let _guard = ConfigDirGuard::new();
        let mut app = app_with_current("first");
        app.start_edit();
        // Focus the Format field (index 3).
        {
            let form = app.form.as_mut().unwrap();
            form.focused = FORMAT_FIELD_IDX;
        }
        assert!(
            app.form
                .as_ref()
                .is_some_and(|f| f.focused == FORMAT_FIELD_IDX),
            "Format field focused"
        );

        // `↓` opens the dropdown (highlight on first item); a second `↓`
        // moves the highlight to the second item (chat_completions).
        crate::tui::input::handle_key(&mut app, KeyCode::Down, KeyModifiers::NONE, &mut None)
            .unwrap();
        assert!(
            app.form.as_ref().is_some_and(|f| f.format_suggest.active),
            "dropdown active after ↓"
        );
        crate::tui::input::handle_key(&mut app, KeyCode::Down, KeyModifiers::NONE, &mut None)
            .unwrap();

        // `Enter` selects the highlighted choice (chat_completions).
        crate::tui::input::handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE, &mut None)
            .unwrap();
        assert_eq!(
            app.form.as_ref().unwrap().fields[FORMAT_FIELD_IDX].value,
            "chat_completions"
        );

        app.save_form_and_close().unwrap();
        let saved = &app.config.providers["first"];
        assert_eq!(saved.api_format, ApiFormat::OpenAI);
        assert_eq!(
            saved.api_version,
            Some(crate::config::OpenAiApiVersion::ChatCompletions)
        );
    }
    /// After detection fails, pressing `a` saves Anthropic even when the
    /// Format field now holds a typed OpenAI choice — the keypress wins.
    #[tokio::test]
    async fn manual_format_overrides_typed_format_field() {
        use crate::tui::state::{API_KEY_FIELD_IDX, BASE_URL_FIELD_IDX, FORMAT_FIELD_IDX, NAME_FIELD_IDX};

        let _guard = ConfigDirGuard::new();
        let mut app = app_with_current("first");
        app.tests.client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .unwrap();

        let base_url = crate::tester::tests::spawn_scenario_server(Default::default()).await;

        app.add();
        {
            let form = app.form.as_mut().unwrap();
            form.fields[NAME_FIELD_IDX].value = "override".to_string();
            form.fields[BASE_URL_FIELD_IDX].value = base_url;
            form.fields[API_KEY_FIELD_IDX].value = "sk-test".to_string();
        }
        // Empty Format field → triggers auto-detection, which fails here.
        app.save_form_and_close().unwrap();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while app
            .form
            .as_ref()
            .is_some_and(|f| f.detect_token.is_some() || f.error.is_none())
        {
            if tokio::time::Instant::now() > deadline {
                panic!("detection never reported back");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
            app.drain_test_results();
        }

        assert!(app.form.as_ref().is_some_and(|f| f.detect_failed));
        // User types an OpenAI choice, then presses `a` (Anthropic).
        app.form.as_mut().unwrap().fields[FORMAT_FIELD_IDX].value = "responses".to_string();
        app.save_form_manual_format(ApiFormat::Anthropic).unwrap();
        assert!(app.form.is_none(), "manual save should close the form");
        let saved = &app.config.providers["override"];
        assert_eq!(saved.api_format, ApiFormat::Anthropic);
        assert_eq!(saved.api_version, None);
    }
}
