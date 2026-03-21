use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use super::super::app::{filter_suggestions, App, VimMode};
use super::super::theme::{self as t};
use super::format::mask_api_key_str;
use super::layout::centered_fixed;

pub(super) fn draw_form(f: &mut Frame, app: &App) {
    let Some(form) = &app.form else { return };

    let in_routes = form.in_routes();

    // Provider color: derived from the Name field so it updates live as the user types.
    let prov_color = t::provider_color(form.fields[0].value.trim());

    // Compute suggestion count for layout height (only when editing target).
    let suggest_items: u16 = if form.route_editing && form.route_edit_target && form.in_routes() {
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
    };

    // ── Title: show action + Vim mode tag ────────────────────────────────────
    let vim_tag = match form.vim_mode {
        VimMode::Normal => "[N]",
        VimMode::Insert => "[I]",
    };
    let title = format!(
        " {} Provider  {} ",
        if form.is_new { "Add" } else { "Edit" },
        vim_tag
    );

    // ── Per-field heights ────────────────────────────────────────────────────
    // Multiline field expands when focused; all others are 3 lines tall.
    let field_heights: Vec<u16> = form
        .fields
        .iter()
        .enumerate()
        .map(|(i, field)| {
            if field.is_multiline && i == form.focused {
                let line_count = field.value.chars().filter(|&c| c == '\n').count() + 1;
                (line_count as u16 + 2).max(3)
            } else {
                3
            }
        })
        .collect();
    let fields_total: u16 = field_heights.iter().sum();

    // Routes section: 1 header line + max(1, rule count) item lines + suggestion panel + 1 blank separator.
    let routes_items = form.routes.len().max(1) as u16;
    let suggest_section = if suggest_items > 0 {
        1 + suggest_items
    } else {
        0
    };
    let routes_height = 1 + routes_items + suggest_section + 1;

    let dialog_height = fields_total + routes_height + 3 + 2 + 2; // fields+routes+hint+borders+pad
    let area = centered_fixed(70, dialog_height, f.area());

    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(t::PRIMARY))
        .title(title.as_str())
        .title_style(Style::default().fg(t::PRIMARY).add_modifier(Modifier::BOLD))
        .padding(Padding::new(2, 2, 1, 1));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Layout: Name, Base URL, API Key, Format, Routes, Notes, Hint
    // Notes (last field) is rendered after the Routes section.
    let notes_h = *field_heights.last().unwrap_or(&3);
    let field_constraints: Vec<Constraint> = field_heights[..field_heights.len() - 1]
        .iter()
        .map(|&h| Constraint::Length(h))
        .chain(std::iter::once(Constraint::Length(routes_height))) // routes (before Notes)
        .chain(std::iter::once(Constraint::Length(notes_h))) // Notes (after Routes)
        .chain(std::iter::once(Constraint::Length(3))) // hint
        .collect();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(field_constraints)
        .split(inner);

    // ── Regular fields ───────────────────────────────────────────────────────
    // Notes (last field, index fields.len()-1) lives at chunks[fields.len()] because
    // the Routes chunk is inserted before it at chunks[fields.len()-1].
    let notes_field_idx = form.fields.len() - 1;
    for (i, field) in form.fields.iter().enumerate() {
        let ci = if i < notes_field_idx { i } else { i + 1 };
        let is_focused = i == form.focused;
        // In Normal vim-mode, show cursor only when the field has focus AND
        // we are also in Insert mode (or the field is a toggle).
        let show_cursor =
            is_focused && field.editable && (form.vim_mode == VimMode::Insert || field.is_toggle);

        let label_style = if is_focused {
            Style::default().fg(prov_color).add_modifier(Modifier::BOLD)
        } else if !field.editable {
            Style::default().fg(t::MUTED)
        } else {
            Style::default().fg(t::TEXT)
        };

        let value_display = if field.is_toggle {
            let selected = Style::default()
                .fg(prov_color)
                .add_modifier(Modifier::REVERSED | Modifier::BOLD);
            let unselected = Style::default().fg(t::MUTED);
            let (left, right) = if field.value == "anthropic" {
                (
                    Span::styled(" anthropic ", selected),
                    Span::styled(" openai ", unselected),
                )
            } else {
                (
                    Span::styled(" anthropic ", unselected),
                    Span::styled(" openai ", selected),
                )
            };
            Line::from(vec![
                Span::styled(format!("{:<10}", field.label), label_style),
                left,
                Span::raw(" "),
                right,
            ])
        } else if field.is_multiline {
            if show_cursor {
                let cursor_pos = field.cursor.min(field.value.len());
                let before_cursor = &field.value[..cursor_pos];
                let cursor_row = before_cursor.chars().filter(|&c| c == '\n').count() as u16;
                let last_nl = before_cursor.rfind('\n').map(|p| p + 1).unwrap_or(0);
                let cursor_col = before_cursor[last_nl..].width() as u16;
                let lines: Vec<Line> = field
                    .value
                    .split('\n')
                    .enumerate()
                    .map(|(row, line)| {
                        if row == cursor_row as usize {
                            let col = cursor_col as usize;
                            let byte_col = line
                                .char_indices()
                                .nth(col)
                                .map(|(b, _)| b)
                                .unwrap_or(line.len());
                            let before = &line[..byte_col];
                            let cursor_char = line[byte_col..].chars().next().unwrap_or(' ');
                            let after_start =
                                byte_col + cursor_char.len_utf8().min(line.len() - byte_col);
                            let after = &line[after_start..];
                            Line::from(vec![
                                Span::raw(before.to_string()),
                                Span::styled(
                                    cursor_char.to_string(),
                                    Style::default()
                                        .fg(prov_color)
                                        .add_modifier(Modifier::REVERSED),
                                ),
                                Span::raw(after.to_string()),
                            ])
                        } else {
                            Line::from(line.to_string())
                        }
                    })
                    .collect();
                let label_line =
                    Line::from(Span::styled(format!("{:<10}", field.label), label_style));
                let mut all_lines = vec![label_line];
                all_lines.extend(lines);
                f.render_widget(Paragraph::new(all_lines), chunks[ci]);
                continue;
            } else if is_focused {
                // Normal mode, focused: highlight the cursor line.
                let cursor_pos = field.cursor.min(field.value.len());
                let before_cursor = &field.value[..cursor_pos];
                let cursor_row = before_cursor.chars().filter(|&c| c == '\n').count();
                let label_line =
                    Line::from(Span::styled(format!("{:<10}", field.label), label_style));
                let lines: Vec<Line> = field
                    .value
                    .split('\n')
                    .enumerate()
                    .map(|(row, l)| {
                        if row == cursor_row {
                            Line::from(Span::styled(l.to_string(), Style::default().fg(prov_color)))
                        } else {
                            Line::from(Span::raw(l.to_string()))
                        }
                    })
                    .collect();
                let mut all_lines = vec![label_line];
                all_lines.extend(lines);
                f.render_widget(Paragraph::new(all_lines), chunks[ci]);
                continue;
            } else {
                let first_line = field.value.lines().next().unwrap_or("");
                let label_line =
                    Line::from(Span::styled(format!("{:<10}", field.label), label_style));
                let content_chars: Vec<char> = first_line.chars().collect();
                let max_w = chunks[ci].width.saturating_sub(2) as usize;
                let display_str = if content_chars.len() > max_w && max_w > 1 {
                    let truncated: String = content_chars[..max_w - 1].iter().collect();
                    format!("{}\u{2026}", truncated)
                } else {
                    first_line.to_string()
                };
                let content_line =
                    Line::from(Span::styled(display_str, Style::default().fg(t::MUTED)));
                f.render_widget(Paragraph::new(vec![label_line, content_line]), chunks[ci]);
                continue;
            }
        } else {
            let display_val = if field.label == "API Key" && !is_focused {
                mask_api_key_str(&field.value).unwrap_or_else(|| field.value.clone())
            } else {
                field.value.clone()
            };

            if show_cursor {
                let cursor_pos = field.cursor.min(display_val.len());
                let before = display_val[..cursor_pos].to_string();
                let cursor_char = display_val[cursor_pos..].chars().next().unwrap_or(' ');
                let after_start =
                    cursor_pos + cursor_char.len_utf8().min(display_val.len() - cursor_pos);
                let after = display_val[after_start..].to_string();
                let (before_span, after_span) = if field.label == "Name" && !display_val.is_empty()
                {
                    let color = t::provider_color(&display_val);
                    (
                        Span::styled(before, Style::default().fg(color)),
                        Span::styled(after, Style::default().fg(color)),
                    )
                } else {
                    (Span::raw(before), Span::raw(after))
                };
                Line::from(vec![
                    Span::styled(format!("{:<10}", field.label), label_style),
                    before_span,
                    Span::styled(
                        cursor_char.to_string(),
                        Style::default()
                            .fg(prov_color)
                            .add_modifier(Modifier::REVERSED),
                    ),
                    after_span,
                ])
            } else {
                let val_style = if field.label == "Name" && !display_val.is_empty() {
                    Style::default().fg(t::provider_color(&display_val))
                } else if !field.editable {
                    Style::default().fg(t::MUTED)
                } else {
                    Style::default()
                };
                Line::from(vec![
                    Span::styled(format!("{:<10}", field.label), label_style),
                    Span::styled(display_val, val_style),
                ])
            }
        };

        f.render_widget(Paragraph::new(value_display), chunks[ci]);
    }

    // ── Routes section ───────────────────────────────────────────────────────
    // Routes is rendered at chunks[fields.len()-1]; Notes follows at chunks[fields.len()].
    let routes_chunk = chunks[form.fields.len() - 1];
    let routes_label_style = if in_routes {
        Style::default().fg(prov_color).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(t::TEXT)
    };

    let mut routes_lines: Vec<Line> =
        vec![Line::from(Span::styled("Routes    ", routes_label_style))];

    if form.routes.is_empty() {
        routes_lines.push(Line::from(Span::styled(
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
            let render_field =
                |text: &str, cursor_pos: usize, active: bool, color: ratatui::style::Color| {
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

                let pat_spans =
                    render_field(&rule.pattern, form.route_pat_cursor, pat_active, prov_color);
                let tgt_text_owned;
                let tgt_text = if rule.target.is_empty() && !tgt_active {
                    "target"
                } else {
                    tgt_text_owned = rule.target.clone();
                    &tgt_text_owned
                };
                let tgt_spans =
                    render_field(tgt_text, form.route_tgt_cursor, tgt_active, prov_color);

                let mut spans = vec![
                    Span::raw("  "),
                    Span::styled(format!("[{toggle_ch}] "), toggle_style),
                ];
                spans.extend(pat_spans);
                spans.push(Span::styled(" -> ", Style::default().fg(t::MUTED)));
                spans.extend(tgt_spans);
                routes_lines.push(Line::from(spans));
            } else if is_selected {
                // Normal mode: highlight selected rule.
                let tgt_text = if rule.target.is_empty() {
                    "target".to_string()
                } else {
                    rule.target.clone()
                };
                routes_lines.push(Line::from(vec![
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
                routes_lines.push(Line::from(vec![
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

        routes_lines.push(Line::from(Span::styled(
            "  ── Suggestions ────────────────────────",
            Style::default().fg(t::MUTED),
        )));
        for (si, model) in suggestions.iter().enumerate() {
            let is_hi = form.route_suggest_active && si == form.route_suggest_idx;
            if is_hi {
                routes_lines.push(Line::from(vec![
                    Span::styled("  ▶ ", Style::default().fg(prov_color)),
                    Span::styled(
                        model.to_string(),
                        Style::default().fg(prov_color).add_modifier(Modifier::BOLD),
                    ),
                ]));
            } else {
                routes_lines.push(Line::from(vec![
                    Span::styled("    ", Style::default()),
                    Span::styled(model.to_string(), Style::default().fg(t::MUTED)),
                ]));
            }
        }
    }
    routes_lines.push(Line::from(""));
    f.render_widget(Paragraph::new(routes_lines), routes_chunk);

    // ── Hint bar ─────────────────────────────────────────────────────────────
    let hint_idx = form.fields.len() + 1;
    if hint_idx < chunks.len() {
        let hint_line = if in_routes {
            if form.route_editing {
                if form.route_edit_target {
                    // Route Insert mode (target field) — show ↓ Suggest hint.
                    Line::from(vec![
                        Span::raw("   "),
                        Span::styled("Esc", Style::default().fg(t::WARNING)),
                        Span::styled("/", Style::default().fg(t::MUTED)),
                        Span::styled("Esc", Style::default().fg(t::WARNING)),
                        Span::styled(" Normal  ", Style::default().fg(t::MUTED)),
                        Span::styled("↓", Style::default().fg(t::PRIMARY)),
                        Span::styled("/", Style::default().fg(t::MUTED)),
                        Span::styled("^J", Style::default().fg(t::PRIMARY)),
                        Span::styled(" Suggest  ", Style::default().fg(t::MUTED)),
                        Span::styled("Tab", Style::default().fg(t::PRIMARY)),
                        Span::styled(" Pat↔Tgt  ", Style::default().fg(t::MUTED)),
                        Span::styled("←/→", Style::default().fg(t::PRIMARY)),
                        Span::styled(" Move cursor", Style::default().fg(t::MUTED)),
                    ])
                } else {
                    // Route Insert mode (pattern field).
                    Line::from(vec![
                        Span::raw("   "),
                        Span::styled("Esc", Style::default().fg(t::WARNING)),
                        Span::styled(" Normal  ", Style::default().fg(t::MUTED)),
                        Span::styled("Tab", Style::default().fg(t::PRIMARY)),
                        Span::styled(" Pat↔Tgt  ", Style::default().fg(t::MUTED)),
                        Span::styled("←/→", Style::default().fg(t::PRIMARY)),
                        Span::styled(" Move cursor", Style::default().fg(t::MUTED)),
                    ])
                }
            } else {
                // Route Normal mode.
                Line::from(vec![
                    Span::raw("   "),
                    Span::styled("a", Style::default().fg(t::SUCCESS)),
                    Span::styled(" Add  ", Style::default().fg(t::MUTED)),
                    Span::styled("Space", Style::default().fg(t::PRIMARY)),
                    Span::styled(" Toggle  ", Style::default().fg(t::MUTED)),
                    Span::styled("dd", Style::default().fg(t::WARNING)),
                    Span::styled(" Del  ", Style::default().fg(t::MUTED)),
                    Span::styled("i", Style::default().fg(t::PRIMARY)),
                    Span::styled(" Pat  ", Style::default().fg(t::MUTED)),
                    Span::styled("t", Style::default().fg(t::PRIMARY)),
                    Span::styled(" Tgt  ", Style::default().fg(t::MUTED)),
                    Span::styled("q", Style::default().fg(t::WARNING)),
                    Span::styled(" Quit", Style::default().fg(t::MUTED)),
                ])
            }
        } else if form.vim_mode == VimMode::Insert {
            // Field Insert mode.
            let focused_field = &form.fields[form.focused];
            if focused_field.is_multiline {
                Line::from(vec![
                    Span::raw("   "),
                    Span::styled("Esc", Style::default().fg(t::WARNING)),
                    Span::styled(" Normal  ", Style::default().fg(t::MUTED)),
                    Span::styled("^J", Style::default().fg(t::PRIMARY)),
                    Span::styled(" Newline", Style::default().fg(t::MUTED)),
                ])
            } else {
                Line::from(vec![
                    Span::raw("   "),
                    Span::styled("Esc", Style::default().fg(t::WARNING)),
                    Span::styled(" Normal  ", Style::default().fg(t::MUTED)),
                    Span::styled("Tab", Style::default().fg(t::PRIMARY)),
                    Span::styled(" Next field", Style::default().fg(t::MUTED)),
                ])
            }
        } else {
            // Field Normal mode.
            Line::from(vec![
                Span::raw("   "),
                Span::styled("i", Style::default().fg(t::PRIMARY)),
                Span::styled(" Insert  ", Style::default().fg(t::MUTED)),
                Span::styled("j/k", Style::default().fg(t::PRIMARY)),
                Span::styled(" Field  ", Style::default().fg(t::MUTED)),
                Span::styled("q", Style::default().fg(t::WARNING)),
                Span::styled(" Quit", Style::default().fg(t::MUTED)),
            ])
        };

        let mut hint_lines = vec![hint_line, Line::from("")];
        if let Some(err) = &form.error {
            hint_lines.push(Line::from(Span::styled(
                format!("✗ {}", err),
                Style::default().fg(t::ERROR),
            )));
        }
        f.render_widget(Paragraph::new(hint_lines), chunks[hint_idx]);
    }
}
