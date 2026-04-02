use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph, Wrap};

use super::super::state::{App, ConfirmAction};
use super::super::theme::{self as t};
use super::layout::{centered_fixed, centered_rect};

pub(super) fn draw_help(f: &mut Frame, _app: &App) {
    // Section: (heading, &[(key, desc)])
    type Section = (&'static str, &'static [(&'static str, &'static str)]);
    let sections: &[Section] = &[
        (
            "Provider List",
            &[
                ("j / k / ↑↓", "Navigate providers"),
                ("gg / G", "Go to top / bottom"),
                ("s", "Switch to selected provider"),
                ("a / o", "Add new provider"),
                ("e / Enter", "Edit selected provider"),
                ("dd", "Delete selected provider"),
                ("t", "Test provider connectivity"),
                ("yy", "Copy provider base URL to clipboard"),
                ("yc", "Copy test curl command to clipboard"),
                ("K / J", "Move provider up / down"),
                ("f", "Toggle fallback mode"),
                ("r", "Reload config from disk"),
                ("S", "Toggle background proxy"),
                ("c", "Clear current provider usage data"),
                ("C", "Clear all providers' usage data"),
                ("q / Esc", "Quit (direct exit if bg proxy running)"),
                ("h / ?", "Show this help"),
            ],
        ),
        (
            "Provider Editor  (default: Normal mode)",
            &[
                ("i / a", "Enter Insert mode"),
                ("Esc", "Exit Insert → Normal  |  Normal → cancel"),
                ("j / k", "Navigate fields (Normal)"),
                ("h / l", "Move cursor / toggle field (Normal)"),
                ("Space", "Toggle format field (Normal)"),
                ("ZZ", "Save and close editor"),
                ("ZQ", "Cancel and close editor"),
                ("Ctrl+S", "Save (works in Insert mode too)"),
                ("Tab / S-Tab", "Next / previous field"),
            ],
        ),
        (
            "Route Rules  (inside editor, Routes section)",
            &[
                ("j / k", "Navigate rules"),
                ("n", "New rule (auto-enters Insert)"),
                ("Space", "Toggle rule enabled / disabled"),
                ("i / Enter", "Edit rule pattern (Insert mode)"),
                ("dd", "Delete selected rule"),
                ("K / J", "Move rule up / down (priority)"),
                ("Esc", "Exit Insert → Normal"),
            ],
        ),
    ];

    let key_w = 14usize;
    let content_width: u16 = 62;

    // Count total lines needed.
    let total_lines: u16 = sections
        .iter()
        .map(|(_, entries)| 2 + entries.len() as u16) // heading blank + heading + entries
        .sum::<u16>()
        + 2; // footer
    let dialog_height = total_lines + 4; // borders + padding

    let area = centered_fixed(content_width, dialog_height, f.area());
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(t::PRIMARY))
        .title(" Help ")
        .title_style(Style::default().fg(t::PRIMARY).add_modifier(Modifier::BOLD))
        .padding(Padding::new(2, 2, 1, 1));
    let inner = block.inner(area);
    f.render_widget(block.clone(), area);

    let mut lines: Vec<Line> = Vec::new();

    for (i, (heading, entries)) in sections.iter().enumerate() {
        if i > 0 {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(
            *heading,
            Style::default()
                .fg(t::TEXT)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        )));
        for (key, desc) in *entries {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {:<width$}", key, width = key_w),
                    Style::default().fg(t::PRIMARY).add_modifier(Modifier::BOLD),
                ),
                Span::styled(*desc, Style::default().fg(t::TEXT)),
            ]));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Press any key to close",
        Style::default().fg(t::MUTED),
    )));

    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

pub(super) fn draw_confirm(f: &mut Frame, app: &App) {
    let area = centered_rect(40, 20, f.area());
    let area = ratatui::layout::Rect {
        height: area.height.max(5),
        ..area
    };

    f.render_widget(Clear, area);

    let prompt_line = match &app.confirm_action {
        Some(ConfirmAction::Delete(id)) => Line::from(vec![
            Span::raw("  Delete "),
            Span::styled(
                id.as_str(),
                Style::default().fg(t::ERROR).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" ?"),
        ]),
        Some(ConfirmAction::Clear) => Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "Clear all providers' usage data",
                Style::default().fg(t::ERROR),
            ),
            Span::raw(" ?"),
        ]),
        Some(ConfirmAction::ClearCurrent) => {
            // Borrow app to get the selected provider name for the prompt.
            let name = app
                .selected_name()
                .unwrap_or("current provider")
                .to_string();
            Line::from(vec![
                Span::raw("  Clear usage data for "),
                Span::styled(
                    name,
                    Style::default().fg(t::ERROR).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" ?"),
            ])
        }
        Some(ConfirmAction::Quit) => Line::from(vec![
            Span::raw("  "),
            Span::styled("Quit", Style::default().fg(t::ERROR)),
            Span::raw(" ?"),
        ]),
        None => Line::from(""),
    };

    let text = vec![
        Line::from(""),
        prompt_line,
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("y", Style::default().fg(t::SUCCESS)),
            Span::styled(" Yes    ", Style::default().fg(t::MUTED)),
            Span::styled("n", Style::default().fg(t::WARNING)),
            Span::styled(" No", Style::default().fg(t::MUTED)),
        ]),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(t::ERROR))
        .title(" Confirm ")
        .title_style(Style::default().fg(t::ERROR).add_modifier(Modifier::BOLD))
        .padding(Padding::horizontal(1));

    f.render_widget(
        Paragraph::new(text).block(block).wrap(Wrap { trim: false }),
        area,
    );
}

