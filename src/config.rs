use std::collections::HashMap;
use std::path::PathBuf;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{AppError, Result};

/// OpenAI API Version enumeration
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub enum OpenAiApiVersion {
    #[serde(rename = "responses")]
    #[default]
    Responses, // Default to new version
    #[serde(rename = "chat_completions")]
    ChatCompletions,
}

// Legacy string constants for compatibility
const API_VERSION_RESPONSES: &str = "responses";
const API_VERSION_CHAT_COMPLETIONS: &str = "chat_completions";

fn default_true() -> bool {
    true
}

// ─── Route Rules ─────────────────────────────────────────────────────────────

/// A single model-routing rule attached to a provider.
/// When enabled and the incoming model name matches `pattern`, this provider
/// is selected ahead of the global `current` setting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteRule {
    /// Stable UUID for this route rule.
    pub id: String,
    /// Glob pattern matched against the request `model` field.
    /// Supports `*` as wildcard (e.g. `"claude-sonnet*"`, `"*opus*"`).
    pub pattern: String,
    /// Model name sent to the upstream when this rule matches.
    /// Empty string = forward the original model name unchanged.
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
    /// When `known_models` is non-empty, the target must also be in the list.
    pub fn is_valid(&self, known_models: &[String]) -> bool {
        !self.pattern.trim().is_empty()
            && !self.target.is_empty()
            && (known_models.is_empty() || known_models.contains(&self.target))
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
}

fn default_listen() -> String {
    "127.0.0.1:7896".to_string()
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
    #[serde(default)]
    pub notes: String,
    /// Model-routing rules. The first enabled rule whose pattern matches the
    /// incoming request model causes this provider to be selected.
    #[serde(default)]
    pub routes: Vec<RouteRule>,
    /// When false, this provider is skipped during request forwarding.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// API version selection. Only effective when api_format = "openai"
    /// "responses" = New Responses API (preferred)
    /// "chat_completions" = Legacy Chat Completions API (compatibility)
    /// None = Auto-detect (try new version first, fallback to legacy)
    #[serde(default)]
    pub api_version: Option<String>,
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

impl Provider {
    /// Resolve api_key: if it starts with '$', read from environment variable.
    pub fn resolve_api_key(&self) -> Result<String> {
        if let Some(env_var) = self.api_key.strip_prefix('$') {
            std::env::var(env_var)
                .map_err(|_| AppError::Config(format!("Environment variable '{env_var}' not set")))
        } else {
            Ok(self.api_key.clone())
        }
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

    /// Get the actual OpenAI API version (defaults to Responses API)
    pub fn openai_api_version(&self) -> &str {
        match self.api_version.as_deref() {
            Some(API_VERSION_CHAT_COMPLETIONS) => API_VERSION_CHAT_COMPLETIONS,
            Some(API_VERSION_RESPONSES) => API_VERSION_RESPONSES,
            _ => API_VERSION_RESPONSES, // Default to new version
        }
    }

    /// Check if this provider should use Responses API format
    pub fn uses_responses_api(&self) -> bool {
        self.api_format == ApiFormat::OpenAI && self.openai_api_version() == API_VERSION_RESPONSES
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

    pub fn resolve_db_path(&self) -> String {
        self.db_path.clone().unwrap_or_else(|| {
            dirs::home_dir()
                .map(|h| h.join(".ccs").join("ccs.db").display().to_string())
                .unwrap_or_else(|| ".ccs/ccs.db".to_string())
        })
    }
}

/// Get the config file path: ~/.ccs/config.json
pub fn config_path() -> Result<PathBuf> {
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
    Ok(config)
}

/// Save config to file atomically (write to temp file, then rename).
pub fn save_config(config: &AppConfig) -> Result<()> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(config)?;
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
    std::fs::rename(&tmp_path, &path)?;
    Ok(())
}

fn default_config() -> AppConfig {
    AppConfig {
        current: String::new(),
        listen: default_listen(),
        providers: IndexMap::new(),
        fallback: false,
        db_path: None,
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

    // ─── Provider helpers ────────────────────────────────────────────────────

    fn make_provider(api_key: &str, api_format: ApiFormat) -> Provider {
        Provider {
            id: "test-id".into(),
            base_url: "https://api.example.com".into(),
            api_key: api_key.to_string(),
            api_format,
            model_map: HashMap::new(),
            notes: String::new(),
            routes: Vec::new(),
            enabled: true,
            api_version: None,
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
    fn openai_api_version_defaults_to_responses() {
        let p = make_provider("key", ApiFormat::OpenAI);
        assert_eq!(p.openai_api_version(), "responses");
    }

    #[test]
    fn openai_api_version_chat_completions() {
        let mut p = make_provider("key", ApiFormat::OpenAI);
        p.api_version = Some("chat_completions".into());
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
        p.api_version = Some("chat_completions".into());
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
}
