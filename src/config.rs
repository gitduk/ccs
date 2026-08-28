use std::collections::HashMap;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{AppError, Result};

/// OpenAI API Version enumeration
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub enum OpenAiApiVersion {
    /// New Responses API (preferred, default)
    #[serde(rename = "responses")]
    #[default]
    Responses,
    /// Legacy Chat Completions API (for compatibility)
    #[serde(rename = "chat_completions")]
    ChatCompletions,
}

fn default_true() -> bool {
    true
}

// ─── Route Rules ─────────────────────────────────────────────────────────────

/// A single model-routing rule attached to a provider.
/// When enabled and the incoming model name matches `pattern`, the request
/// model is rewritten to `target` before sending to the selected provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteRule {
    /// Stable UUID for this route rule.
    pub id: String,
    /// Glob pattern matched against the request `model` field.
    /// Supports `*` as wildcard (e.g. `"claude-sonnet*"`, `"*opus*"`).
    pub pattern: String,
    /// Model name sent to upstream when this rule matches.
    /// Empty string = do not rewrite model (passthrough).
    #[serde(default)]
    pub target: String,
    /// When false this rule is skipped during routing.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl RouteRule {
    pub fn new(pattern: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            pattern: pattern.into(),
            target: String::new(),
            enabled: true,
        }
    }

    /// Returns true when this rule is enabled and `model` matches `pattern`.
    pub fn matches(&self, model: &str) -> bool {
        self.enabled && glob_match(&self.pattern, model)
    }

    /// Returns true when this rule has a valid pattern and target.
    /// When `known_models` is non-empty, the target must contain at least one
    /// model from the list (e.g. target `opencode-go/kimi-k2.7-code` passes
    /// when `kimi-k2.7-code` is in the known model list).
    pub fn is_valid(&self, known_models: &[String]) -> bool {
        !self.pattern.trim().is_empty()
            && !self.target.is_empty()
            && (known_models.is_empty()
                || known_models
                    .iter()
                    .any(|m| self.target.contains(m.as_str())))
    }
}

/// Glob pattern matching where `*` matches any sequence of characters.
/// `**` is treated the same as `*` — there is no directory-separator semantics.
///
/// Examples:
/// - `"claude-sonnet*"` matches `"claude-sonnet-4-20250514"`
/// - `"*opus*"`          matches `"anthropic/claude-opus-4"`
/// - `"claude-opus-4"`  only matches exactly `"claude-opus-4"`
pub fn glob_match(pattern: &str, text: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == text;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    let mut remaining = text;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            // First segment must be a strict prefix.
            if !remaining.starts_with(part) {
                return false;
            }
            remaining = &remaining[part.len()..];
        } else if i == parts.len() - 1 {
            // Last segment must be a strict suffix.
            return remaining.ends_with(part);
        } else {
            // Middle segments must appear somewhere in the remainder.
            match remaining.find(part) {
                Some(pos) => remaining = &remaining[pos + part.len()..],
                None => return false,
            }
        }
    }
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub current: String,
    #[serde(default = "default_listen")]
    pub listen: String,
    pub providers: IndexMap<String, Provider>,
    #[serde(default)]
    pub fallback: bool,
    #[serde(default)]
    pub db_path: Option<String>,
    /// Maximum number of recent requests shown in the TUI. Default: 100.
    #[serde(default = "default_request_log_limit")]
    pub request_log_limit: usize,
}

fn default_listen() -> String {
    "127.0.0.1:7896".to_string()
}

