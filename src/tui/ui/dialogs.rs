use ratatui::Frame;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph, Wrap};

use super::super::state::{App, ConfirmAction};
use super::super::theme::{self as t};
use super::layout::{centered_fixed, centered_rect};

/// `scroll` is the caller's persisted offset; it is clamped here because
/// only the draw knows how many rows the sections actually render to.
pub(super) fn draw_help(f: &mut Frame, scroll: &mut u16) {
    // Section: (heading, &[(key, desc)])
    type Section = (&'static str, &'static [(&'static str, &'static str)]);
    let sections: &[Section] = &[
        (
            "Provider List",
            &[
                ("j / k / ↑↓", "Navigate providers"),
                ("gg / G", "Go to top / bottom"),
                ("K / J", "Move provider up / down"),
                ("s", "Switch to selected provider"),
                ("a", "Add new provider"),
                ("e / Enter", "Edit selected provider"),
                ("dd", "Delete selected provider"),
                ("p", "Toggle provider enabled / disabled"),
                ("o", "Set/clear selected provider's pinned port"),
                ("f", "Toggle this provider's fallback"),
                ("u", "Run quota command & refresh"),
                ("U", "Configure / preview quota command"),
                ("yy", "Copy provider base URL to clipboard"),
                ("yc", "Copy test curl command to clipboard"),
                ("S", "Toggle / restart background proxy"),
                ("r", "Reload config from disk"),
                ("c", "Clear current provider usage data"),
                ("C", "Clear all providers' usage data"),
                ("l", "Open request log / expand disabled block"),
                ("m", "Browse models"),
                ("Ctrl-L", "Clear message log"),
                ("h / ?", "Show this help"),
                ("q / Esc", "Quit (direct exit if bg proxy running)"),
            ],
        ),
        (
            "Provider Editor  (default: Normal mode)",
            &[
                ("i / a", "Enter Insert mode"),
                ("Esc / q", "Exit Insert → Normal  |  Normal → save & close"),
                ("j / k", "Navigate fields (Normal)"),
                ("h / l", "Move cursor (Normal)"),
                ("0 / $", "Jump to field start / end (Normal)"),
                ("Tab / S-Tab", "Next / previous field"),
                ("Format field", "↓/↑ choose (Insert mode) · Enter select"),
            ],
        ),
        (
            "Route Rules  (inside editor, Routes section)",
            &[
                ("j / k", "Navigate rules"),
                ("a / o", "New rule (auto-enters Insert)"),
                ("Space", "Toggle rule enabled / disabled"),
                ("i / Enter", "Edit rule pattern (Insert mode)"),
                ("t", "Edit rule target (with suggestions)"),
                ("dd", "Delete selected rule"),
                ("K / J", "Move rule up / down (priority)"),
                ("Esc", "Exit Insert → Normal"),
            ],
        ),
        (
            "Request Log  (l)",
            &[
                ("j / k", "Select request"),
                ("J / K", "Scroll detail half a page"),
                ("gg / G", "Detail to top / bottom"),
                ("Home / End", "Detail to top / bottom"),
                ("q / Esc", "Back to provider list"),
            ],
        ),
        (
            "Models  (m)",
            &[
                ("j / k", "Navigate models"),
                ("t", "Test highlighted model"),
                ("p", "Set/clear this provider's Test Model"),
                ("C-d / C-u", "Jump 10 entries"),
                ("gg / G", "Go to top / bottom"),
                ("i", "Filter models (Insert)"),
                ("C-j / C-k", "Navigate while filtering"),
                ("yy / Enter", "Copy selected model name"),
                ("q / Esc", "Back to provider list"),
            ],
        ),
        (
            "Quota  (U)",
            &[
                ("i / a", "Edit command (Insert)"),
                ("s", "Run command and preview output"),
                ("j / k", "Scroll preview"),
                ("C-l", "Clear command (Insert)"),
                ("q / Esc", "Save and close"),
            ],
        ),
    ];

    let key_w = 14usize;
    let width_pct: u16 = 66;

    // Count total lines needed.
    let total_lines: u16 = sections
        .iter()
        .map(|(_, entries)| 2 + entries.len() as u16) // heading blank + heading + entries
        .sum::<u16>();
    let dialog_height = total_lines + 4; // borders + padding

    let area = centered_fixed(width_pct, dialog_height, f.area());
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(t::PRIMARY))
        .title(" Help ")
        .title_style(Style::default().fg(t::PRIMARY).add_modifier(Modifier::BOLD))
        .title_bottom(
            Line::from(" j / k scroll · any other key closes ")
                .style(Style::default().fg(t::MUTED))
                .centered(),
        )
        .padding(Padding::new(2, 2, 1, 1));
    let inner = block.inner(area);
    f.render_widget(block, area);

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

    // Wrapping is on, so the scrollable extent is counted in rendered rows,
    // not source lines: a description too long for the dialog occupies several.
    let inner_w = inner.width.max(1) as usize;
    let rendered_rows: usize = lines
        .iter()
        .map(|l| l.width().div_ceil(inner_w).max(1))
        .sum();
    let max_scroll = (rendered_rows as u16).saturating_sub(inner.height);
    *scroll = (*scroll).min(max_scroll);

    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((*scroll, 0)),
        inner,
    );
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

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend};

    use super::draw_help;

    fn render_help(width: u16, height: u16, scroll: &mut u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| draw_help(f, scroll)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn help_top_shows_first_section_and_hides_last() {
        let mut scroll = 0;
        let out = render_help(120, 24, &mut scroll);
        assert!(out.contains("Provider List"));
        assert!(!out.contains("Quota  (U)"));
    }

    #[test]
    fn help_scroll_clamps_to_the_last_rendered_row() {
        let mut scroll = u16::MAX;
        let out = render_help(120, 24, &mut scroll);
        assert!(scroll < u16::MAX, "scroll was not clamped");
        assert!(out.contains("Quota  (U)"));
        assert!(out.contains("Save and close"));
    }

    #[test]
    fn help_does_not_scroll_when_the_terminal_fits_every_section() {
        let mut scroll = u16::MAX;
        render_help(120, 80, &mut scroll);
        assert_eq!(scroll, 0);
    }
}
