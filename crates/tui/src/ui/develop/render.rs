use ratatui::layout::Rect;
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Padding, Paragraph};

use crate::app::AppState;
use crate::ui::{wrap_line, wrap_lines};

use super::compute_model_lines;

pub fn render_develop_view(
    frame: &mut ratatui::Frame,
    state: &mut AppState,
    model_area: Rect,
    input_area: Rect,
    prompt_lines: Vec<Line<'static>>,
    cursor_visual_line: usize,
) {
    render_model_pane(frame, state, model_area);
    render_prompt_input(frame, state, input_area, prompt_lines, cursor_visual_line);
}

fn render_model_pane(frame: &mut ratatui::Frame, state: &mut AppState, area: Rect) {
    let model_block = Block::default().padding(Padding::horizontal(1));
    let inner = model_block.inner(area);

    let content_height = inner.height;
    let content_width = (inner.width as usize).max(10);
    state.last_model_height = content_height;
    state.model.content_rect = (
        inner.x,
        inner.y,
        inner.width,
        content_height,
    );

    if let Some((_, inst)) = state.model.copy_flash {
        if inst.elapsed().as_secs_f32() >= 2.0 {
            state.model.copy_flash = None;
            state.model.update_markdown();
        }
    }

    let raw_lines = compute_model_lines(&state.model);

    let prefix_count = (state.model.error.is_some() as usize)
        + (state.model.toolchain_status.is_some() as usize);

    let mut raw_to_visual_start = Vec::with_capacity(raw_lines.len());
    let mut raw_to_visual_end = Vec::with_capacity(raw_lines.len());
    let mut current_visual = 0;
    for line in &raw_lines {
        let wrapped_count = wrap_line(line.clone(), content_width).len();
        raw_to_visual_start.push(current_visual);
        raw_to_visual_end.push(current_visual + wrapped_count.saturating_sub(1));
        current_visual += wrapped_count;
    }

    for block in &mut state.model.code_blocks {
        let b_raw_start = prefix_count + block.raw_start_line;
        let b_raw_end = prefix_count + block.raw_end_line;
        if b_raw_start < raw_to_visual_start.len() && b_raw_end < raw_to_visual_end.len() {
            block.visual_start_line = raw_to_visual_start[b_raw_start];
            block.visual_end_line = raw_to_visual_end[b_raw_end];
        }
    }

    let model_lines = wrap_lines(raw_lines, content_width);
    let total_lines = model_lines.len() as u16;
    state.model.rendered_lines_count = model_lines.len();

    let max_scroll = total_lines.saturating_sub(content_height);
    if state.model.auto_scroll || state.model.scroll > max_scroll {
        state.model.scroll = max_scroll;
    }

    let model_paragraph = Paragraph::new(model_lines)
        .block(model_block)
        .scroll((state.model.scroll, 0));
    frame.render_widget(model_paragraph, area);
}

fn tokenize_with_offsets(s: &str) -> Vec<(&str, bool, usize)> {
    let mut tokens = Vec::new();
    let chars: Vec<(usize, char)> = s.char_indices().collect();
    if chars.is_empty() {
        return tokens;
    }
    let mut i = 0;
    while i < chars.len() {
        let is_ws = chars[i].1.is_whitespace();
        let start = chars[i].0;
        while i < chars.len() && chars[i].1.is_whitespace() == is_ws {
            i += 1;
        }
        let end = if i < chars.len() { chars[i].0 } else { s.len() };
        tokens.push((&s[start..end], is_ws, start));
    }
    tokens
}

