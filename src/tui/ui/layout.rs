use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::symbols::border;

use crate::config::RouteRule;

pub(super) const ROUTE_LABEL_WIDTH: usize = 7; // "Routes "

// ── Shared spacing tokens ──────────────────────────────────────────
// These keep border characters, title dash counts, and gap widths in
// sync across view.rs, logs.rs, and stats_panel.rs.

/// Horizontal dash character for title separators and dashed borders.
pub(super) const DASH: &str = "╌";

/// Number of dash chars flanking a section title (e.g. "╌╌ Title ╌╌╌").
pub(super) const TITLE_SIDE: usize = 2;

/// Gap (spaces) between dash fill and suffix text in title lines.
pub(super) const SUFFIX_GAP: usize = 1;

/// Border set: dashed `╌` on top, solid everywhere else.
pub(super) const BORDER_DASHED_TOP: border::Set = border::Set {
    horizontal_top: DASH,
    ..border::PLAIN
};

/// Border set: dashed `╎` on the left (inner column divider), solid elsewhere.
pub(super) const BORDER_INNER_DIVIDER: border::Set = border::Set {
    vertical_left: "╎",
    ..border::PLAIN
};

/// Pack enabled routes into wrapped lines given the available text width.
/// Returns groups of routes, each group rendered on one line.
pub(super) fn pack_routes<'a>(
    routes: &[&'a RouteRule],
    avail_width: usize,
) -> Vec<Vec<&'a RouteRule>> {
    use unicode_width::UnicodeWidthStr;

    let mut result: Vec<Vec<&RouteRule>> = vec![];
    let mut current: Vec<&RouteRule> = vec![];
    let mut used = 0usize;

    for route in routes {
        let item_w = route.pattern.width() + 3 + route.target.width(); // 3 = " → "
        let sep_w = if current.is_empty() { 0 } else { 2 };
        if current.is_empty() || used + sep_w + item_w <= avail_width {
            current.push(route);
            used += sep_w + item_w;
        } else {
            result.push(std::mem::take(&mut current));
            current.push(route);
            used = item_w;
        }
    }
    if !current.is_empty() {
        result.push(current);
    }
    result
}

pub(super) fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

pub(super) fn centered_fixed(percent_x: u16, height: u16, r: Rect) -> Rect {
    let height = height.min(r.height);
    let v_margin = (r.height - height) / 2;
    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(v_margin),
            Constraint::Length(height),
            Constraint::Min(0),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vert[1])[1]
}
