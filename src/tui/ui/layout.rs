use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::symbols::border;

use crate::config::RouteRule;
use crate::tui::state::App;

// ── Panel system ───────────────────────────────────────────────────
// A single layout planner owns all main-screen geometry.  Renderers
// receive plain Rects and never compute sibling heights.

/// Identity of each main-screen panel, used to key into ScreenPlan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PanelId {
    Providers,
    Detail,
    Stats,
    Logs,
}

/// Height characteristics declared by each panel.
struct HeightSpec {
    /// Minimum lines the panel needs to show anything useful.
    min: u16,
    /// Preferred lines given unlimited space.
    ideal: u16,
}

/// A panel that has been assigned a concrete screen region.
#[derive(Debug, Clone, Copy)]
pub(super) struct PanelPlacement {
    pub id: PanelId,
    pub area: Rect,
}

/// The complete layout plan for one draw cycle.
pub(super) struct ScreenPlan {
    /// Left-column placements in top-to-bottom order.
    pub left: Vec<PanelPlacement>,
    /// Right logs panel, or None when width < split threshold.
    pub logs: Option<PanelPlacement>,
    /// True when the right logs column is present.
    pub split: bool,
    /// Height used by Providers + Detail on the left (used by logs panel
    /// to align its Messages / Recent Requests divider).
    pub top_height: u16,
}

// ── Layout constants ───────────────────────────────────────────────

/// Fixed width of the right-column logs panel (in terminal columns).
pub(super) const LOGS_PANEL_WIDTH: u16 = 44;
/// Minimum terminal width to enable the right-column split.
const MIN_WIDTH_FOR_SPLIT: u16 = 120;
/// Minimum height for the detail panel to be shown at all.
const DETAIL_MIN_HEIGHT: u16 = 2;

// ── Per-panel height spec helpers ─────────────────────────────────

/// Minimum / ideal height for the Providers table.
fn providers_spec(app: &App) -> HeightSpec {
    let ideal = (app.providers.names.len() as u16 + 2).max(3);
    HeightSpec { min: 3, ideal }
}

/// Minimum / ideal height for the Detail panel.
/// `route_avail` is the inner text width available for route wrapping.
fn detail_spec(app: &App, route_avail: usize) -> HeightSpec {
    if app.providers.names.is_empty() {
        return HeightSpec { min: 2, ideal: 2 };
    }
    // base: blank + "Info" title + status line
    let base: u16 = 3;
    let max_routes: u16 = app
        .config
        .providers
        .values()
        .map(|p| {
            let enabled: Vec<_> = p.routes.iter().filter(|r| r.enabled).collect();
            if enabled.is_empty() {
                0u16
            } else {
                pack_routes(&enabled, route_avail).len() as u16
            }
        })
        .max()
        .unwrap_or(0);
    let ideal = base + max_routes;
    HeightSpec {
        min: DETAIL_MIN_HEIGHT,
        ideal,
    }
}

/// Minimum / ideal height for the Stats panel (By Provider section only;
/// By Model gets whatever space remains).
fn stats_spec(app: &App) -> HeightSpec {
    // blank + title + blank + N provider rows + bottom border
    let n = app.providers.names.len() as u16;
    let min = 3 + n;
    HeightSpec { min, ideal: min }
}

// ── Main planner ──────────────────────────────────────────────────

