//! Shared UI infrastructure.
//!
//! Feature-specific panels live at `src/tui/*.rs`.
//! This module keeps shared dialogs, layout primitives, and top-level draw dispatch.

mod chrome;
mod dialogs;
pub(super) mod format;
pub(super) mod layout;

use ratatui::Frame;
use ratatui::layout::Rect;

use super::state::{App, Mode};
use crate::tui::logs::draw_panel as draw_logs_panel;
use crate::tui::provider_stats::draw_panel as draw_provider_stats;
use crate::tui::providers::{draw_detail_panel, draw_table};
use layout::{PanelId, plan_main_screen};

pub fn draw(f: &mut Frame, app: &mut App) {
    draw_main(f, app, f.area());

    match &app.mode {
        Mode::Editing => super::editor::draw_popup(f, app),
        Mode::Confirm => dialogs::draw_confirm(f, app),
        Mode::Help => dialogs::draw_help(f, &mut app.help_scroll),
        Mode::Models => super::models::draw_popup(f, app),
        Mode::Logs => super::logs::draw_popup(f, app),
        Mode::QuotaConfig => super::quota_panel::draw_popup(f, app),
        Mode::QuickInput => super::quick_panel::draw_popup(f, app),
        Mode::Normal => {}
    }
}

fn draw_main(f: &mut Frame, app: &mut App, area: Rect) {
    let plan = plan_main_screen(app, area);

    chrome::draw(f, &plan, app);

    for placement in &plan.left {
        match placement.id {
            PanelId::Providers => draw_table(f, app, placement.area),
            PanelId::Detail => draw_detail_panel(f, app, placement.area),
            PanelId::Stats => draw_provider_stats(f, app, placement.area),
            PanelId::Logs => {}
        }
    }

    if let Some(logs) = plan.logs {
        draw_logs_panel(f, app, logs.area, plan.top_height);
    }
}
