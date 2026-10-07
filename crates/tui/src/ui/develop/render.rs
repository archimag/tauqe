use ratatui::layout::Rect;
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Padding, Paragraph, Wrap};

use crate::app::AppState;
use crate::ui::{wrap_line, wrap_lines};

use super::compute_model_lines;

pub fn render_develop_view(
    frame: &mut ratatui::Frame,
    state: &mut AppState,
    model_area: Rect,
    input_area: Rect,
    cursor_visual_line: usize,
) {
    render_model_pane(frame, state, model_area);
    render_prompt_input(frame, state, input_area, cursor_visual_line);
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
    if let Some((_, inst)) = state.model.copy_notification {
        if inst.elapsed().as_secs_f32() >= 2.5 {
            state.model.copy_notification = None;
        }
    }

    let raw_lines = compute_model_lines(&state.model);

    let prefix_count = (state.model.error.is_some() as usize)
        + (state.model.git_notification.is_some() as usize)
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

fn render_prompt_input(
    frame: &mut ratatui::Frame,
    state: &AppState,
    area: Rect,
    cursor_visual_line: usize,
) {
    let is_busy = state.model.is_busy();
    let editor = &state.input_editor;
    let text = editor.get_text();
    let mut input_lines = Vec::new();
    let (cur_line_idx, cur_col) = editor.cursor_line_col();

    if text.is_empty() {
        let prompt_sym_style = if is_busy {
            Style::default().fg(Color::DarkGray).bold()
        } else {
            Style::default().fg(Color::Cyan).bold()
        };
        input_lines.push(Line::from(vec![
            Span::styled(" > ", prompt_sym_style),
            Span::styled("█", Style::default().fg(Color::Yellow)),
        ]));
    } else {
        let lines = editor.get_lines();

        for (l_idx, line_str) in lines.iter().enumerate() {
            let prefix = if l_idx == 0 { " > " } else { "   " };
            let mut spans = vec![Span::styled(
                prefix,
                Style::default().fg(Color::Cyan).bold(),
            )];

            if l_idx == cur_line_idx {
                let char_indices: Vec<(usize, char)> = line_str.char_indices().collect();
                if cur_col >= char_indices.len() {
                    spans.push(Span::raw(line_str.to_string()));
                    spans.push(Span::styled("█", Style::default().fg(Color::Yellow)));
                } else {
                    let (byte_offset, c) = char_indices[cur_col];
                    let before = &line_str[..byte_offset];
                    let char_len = c.len_utf8();
                    let after = &line_str[byte_offset + char_len..];

                    if !before.is_empty() {
                        spans.push(Span::raw(before.to_string()));
                    }
                    spans.push(Span::styled(
                        c.to_string(),
                        Style::default().bg(Color::White).fg(Color::Black).bold(),
                    ));
                    if !after.is_empty() {
                        spans.push(Span::raw(after.to_string()));
                    }
                }
            } else {
                spans.push(Span::raw(line_str.to_string()));
            }
            input_lines.push(Line::from(spans));
        }
    }

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
        .wrap(Wrap { trim: false })
        .scroll((input_scroll, 0));
    frame.render_widget(input_paragraph, area);
}
