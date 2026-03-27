use ratatui::Frame;
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