pub fn build_prompt_lines(
    editor: &crate::editor::InputEditor,
    content_width: usize,
    is_busy: bool,
) -> (Vec<Line<'static>>, usize) {
    let content_width = content_width.max(10);
    let prefix_style = if is_busy {
        Style::default().fg(Color::DarkGray).bold()
    } else {
        Style::default().fg(Color::Cyan).bold()
    };

    if editor.is_empty() {
        let line = Line::from(vec![
            Span::styled(" > ", prefix_style),
            Span::styled("█", Style::default().fg(Color::Yellow)),
        ]);
        return (vec![line], 0);
    }

    let mut visual_lines: Vec<Line<'static>> = Vec::new();
    let mut cursor_visual_line: usize = 0;
    let mut byte_offset = 0;
    let logical_lines: Vec<&str> = editor.text.split('\n').collect();

    for raw_line in logical_lines {
        let line_len = raw_line.len();
        let line_start = byte_offset;
        let line_end = byte_offset + line_len;
        byte_offset = line_end + 1;

        let cursor_in_line = editor.cursor >= line_start && editor.cursor <= line_end;
        let cursor_offset = if cursor_in_line {
            Some(editor.cursor - line_start)
        } else {
            None
        };

        let mut current_spans: Vec<Span<'static>> = Vec::new();
        let mut current_width: usize = 0;

        let is_first_visual = visual_lines.is_empty();
        current_spans.push(Span::styled(
            if is_first_visual { " > " } else { "   " },
            prefix_style,
        ));
        current_width += 3;

        if raw_line.is_empty() {
            if cursor_offset == Some(0) {
                current_spans.push(Span::styled("█", Style::default().fg(Color::Yellow)));
                cursor_visual_line = visual_lines.len();
            }
            visual_lines.push(Line::from(current_spans));
            continue;
        }

        let tokens = tokenize_with_offsets(raw_line);
        for (token_str, is_ws, token_start) in tokens {
            let token_end = token_start + token_str.len();
            let has_cursor = cursor_offset.is_some_and(|c| c >= token_start && c < token_end);

            let token_spans: Vec<Span<'static>> = match cursor_offset {
                Some(c_off) if c_off >= token_start && c_off < token_end => {
                    let local_off = c_off - token_start;
                    let before = &token_str[..local_off];
                    let mut chars = token_str[local_off..].chars();
                    if let Some(ch) = chars.next() {
                        let ch_len = ch.len_utf8();
                        let after = &token_str[local_off + ch_len..];

                        let mut s = Vec::new();
                        if !before.is_empty() {
                            s.push(Span::raw(before.to_string()));
                        }
                        s.push(Span::styled(
                            ch.to_string(),
                            Style::default().bg(Color::White).fg(Color::Black).bold(),
                        ));
                        if !after.is_empty() {
                            s.push(Span::raw(after.to_string()));
                        }
                        s
                    } else {
                        vec![Span::raw(token_str.to_string())]
                    }
                }
                _ => vec![Span::raw(token_str.to_string())],
            };

            let token_width: usize = token_spans.iter().map(|s| s.width()).sum();

            if is_ws {
                if current_width + token_width <= content_width {
                    if has_cursor {
                        cursor_visual_line = visual_lines.len();
                    }
                    current_spans.extend(token_spans);
                    current_width += token_width;
                } else if current_width > 3 {
                    visual_lines.push(Line::from(std::mem::take(&mut current_spans)));
                    current_spans.push(Span::styled("   ", prefix_style));
                    current_width = 3;
                }
            } else if current_width + token_width <= content_width {
                if has_cursor {
                    cursor_visual_line = visual_lines.len();
                }
                current_spans.extend(token_spans);
                current_width += token_width;
            } else {
                if current_width > 3 {
                    visual_lines.push(Line::from(std::mem::take(&mut current_spans)));
                    current_spans.push(Span::styled("   ", prefix_style));
                    current_width = 3;
                }

                if current_width + token_width <= content_width {
                    if has_cursor {
                        cursor_visual_line = visual_lines.len();
                    }
                    current_spans.extend(token_spans);
                    current_width += token_width;
                } else {
                    for span in token_spans {
                        let style = span.style;
                        for ch in span.content.chars() {
                            let ch_str = ch.to_string();
                            let ch_w = Span::raw(&ch_str).width();
                            if current_width + ch_w > content_width && current_width > 3 {
                                visual_lines.push(Line::from(std::mem::take(&mut current_spans)));
                                current_spans.push(Span::styled("   ", prefix_style));
                                current_width = 3;
                            }
                            if style != Style::default() {
                                cursor_visual_line = visual_lines.len();
                            }
                            current_spans.push(Span::styled(ch_str, style));
                            current_width += ch_w;
                        }
                    }
                }
            }
        }

        if cursor_offset == Some(line_len) {
            let cursor_span = Span::styled("█", Style::default().fg(Color::Yellow));
            let cursor_w = cursor_span.width();
            if current_width + cursor_w > content_width && current_width > 3 {
                visual_lines.push(Line::from(std::mem::take(&mut current_spans)));
                current_spans.push(Span::styled("   ", prefix_style));
            }
            cursor_visual_line = visual_lines.len();
            current_spans.push(cursor_span);
        }

        if !current_spans.is_empty() {
            visual_lines.push(Line::from(current_spans));
        }
    }

    if visual_lines.is_empty() {
        visual_lines.push(Line::from(vec![
            Span::styled(" > ", prefix_style),
            Span::styled("█", Style::default().fg(Color::Yellow)),
        ]));
    }

    (visual_lines, cursor_visual_line)
}

fn render_prompt_input(
    frame: &mut ratatui::Frame,
    state: &AppState,
    area: Rect,
    input_lines: Vec<Line<'static>>,
    cursor_visual_line: usize,
) {
    let is_busy = state.model.is_busy();
    let visible_input_lines = area.height.saturating_sub(2);
    let input_scroll =
        if visible_input_lines > 0 && cursor_visual_line >= visible_input_lines as usize {
            (cursor_visual_line + 1 - visible_input_lines as usize) as u16
        } else {
            0
        };

    let border_style = if is_busy {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    };

    let input_paragraph = Paragraph::new(input_lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border_style),
        )
        .scroll((input_scroll, 0));
    frame.render_widget(input_paragraph, area);
}
