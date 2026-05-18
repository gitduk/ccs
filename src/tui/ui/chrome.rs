//! Outer frame around the main-screen left column.
//!
//! The "chrome" is a single L-shaped frame that surrounds the Providers,
//! Detail, and Stats panels as one unit. It owns the top/bottom/left (and
//! optional right) borders plus the app title so individual panels don't
//! need to coordinate border edges with their neighbors.

use ratatui::Frame;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders};

use crate::tui::state::App;
use crate::tui::theme::{self as t};

use super::layout::{BORDER_DASHED_TOP, ScreenPlan};

/// Draw the outer L-frame plus the app title bar.
pub(super) fn draw(f: &mut Frame, plan: &ScreenPlan, app: &App) {
    let borders = if plan.split {
        Borders::TOP | Borders::LEFT | Borders::BOTTOM
    } else {
        Borders::TOP | Borders::LEFT | Borders::BOTTOM | Borders::RIGHT
    };
    let (title_left, title_right) = make_app_title(app);
    let block = Block::default()
        .borders(borders)
        .border_set(BORDER_DASHED_TOP)
        .border_style(Style::default().fg(t::MUTED))
        .title_top(title_left)
        .title_top(title_right);
    f.render_widget(block, plan.left_frame);
}

fn fmt_kbps(kbps: f32) -> String {
    if kbps >= 1024.0 {
        format!("{:.1}MB/s", kbps / 1024.0)
    } else {
        format!("{:.0}KB/s", kbps)
    }
}

fn make_app_title(app: &App) -> (Line<'static>, Line<'static>) {
    let left = Line::from(vec![
        Span::styled(
            " Claude Code Switcher",
            Style::default().fg(t::TEXT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  v{} ", env!("CARGO_PKG_VERSION")),
            Style::default().fg(t::MUTED),
        ),
    ]);
    let listen = app.config.listen.to_string();
    let fallback_label = if app.config.fallback {
        "Fallback on"
    } else {
        "Fallback off"
    };
    let si = &app.sysinfo;
    let mem_str = if si.mem_mb >= 1024 {
        format!("{:.1}GB", si.mem_mb as f32 / 1024.0)
    } else {
        format!("{}MB", si.mem_mb)
    };
    let sysinfo_str = format!(
        "cpu {:.1}%  mem {}  ↑{}  ↓{}",
        si.cpu_pct,
        mem_str,
        fmt_kbps(si.net_in_kbps),
        fmt_kbps(si.net_out_kbps),
    );
    let right = Line::from(vec![
        Span::styled("╌╌ ", Style::default().fg(t::MUTED)),
        Span::styled(
            listen,
            if app.bg_proxy_pid.is_some() {
                Style::default().fg(t::SUCCESS).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(t::MUTED)
            },
        ),
        Span::styled("  ", Style::default()),
        Span::styled(
            fallback_label,
            if app.config.fallback {
                Style::default().fg(t::SUCCESS).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(t::MUTED)
            },
        ),
        Span::styled("  │  ", Style::default().fg(t::MUTED)),
        Span::styled(sysinfo_str, Style::default().fg(t::MUTED)),
        Span::styled(" ╌╌ ", Style::default().fg(t::MUTED)),
    ])
    .right_aligned();
    (left, right)
}
