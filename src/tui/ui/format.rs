use ratatui::style::Style;
use ratatui::text::Span;
use ratatui::widgets::Cell;

use super::super::theme::{self as t};

pub(super) fn truncate_error(e: &str) -> String {
    const MAX: usize = 30;

    // HTML/XML body returned instead of JSON — not useful to display verbatim.
    let trimmed = e.trim_start();
    if trimmed.starts_with('<') {
        return "non-JSON response (HTML/XML)".to_string();
    }

    // Strip verbose reqwest chain: "...: error sending request for url (...): <root cause>"
    // Pick the last colon-separated segment that is short enough and not a URL.
    let msg: &str = e
        .split(": ")
        .filter(|seg| !seg.starts_with("http") && seg.chars().count() <= MAX * 2)
        .last()
        .unwrap_or_else(|| e.split(':').next().unwrap_or(e));

    if msg.chars().count() > MAX {
        let truncated: String = msg.chars().take(MAX).collect();
        format!("{}…", truncated)
    } else {
        msg.to_string()
    }
}

pub(super) fn fmt_latency(ms: u64) -> String {
    if ms >= 1000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{ms}ms")
    }
}

pub(super) fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        format!("{n}")
    }
}

/// Max content width with a fallback default and an upper cap.
pub(super) fn max_content_width(
    content_lens: impl Iterator<Item = usize>,
    default: usize,
    cap: usize,
) -> usize {
    content_lens.max().unwrap_or(default).min(cap)
}

/// Column width = max(header length, max content length) + 4 gap.
pub(super) fn col_width(header: &str, content_lens: impl Iterator<Item = usize>) -> u16 {
    (max_content_width(content_lens, 0, usize::MAX).max(header.len()) + 4) as u16
}

pub(super) fn api_key_display_len(key: &str) -> usize {
    if key.is_empty() {
        "(not set)".len()
    } else if key.starts_with('$') {
        key.chars().count()
    } else if key.chars().count() > 8 {
        11 // "abcd···wxyz"
    } else {
        4 // "····"
    }
}

/// Mask a raw API key for display: `abcd···wxyz` (long) or `····` (short).
/// Returns the key unchanged if it is empty or starts with `$` (env-var ref).
pub(super) fn mask_api_key_str(key: &str) -> Option<String> {
    if key.is_empty() || key.starts_with('$') {
        return None;
    }
    let n = key.chars().count();
    Some(if n > 8 {
        let prefix: String = key.chars().take(4).collect();
        let suffix: String = key.chars().skip(n - 4).collect();
        format!("{prefix}···{suffix}")
    } else {
        "····".to_string()
    })
}

pub(super) fn masked_api_key(key: &str) -> Cell<'static> {
    match mask_api_key_str(key) {
        Some(masked) => Cell::from(Span::styled(masked, Style::default().fg(t::MUTED))),
        None if key.is_empty() => {
            Cell::from(Span::styled("(not set)", Style::default().fg(t::MUTED)))
        }
        None => Cell::from(Span::styled(
            key.to_string(),
            Style::default().fg(t::WARNING),
        )),
    }
}

pub(super) fn config_path_display() -> String {
    crate::config::config_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "~/.ccs/config.json".to_string())
}

/// Strip the `org/` namespace prefix from a model id, keeping only the part
/// after the last `/`. Returns the original string if there is no `/`.
///
/// Examples:
///   `qwen/qwen3.6-plus-preview:free` → `qwen3.6-plus-preview:free`
///   `claude-sonnet-4.6`              → `claude-sonnet-4.6`
pub(super) fn strip_model_prefix(model: &str) -> &str {
    model.rfind('/').map_or(model, |i| &model[i + 1..])
}
