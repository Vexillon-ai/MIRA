// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tui/render.rs
use unicode_width::UnicodeWidthStr;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{
        Block, Borders, Clear, List, ListItem, ListState,
        Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
    },
};
use crate::tui::app::{AppState, Role};
use crate::tui::layout::compute_layout;
use crate::tui::markdown::{process_content, ContentLine};

/// Returns the number of terminal columns the status bar would need to fit
/// primary info and keybinding hints on a single row.
pub fn status_content_width(state: &AppState) -> u16 {
    let primary = format!(
        " \u{25cf} {} \u{203a} {}   {} tokens  {} tools  {} memories",
        state.provider_label, state.model_label,
        state.token_count, state.tool_count, state.memory_count,
    );
    let cost = match &state.last_turn_cost {
        Some(c) => match c.cost_usd {
            Some(usd) => format!(
                "  · last: {}↑ {}↓ tok  {}",
                c.prompt, c.completion,
                mira::providers::openrouter::format_usd(usd),
            ),
            None => format!("  · last: {}↑ {}↓ tok", c.prompt, c.completion),
        },
        None => String::new(),
    };
    let hints = format!(
        "  [{}]  [{}]  \u{2191}\u{2193} scroll  Alt+\u{2191}\u{2193} history  End=bottom  Ctrl+P palette  F5 theme  F6 layout",
        state.backend_label, state.layout_mode.as_str(),
    );
    // All characters here are single-column; char count == display width
    (primary.chars().count() + cost.chars().count() + hints.chars().count()) as u16
}

pub fn render_all(f: &mut Frame, state: &mut AppState) {
    let area = f.area();
    let layout = compute_layout(state.layout_mode.clone(), area, status_content_width(state));

    render_chat(f, state, layout.chat_area);
    render_input(f, state, layout.input_area);

    if let Some(status_area) = layout.status_area {
        render_status_bar(f, state, status_area);
    }
    if let Some(sidebar_area) = layout.sidebar_area {
        render_sidebar(f, state, sidebar_area);
    }
    if state.show_completions && !state.completions.is_empty() {
        render_completions(f, state, layout.input_area);
    }
    // Server-unreachable banner. Painted over the top row of the chat area so
    // it's visible regardless of layout and auto-scrolls-out-of-view is impossible.
    if state.backend_label == "server" && !state.health_ok {
        render_unreachable_banner(f, state, layout.chat_area);
    }
    if state.palette_open {
        render_command_palette(f, state, area);
    }
}

/// One-line bright banner shown at the top of the chat panel when the TUI
/// is in server mode and the last health probe failed. Cleared automatically
/// once `state.health_ok` flips back to true (e.g. after `/reconnect`).
fn render_unreachable_banner(f: &mut Frame, state: &AppState, chat_area: Rect) {
    if chat_area.width < 4 || chat_area.height < 1 {
        return;
    }
    // Sit inside the block border — (+1, +1) offsets, (-2) from width.
    let banner_area = Rect {
        x:      chat_area.x + 1,
        y:      chat_area.y + 1,
        width:  chat_area.width.saturating_sub(2),
        height: 1,
    };

    let text = format!(
        " \u{26a0} Server unreachable — type /reconnect to retry ({})",
        state.backend_label,
    );
    let style = Style::default()
        .fg(Color::Black)
        .bg(Color::Yellow)
        .add_modifier(Modifier::BOLD);

    f.render_widget(Clear, banner_area);
    f.render_widget(
        Paragraph::new(text).style(style),
        banner_area,
    );
}

