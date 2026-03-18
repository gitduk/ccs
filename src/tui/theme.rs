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

/// Color for the provider at the given position in the provider list.
/// Using index (not name hash) guarantees no two providers share a color.
pub fn provider_color(index: usize) -> Color {
    const PALETTE: &[Color] = &[
        Color::Cyan,
        Color::Green,
        Color::Magenta,
        Color::Yellow,
        Color::Indexed(208), // orange
        Color::Indexed(81),  // sky blue
        Color::Indexed(118), // lime
        Color::Indexed(213), // pink
    ];
    PALETTE[index % PALETTE.len()]
}
