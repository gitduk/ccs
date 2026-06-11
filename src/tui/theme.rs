use ratatui::style::Color;

// Base palette — One Dark family, all RGB for consistent rendering across terminal themes.
pub const PRIMARY: Color = Color::Rgb(97, 175, 239); // #61afef, blue
pub const SUCCESS: Color = Color::Rgb(152, 195, 121); // #98c379, green
pub const WARNING: Color = Color::Rgb(229, 192, 123); // #e5c07b, yellow
pub const ERROR: Color = Color::Rgb(224, 108, 117); // #e06c75, red
pub const MUTED: Color = Color::Rgb(92, 99, 112); // #5c6370, comment gray
pub const TEXT: Color = Color::Rgb(171, 178, 191); // #abb2bf, foreground
pub const HIGHLIGHT_BG: Color = Color::Rgb(62, 68, 81); // #3e4451, selection
pub const HIGHLIGHT_FG: Color = Color::Rgb(255, 255, 255);