fn default_request_log_limit() -> usize {
    100
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provider {
    /// Stable UUID — assigned on first save, never changes even if name is renamed.
    #[serde(default)]
    pub id: String,
    pub base_url: String,
    pub api_key: String,
    pub api_format: ApiFormat,
    #[serde(default)]
    pub model_map: HashMap<String, String>,
    /// Model-routing rules. The first enabled rule whose pattern matches the
    /// incoming request model rewrites it before forwarding (see
    /// [`Provider::resolve_model`]); routes never influence provider choice.
    #[serde(default)]
    pub routes: Vec<RouteRule>,
    /// When false, this provider is skipped during request forwarding.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// When true, this provider participates in the fallback rotation.
    #[serde(default = "default_true")]
    pub fallback: bool,
    /// Only effective when api_format = OpenAI. See [`OpenAiApiVersion`] for variants.
    #[serde(default)]
    pub api_version: Option<OpenAiApiVersion>,
    /// Compat quirk: inject empty thinking blocks into assistant history
    /// turns when thinking is enabled. DeepSeek-compatible providers reject
    /// such history without them; real Anthropic should receive it untouched.
    /// Defaults to true to preserve existing behavior.
    #[serde(default = "default_true")]
    pub inject_thinking_history: bool,
    /// When true (DeepSeek Anthropic endpoint), assistant history must stay
    /// thinking-consistent even without an explicit `thinking` param: once any
    /// turn carries a thinking block, every earlier turn must too. Lets
    /// `patch_thinking_history` fire on a mixed history; defaults to false so
    /// tolerant providers (ark, genuine Anthropic) keep the old behavior.
    #[serde(default)]
    pub strict_thinking_history: bool,
    /// Shell command used by the Quota panel and main-table Quota column.
    #[serde(default, alias = "quota_curl", skip_serializing_if = "Option::is_none")]
    pub quota_command: Option<String>,
    /// Optional dedicated listening port for this provider. When set and the
    /// provider is enabled, ccs spawns a pinned listener on this port whose
    /// requests are routed exclusively to this provider (no fallback).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Pinned model for the Test/Retest action (`t` in the TUI) and format
    /// auto-detection on add. When set, testing always uses this exact model
    /// instead of auto-picking one from the fetched model list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_model: Option<String>,
    /// Cap the outgoing token limit (`max_tokens` on Anthropic /
    /// `max_output_tokens` / `max_completion_tokens` on OpenAI) at this value.
    /// `None` = pass the client's limit through untouched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens_cap: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ApiFormat {
    #[serde(rename = "anthropic")]
    Anthropic,
    #[serde(rename = "openai")]
    OpenAI,
}

impl std::fmt::Display for ApiFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiFormat::Anthropic => write!(f, "anthropic"),
            ApiFormat::OpenAI => write!(f, "openai"),
        }
    }
}

/// Resolve a configured API key: if it starts with '$', read from the
/// environment variable it names. Free function (not just `Provider::
/// resolve_api_key`) so callers that only have a raw key string — not a
/// full `Provider` — don't need to hand-roll the same `$`-prefix parsing.
pub fn resolve_api_key_str(raw: &str) -> Result<String> {
    if let Some(env_var) = raw.strip_prefix('$') {
        std::env::var(env_var).map_err(|e| {
            tracing::warn!("Failed to resolve API key from env var: {e}");
            AppError::Config("API key environment variable not found or invalid".to_string())
        })
    } else {
        Ok(raw.to_string())
    }
}

impl Provider {
    /// Resolve api_key: if it starts with '$', read from environment variable.
    pub fn resolve_api_key(&self) -> Result<String> {
        resolve_api_key_str(&self.api_key)
    }

    /// Build the auth header (name, value) for this provider's API format.
    pub fn auth_header(&self, api_key: &str) -> (&'static str, String) {
        match self.api_format {
            ApiFormat::Anthropic => ("x-api-key", api_key.to_string()),
            ApiFormat::OpenAI => ("authorization", format!("Bearer {api_key}")),
        }
    }

    /// Map model name using model_map, or return original.
    pub fn map_model(&self, model: &str) -> String {
        self.model_map
            .get(model)
            .cloned()
            .unwrap_or_else(|| model.to_string())
    }

    /// Resolve an incoming model for this provider: first match routes (glob
    /// patterns with targets), then apply model_map. Returns the effective
    /// model name and the matched route pattern (for metrics), if any.
    ///
    /// Why per-provider: each provider rewrites models via its own routes; a
    /// fallback to a different-format provider must apply *that* provider's
    /// rules, not the originally-selected provider's.
    pub fn resolve_model(&self, model: &str) -> (String, Option<String>) {
        let (after_routes, pattern) = self
            .routes
            .iter()
            .find(|r| r.matches(model))
            .filter(|r| !r.target.is_empty())
            .map(|r| (r.target.clone(), Some(r.pattern.clone())))
            .unwrap_or_else(|| (model.to_string(), None));
        (self.map_model(&after_routes), pattern)
    }

