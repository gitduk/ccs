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
use unicode_width::UnicodeWidthStr;

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
    let (title_left, title_right) = make_app_title(app, plan.left_frame.width);
    let block = Block::default()
        .borders(borders)
        .border_set(BORDER_DASHED_TOP)
        .border_style(Style::default().fg(t::MUTED))
        .title_top(title_left)
        .title_top(title_right);
    f.render_widget(block, plan.left_frame);
}

const VERSION_STR: &str = concat!(" v", env!("CARGO_PKG_VERSION"), " ");

fn logo_spans() -> Vec<Span<'static>> {
    vec![Span::styled(
        " Claude Code Switcher",
        Style::default().fg(t::TEXT).add_modifier(Modifier::BOLD),
    )]
}

fn version_spans() -> Vec<Span<'static>> {
    vec![Span::styled(VERSION_STR, Style::default().fg(t::MUTED))]
}

fn sys_info_spans(cpu_pct: f32, mem_mb: u32) -> Vec<Span<'static>> {
    vec![Span::styled(
        format!("cpu {cpu_pct:.1}% mem {}", mem_label(mem_mb)),
        Style::default().fg(t::MUTED),
    )]
}

fn mem_label(mem_mb: u32) -> String {
    if mem_mb >= 1024 {
        format!("{:.1}GB", mem_mb as f32 / 1024.0)
    } else {
        format!("{mem_mb}MB")
    }
}

fn app_info_spans(listen: &str, has_proxy: bool, stale_version: Option<&str>) -> Vec<Span<'static>> {
    let mut spans = vec![Span::styled(
        listen.to_owned(),
        if has_proxy {
            Style::default().fg(t::SUCCESS).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(t::MUTED)
        },
    )];
    // The stale badge describes the *running* background proxy; never show it
    // once the process is gone (or was never there).
    if has_proxy
        && let Some(v) = stale_version
    {
        spans.push(Span::styled(
            format!(" v{v}"),
            Style::default().fg(t::WARNING),
        ));
    }
    spans
}

fn divider() -> Vec<Span<'static>> {
    vec![Span::styled(" │ ", Style::default().fg(t::MUTED))]
}

fn spans_width(spans: &[Span<'static>]) -> usize {
    spans.iter().map(|span| span.content.width()).sum()
}

fn title_spans_fit(left_width: usize, right_spans: &[Span<'static>], width: u16) -> bool {
    left_width + spans_width(right_spans) <= usize::from(width)
}

fn worst_case_sys_info_width(mem_mb: u32) -> usize {
    format!("cpu 100.0% mem {}", mem_label(mem_mb)).width()
}

fn title_parts_fit(left_width: usize, right_width: usize, width: u16) -> bool {
    left_width + right_width <= usize::from(width)
}

fn make_app_title(app: &App, width: u16) -> (Line<'static>, Line<'static>) {
    let si = app.sysinfo_sampler.current();
    make_app_title_with_sysinfo(app, width, si.cpu_pct, si.mem_mb)
}

fn make_app_title_with_sysinfo(
    app: &App,
    width: u16,
    cpu_pct: f32,
    mem_mb: u32,
) -> (Line<'static>, Line<'static>) {
    let left_spans = [logo_spans(), version_spans()].concat();
    let right_spans = topbar_right_spans(
        &left_spans,
        width,
        sys_info_spans(cpu_pct, mem_mb),
        mem_mb,
        app_info_spans(&app.config.listen, app.bg_proxy_pid.is_some(), app.bg_proxy_stale_version.as_deref()),
    );
    let right = Line::from(right_spans).right_aligned();
    (Line::from(left_spans), right)
}

fn topbar_right_spans(
    left_spans: &[Span<'static>],
    width: u16,
    sys_info: Vec<Span<'static>>,
    mem_mb: u32,
    app_info: Vec<Span<'static>>,
) -> Vec<Span<'static>> {
    let left_width = spans_width(left_spans);
    let right_spans = [
        vec![Span::styled("╌╌ ", Style::default().fg(t::MUTED))],
        sys_info.clone(),
        divider(),
        app_info,
        vec![Span::styled(" ╌╌ ", Style::default().fg(t::MUTED))],
    ]
    .concat();
    let full_right_width =
        spans_width(&right_spans) - spans_width(&sys_info) + worst_case_sys_info_width(mem_mb);
    let sys_info_spans = [
        vec![Span::styled("╌╌ ", Style::default().fg(t::MUTED))],
        sys_info,
        vec![Span::styled(" ", Style::default())],
    ]
    .concat();
    if title_parts_fit(left_width, full_right_width, width) {
        right_spans
    } else if title_spans_fit(left_width, &sys_info_spans, width) {
        sys_info_spans
    } else {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(spans: &[Span<'static>]) -> String {
        spans.iter().map(|span| span.content.as_ref()).collect()
    }

    fn full_right_width(left: &[Span<'static>], mem_mb: u32, app_info: &[Span<'static>]) -> u16 {
        (spans_width(left)
            + spans_width(
                &[
                    vec![Span::styled("╌╌ ", Style::default())],
                    vec![Span::styled(
                        format!("cpu 100.0% mem {}", mem_label(mem_mb)),
                        Style::default(),
                    )],
                    divider(),
                    app_info.to_vec(),
                    vec![Span::styled(" ╌╌ ", Style::default())],
                ]
                .concat(),
            )) as u16
    }

    #[test]
    fn full_right_title_keeps_module_spacing() {
        let left = [logo_spans(), version_spans()].concat();
        let app_info = app_info_spans("0.0.0.0:7896", true, None);
        let sys_info = sys_info_spans(6.5, 154);
        let width = full_right_width(&left, 154, &app_info);

        let right = topbar_right_spans(&left, width, sys_info, 154, app_info);

        assert_eq!(text(&right), "╌╌ cpu 6.5% mem 154MB │ 0.0.0.0:7896 ╌╌ ");
    }

    #[test]
    fn sys_info_only_keeps_right_padding() {
        let left = [logo_spans(), version_spans()].concat();
        let app_info = app_info_spans("0.0.0.0:7896", true, None);
        let sys_info = sys_info_spans(6.5, 154);
        let width = (spans_width(&left) + spans_width(&sys_info) + 4) as u16;

        let right = topbar_right_spans(&left, width, sys_info, 154, app_info);

        assert_eq!(text(&right), "╌╌ cpu 6.5% mem 154MB ");
    }

    #[test]
    fn hidden_right_title_has_no_partial_spacing() {
        let left = [logo_spans(), version_spans()].concat();
        let app_info = app_info_spans("0.0.0.0:7896", true, None);
        let sys_info = sys_info_spans(6.5, 154);
        let width = (spans_width(&left) + spans_width(&sys_info)) as u16;
        let right = topbar_right_spans(&left, width, sys_info, 154, app_info);

        assert!(right.is_empty());
    }
    #[test]
    fn stale_background_version_shown_next_to_listen() {
        let left = [logo_spans(), version_spans()].concat();
        let app_info = app_info_spans("0.0.0.0:7896", true, Some("0.60.1"));
        let sys_info = sys_info_spans(6.5, 154);
        let width = full_right_width(&left, 154, &app_info);

        let right = topbar_right_spans(&left, width, sys_info, 154, app_info);

        assert_eq!(text(&right), "╌╌ cpu 6.5% mem 154MB │ 0.0.0.0:7896 v0.60.1 ╌╌ ");
    }
}