pub fn render_chat(f: &mut Frame, state: &mut AppState, area: Rect) {
    let theme = &state.theme;
    let block = Block::default()
        .title(" MIRA ")
        .title_style(theme.title_style())
        .borders(Borders::ALL)
        .border_style(theme.border_style())
        .style(Style::default().bg(theme.bg));

    let inner = block.inner(area);
    f.render_widget(block, area);

    // Width available for paragraph text (reserve 1 col on the right for
    // the scrollbar so the paragraph never wraps into that column).
    let para_width = inner.width.saturating_sub(1);

    let mut lines: Vec<Line> = Vec::new();
    for entry in &state.messages {
        let (label, label_style) = match entry.role {
            Role::User      => ("You  ", theme.user_msg_style()),
            Role::Assistant => ("MIRA ", theme.ai_msg_style()),
            Role::System    => ("sys  ", theme.system_msg_style()),
        };
        // Timestamp header
        let ts_display = if entry.timestamp.len() >= 16 {
            entry.timestamp[..16].replace('T', " ")
        } else {
            entry.timestamp.clone()
        };
        lines.push(Line::from(vec![
            Span::styled(label, label_style.add_modifier(Modifier::BOLD)),
            Span::styled(format!("  {}", ts_display), theme.dim_style()),
        ]));
        // Content — plain text is pre-wrapped with hanging indent so that
        // all continuation rows stay aligned.
        let msg_style = match entry.role {
            Role::User      => theme.user_msg_style(),
            Role::Assistant => theme.ai_msg_style(),
            Role::System    => theme.dim_style(),
        };
        for cl in process_content(&entry.content) {
            if let ContentLine::Plain(s) = cl {
                lines.extend(wrap_with_indent(&s, msg_style, para_width));
            } else {
                lines.push(render_content_line(cl, msg_style, theme));
            }
        }
        lines.push(Line::default());
    }

    // Streaming in-progress
    if state.is_streaming && !state.streaming_buffer.is_empty() {
        lines.push(Line::from(Span::styled(
            "MIRA \u{25ae}",
            theme.ai_msg_style().add_modifier(Modifier::BOLD),
        )));
        for cl in process_content(&state.streaming_buffer) {
            if let ContentLine::Plain(s) = cl {
                lines.extend(wrap_with_indent(&s, theme.ai_msg_style(), para_width));
            } else {
                lines.push(render_content_line(cl, theme.ai_msg_style(), theme));
            }
        }
    }

    let para_area = Rect { width: para_width, ..inner };

    // Build the paragraph first so we can call line_count() — ratatui's own
    // word-wrap counter — for a scroll value that exactly matches rendering.
    let paragraph = Paragraph::new(Text::from(lines))
        .wrap(Wrap { trim: false })
        .style(Style::default().fg(theme.fg).bg(theme.bg));

    let total_rows = paragraph.line_count(para_area.width).min(u16::MAX as usize) as u16;
    let visible    = inner.height;
    let max_scroll = total_rows.saturating_sub(visible);

    let scroll = if state.auto_scroll {
        max_scroll
    } else {
        let clamped = state.scroll_offset.min(max_scroll);
        if clamped >= max_scroll {
            state.auto_scroll = true;
        }
        state.scroll_offset = clamped;
        clamped
    };

    f.render_widget(paragraph.scroll((scroll, 0)), para_area);

    // Scrollbar sits in the rightmost column of `area` (on top of the block's
    // right border) — kept separate from the paragraph area.
    if max_scroll > 0 {
        let mut sb_state = ScrollbarState::new(max_scroll as usize)
            .position(scroll as usize);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("↑"))
            .end_symbol(Some("↓"))
            .track_symbol(Some("│"))
            .thumb_symbol("█")
            .style(theme.dim_style());
        f.render_stateful_widget(scrollbar, area, &mut sb_state);
    }
}

/// Wrap `text` into one or more `Line` values so that every row — including
/// continuations — begins with the standard 7-space hanging indent.
///
/// This is done before handing lines to ratatui so that the widget's own
/// word-wrap never sees a line that needs splitting, which means:
///   • continuation rows carry the indent (the cosmetic fix), and
///   • `Paragraph::line_count()` returns an accurate count (the scroll fix).
fn wrap_with_indent(text: &str, style: Style, para_width: u16) -> Vec<Line<'static>> {
    const INDENT: &str = "       "; // 7 spaces — matches table/timestamp gutter
    const INDENT_W: usize = 7;

    let available = (para_width as usize).saturating_sub(INDENT_W);

    // Fast path: entire text fits on one row.
    if available == 0 || UnicodeWidthStr::width(text) <= available {
        return vec![Line::from(Span::styled(
            format!("{}{}", INDENT, text),
            style,
        ))];
    }

    // Greedy word-wrap: pack space-separated words until the row is full,
    // then start a new indented row.
    let mut rows: Vec<Line<'static>> = Vec::new();
    let mut row   = String::new();
    let mut row_w = 0usize;

    for word in text.split_whitespace() {
        let w = UnicodeWidthStr::width(word);
        if row.is_empty() {
            row.push_str(word);
            row_w = w;
        } else if row_w + 1 + w <= available {
            row.push(' ');
            row.push_str(word);
            row_w += 1 + w;
        } else {
            rows.push(Line::from(Span::styled(
                format!("{}{}", INDENT, std::mem::take(&mut row)),
                style,
            )));
            row.push_str(word);
            row_w = w;
        }
    }
    // Push the last (or only) accumulated row.
    rows.push(Line::from(Span::styled(
        format!("{}{}", INDENT, row),
        style,
    )));
    rows
}

