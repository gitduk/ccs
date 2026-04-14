use ratatui::style::Color;

use crate::config::ApiFormat;

// Base palette
pub const PRIMARY: Color = Color::Rgb(97, 175, 239); // #61afef, One Dark blue
pub const SUCCESS: Color = Color::Green;
pub const WARNING: Color = Color::Yellow;
pub const ERROR: Color = Color::Red;
pub const MUTED: Color = Color::DarkGray;
pub const TEXT: Color = Color::White;
pub const HIGHLIGHT_BG: Color = Color::Rgb(0, 95, 95); // #005f5f
pub const HIGHLIGHT_FG: Color = Color::White;

// Semantic: API format
pub fn format_color(fmt: &ApiFormat) -> Color {
    match fmt {
        ApiFormat::Anthropic => WARNING,
        ApiFormat::OpenAI => PRIMARY,
    }
}

/// Color for a route target string, based on the model platform.
/// Matches on well-known prefixes: claude → yellow, gpt/o1/o3/o4 → blue, gemini → cornflower blue, others → white.
pub fn route_target_color(target: &str) -> Color {
    let t = target.to_ascii_lowercase();
    if t.contains("claude") {
        WARNING // yellow
    } else if t.contains("gemini") {
        Color::Rgb(138, 180, 248) // #8AB4F8, Google blue
    } else if t.contains("gpt") || t.starts_with("o1") || t.starts_with("o3") || t.starts_with("o4")
    {
        Color::Rgb(16, 163, 127) // #10A37F, OpenAI teal
    } else {
        TEXT
    }
}
