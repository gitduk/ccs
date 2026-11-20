use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Cell;

use super::super::state::{SUGGEST_WINDOW_ROWS, SuggestState};
use super::super::theme::{self as t};

/// Split `text` at `cursor` into three spans — text before, the
/// highlighted character under the cursor (reversed, in `color`), and text
/// after — for rendering an editable field's caret in an Insert-mode popup.
pub(crate) fn cursor_split_spans(text: &str, cursor: usize, color: Color) -> Vec<Span<'static>> {
    let cursor_pos = cursor.min(text.len());
    let before = text[..cursor_pos].to_string();
    let cursor_char = text[cursor_pos..].chars().next().unwrap_or(' ');
    let after_start = cursor_pos + cursor_char.len_utf8().min(text.len() - cursor_pos);
    let after = text[after_start..].to_string();
    vec![
        Span::raw(before),
        Span::styled(
            cursor_char.to_string(),
            Style::default().fg(color).add_modifier(Modifier::REVERSED),
        ),
        Span::raw(after),
    ]
}

/// The search/filter row shared by every panel list: label, then the value
/// with the caret shown while the box has focus, plain muted text otherwise.
pub(crate) fn search_line(
    label: &str,
    field: &super::super::state::FormField,
    active: bool,
) -> Line<'static> {
    let mut spans = vec![Span::styled(
        format!("{label}  "),
        Style::default().fg(t::MUTED),
    )];
    if active {
        spans.extend(cursor_split_spans(&field.value, field.cursor, t::PRIMARY));
    } else {
        spans.push(Span::styled(
            field.value.clone(),
            Style::default().fg(t::MUTED),
        ));
    }
    Line::from(spans)
}

/// Dropdown list presentation.
pub(crate) enum SuggestionStyle {
    /// Routes target: titled list with (n/m) counter, indented, ▶ highlight.
    Classic,
    /// Fixed Format-field options: titled list of the API formats.
    Options,
}

/// Render a dropdown list (`SUGGEST_WINDOW_ROWS`-row window, scrolled to the
/// highlight). Empty list → empty Vec, so callers can unconditionally `extend`.
pub(crate) fn render_suggestion_lines(
    suggestions: &[&str],
    suggest: &SuggestState,
    prov_color: Color,
    style: SuggestionStyle,
) -> Vec<Line<'static>> {
    if suggestions.is_empty() {
        return vec![];
    }

    let mut lines = Vec::new();
    let total = suggestions.len();
    // A list that shrank under a stale offset would otherwise index past its end.
    let scroll = suggest
        .scroll
        .min(total.saturating_sub(SUGGEST_WINDOW_ROWS));
    let window = &suggestions[scroll..total.min(scroll + SUGGEST_WINDOW_ROWS)];

    let (title, counter) = match style {
        SuggestionStyle::Classic => ("Suggestions", total > SUGGEST_WINDOW_ROWS),
        SuggestionStyle::Options => ("Options", false),
    };
    let head = if counter {
        format!("  ── {title} ({}/{}) ", scroll + window.len(), total)
    } else {
        format!("  ── {title} ")
    };
    // Pad with ─ so every titled header spans the same fixed width.
    let dashes = "─".repeat(41usize.saturating_sub(head.chars().count()));
    let scroll_hint = format!("{head}{dashes}");
    lines.push(Line::from(Span::styled(
        scroll_hint,
        Style::default().fg(t::MUTED),
    )));

    for (wi, model) in window.iter().enumerate() {
        let is_hi = suggest.active && scroll + wi == suggest.idx;
        let span = if is_hi {
            Span::styled(
                format!("  ▶ {model}"),
                Style::default().fg(prov_color).add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(format!("    {model}"), Style::default().fg(t::MUTED))
        };
        lines.push(Line::from(span));
    }
    lines
}

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