    /// Get the actual OpenAI API version (defaults to Responses API)
    pub fn openai_api_version(&self) -> &str {
        match self.openai_api_version_enum() {
            OpenAiApiVersion::ChatCompletions => "chat_completions",
            OpenAiApiVersion::Responses => "responses",
        }
    }

    pub fn openai_api_version_enum(&self) -> OpenAiApiVersion {
        self.api_version
            .clone()
            .unwrap_or(OpenAiApiVersion::Responses)
    }

    /// Check if this provider should use Responses API format
    pub fn uses_responses_api(&self) -> bool {
        self.api_format == ApiFormat::OpenAI
            && !matches!(self.api_version, Some(OpenAiApiVersion::ChatCompletions))
    }

    /// Compute the chat endpoint URL for this provider's API format and the
    /// given OpenAI API version (ignored for Anthropic format). Single source
    /// of truth for the format/version -> path mapping.
    pub fn endpoint_url(&self, version: OpenAiApiVersion) -> String {
        let base = self.base_url.trim_end_matches('/');
        match self.api_format {
            ApiFormat::Anthropic => format!("{base}/v1/messages"),
            ApiFormat::OpenAI => match version {
                OpenAiApiVersion::ChatCompletions => format!("{base}/v1/chat/completions"),
                OpenAiApiVersion::Responses => format!("{base}/v1/responses"),
            },
        }
    }

    /// Return the (endpoint URL, JSON body string) for a minimal probe request
    /// using this provider's configured API format and version. Used for curl
    /// preview/debug output; live proxy requests use the transform/forwarder path.
    pub fn chat_url_and_body(&self, model: &str) -> (String, String) {
        let version = self.openai_api_version_enum();
        let url = self.endpoint_url(version.clone());
        let body = if self.api_format == ApiFormat::OpenAI && version == OpenAiApiVersion::Responses
        {
            format!(r#"{{"model":"{model}","max_output_tokens":1,"input":"ping"}}"#)
        } else {
            format!(
                r#"{{"model":"{model}","max_tokens":1,"messages":[{{"role":"user","content":"ping"}}]}}"#
            )
        };
        (url, body)
    }
}

impl AppConfig {
    /// Get the current provider.
    pub fn current_provider(&self) -> Result<(&str, &Provider)> {
        self.providers
            .get(&self.current)
            .map(|p| (self.current.as_str(), p))
            .ok_or_else(|| AppError::ProviderNotFound(self.current.clone()))
    }

    /// Get the current provider, rejecting disabled ones.
    pub fn current_enabled_provider(&self) -> Result<(&str, &Provider)> {
        let (name, p) = self.current_provider()?;
        if !p.enabled {
            return Err(AppError::ProviderNotFound(format!(
                "{} (disabled)",
                self.current
            )));
        }
        Ok((name, p))
    }

    /// Build a name → id map for all providers (used for DB migration).
    pub fn name_to_id_map(&self) -> std::collections::HashMap<String, String> {
        self.providers
            .iter()
            .map(|(n, p)| (n.clone(), p.id.clone()))
            .collect()
    }

    /// Parse the port number out of the global `listen` address.
    /// Returns None if `listen` is malformed (we don't fail load on bad listen
    /// here — the bind step will surface the error).
    pub fn listen_port(&self) -> Option<u16> {
        self.listen.rsplit(':').next()?.parse().ok()
    }

