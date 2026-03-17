use ratatui::style::Color;

use crate::config::ApiFormat;

// Base palette
pub const PRIMARY: Color = Color::Cyan;
pub const SUCCESS: Color = Color::Green;
pub const WARNING: Color = Color::Yellow;
pub const ERROR: Color = Color::Red;
pub const MUTED: Color = Color::DarkGray;
pub const TEXT: Color = Color::White;
pub const HIGHLIGHT_BG: Color = Color::Indexed(236);

// Semantic: API format
pub fn format_color(fmt: &ApiFormat) -> Color {
    match fmt {
        ApiFormat::Anthropic => WARNING,
        ApiFormat::OpenAI => PRIMARY,
    }
}

// Semantic: server status
pub fn status_color(running: bool) -> Color {
    if running { SUCCESS } else { MUTED }
}