/// Render the Models browser popup.
///
/// Layout (inside the outer border):
///   Line 1  — search box  (Length 1)
///   Line 2  — divider     (Length 1)
///   Rest    — model list  (Min 0, scrollable)
pub(super) fn draw_models(f: &mut Frame, app: &App) {
    let filter = app.models_search_field.value.to_lowercase();

    // Build filtered list: (provider_name, [model, ...]) sorted, empty providers skipped.
    let mut providers: Vec<&String> = app.provider_models.keys().collect();
    providers.sort_unstable();

    let filtered: Vec<(&str, Vec<&str>)> = providers
        .iter()
        .filter_map(|prov| {
            let models = app.provider_models.get(*prov)?;
            let mut matched: Vec<&str> = models
                .iter()
                .filter(|m| filter.is_empty() || m.to_lowercase().contains(&filter))
                .map(|s| s.as_str())
                .collect();
            if matched.is_empty() {
                return None;
            }
            matched.sort_unstable();
            Some((prov.as_str(), matched))
        })
        .collect();

    // heading-per-provider + model rows + blank lines between groups; capped at u16::MAX.
    let content_lines = (filtered.iter().map(|(_, ms)| 1 + ms.len()).sum::<usize>()
        + filtered.len().saturating_sub(1))
    .min(u16::MAX as usize) as u16;

    // content + search(1) + divider(1) + border top/bottom(2) = +4 overhead.
    let dialog_height = (content_lines + 4).min(f.area().height * 4 / 5).max(6);
    let area = centered_fixed(80, dialog_height, f.area());
    f.render_widget(Clear, area);

    // Mode indicator in title, consistent with the Edit Provider form.
    let mode_tag = if app.models_insert { "[I]" } else { "[N]" };
    let outer_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(t::PRIMARY))
        .title(format!(" Models  {mode_tag} "))
        .title_style(Style::default().fg(t::TEXT).add_modifier(Modifier::BOLD))
        .padding(Padding::new(1, 1, 0, 0));
    let inner = outer_block.inner(area);
    f.render_widget(outer_block, area);

    // Split inner area: search line | divider | list.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // search box
            Constraint::Length(1), // divider
            Constraint::Min(0),    // model list
        ])
        .split(inner);

    // ── Search box ────────────────────────────────────────────────────────
    // In Insert mode: show cursor at the correct position (reversed block).
    // In Normal mode: render as plain dimmed text (no cursor).
    let search_line = if app.models_insert {
        let val = &app.models_search_field.value;
        let cur = app.models_search_field.cursor.min(val.len());
        let before = &val[..cur];
        let cursor_ch = val[cur..].chars().next().unwrap_or(' ');
        let after_start = cur
            + if cur < val.len() {
                cursor_ch.len_utf8()
            } else {
                0
            };
        let after = if after_start <= val.len() {
            &val[after_start..]
        } else {
            ""
        };
        Line::from(vec![
            Span::styled("Search  ", Style::default().fg(t::MUTED)),
            Span::raw(before.to_string()),
            Span::styled(
                cursor_ch.to_string(),
                Style::default()
                    .fg(t::PRIMARY)
                    .add_modifier(Modifier::REVERSED),
            ),
            Span::raw(after.to_string()),
        ])
    } else {
        Line::from(vec![
            Span::styled("Search  ", Style::default().fg(t::MUTED)),
            Span::styled(
                &app.models_search_field.value,
                Style::default().fg(t::MUTED),
            ),
        ])
    };
    f.render_widget(Paragraph::new(search_line), chunks[0]);

    // ── Divider ───────────────────────────────────────────────────────────
    let divider = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(t::MUTED));
    f.render_widget(divider, chunks[1]);

    // ── Model list ────────────────────────────────────────────────────────
    let list_height = chunks[2].height;

    // Build lines and a flat index for highlight lookup.
    let mut lines: Vec<Line> = Vec::new();
    let mut flat_idx: usize = 0; // global index into the flat model list

    if filtered.is_empty() {
        lines.push(Line::from(Span::styled(
            "  No matches",
            Style::default().fg(t::MUTED),
        )));
    } else {
        for (gi, (prov, models)) in filtered.iter().enumerate() {
            if gi > 0 {
                lines.push(Line::from(""));
            }
            lines.push(Line::from(Span::styled(
                format!("  {prov}"),
                Style::default()
                    .fg(t::provider_color(prov))
                    .add_modifier(Modifier::BOLD),
            )));
            let last = models.len().saturating_sub(1);
            for (mi, model) in models.iter().enumerate() {
                let is_selected = flat_idx == app.models_selected;
                let prefix = if mi == last { "    └ " } else { "    ├ " };
                if is_selected {
                    let prov_color = t::provider_color(prov);
                    lines.push(Line::from(vec![
                        Span::styled("  ▶ ", Style::default().fg(prov_color)),
                        Span::styled(
                            model.to_string(),
                            Style::default().fg(prov_color).add_modifier(Modifier::BOLD),
                        ),
                    ]));
                } else {
                    lines.push(Line::from(vec![
                        Span::styled(prefix, Style::default().fg(t::MUTED)),
                        Span::styled(*model, Style::default().fg(t::TEXT)),
                    ]));
                }
                flat_idx += 1;
            }
        }
    }

    // Clamp scroll so we never scroll past the last line.
    let max_scroll = (lines.len().min(u16::MAX as usize) as u16).saturating_sub(list_height);
    let scroll = app.models_scroll.min(max_scroll);

    f.render_widget(Paragraph::new(lines).scroll((scroll, 0)), chunks[2]);
}