    /// Validate provider port assignments. Rejects:
    /// - Two providers (regardless of enabled state) sharing the same port —
    ///   keeping the rule strict avoids "enable flip" surprises at runtime.
    /// - A provider port that collides with the global `listen` port.
    pub fn validate_ports(&self) -> Result<()> {
        let mut seen: HashMap<u16, &str> = HashMap::new();
        let listen_port = self.listen_port();
        for (name, provider) in &self.providers {
            let Some(port) = provider.port else { continue };
            if Some(port) == listen_port {
                return Err(AppError::Config(format!(
                    "Provider '{name}' port {port} conflicts with global listen address '{}'",
                    self.listen
                )));
            }
            if let Some(other) = seen.insert(port, name.as_str()) {
                return Err(AppError::Config(format!(
                    "Providers '{other}' and '{name}' both claim port {port}"
                )));
            }
        }
        Ok(())
    }

    /// Stable-partition providers so enabled ones come first and disabled ones
    /// last, preserving relative order within each group. Keeps the TUI's
    /// enabled/disabled fold in sync with row order.
    pub fn sort_providers_by_enabled(&mut self) {
        let mut enabled: Vec<(String, Provider)> = Vec::new();
        let mut disabled: Vec<(String, Provider)> = Vec::new();
        for (name, provider) in std::mem::take(&mut self.providers) {
            if provider.enabled {
                enabled.push((name, provider));
            } else {
                disabled.push((name, provider));
            }
        }
        for (name, provider) in enabled.into_iter().chain(disabled) {
            self.providers.insert(name, provider);
        }
    }

    pub fn resolve_db_path(&self) -> String {
        self.db_path.clone().unwrap_or_else(|| {
            if let Ok(dir) = std::env::var("CCS_CONFIG_DIR") {
                return PathBuf::from(dir).join("ccs.db").display().to_string();
            }
            dirs::home_dir()
                .map(|h| h.join(".ccs").join("ccs.db").display().to_string())
                .unwrap_or_else(|| {
                    tracing::warn!(
                        "Home directory not found; using /tmp for database (data loss risk)"
                    );
                    "/tmp/ccs.db".to_string()
                })
        })
    }
}

/// Get the config file path: ~/.ccs/config.json, or `$CCS_CONFIG_DIR/config.json`
/// when set — lets tests (and anyone running multiple isolated instances)
/// redirect config/pid-file I/O without ever touching the real one.
pub fn config_path() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("CCS_CONFIG_DIR") {
        return Ok(PathBuf::from(dir).join("config.json"));
    }
    let home =
        dirs::home_dir().ok_or_else(|| AppError::Config("Cannot find home directory".into()))?;
    Ok(home.join(".ccs").join("config.json"))
}

/// Load config from file. Returns default config if file doesn't exist.
/// Assigns stable UUIDs to any provider that doesn't have one yet and saves back.
pub fn load_config() -> Result<AppConfig> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(default_config());
    }
    let content = std::fs::read_to_string(&path)?;
    let mut config: AppConfig = serde_json::from_str(&content)?;
    let mut needs_save = false;
    for provider in config.providers.values_mut() {
        if provider.id.is_empty() {
            provider.id = Uuid::new_v4().to_string();
            needs_save = true;
        }
    }
    if needs_save {
        save_config(&config)?;
    }
    config.validate_ports()?;
    Ok(config)
}

/// Write a file atomically: write to a `.tmp` sibling with 0600 permissions,
/// fsync, then rename over the target — a crash never leaves a partial file.
pub(crate) fn write_file_atomic(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp_path = path.with_extension("json.tmp");
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp_path)?;
        f.write_all(content.as_bytes())?;
        f.sync_all()?;
    }
    #[cfg(not(unix))]
    std::fs::write(&tmp_path, &content)?;
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

/// Save config to file atomically (write to temp file, then rename).
pub fn save_config(config: &AppConfig) -> Result<()> {
    config.validate_ports()?;
    let path = config_path()?;
    let content = serde_json::to_string_pretty(config)?;
    write_file_atomic(&path, &content)
}

fn default_config() -> AppConfig {
    AppConfig {
        current: String::new(),
        listen: default_listen(),
        providers: IndexMap::new(),
        fallback: false,
        db_path: None,
        request_log_limit: default_request_log_limit(),
    }
}

