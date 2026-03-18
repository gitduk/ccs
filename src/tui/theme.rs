use ratatui::style::Color;

use crate::config::ApiFormat;

// Base palette
pub const PRIMARY: Color = Color::Cyan;
pub const SUCCESS: Color = Color::Green;
pub const WARNING: Color = Color::Yellow;
pub const ERROR: Color = Color::Red;
pub const MUTED: Color = Color::DarkGray;
pub const TEXT: Color = Color::White;
pub const HIGHLIGHT_BG: Color = Color::Indexed(23);
pub const HIGHLIGHT_FG: Color = Color::White;

// Semantic: API format
pub fn format_color(fmt: &ApiFormat) -> Color {
    match fmt {
        ApiFormat::Anthropic => WARNING,
        ApiFormat::OpenAI => PRIMARY,
    }
}

/// Stable color for a provider, derived from its ID.
/// The same ID always maps to the same color regardless of list order.
pub fn provider_color(id: &str) -> Color {
    // 20 visually distinct colors covering the full hue wheel; green
    // is omitted to avoid confusion with the SUCCESS indicator.
    const PALETTE: &[Color] = &[
        Color::Cyan,
        Color::Magenta,
        Color::Yellow,
        Color::Indexed(39),  // dodger blue
        Color::Indexed(208), // orange
        Color::Indexed(81),  // sky blue
        Color::Indexed(213), // pink
        Color::Indexed(147), // lavender
        Color::Indexed(203), // salmon
        Color::Indexed(51),  // aqua
        Color::Indexed(220), // gold
        Color::Indexed(105), // medium purple
        Color::Indexed(159), // pale cyan
        Color::Indexed(223), // peach
        Color::Indexed(78),  // seafoam
        Color::Indexed(199), // deep pink
        Color::Indexed(75),  // cornflower blue
        Color::Indexed(171), // orchid
        Color::Indexed(215), // sandy brown
        Color::Indexed(123), // aquamarine
    ];
    // FNV-1a, then Murmur3-style finalisation to avalanche short strings.
    let mut h: u64 = 0xcbf29ce484222325;
    for byte in id.bytes() {
        h ^= byte as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51afd7ed558ccd);
    h ^= h >> 33;
    PALETTE[h as usize % PALETTE.len()]
}

