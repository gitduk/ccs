use ratatui::style::Color;

use crate::config::ApiFormat;

// Base palette — One Dark family, all RGB for consistent rendering across terminal themes.
pub const PRIMARY: Color = Color::Rgb(97, 175, 239); // #61afef, blue
pub const SUCCESS: Color = Color::Rgb(152, 195, 121); // #98c379, green
pub const WARNING: Color = Color::Rgb(229, 192, 123); // #e5c07b, yellow
pub const ERROR: Color = Color::Rgb(224, 108, 117); // #e06c75, red
pub const MUTED: Color = Color::Rgb(92, 99, 112); // #5c6370, comment gray
pub const TEXT: Color = Color::Rgb(171, 178, 191); // #abb2bf, foreground
pub const HIGHLIGHT_BG: Color = Color::Rgb(62, 68, 81); // #3e4451, selection
pub const HIGHLIGHT_FG: Color = Color::Rgb(255, 255, 255);

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
