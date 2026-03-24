use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
pub struct LastRequestError {
    pub status: u16,
    pub model: String,
    /// Route pattern that matched (e.g. `claude-*`), empty if no routing.
    pub pattern: String,
    pub message: String,
}

#[derive(Debug, Default, Clone)]
pub struct ProviderStats {
    pub input: u64,
    pub output: u64,
    pub requests: u64,
    pub failures: u64,
}

#[derive(Debug, Default, Clone)]
pub struct ModelStats {
    pub input: u64,
    pub output: u64,
}

#[derive(Debug, Default, Clone)]
pub struct TokenMetrics {
    pub by_provider: HashMap<String, ProviderStats>,
    pub by_model: HashMap<String, ModelStats>,
    /// Last request error per provider name; cleared on next successful request.
    pub last_error: HashMap<String, LastRequestError>,
}

impl TokenMetrics {
    pub fn record_error(
        &mut self,
        name: &str,
        status: u16,
        model: &str,
        pattern: &str,
        message: &str,
    ) {
        self.last_error.insert(
            name.to_string(),
            LastRequestError {
                status,
                model: model.to_string(),
                pattern: pattern.to_string(),
                message: message.to_string(),
            },
        );
    }

    pub fn clear_error(&mut self, name: &str) {
        self.last_error.remove(name);
    }
}

pub type SharedMetrics = Arc<Mutex<TokenMetrics>>;