/// Convert a `ContentLine` into a ratatui `Line`, rendering table rows with
/// aligned columns and dim `|` borders.
fn render_content_line(cl: ContentLine, text_style: Style, theme: &crate::tui::theme::Theme) -> Line<'static> {
    let indent = "       ";
    let border_style = theme.dim_style();
    let header_style = text_style.add_modifier(Modifier::BOLD);

    match cl {
        ContentLine::Plain(s) => Line::from(Span::styled(format!("{}{}", indent, s), text_style)),

        ContentLine::TableTop { col_widths } => {
            let mut spans: Vec<Span<'static>> = vec![
                Span::raw(indent.to_string()),
                Span::styled("┌".to_string(), border_style),
            ];
            for (j, w) in col_widths.iter().enumerate() {
                spans.push(Span::styled("─".repeat(w + 2), border_style));
                let cap = if j + 1 < col_widths.len() { "┬" } else { "┐" };
                spans.push(Span::styled(cap.to_string(), border_style));
            }
            Line::from(spans)
        }

        ContentLine::TableRow { cells, col_widths, is_header } => {
            let cell_style = if is_header { header_style } else { text_style };
            let mut spans: Vec<Span<'static>> = vec![
                Span::raw(indent.to_string()),
                Span::styled("│".to_string(), border_style),
            ];
            for (j, cell) in cells.iter().enumerate() {
                let w = col_widths.get(j).copied().unwrap_or(cell.len());
                spans.push(Span::styled(format!(" {:<width$} ", cell, width = w), cell_style));
                spans.push(Span::styled("│".to_string(), border_style));
            }
            Line::from(spans)
        }

        ContentLine::TableSep { col_widths } => {
            let mut spans: Vec<Span<'static>> = vec![
                Span::raw(indent.to_string()),
                Span::styled("├".to_string(), border_style),
            ];
            for (j, w) in col_widths.iter().enumerate() {
                spans.push(Span::styled("─".repeat(w + 2), border_style));
                let cap = if j + 1 < col_widths.len() { "┼" } else { "┤" };
                spans.push(Span::styled(cap.to_string(), border_style));
            }
            Line::from(spans)
        }

        ContentLine::TableBottom { col_widths } => {
            let mut spans: Vec<Span<'static>> = vec![
                Span::raw(indent.to_string()),
                Span::styled("└".to_string(), border_style),
            ];
            for (j, w) in col_widths.iter().enumerate() {
                spans.push(Span::styled("─".repeat(w + 2), border_style));
                let cap = if j + 1 < col_widths.len() { "┴" } else { "┘" };
                spans.push(Span::styled(cap.to_string(), border_style));
            }
            Line::from(spans)
        }
    }
}

pub fn render_input(f: &mut Frame, state: &AppState, area: Rect) {
    let theme = &state.theme;
    let title = if state.is_streaming { " \u{27f3} Thinking\u{2026} " } else { " Message " };
    let block = Block::default()
        .title(title)
        .title_style(theme.title_style())
        .borders(Borders::ALL)
        .border_style(if state.is_streaming {
            theme.border_style().add_modifier(Modifier::SLOW_BLINK)
        } else {
            theme.border_style()
        })
        .style(Style::default().bg(theme.bg_input));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let display = format!("{}_", state.input);
    let paragraph = Paragraph::new(display)
        .style(theme.input_style())
        .wrap(Wrap { trim: false });
    f.render_widget(paragraph, inner);
}

