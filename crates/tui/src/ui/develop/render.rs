use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Padding, Paragraph};

use crate::app::AppState;
use crate::ui::wrap_line;

use super::{compute_model_lines, DevelopFocus, SPINNER_FRAMES};

/// Maximum number of batch steps rendered in the Develop Plan Stepper.
const MAX_STEPPER_STEPS: usize = 8;

pub fn render_develop_view(
    frame: &mut ratatui::Frame,
    state: &mut AppState,
    model_area: Rect,
    input_area: Rect,
    prompt_lines: Vec<Line<'static>>,
    cursor_visual_line: usize,
) {
    if let Some(lines) = plan_stepper_lines(state) {
        let body = u16::try_from(lines.len()).unwrap_or(u16::MAX);
        let desired = body.saturating_add(2);
        let max_allowed = model_area.height.saturating_sub(4);
        let stepper_height = desired.min(max_allowed);

        if stepper_height >= 3 {
            let chunks = Layout::vertical([
                Constraint::Length(stepper_height),
                Constraint::Min(0),
            ])
            .split(model_area);
            render_plan_stepper(frame, lines, chunks[0]);
            render_model_pane(frame, state, chunks[1]);
        } else {
            render_model_pane(frame, state, model_area);
        }
    } else {
        render_model_pane(frame, state, model_area);
    }

    render_prompt_input(frame, state, input_area, prompt_lines, cursor_visual_line);
}

/// Builds the compact Plan Stepper header for the active plan execution context.
///
/// Returns `None` for ad-hoc turns (no plan context) or when the referenced
/// plan is not loaded, so Develop keeps its standard layout with zero overhead.
fn plan_stepper_lines(state: &AppState) -> Option<Vec<Line<'static>>> {
    let plan_ctx = state.model.plan_context.as_ref()?;
    let plan = state
        .plans_view
        .plans
        .iter()
        .find(|plan| plan.id == plan_ctx.plan_id)?;

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled("Plan: ", Style::default().fg(Color::DarkGray).bold()),
        Span::styled(plan.title.clone(), Style::default().fg(Color::Cyan).bold()),
    ]));

    let step_ids: Vec<&str> = if plan_ctx.batch_step_ids.is_empty() {
        vec![plan_ctx.step_id.as_str()]
    } else {
        plan_ctx.batch_step_ids.iter().map(String::as_str).collect()
    };

    for id in step_ids.into_iter().take(MAX_STEPPER_STEPS) {
        let Some(item) = plan.find_item(id) else {
            continue;
        };
        let is_active = id == plan_ctx.step_id;

        let (badge, badge_style) = if is_active {
            (
                SPINNER_FRAMES[state.model.spinner_frame % SPINNER_FRAMES.len()],
                Style::default().fg(Color::Yellow).bold(),
            )
        } else {
            plan_status_badge(item.status)
        };

        let title_style = if is_active {
            Style::default().fg(Color::White).bold()
        } else {
            Style::default().fg(Color::Gray)
        };

        lines.push(Line::from(vec![
            Span::styled(
                if is_active { "> " } else { "  " },
                Style::default().fg(Color::Cyan),
            ),
            Span::styled(badge.to_string(), badge_style),
            Span::raw(" "),
            Span::styled(
                format!("#{} ", item.id),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(item.title.clone(), title_style),
        ]));
    }

    Some(lines)
}

fn plan_status_badge(status: tauqe_protocol::PlanItemStatus) -> (&'static str, Style) {
    match status {
        tauqe_protocol::PlanItemStatus::Done => ("✓", Style::default().fg(Color::Green).bold()),
        tauqe_protocol::PlanItemStatus::InProgress => {
            ("▶", Style::default().fg(Color::Yellow).bold())
        }
        tauqe_protocol::PlanItemStatus::Todo => ("○", Style::default().fg(Color::DarkGray)),
        tauqe_protocol::PlanItemStatus::Blocked => ("!", Style::default().fg(Color::Red).bold()),
        tauqe_protocol::PlanItemStatus::Cancelled => ("✗", Style::default().fg(Color::Red)),
        tauqe_protocol::PlanItemStatus::Discussion => {
            ("?", Style::default().fg(Color::Magenta).bold())
        }
    }
}

fn render_plan_stepper(frame: &mut ratatui::Frame, lines: Vec<Line<'static>>, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .padding(Padding::horizontal(1));
    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, area);
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

    if state.model.content_width != content_width {
        state.model.content_width = content_width;
        state.model.reasoning.max_width = Some(content_width);
        state.model.render_markdown_now();
        if !state.model.reasoning.is_empty() {
            state.model.reasoning.update_markdown();
        }
    }

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
    let mut model_lines = Vec::with_capacity(raw_lines.len());
    let mut current_visual = 0;
    for line in raw_lines {
        let wrapped = wrap_line(line, content_width);
        let wrapped_count = wrapped.len();
        raw_to_visual_start.push(current_visual);
        raw_to_visual_end.push(current_visual + wrapped_count.saturating_sub(1));
        current_visual += wrapped_count;
        model_lines.extend(wrapped);
    }

    for block in &mut state.model.code_blocks {
        let b_raw_start = prefix_count + block.raw_start_line;
        let b_raw_end = prefix_count + block.raw_end_line;
        if b_raw_start < raw_to_visual_start.len() && b_raw_end < raw_to_visual_end.len() {
            block.visual_start_line = raw_to_visual_start[b_raw_start];
            block.visual_end_line = raw_to_visual_end[b_raw_end];
        }
    }

    let total_lines = model_lines.len() as u16;
    state.model.rendered_lines_count = model_lines.len();

    let max_scroll = total_lines.saturating_sub(content_height);
    if state.model.auto_scroll || state.model.scroll > max_scroll {
        state.model.scroll = max_scroll;
    }

    let scroll_start = state.model.scroll as usize;
    let visible_count = content_height as usize;
    let visible_lines: Vec<Line<'static>> = model_lines
        .into_iter()
        .skip(scroll_start)
        .take(visible_count)
        .collect();

    let model_paragraph = Paragraph::new(visible_lines).block(model_block);
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

    let in_viewport = state.model.focus == DevelopFocus::Viewport;
    let border_style = if is_busy {
        Style::default().fg(Color::Yellow)
    } else if in_viewport {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default()
    };

    let mut input_block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style);
    if in_viewport {
        input_block = input_block.title(" [VIEWPORT] ");
    }

    let input_paragraph = Paragraph::new(input_lines)
        .block(input_block)
        .scroll((input_scroll, 0));
    frame.render_widget(input_paragraph, area);
}
