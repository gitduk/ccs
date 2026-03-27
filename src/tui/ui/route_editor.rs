use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::super::state::{App, ProviderForm, filter_suggestions};
use super::super::theme::{self as t};

/// Compute how many suggestion items to display (used for layout height calculation).
pub(super) fn suggest_count(form: &ProviderForm, app: &App) -> u16 {
    if form.route_editing && form.route_edit_target && form.in_routes() {
        let prov_key = form
            .original_name
            .as_deref()
            .unwrap_or_else(|| form.fields[0].value.trim());
        let models = app
            .provider_models
            .get(prov_key)
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        let tgt_filter = form
            .routes
            .get(form.route_cursor)
            .map(|r| r.target.as_str())
            .unwrap_or("");
        filter_suggestions(models, tgt_filter).len() as u16
    } else {
        0
    }
}

/// Render the Routes section (rules list + suggestion panel) into the given area.
pub(super) fn draw_routes_section(
    f: &mut Frame,
    form: &ProviderForm,
    app: &App,
    area: Rect,
    prov_color: Color,
    in_routes: bool,
) {
    let routes_label_style = if in_routes {
        Style::default().fg(prov_color).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(t::TEXT)
    };

    let mut lines: Vec<Line> = vec![Line::from(Span::styled("Routes    ", routes_label_style))];

    if form.routes.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no rules)",
            Style::default().fg(t::MUTED),
        )));
    } else {
        for (i, rule) in form.routes.iter().enumerate() {
            let is_selected = in_routes && i == form.route_cursor;
            let toggle_ch = if rule.enabled { '✓' } else { ' ' };
            let toggle_style = if rule.enabled {
                Style::default().fg(t::SUCCESS)
            } else {
                Style::default().fg(t::MUTED)
            };

            // Helper: render a text field with optional cursor at `cursor_pos`.
            let render_field = |text: &str, cursor_pos: usize, active: bool, color: Color| {
                if active {
                    let cursor_pos = cursor_pos.min(text.len());
                    let before = &text[..cursor_pos];
                    let cursor_char = text[cursor_pos..].chars().next().unwrap_or(' ');
                    let after_start = cursor_pos
                        + if cursor_pos < text.len() {
                            cursor_char.len_utf8()
                        } else {
                            0
                        };
                    let after = if after_start <= text.len() {
                        &text[after_start..]
                    } else {
                        ""
                    };
                    vec![
                        Span::raw(before.to_string()),
                        Span::styled(
                            cursor_char.to_string(),
                            Style::default().fg(color).add_modifier(Modifier::REVERSED),
                        ),
                        Span::raw(after.to_string()),
                    ]
                } else {
                    vec![Span::raw(text.to_string())]
                }
            };

            if is_selected && form.route_editing {
                // Insert mode: show cursor in the active field.
                let pat_active = !form.route_edit_target;
                let tgt_active = form.route_edit_target;

                let pat_spans = render_field(
                    &rule.pattern,
                    form.route_pat_field.cursor,
                    pat_active,
                    prov_color,
                );
                let tgt_text_owned;
                let tgt_text = if rule.target.is_empty() && !tgt_active {
                    "target"
                } else {
                    tgt_text_owned = rule.target.clone();
                    &tgt_text_owned
                };
                let tgt_spans = render_field(
                    tgt_text,
                    form.route_tgt_field.cursor,
                    tgt_active,
                    prov_color,
                );

                let mut spans = vec![
                    Span::raw("  "),
                    Span::styled(format!("[{toggle_ch}] "), toggle_style),
                ];
                spans.extend(pat_spans);
                spans.push(Span::styled(" -> ", Style::default().fg(t::MUTED)));
                spans.extend(tgt_spans);
                lines.push(Line::from(spans));
            } else if is_selected {
                // Normal mode: highlight selected rule.
                let tgt_text = if rule.target.is_empty() {
                    "target".to_string()
                } else {
                    rule.target.clone()
                };
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(format!("[{toggle_ch}] "), Style::default().fg(prov_color)),
                    Span::styled(
                        rule.pattern.as_str(),
                        Style::default().fg(t::TEXT).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(" -> ", Style::default().fg(t::MUTED)),
                    Span::styled(tgt_text, Style::default().fg(t::TEXT)),
                ]));
            } else {
                let pat_style = if rule.enabled {
                    Style::default().fg(t::TEXT)
                } else {
                    Style::default().fg(t::MUTED)
                };
                let tgt_text = if rule.target.is_empty() {
                    "target".to_string()
                } else {
                    rule.target.clone()
                };
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(format!("[{toggle_ch}] "), toggle_style),
                    Span::styled(rule.pattern.as_str(), pat_style),
                    Span::styled(" -> ", Style::default().fg(t::MUTED)),
                    Span::styled(tgt_text, pat_style),
                ]));
            }
        }
    }

    // ── Suggestion panel ───────────────────────────────────────────────────
    let suggest_items = suggest_count(form, app);
    if suggest_items > 0 {
        let prov_key = form
            .original_name
            .as_deref()
            .unwrap_or_else(|| form.fields[0].value.trim());
        let models = app
            .provider_models
            .get(prov_key)
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        let tgt_filter = form
            .routes
            .get(form.route_cursor)
            .map(|r| r.target.as_str())
            .unwrap_or("");
        let suggestions = filter_suggestions(models, tgt_filter);

        lines.push(Line::from(Span::styled(
            "  ── Suggestions ────────────────────────",
            Style::default().fg(t::MUTED),
        )));
        for (si, model) in suggestions.iter().enumerate() {
            let is_hi = form.route_suggest_active && si == form.route_suggest_idx;
            if is_hi {
                lines.push(Line::from(vec![
                    Span::styled("  ▶ ", Style::default().fg(prov_color)),
                    Span::styled(
                        model.to_string(),
                        Style::default().fg(prov_color).add_modifier(Modifier::BOLD),
                    ),
                ]));
            } else {
                lines.push(Line::from(vec![
                    Span::styled("    ", Style::default()),
                    Span::styled(model.to_string(), Style::default().fg(t::MUTED)),
                ]));
            }
        }
    }

    lines.push(Line::from(""));
    f.render_widget(Paragraph::new(lines), area);
}