pub fn render_status_bar(f: &mut Frame, state: &AppState, area: Rect) {
    let theme = &state.theme;
    let health_icon = if state.health_ok { "\u{25cf}" } else { "\u{25cb}" };
    let health_style = if state.health_ok {
        Style::default().fg(Color::Green)
    } else {
        Style::default().fg(Color::Red)
    };

    // Primary info: health + provider/model + counters
    let mut primary_spans = vec![
        Span::styled(format!(" {} ", health_icon), health_style),
        Span::styled(
            format!("{} \u{203a} {} ", state.provider_label, state.model_label),
            theme.status_style().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                "  {} tokens  {} tools  {} memories",
                state.token_count, state.tool_count, state.memory_count
            ),
            theme.dim_style(),
        ),
    ];
    // Per-turn cost footer: tokens always; $ only when we have pricing.
    if let Some(c) = &state.last_turn_cost {
        let suffix = match c.cost_usd {
            Some(usd) => format!(
                "  · last: {}↑ {}↓ tok  {}",
                c.prompt, c.completion,
                mira::providers::openrouter::format_usd(usd),
            ),
            None => format!(
                "  · last: {}↑ {}↓ tok",
                c.prompt, c.completion,
            ),
        };
        primary_spans.push(Span::styled(suffix, theme.dim_style()));
    }

    // Backend indicator: colour depends on whether the last health probe
    // succeeded; in local mode this is always green since there is nothing
    // to lose reachability to.
    let backend_style = if state.health_ok {
        Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
    };

    // Hints: backend + layout mode + keybinding cheat-sheet
    let hints_spans = vec![
        Span::styled(format!("  [{}]", state.backend_label), backend_style),
        Span::styled(
            format!(
                "  [{}]  \u{2191}\u{2193} scroll  Alt+\u{2191}\u{2193} history  End=bottom  Ctrl+P palette  F5 theme  F6 layout",
                state.layout_mode.as_str()
            ),
            theme.dim_style(),
        ),
    ];

    let bar = if area.height >= 2 {
        // Narrow terminal — stack primary info and hints on separate rows
        Paragraph::new(Text::from(vec![
            Line::from(primary_spans),
            Line::from(hints_spans),
        ]))
    } else {
        // Wide terminal — single row
        let mut spans = primary_spans;
        spans.extend(hints_spans);
        Paragraph::new(Line::from(spans))
    };

    f.render_widget(bar.style(theme.status_style()), area);
}

pub fn render_sidebar(f: &mut Frame, state: &AppState, area: Rect) {
    let theme = &state.theme;
    let block = Block::default()
        .title(" Panel ")
        .title_style(theme.title_style())
        .borders(Borders::ALL)
        .border_style(theme.border_style())
        .style(Style::default().bg(theme.bg_panel));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let health_icon  = if state.health_ok { "\u{25cf}" } else { "\u{25cb}" };
    let health_style = if state.health_ok {
        Style::default().fg(Color::Green)
    } else {
        Style::default().fg(Color::Red)
    };
    let bold = theme.status_style().add_modifier(Modifier::BOLD);
    let dim  = theme.dim_style();

    let id_short = &state.session_id[..state.session_id.len().min(8)];

    let mut items: Vec<ListItem> = vec![
        // ── Provider with health indicator ──
        ListItem::new(Line::from(vec![
            Span::styled(format!(" {} ", health_icon), health_style),
            Span::styled(state.provider_label.clone(), bold),
        ])),
        ListItem::new(
            Line::from(Span::styled(
                format!("  {}", state.model_label),
                dim,
            ))
        ),
        ListItem::new(""),
        // ── Session ──
        ListItem::new(Line::from(Span::styled(
            "\u{2500}\u{2500} Session \u{2500}\u{2500}", dim,
        ))),
        ListItem::new(format!("  ID:     {}", id_short)),
        ListItem::new(format!("  Msgs:   {}", state.messages.len())),
        ListItem::new(format!("  Tokens: {}", state.token_count)),
        ListItem::new(""),
        // ── Themes ──
        ListItem::new(Line::from(Span::styled(
            "\u{2500}\u{2500} Themes \u{2500}\u{2500}", dim,
        ))),
    ];

    for name in crate::tui::theme::Theme::all_names() {
        let is_active = *name == state.theme.name;
        let marker = if is_active { "\u{25b6} " } else { "  " };
        let style  = if is_active { theme.highlight_style() } else { dim };
        items.push(ListItem::new(format!("{}{}", marker, name)).style(style));
    }

    // ── Shortcuts ──
    items.push(ListItem::new(""));
    items.push(ListItem::new(Line::from(Span::styled(
        "\u{2500}\u{2500} Shortcuts \u{2500}\u{2500}", dim,
    ))));
    for (key, desc) in &[
        ("\u{2191}\u{2193}",         "scroll"),
        ("Alt+\u{2191}\u{2193}",     "history"),
        ("End",                      "bottom"),
        ("Ctrl+P",                   "palette"),
        ("Tab",                      "completions"),
        ("F5",                       "theme"),
        ("F6",                       "layout"),
        ("Ctrl+C",                   "quit"),
    ] {
        items.push(
            ListItem::new(Line::from(vec![
                Span::styled(format!("  {:8} ", key), bold),
                Span::styled(desc.to_string(), dim),
            ]))
        );
    }

    let list = List::new(items)
        .style(Style::default().fg(theme.fg).bg(theme.bg_panel));
    f.render_widget(list, inner);
}

