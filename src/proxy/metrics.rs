use std::collections::HashMap;
use std::sync::{Arc, Mutex};

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
}

impl TokenMetrics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_failure(&mut self, name: &str) {
        self.by_provider.entry(name.to_string()).or_default().failures += 1;
    }

    pub fn record_request(&mut self, name: &str) {
        self.by_provider.entry(name.to_string()).or_default().requests += 1;
    }

    pub fn record_tokens(&mut self, input: u64, output: u64, name: &str) {
        let s = self.by_provider.entry(name.to_string()).or_default();
        s.input += input;
        s.output += output;
    }

    pub fn record_model_tokens(&mut self, input: u64, output: u64, model: &str) {
        let s = self.by_model.entry(model.to_string()).or_default();
        s.input += input;
        s.output += output;
    }
}

pub type SharedMetrics = Arc<Mutex<TokenMetrics>>;