/// Pad a string on the right to `width` display columns (wide chars count 2).
pub(crate) fn pad_width(s: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthStr;
    format!("{s}{}", " ".repeat(width.saturating_sub(s.width())))
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

/// Truncate an error string to `max` display columns: keep the HTTP status
/// and a sized slice of its detail (so a 429 rate-limit text stays actionable),
/// and strip verbose reqwest chains down to their root cause.
pub(crate) fn truncate_error_width(e: &str, max: usize) -> String {
    let max = max.max(1);

    // HTML/XML body returned instead of JSON — not useful to display verbatim.
    let trimmed = e.trim_start();
    if trimmed.starts_with('<') {
        return "non-JSON response (HTML/XML)".to_string();
    }

    // Structured "HTTP <status>: <detail>" from test_latency: keep the status
    // and truncate the detail, so a 429 rate-limit text stays actionable.
    // `status` is ASCII ("HTTP 400"), so chars == columns.
    if let Some((status, detail)) = e.split_once(": ")
        && status.starts_with("HTTP ")
    {
        let detail = detail.trim();
        let room = max.saturating_sub(status.chars().count() + 2);
        return if room == 0 || detail.is_empty() {
            status.to_string()
        } else {
            format!("{status}: {}", truncate_width(detail, room))
        };
    }

    // Strip verbose reqwest chain: "...: error sending request for url (...): <root cause>"
    // Pick the last colon-separated segment that isn't a URL; the winner is
    // truncated to fit below, so a tight budget never picks a wrong segment.
    let msg: &str = e
        .split(": ")
        .filter(|seg| !seg.starts_with("http"))
        .last()
        .unwrap_or(e);

    truncate_width(msg, max)
}

/// Widest a rendered attribute value may grow in a row.
pub(crate) const MAX_CELL_W: usize = 24;

/// Format a catalog attribute number for a column: thousands compact to `k`/`M`/`G`
/// (`1M`, `384k`), ordinary fractions keep up to three decimals without trailing
/// zeros (`0.15`), and values too small for that keep their exact form
/// (`0.000002` — per-token prices live down there).
pub(crate) fn fmt_number(v: f64) -> String {
    if !v.is_finite() {
        return "-".to_string();
    }
    let abs = v.abs();
    let compact = if abs >= 1e9 {
        Some((v / 1e9, "G"))
    } else if abs >= 1e6 {
        Some((v / 1e6, "M"))
    } else if abs >= 1e3 {
        Some((v / 1e3, "k"))
    } else {
        None
    };
    if let Some((scaled, suffix)) = compact {
        let text = format!("{scaled:.1}");
        let text = text.strip_suffix(".0").unwrap_or(&text);
        return format!("{text}{suffix}");
    }
    if v == 0.0 {
        return "0".to_string();
    }
    if abs < 0.001 {
        return format!("{v}");
    }
    let text = format!("{v:.3}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// One attribute's value for a model: `scale` and `prefix` applied to each path,
/// paths joined with `/` (`$0.15/$0.6`). `None` when no path carries a value.
pub(crate) fn fmt_attr_value(
    attr: &crate::catalog::ModelAttr,
    model: &crate::catalog::CatalogModel,
) -> Option<String> {
    let values: Vec<String> = attr
        .paths
        .iter()
        .filter_map(|path| model.attr(path).map(|raw| scaled_text(attr, raw)))
        .collect();
    (!values.is_empty()).then(|| values.join("/"))
}

/// A catalog value as a number. Prices arrive as strings (`"0.000002"`) as
/// often as numbers, so both forms go through the same numeric path.
pub(crate) fn attr_number(raw: &serde_json::Value) -> Option<f64> {
    match raw {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.parse::<f64>().ok(),
        _ => None,
    }
}

fn scaled_text(attr: &crate::catalog::ModelAttr, raw: &serde_json::Value) -> String {
    let text = match attr_number(raw) {
        Some(v) => fmt_number(v * attr.scale),
        // A string attribute is its own text: `to_string` on the JSON value
        // would render the quotes and escapes.
        None => match raw {
            serde_json::Value::String(s) => truncate_width(s, MAX_CELL_W),
            other => truncate_width(&other.to_string(), MAX_CELL_W),
        },
    };
    format!("{}{text}", attr.prefix)
}

/// Every attribute column's text for one model, joined for the row's right side.
/// One cell per configured column, in column order; a model without a value
/// there gets `-` so every row keeps the same number of columns.
pub(crate) fn fmt_model_cells(
    attrs: &[crate::catalog::ModelAttr],
    model: &crate::catalog::CatalogModel,
) -> Vec<String> {
    attrs
        .iter()
        .map(|attr| fmt_attr_value(attr, model).unwrap_or_else(|| "-".to_string()))
        .collect()
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

#[cfg(test)]
mod tests {
    use super::{
        SUGGEST_WINDOW_ROWS, SuggestState, SuggestionStyle, render_suggestion_lines,
        truncate_error_width,
    };

    #[test]
    fn a_scroll_offset_past_a_shrunken_list_still_renders() {
        // The filter shrank the list while the highlight kept its offset.
        let suggest = SuggestState {
            active: true,
            idx: 9,
            scroll: 9,
        };
        let lines = render_suggestion_lines(
            &["a", "b"],
            &suggest,
            ratatui::style::Color::Blue,
            SuggestionStyle::Classic,
        );

        assert_eq!(lines.len(), 1 + 2, "header plus every remaining row");
        assert!(lines.len() <= 1 + SUGGEST_WINDOW_ROWS);
    }

    #[test]
    fn fmt_number_keeps_small_prices_and_compacts_counts() {
        assert_eq!(super::fmt_number(0.15), "0.15");
        assert_eq!(super::fmt_number(0.6), "0.6");
        assert_eq!(super::fmt_number(2.0), "2");
        assert_eq!(super::fmt_number(0.000002), "0.000002");
        assert_eq!(super::fmt_number(1_000_000.0), "1M");
        assert_eq!(super::fmt_number(384_000.0), "384k");
        assert_eq!(super::fmt_number(0.0), "0");
    }

    #[test]
    fn an_attribute_renders_each_path_scaled_and_prefixed() {
        use crate::catalog::{CatalogModel, ModelAttr};
        use serde_json::json;

        let mut model = CatalogModel::name_only("m");
        // Per-token prices arrive as strings as often as numbers.
        model
            .attrs
            .insert("pricing.prompt".into(), json!("0.00000015"));
        model
            .attrs
            .insert("pricing.completion".into(), json!("0.0000006"));
        let price = ModelAttr {
            label: "price".into(),
            paths: vec!["pricing.prompt".into(), "pricing.completion".into()],
            scale: 1e6,
            prefix: "$".into(),
        };

        assert_eq!(
            super::fmt_attr_value(&price, &model).as_deref(),
            Some("$0.15/$0.6")
        );

        // A value already in the display unit needs no scaling.
        let mut ctx_model = CatalogModel::name_only("m");
        ctx_model
            .attrs
            .insert("limit.context".into(), json!(1000000));
        let context = ModelAttr {
            label: "ctx".into(),
            paths: vec!["limit.context".into()],
            scale: 1.0,
            prefix: String::new(),
        };
        assert_eq!(
            super::fmt_attr_value(&context, &ctx_model).as_deref(),
            Some("1M")
        );

        // A string attribute renders as its text, not as quoted JSON.
        let mut text_model = CatalogModel::name_only("m");
        text_model
            .attrs
            .insert("api.base_url".into(), json!("https://opencode.ai/v1"));
        let text = ModelAttr {
            label: "url".into(),
            paths: vec!["api.base_url".into()],
            scale: 1.0,
            prefix: String::new(),
        };
        assert_eq!(
            super::fmt_attr_value(&text, &text_model).as_deref(),
            Some("https://opencode.ai/v1")
        );

        // No path carries a value → no column text, but the cell stays so the
        // row keeps its column count.
        let empty = CatalogModel::name_only("bare");
        assert_eq!(super::fmt_attr_value(&price, &empty), None);
        assert_eq!(super::fmt_model_cells(&[price], &empty), vec!["-"]);
    }

    #[test]
    fn truncate_error_width_keeps_http_status_and_message() {
        let out = truncate_error_width(
            "HTTP 429: You have exceeded the rate limit for this model. Please retry in 60 seconds.",
            30,
        );
        assert!(out.starts_with("HTTP 429: You have exceeded"), "{out}");
        assert!(out.ends_with('…'), "{out}");
        assert!(out.chars().count() <= 30, "{out}");
    }

    #[test]
    fn truncate_error_width_short_status_without_detail_is_unchanged() {
        assert_eq!(truncate_error_width("HTTP 502", 30), "HTTP 502");
    }

    #[test]
    fn truncate_error_width_keeps_detail_starting_with_url() {
        let out = truncate_error_width(
            "HTTP 429: https://openrouter.ai/docs/rate-limits exceeded",
            30,
        );
        assert!(out.starts_with("HTTP 429: https://openrouter."), "{out}");
        assert!(out.chars().count() <= 30, "{out}");
    }

    #[test]
    fn truncate_error_width_picks_last_reqwest_segment() {
        assert_eq!(
            truncate_error_width(
                "error sending request for url (http://x): connection refused",
                30
            ),
            "connection refused"
        );
    }

    #[test]
    fn truncate_error_width_tight_budget_still_keeps_root_cause() {
        // A narrow row must not fall back to the verbose reqwest prefix:
        // the last non-URL segment wins regardless of the budget, and is
        // then cut to fit ("connection refused" → "conn…").
        let out = truncate_error_width(
            "error sending request for url (http://x): connection refused",
            5,
        );
        assert_eq!(out, "conn…", "{out}");
    }

    #[test]
    fn truncate_error_width_marks_html_bodies() {
        assert_eq!(
            truncate_error_width("<html><body>upstream down</body></html>", 30),
            "non-JSON response (HTML/XML)"
        );
    }

    #[test]
    fn truncate_error_width_wider_budget_shows_more_detail() {
        let out = truncate_error_width(
            "HTTP 400: Model 'fish-audio/s1-free' is not supported by this provider; choose one from the catalog.",
            80,
        );
        assert!(
            out.starts_with("HTTP 400: Model 'fish-audio/s1-free'"),
            "{out}"
        );
        assert!(out.chars().count() <= 80, "{out}");
    }
}