pub fn render_completions(f: &mut Frame, state: &AppState, input_area: Rect) {
    if state.completions.is_empty() {
        return;
    }
    let theme = &state.theme;

    // Show at most 8 rows; popup sits just above the input bar
    let visible_rows = state.completions.len().min(8) as u16;
    let height = visible_rows + 2; // +2 for border
    let popup_area = Rect {
        x:      input_area.x,
        y:      input_area.y.saturating_sub(height),
        width:  input_area.width,
        height,
    };

    let block = Block::default()
        .title(" Tab / ↑↓: completions ")
        .borders(Borders::ALL)
        .border_style(theme.border_style())
        .style(Style::default().bg(theme.bg_panel));

    f.render_widget(Clear, popup_area);

    let items: Vec<ListItem> = state.completions.iter().enumerate().map(|(i, c)| {
        let is_sel = state.completion_sel == Some(i);
        let cmd_style = if is_sel {
            theme.highlight_style()
        } else {
            theme.input_style()
        };
        let line = Line::from(vec![
            Span::styled(format!("  {:22}", c.command), cmd_style),
            Span::styled(format!(" {}", c.description), theme.dim_style()),
        ]);
        ListItem::new(line)
    }).collect();

    // Use ListState so ratatui scrolls the list to keep the selection visible
    let mut list_state = ListState::default();
    list_state.select(state.completion_sel);

    let list = List::new(items)
        .block(block)
        .highlight_style(theme.highlight_style());

    f.render_stateful_widget(list, popup_area, &mut list_state);
}

