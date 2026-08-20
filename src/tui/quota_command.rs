//! Quota command execution helpers.
//!
//! This module runs the saved shell command and converts its output into the
//! compact text shown in the provider table and quota preview panel.

use std::time::Duration;

use crate::config::Provider;
use crate::tui::state::QuotaResult;
use crate::tui::ui::format::truncate_chars;

const API_KEY: &str = "_API_KEY";
const BASE_URL: &str = "_BASE_URL";
const PROVIDER: &str = "_PROVIDER";

/// Single source for the names the quota panel advertises and `provider_env`
/// exports, so a rename can't drift between the hint and the shell.
pub const ENV_VARS: [&str; 3] = [API_KEY, BASE_URL, PROVIDER];

pub type ProviderEnv = [(&'static str, String); ENV_VARS.len()];

/// Variables exported to the quota command's shell, so a command can reference
/// `$_API_KEY` instead of carrying the secret in plaintext. Errors when the
/// provider's key is a `$VAR` reference that the environment doesn't define.
pub fn provider_env(name: &str, provider: &Provider) -> Result<ProviderEnv, String> {
    let api_key = provider.resolve_api_key().map_err(|e| e.to_string())?;
    Ok([
        (API_KEY, api_key),
        (BASE_URL, provider.base_url.clone()),
        (PROVIDER, name.to_string()),
    ])
}

/// Execute the stored Quota shell command and return a preview payload.
pub async fn run(command: &str, env: &[(&'static str, String)]) -> Result<QuotaResult, String> {
    use tokio::process::Command;

    let output = tokio::time::timeout(
        Duration::from_secs(15),
        Command::new("sh")
            .arg("-lc")
            .arg(command)
            .envs(env.iter().map(|(k, v)| (*k, v)))
            .output(),
    )
    .await
    .map_err(|_| "Command timed out after 15s".to_string())?
    .map_err(|e| format!("Command failed to start: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

    if !output.status.success() {
        let detail = if !stderr.is_empty() {
            stderr
        } else if !stdout.is_empty() {
            stdout
        } else {
            format!("exit status {}", output.status)
        };
        return Err(detail);
    }

    let output_text = if !stdout.is_empty() {
        truncate_chars(&stdout, 8000)
    } else if !stderr.is_empty() {
        truncate_chars(&stderr, 8000)
    } else {
        "<empty output>".to_string()
    };

    Ok(QuotaResult {
        output: output_text,
    })
}

pub fn cell_text(result: &QuotaResult) -> String {
    result
        .output
        .lines()
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| "<empty>".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(api_key: &str) -> Provider {
        let mut p = crate::tui::testing::tests::provider("pid");
        p.base_url = "https://example.com".to_string();
        p.api_key = api_key.to_string();
        p
    }

    #[tokio::test]
    async fn command_can_reference_provider_vars() {
        let env = provider_env("acme", &provider("secret-key")).unwrap();
        let result = run("echo \"$_PROVIDER|$_BASE_URL|$_API_KEY\"", &env)
            .await
            .unwrap();
        assert_eq!(result.output, "acme|https://example.com|secret-key");
    }

    #[tokio::test]
    async fn missing_env_key_reference_is_reported() {
        let err = provider_env("acme", &provider("$CCS_TEST_UNSET_KEY_VAR")).unwrap_err();
        assert!(!err.is_empty());
    }
}