/// Shared across every test module in the crate (not just this file's own
/// `tests` below) — `CCS_CONFIG_DIR` is process-global, so any test reading
/// or writing config through `App`/`config::save_config` must go through
/// this guard, on the same lock, or risk racing another test's override
/// under `cargo test`'s default parallelism. One such gap already caused a
/// test to silently overwrite a real `~/.ccs/config.json`.
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Mutex;

    pub struct ConfigDirGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl ConfigDirGuard {
        pub fn new() -> Self {
            static LOCK: Mutex<()> = Mutex::new(());
            let _lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let dir = format!("/tmp/ccs-test-confdir-{}", uuid::Uuid::new_v4());
            std::fs::create_dir_all(&dir).unwrap();
            // SAFETY: serialized by `_lock` — no other test can be
            // reading/writing CCS_CONFIG_DIR while this guard is alive.
            unsafe { std::env::set_var("CCS_CONFIG_DIR", &dir) };
            Self { _lock }
        }
    }

    impl Default for ConfigDirGuard {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Drop for ConfigDirGuard {
        fn drop(&mut self) {
            // SAFETY: see ConfigDirGuard::new.
            unsafe { std::env::remove_var("CCS_CONFIG_DIR") };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── glob_match ───────────────────────────────────────────────────────────

    #[test]
    fn glob_exact_match() {
        assert!(glob_match("claude-opus-4", "claude-opus-4"));
        assert!(!glob_match("claude-opus-4", "claude-opus-4-20250514"));
    }

    #[test]
    fn glob_suffix_wildcard() {
        assert!(glob_match("claude-sonnet*", "claude-sonnet-4-20250514"));
        assert!(glob_match("claude-sonnet*", "claude-sonnet"));
        assert!(!glob_match("claude-sonnet*", "claude-haiku-4"));
    }

    #[test]
    fn glob_prefix_wildcard() {
        assert!(glob_match("*opus*", "anthropic/claude-opus-4"));
        assert!(glob_match("*opus*", "opus"));
        assert!(!glob_match("*opus*", "haiku"));
    }

    #[test]
    fn glob_middle_wildcard() {
        assert!(glob_match("claude*4", "claude-sonnet-4"));
        assert!(glob_match("claude*4", "claude-opus-4"));
        assert!(!glob_match("claude*4", "claude-sonnet-3"));
    }

    #[test]
    fn glob_multiple_wildcards() {
        assert!(glob_match(
            "*claude*sonnet*",
            "anthropic/claude-sonnet-4-20250514"
        ));
        assert!(!glob_match("*claude*sonnet*", "anthropic/claude-opus-4"));
    }

    #[test]
    fn glob_star_only_matches_anything() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("*", ""));
    }

    #[test]
    fn glob_double_star_same_as_single() {
        assert!(glob_match("**", "anything"));
        assert!(glob_match("claude**sonnet", "claude-sonnet"));
    }

    // ─── RouteRule ────────────────────────────────────────────────────────────

    #[test]
    fn route_rule_matches_when_enabled() {
        let rule = RouteRule {
            id: "id".into(),
            pattern: "claude-sonnet*".into(),
            target: "mapped-model".into(),
            enabled: true,
        };
        assert!(rule.matches("claude-sonnet-4-20250514"));
        assert!(!rule.matches("claude-haiku-4"));
    }

    #[test]
    fn route_rule_disabled_never_matches() {
        let rule = RouteRule {
            id: "id".into(),
            pattern: "*".into(),
            target: "mapped-model".into(),
            enabled: false,
        };
        assert!(!rule.matches("anything"));
    }

    #[test]
    fn route_rule_is_valid_basic() {
        let rule = RouteRule {
            id: "id".into(),
            pattern: "claude-sonnet*".into(),
            target: "mapped-model".into(),
            enabled: true,
        };
        // No known_models constraint → valid if pattern and target are non-empty.
        assert!(rule.is_valid(&[]));
    }

    #[test]
    fn route_rule_is_invalid_empty_pattern() {
        let rule = RouteRule {
            id: "id".into(),
            pattern: "   ".into(),
            target: "mapped-model".into(),
            enabled: true,
        };
        assert!(!rule.is_valid(&[]));
    }

    #[test]
    fn route_rule_is_invalid_empty_target() {
        let rule = RouteRule {
            id: "id".into(),
            pattern: "claude-sonnet*".into(),
            target: String::new(),
            enabled: true,
        };
        assert!(!rule.is_valid(&[]));
    }

    #[test]
    fn route_rule_is_invalid_when_target_not_in_known_models() {
        let rule = RouteRule {
            id: "id".into(),
            pattern: "claude-sonnet*".into(),
            target: "unknown-model".into(),
            enabled: true,
        };
        let known = vec!["gpt-4o".to_string(), "gpt-4o-mini".to_string()];
        assert!(!rule.is_valid(&known));
    }

    #[test]
    fn route_rule_is_valid_when_target_in_known_models() {
        let rule = RouteRule {
            id: "id".into(),
            pattern: "claude-sonnet*".into(),
            target: "gpt-4o".into(),
            enabled: true,
        };
        let known = vec!["gpt-4o".to_string()];
        assert!(rule.is_valid(&known));
    }

    #[test]
    fn route_rule_is_valid_when_target_contains_known_model() {
        let rule = RouteRule {
            id: "id".into(),
            pattern: "*".into(),
            target: "opencode-go/kimi-k2.7-code".into(),
            enabled: true,
        };
        let known = vec!["kimi-k2.7-code".to_string()];
        assert!(rule.is_valid(&known));
    }

    // ─── Provider helpers ────────────────────────────────────────────────────

    fn make_provider(api_key: &str, api_format: ApiFormat) -> Provider {
        Provider {
            id: "test-id".into(),
            base_url: "https://api.example.com".into(),
            api_key: api_key.to_string(),
            api_format,
            model_map: HashMap::new(),
            routes: Vec::new(),
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

    #[test]
    fn resolve_api_key_plain_text() {
        let p = make_provider("sk-my-key", ApiFormat::Anthropic);
        assert_eq!(p.resolve_api_key().unwrap(), "sk-my-key");
    }

    #[test]
    fn resolve_api_key_from_env() {
        // SAFETY: single-threaded test, no concurrent env access.
        unsafe { std::env::set_var("TEST_CCS_API_KEY", "env-value-123") };
        let p = make_provider("$TEST_CCS_API_KEY", ApiFormat::Anthropic);
        assert_eq!(p.resolve_api_key().unwrap(), "env-value-123");
        unsafe { std::env::remove_var("TEST_CCS_API_KEY") };
    }

    #[test]
    fn resolve_api_key_missing_env_errors() {
        // SAFETY: single-threaded test, no concurrent env access.
        unsafe { std::env::remove_var("TEST_CCS_MISSING_KEY") };
        let p = make_provider("$TEST_CCS_MISSING_KEY", ApiFormat::Anthropic);
        assert!(p.resolve_api_key().is_err());
    }

    #[test]
    fn auth_header_anthropic_format() {
        let p = make_provider("key", ApiFormat::Anthropic);
        let (name, value) = p.auth_header("my-api-key");
        assert_eq!(name, "x-api-key");
        assert_eq!(value, "my-api-key");
    }

    #[test]
    fn auth_header_openai_format() {
        let p = make_provider("key", ApiFormat::OpenAI);
        let (name, value) = p.auth_header("my-api-key");
        assert_eq!(name, "authorization");
        assert_eq!(value, "Bearer my-api-key");
    }

    #[test]
    fn map_model_with_mapping() {
        let mut p = make_provider("key", ApiFormat::OpenAI);
        p.model_map.insert(
            "claude-sonnet-4-20250514".into(),
            "anthropic/claude-sonnet-4-20250514".into(),
        );
        assert_eq!(
            p.map_model("claude-sonnet-4-20250514"),
            "anthropic/claude-sonnet-4-20250514"
        );
    }

    #[test]
    fn map_model_passthrough_when_no_mapping() {
        let p = make_provider("key", ApiFormat::OpenAI);
        assert_eq!(p.map_model("claude-opus-4"), "claude-opus-4");
    }

    #[test]
    fn provider_fallback_defaults_true_for_legacy_configs() {
        let provider: Provider = serde_json::from_value(serde_json::json!({
            "id": "legacy-id",
            "base_url": "https://api.example.com",
            "api_key": "key",
            "api_format": "anthropic",
            "enabled": true,
            "notes": "",
            "routes": [],
            "model_map": {}
        }))
        .unwrap();
        assert!(provider.fallback);
    }

    #[test]
    fn openai_api_version_defaults_to_responses() {
        let p = make_provider("key", ApiFormat::OpenAI);
        assert_eq!(p.openai_api_version(), "responses");
    }

    #[test]
    fn openai_api_version_chat_completions() {
        let mut p = make_provider("key", ApiFormat::OpenAI);
        p.api_version = Some(OpenAiApiVersion::ChatCompletions);
        assert_eq!(p.openai_api_version(), "chat_completions");
    }

    #[test]
    fn uses_responses_api_true_for_openai_with_responses_version() {
        let p = make_provider("key", ApiFormat::OpenAI);
        assert!(p.uses_responses_api());
    }

    #[test]
    fn uses_responses_api_false_for_anthropic() {
        let p = make_provider("key", ApiFormat::Anthropic);
        assert!(!p.uses_responses_api());
    }

    #[test]
    fn uses_responses_api_false_for_chat_completions() {
        let mut p = make_provider("key", ApiFormat::OpenAI);
        p.api_version = Some(OpenAiApiVersion::ChatCompletions);
        assert!(!p.uses_responses_api());
    }

    // ─── AppConfig helpers ───────────────────────────────────────────────────

    fn make_config(current: &str, providers: &[(&str, bool)]) -> AppConfig {
        let mut map = IndexMap::new();
        for (name, enabled) in providers {
            let mut p = make_provider("key", ApiFormat::Anthropic);
            p.id = format!("id-{name}");
            p.enabled = *enabled;
            map.insert(name.to_string(), p);
        }
        AppConfig {
            current: current.to_string(),
            listen: "127.0.0.1:7896".into(),
            providers: map,
            fallback: false,
            db_path: None,
            request_log_limit: 100,
        }
    }

    #[test]
    fn current_provider_ok() {
        let cfg = make_config("prov-a", &[("prov-a", true)]);
        let (name, _) = cfg.current_provider().unwrap();
        assert_eq!(name, "prov-a");
    }

    #[test]
    fn current_provider_not_found() {
        let cfg = make_config("missing", &[("prov-a", true)]);
        assert!(cfg.current_provider().is_err());
    }

    #[test]
    fn current_enabled_provider_ok() {
        let cfg = make_config("prov-a", &[("prov-a", true)]);
        assert!(cfg.current_enabled_provider().is_ok());
    }

    #[test]
    fn current_enabled_provider_disabled_errors() {
        let cfg = make_config("prov-a", &[("prov-a", false)]);
        assert!(cfg.current_enabled_provider().is_err());
    }

    #[test]
    fn name_to_id_map_correct() {
        let cfg = make_config("prov-a", &[("prov-a", true), ("prov-b", true)]);
        let map = cfg.name_to_id_map();
        assert_eq!(map.get("prov-a").unwrap(), "id-prov-a");
        assert_eq!(map.get("prov-b").unwrap(), "id-prov-b");
    }

    #[test]
    fn sort_providers_by_enabled_partitions_stably() {
        let mut cfg = make_config(
            "prov-a",
            &[
                ("prov-a", true),
                ("prov-b", false),
                ("prov-c", true),
                ("prov-d", false),
            ],
        );
        cfg.sort_providers_by_enabled();
        let order: Vec<&str> = cfg.providers.keys().map(|k| k.as_str()).collect();
        assert_eq!(order, vec!["prov-a", "prov-c", "prov-b", "prov-d"]);
    }

    #[test]
    fn sort_providers_by_enabled_is_stable_within_groups() {
        let mut cfg = make_config(
            "prov-a",
            &[("prov-a", false), ("prov-b", false), ("prov-c", true)],
        );
        cfg.sort_providers_by_enabled();
        let order: Vec<&str> = cfg.providers.keys().map(|k| k.as_str()).collect();
        assert_eq!(order, vec!["prov-c", "prov-a", "prov-b"]);
    }
}