pub fn render_command_palette(f: &mut Frame, state: &AppState, area: Rect) {
    let theme = &state.theme;
    // Wider popup to accommodate the longer command + description columns
    let width  = area.width.min(82).max(52);
    let height = 20u16.min(area.height.saturating_sub(4));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + 2;
    let popup_area = Rect { x, y, width, height };

    f.render_widget(Clear, popup_area);

    let block = Block::default()
        .title(" \u{2318} Command Palette ")
        .title_style(theme.title_style())
        .borders(Borders::ALL)
        .border_style(theme.border_style())
        .style(Style::default().bg(theme.bg_panel));

    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    // Query input row
    let query_area = Rect { x: inner.x, y: inner.y, width: inner.width, height: 1 };
    f.render_widget(
        Paragraph::new(format!("> {}_", state.palette_query))
            .style(theme.input_style()),
        query_area,
    );

    // Results list (starts 2 rows below the query: 1 for input, 1 blank)
    let results_area = Rect {
        x:      inner.x,
        y:      inner.y + 2,
        width:  inner.width,
        height: inner.height.saturating_sub(2),
    };

    let cmds = crate::tui::completion::all_commands();
    let q = state.palette_query.to_lowercase();
    let filtered: Vec<_> = cmds.iter()
        .filter(|c| {
            q.is_empty()
                || c.command.contains(q.as_str())
                || c.description.to_lowercase().contains(q.as_str())
        })
        .collect();

    // Clamp palette_sel to the filtered list so ListState is always valid
    let sel = if filtered.is_empty() {
        None
    } else {
        Some(state.palette_sel.min(filtered.len() - 1))
    };

    let items: Vec<ListItem> = filtered.iter().enumerate().map(|(i, c)| {
        let is_sel = sel == Some(i);
        let cmd_style = if is_sel { theme.highlight_style() } else { theme.input_style() };
        ListItem::new(Line::from(vec![
            // Command column: 3 chars wider than before (22 → 25)
            Span::styled(format!("  {:25}", c.command), cmd_style),
            Span::styled(format!(" {}", c.description), theme.dim_style()),
        ]))
    }).collect();

    let mut list_state = ListState::default();
    list_state.select(sel);

    let list = List::new(items)
        .highlight_style(theme.highlight_style());

    f.render_stateful_widget(list, results_area, &mut list_state);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use crate::tui::app::{AppState, Role};
    use crate::tui::layout::LayoutMode;

    #[test]
    fn test_render_does_not_panic() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = AppState::new();
        state.push_message(Role::User, "hello".to_string());
        state.push_message(Role::Assistant, "hi there".to_string());
        terminal.draw(|f| render_all(f, &mut state)).unwrap();
    }

    #[test]
    fn test_render_with_streaming() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = AppState::new();
        state.is_streaming = true;
        state.streaming_buffer = "partial response...".to_string();
        terminal.draw(|f| render_all(f, &mut state)).unwrap();
    }

    #[test]
    fn test_render_right_full_layout() {
        let backend = TestBackend::new(160, 50);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = AppState::new();
        state.layout_mode = LayoutMode::RightFull;
        state.health_ok = true;
        state.token_count = 1234;
        terminal.draw(|f| render_all(f, &mut state)).unwrap();
    }

    #[test]
    fn test_render_left_full_layout() {
        let backend = TestBackend::new(160, 50);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = AppState::new();
        state.layout_mode = LayoutMode::LeftFull;
        state.health_ok = false;
        terminal.draw(|f| render_all(f, &mut state)).unwrap();
    }

    #[test]
    fn test_render_right_only_layout() {
        let backend = TestBackend::new(160, 50);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = AppState::new();
        state.layout_mode = LayoutMode::RightOnly;
        terminal.draw(|f| render_all(f, &mut state)).unwrap();
    }

    #[test]
    fn test_render_left_only_layout() {
        let backend = TestBackend::new(160, 50);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = AppState::new();
        state.layout_mode = LayoutMode::LeftOnly;
        terminal.draw(|f| render_all(f, &mut state)).unwrap();
    }

    #[test]
    fn test_unreachable_banner_only_in_server_mode() {
        // Server + unhealthy → banner visible.
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = AppState::new();
        state.backend_label = "server".to_string();
        state.health_ok     = false;
        terminal.draw(|f| render_all(f, &mut state)).unwrap();
        let buf = terminal.backend().buffer();
        let rendered: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(
            rendered.contains("Server unreachable"),
            "banner should appear in server-mode when unhealthy"
        );

        // Local + unhealthy → banner hidden (nothing to reconnect to).
        let backend2 = TestBackend::new(120, 40);
        let mut terminal2 = Terminal::new(backend2).unwrap();
        let mut state2 = AppState::new();
        state2.backend_label = "local".to_string();
        state2.health_ok     = false;
        terminal2.draw(|f| render_all(f, &mut state2)).unwrap();
        let buf2 = terminal2.backend().buffer();
        let rendered2: String = buf2.content().iter().map(|c| c.symbol()).collect();
        assert!(
            !rendered2.contains("Server unreachable"),
            "banner must NOT appear in local mode"
        );
    }

    #[test]
    fn test_backend_label_appears_in_status_bar() {
        let backend = TestBackend::new(200, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = AppState::new();
        state.backend_label = "server".to_string();
        state.health_ok     = true;
        terminal.draw(|f| render_all(f, &mut state)).unwrap();
        let buf = terminal.backend().buffer();
        let rendered: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(
            rendered.contains("[server]"),
            "status bar should show [server] indicator"
        );
    }
}
