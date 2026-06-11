use ratatui::style::Style;
use ratatui::text::Span;
use ratatui::widgets::Cell;

use super::super::theme::{self as t};

/// Truncate a string to `max` display columns (wide chars count as 2),
/// appending `…` if truncated. Use for table layout; `truncate_chars` for
/// plain char-count limits.
pub(crate) fn truncate_width(s: &str, max: usize) -> String {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    if s.width() <= max {
        return s.to_string();
    }
    let mut w = 0usize;
    let mut end = 0usize;
    for (i, ch) in s.char_indices() {
        let cw = ch.width().unwrap_or(0);
        if w + cw > max.saturating_sub(1) {
            break;
        }
        w += cw;
        end = i + ch.len_utf8();
    }
    format!("{}…", &s[..end])
}

/// Truncate a string to `max` characters, appending `…` if truncated.
pub(crate) fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

pub(crate) fn truncate_error(e: &str) -> String {
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

    truncate_chars(msg, MAX)
}

pub(crate) fn fmt_latency(ms: u64) -> String {
    if ms >= 1000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{ms}ms")
    }
}

pub(crate) fn format_tokens(n: u64) -> String {
    if n == 0 {
        "—".to_string()
    } else if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{n}")
    }
}

/// Max content width with a fallback default and an upper cap.
pub(crate) fn max_content_width(
    content_lens: impl Iterator<Item = usize>,
    default: usize,
    cap: usize,
) -> usize {
    content_lens.max().unwrap_or(default).min(cap)
}

/// Column width = max(header length, max content length), exact fit.
pub(crate) fn col_width(header: &str, content_lens: impl Iterator<Item = usize>) -> u16 {
    max_content_width(content_lens, 0, usize::MAX).max(header.len()) as u16
}

/// Display width of what `masked_api_key` renders for this key.
/// Derived from the actual masked string so the two can never drift.
pub(crate) fn api_key_display_len(key: &str) -> usize {
    match mask_api_key_str(key) {
        Some(masked) => masked.chars().count(),
        None if key.is_empty() => "(not set)".len(),
        None => key.chars().count(),
    }
}

/// Mask a raw API key for display: `abcd···wxyz` (long) or `····` (short).
/// Returns the key unchanged if it is empty or starts with `$` (env-var ref).
pub(crate) fn mask_api_key_str(key: &str) -> Option<String> {
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

pub(crate) fn masked_api_key(key: &str) -> Cell<'static> {
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

pub(crate) fn config_path_display() -> String {
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
pub(crate) fn strip_model_prefix(model: &str) -> &str {
    model.rfind('/').map_or(model, |i| &model[i + 1..])
}

/// Shorten a model name for compact display:
/// 1. Strip `org/` namespace prefix (via strip_model_prefix)
/// 2. Strip leading `claude-` if present — the family name adds no information
///    in a mixed-provider log where the provider column already gives context.
///
/// Examples:
///   `anthropic/claude-sonnet-4.6` → `sonnet-4.6`
///   `claude-haiku-3.5`            → `haiku-3.5`
///   `gpt-4o`                      → `gpt-4o`
///   `qwen/qwen3-plus:free`        → `qwen3-plus:free`
pub(crate) fn shorten_model_name(model: &str) -> &str {
    let stripped = strip_model_prefix(model);
    stripped.strip_prefix("claude-").unwrap_or(stripped)
}

/// Pack enabled routes into wrapped lines given the available text width.
/// Returns groups of routes, each group rendered on one line.
pub(crate) const DETAIL_HEIGHT: u16 = 4;
