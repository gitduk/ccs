use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::symbols::border;

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
    /// Outer L-frame rect for the left column. The chrome module draws
    /// the top/bottom/left (and optional right) borders here.
    pub left_frame: Rect,
    /// Left-column panel *content* placements (inside left_frame), in
    /// top-to-bottom order. Panels render directly into these rects with
    /// no borders of their own.
    pub left: Vec<PanelPlacement>,
    /// Right logs panel, or None when width < split threshold.
    pub logs: Option<PanelPlacement>,
    /// True when the right logs column is present.
    pub split: bool,
    /// Row index (inside the logs inner area) where the Messages section
    /// ends and the Recent Requests section begins. Chosen so the
    /// Messages end aligns with the left column's Detail end.
    pub top_height: u16,
}

/// Allocated heights for left-column panels.
struct AllocatedHeights {
    providers: u16,
    detail: u16,
    stats: u16,
}

// ── Layout constants ───────────────────────────────────────────────

/// Fixed width of the right-column logs panel (in terminal columns).
pub(super) const LOGS_PANEL_WIDTH: u16 = 44;
/// Minimum terminal width to enable the right-column split.
const MIN_WIDTH_FOR_SPLIT: u16 = 120;
/// Minimum height for the detail panel to be shown at all.
const DETAIL_MIN_HEIGHT: u16 = 2;

// ── Per-panel height spec helpers ─────────────────────────────────

/// Minimum / ideal height for the Providers table content (no borders;
/// the outer chrome owns top/bottom). One row of top padding + one
/// header row + one row per provider.
fn providers_spec(app: &App) -> HeightSpec {
    let ideal = (app.provider_count() as u16 + 2).max(3);
    HeightSpec { min: 3, ideal }
}

/// Minimum / ideal height for the Detail panel.
/// `route_avail` is the inner text width available for route wrapping.
fn detail_spec(app: &App) -> HeightSpec {
    HeightSpec {
        min: DETAIL_MIN_HEIGHT,
        ideal: app.detail_line_count.max(DETAIL_MIN_HEIGHT),
    }
}

/// Minimum / ideal height for the Stats panel (By Provider section only;
/// By Model gets whatever space remains). No bottom border — owned by
/// the chrome.
fn stats_spec(app: &App) -> HeightSpec {
    // blank + title + N provider rows
    let n = app.provider_count() as u16;
    let min = 2 + n;
    HeightSpec { min, ideal: min }
}

// ── Main planner ──────────────────────────────────────────────────

/// Allocate vertical space to left-column panels given their specs.
fn allocate_heights(
    prov_spec: HeightSpec,
    det_spec: HeightSpec,
    sta_spec: HeightSpec,
    total_height: u16,
) -> AllocatedHeights {
    let providers = prov_spec.ideal.min(total_height.max(prov_spec.min));
    let remaining = total_height.saturating_sub(providers);

    let show_detail = remaining >= det_spec.min;
    let detail = if show_detail {
        det_spec.ideal.min(remaining)
    } else {
        0
    };

    let leftover = total_height.saturating_sub(providers + detail);
    let show_stats = leftover >= sta_spec.min;
    let stats = if show_stats { leftover } else { 0 };

    AllocatedHeights {
        providers,
        detail,
        stats,
    }
}

/// Build left-column panel placements from allocated heights.
/// Panels with height 0 are omitted (convention: 0 = hidden).
fn build_left_column(heights: &AllocatedHeights, left_rect: Rect) -> Vec<PanelPlacement> {
    let panels = [
        (PanelId::Providers, heights.providers),
        (PanelId::Detail, heights.detail),
        (PanelId::Stats, heights.stats),
    ];

    panels
        .into_iter()
        .filter(|(_, h)| *h > 0)
        .scan(left_rect.y, |y, (id, height)| {
            let rect = Rect {
                x: left_rect.x,
                y: *y,
                width: left_rect.width,
                height,
            };
            *y += height;
            Some(PanelPlacement { id, area: rect })
        })
        .collect()
}

/// Compute the layout plan for the main screen given the current
/// terminal area and application state.  This is the single source of
/// truth for all main-screen panel geometry.
pub(super) fn plan_main_screen(app: &App, area: Rect) -> ScreenPlan {
    let split = area.width >= MIN_WIDTH_FOR_SPLIT;

    let left_frame = if split {
        Rect {
            x: area.x,
            y: area.y,
            width: area.width.saturating_sub(LOGS_PANEL_WIDTH),
            height: area.height,
        }
    } else {
        area
    };

    // Chrome consumes: 1 row top, 1 row bottom, 1 col left, and 1 col right
    // when no logs column is shown (split = false).
    let right_frame = if split { 0 } else { 1 };
    let inner_left = Rect {
        x: left_frame.x.saturating_add(1),
        y: left_frame.y.saturating_add(1),
        width: left_frame.width.saturating_sub(1 + right_frame),
        height: left_frame.height.saturating_sub(2),
    };

    let prov_spec = providers_spec(app);
    let det_spec = detail_spec(app);
    let sta_spec = stats_spec(app);

    let heights = allocate_heights(prov_spec, det_spec, sta_spec, inner_left.height);

    let left = build_left_column(&heights, inner_left);

    // top_height: row inside the logs inner area where Messages ends and
    // Requests begins. We want Messages' last row to align with Detail's
    // last content row (with a +1 offset when Stats is shown, to land on
    // the blank first row of Stats).  Since the left column content is
    // shifted down by 1 row by the chrome's top border, but the logs
    // column has no top border, we add 1 here to compensate.
    let top_height = 1u16
        .saturating_add(heights.providers)
        .saturating_add(heights.detail)
        .saturating_add(if heights.stats > 0 { 1 } else { 0 });

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
        left_frame,
        left,
        logs,
        split,
        top_height,
    }
}

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