/// Compute the layout plan for the main screen given the current
/// terminal area and application state.  This is the single source of
/// truth for all main-screen panel geometry.
pub(super) fn plan_main_screen(app: &App, area: Rect) -> ScreenPlan {
    let split = area.width >= MIN_WIDTH_FOR_SPLIT;

    // The left column is narrower when the right panel is present.
    let left_width = if split {
        area.width.saturating_sub(LOGS_PANEL_WIDTH) as usize
    } else {
        area.width as usize
    };

    // detail panel: LEFT border (1) + horizontal padding (2) = 3 overhead on each side → 4 total
    let detail_inner_width = left_width.saturating_sub(4);
    let route_avail = detail_inner_width.saturating_sub(ROUTE_LABEL_WIDTH);

    let prov_spec = providers_spec(app);
    let det_spec = detail_spec(app, route_avail);
    let sta_spec = stats_spec(app);

    // ── Vertical allocation: providers first ──────────────────────
    let table_height = prov_spec.ideal.min(area.height.max(prov_spec.min));
    let remaining = area.height.saturating_sub(table_height);

    let show_detail = remaining >= det_spec.min;
    let detail_height = if show_detail {
        det_spec.ideal.min(remaining)
    } else {
        0
    };

    let leftover = area.height.saturating_sub(table_height + detail_height);
    let show_stats = leftover >= sta_spec.min;
    let stats_height = if show_stats { leftover } else { 0 };

    // top_height: how many lines the Providers+Detail block occupies.
    // The logs panel uses this to split Messages vs Recent Requests at
    // the same visual row as "By Provider".
    // stats_panel starts with one blank line, so we add 1 when stats are shown.
    let top_height = table_height
        .saturating_add(detail_height)
        .saturating_add(if show_stats { 1 } else { 0 });

    // ── Build left-column Rects manually (no Min constraints) ─────
    let left_rect = if split {
        Rect {
            x: area.x,
            y: area.y,
            width: area.width.saturating_sub(LOGS_PANEL_WIDTH),
            height: area.height,
        }
    } else {
        area
    };

    let mut left = Vec::new();
    let mut y = left_rect.y;

    let prov_rect = Rect {
        x: left_rect.x,
        y,
        width: left_rect.width,
        height: table_height,
    };
    left.push(PanelPlacement {
        id: PanelId::Providers,
        area: prov_rect,
    });
    y += table_height;

    if show_detail {
        let det_rect = Rect {
            x: left_rect.x,
            y,
            width: left_rect.width,
            height: detail_height,
        };
        left.push(PanelPlacement {
            id: PanelId::Detail,
            area: det_rect,
        });
        y += detail_height;
    }

    if show_stats {
        let sta_rect = Rect {
            x: left_rect.x,
            y,
            width: left_rect.width,
            height: stats_height,
        };
        left.push(PanelPlacement {
            id: PanelId::Stats,
            area: sta_rect,
        });
    }

    // ── Right column ──────────────────────────────────────────────
    let logs = if split {
        let logs_rect = Rect {
            x: area.x + area.width.saturating_sub(LOGS_PANEL_WIDTH),
            y: area.y,
            width: LOGS_PANEL_WIDTH,
            height: area.height,
        };
        Some(PanelPlacement {
            id: PanelId::Logs,
            area: logs_rect,
        })
    } else {
        None
    };

    ScreenPlan {
        left,
        logs,
        split,
        top_height,
    }
}

pub(crate) const ROUTE_LABEL_WIDTH: usize = 7; // "Routes "

// ── Shared spacing tokens ──────────────────────────────────────────
// These keep border characters, title dash counts, and gap widths in
// sync across view.rs, logs.rs, and stats_panel.rs.

/// Horizontal dash character for title separators and dashed borders.
pub(crate) const DASH: &str = "╌";

/// Number of dash chars flanking a section title (e.g. "╌╌ Title ╌╌╌").
pub(crate) const TITLE_SIDE: usize = 2;

/// Gap (spaces) between dash fill and suffix text in title lines.
pub(crate) const SUFFIX_GAP: usize = 1;

/// Border set: dashed `╌` on top, solid everywhere else.
pub(crate) const BORDER_DASHED_TOP: border::Set = border::Set {
    horizontal_top: DASH,
    ..border::PLAIN
};

/// Border set: dashed `╎` on the left (inner column divider), solid elsewhere.
pub(crate) const BORDER_INNER_DIVIDER: border::Set = border::Set {
    vertical_left: "╎",
    ..border::PLAIN
};

/// Pack enabled routes into wrapped lines given the available text width.
/// Returns groups of routes, each group rendered on one line.
pub(crate) fn pack_routes<'a>(
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

pub(crate) fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
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

pub(crate) fn centered_fixed(percent_x: u16, height: u16, r: Rect) -> Rect {
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
