use std::collections::HashMap;
use std::path::PathBuf;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{AppError, Result};

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
    "0.0.0.0:7896".to_string()
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

    /// Map model name using model_map, or return original.
    pub fn map_model(&self, model: &str) -> String {
        self.model_map
            .get(model)
            .cloned()
            .unwrap_or_else(|| model.to_string())
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

/// Save config to file, creating parent directory if needed.
pub fn save_config(config: &AppConfig) -> Result<()> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(config)?;
    std::fs::write(&path, &content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
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
