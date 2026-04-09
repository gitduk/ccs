//! Quota command execution helpers.
//!
//! This module runs the saved shell command and converts its output into the
//! compact text shown in the provider table and quota preview panel.

use std::time::Duration;

use crate::tui::state::QuotaResult;
use crate::tui::ui::format::truncate_chars;

/// Execute the stored Quota shell command and return a preview payload.
pub async fn run(command: &str) -> Result<QuotaResult, String> {
    use tokio::process::Command;

    let output = tokio::time::timeout(
        Duration::from_secs(15),
        Command::new("sh").arg("-lc").arg(command).output(),
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
