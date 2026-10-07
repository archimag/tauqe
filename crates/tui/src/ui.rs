use std::path::Path;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use tauqe_protocol::{ContextAccess, ContextLayer, UiHistoryItem, UiHistoryKind};

use crate::app::{AppState, ViewMode};
use crate::context_view::ContextRow;
use crate::model_view::compute_model_lines;

pub fn extract_current_round(text: &str) -> Option<String> {
    let last_round = text.rfind("[Round ");
    let last_retry = text.rfind("[Patch Retry ");
    match (last_round, last_retry) {
        (Some(r), Some(t)) if r >= t => extract_bracketed(&text[r..]),
        (Some(_), Some(t)) => extract_bracketed(&text[t..]),
        (Some(r), None) => extract_bracketed(&text[r..]),
        (None, Some(t)) => extract_bracketed(&text[t..]),
        (None, None) => None,
    }
}

fn extract_bracketed(slice: &str) -> Option<String> {
    slice.find(']').map(|end| slice[1..end].to_string())
}

pub fn compute_history_items_line_count(items: &[UiHistoryItem], max_width: Option<usize>) -> usize {
    compute_history_lines(items, max_width).len()
}

fn tokenize(s: &str) -> Vec<(&str, bool)> {
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
        tokens.push((&s[start..end], is_ws));
    }
    tokens
}

pub fn wrap_line(line: Line<'static>, max_width: usize) -> Vec<Line<'static>> {
    if max_width == 0 || line.width() <= max_width {
        return vec![line];
    }

    let indent = if let Some(first_span) = line.spans.first() {
        let leading_spaces = first_span.content.chars().take_while(|c| *c == ' ').count();
        if leading_spaces > 0 {
            " ".repeat(leading_spaces.min(8))
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    let mut wrapped_lines: Vec<Line<'static>> = Vec::new();
    let mut current_spans: Vec<Span<'static>> = Vec::new();
    let mut current_width: usize = 0;

    for span in line.spans {
        let style = span.style;
        let content = span.content.into_owned();
        let tokens = tokenize(&content);

        for (token_text, is_ws) in tokens {
            let token_span = Span::styled(token_text.to_string(), style);
            let token_width = token_span.width();

            if is_ws {
                if current_spans.is_empty() && !wrapped_lines.is_empty() {
                    continue;
                }
                if current_width + token_width <= max_width {
                    current_spans.push(token_span);
                    current_width += token_width;
                } else if !current_spans.is_empty() {
                    wrapped_lines.push(Line::from(std::mem::take(&mut current_spans)));
                    current_width = 0;
                }
            } else {
                if current_spans.is_empty() && !wrapped_lines.is_empty() && !indent.is_empty() {
                    let indent_span = Span::raw(indent.clone());
                    let indent_w = indent_span.width();
                    current_spans.push(indent_span);
                    current_width += indent_w;
                }

                if current_width + token_width <= max_width {
                    current_spans.push(token_span);
                    current_width += token_width;
                } else {
                    let has_words = current_spans.iter().any(|s| !s.content.trim().is_empty());
                    if has_words {
                        wrapped_lines.push(Line::from(std::mem::take(&mut current_spans)));
                        current_width = 0;
                        if !indent.is_empty() {
                            let indent_span = Span::raw(indent.clone());
                            let indent_w = indent_span.width();
                            current_spans.push(indent_span);
                            current_width += indent_w;
                        }
                    }

                    if current_width + token_width <= max_width {
                        current_spans.push(token_span);
                        current_width += token_width;
                    } else {
                        // Word is longer than available line width: split character by character
                        let mut sub = String::new();
                        for ch in token_text.chars() {
                            let ch_w = Span::raw(ch.to_string()).width();
                            if current_width + ch_w > max_width && !sub.is_empty() {
                                current_spans.push(Span::styled(std::mem::take(&mut sub), style));
                                wrapped_lines.push(Line::from(std::mem::take(&mut current_spans)));
                                current_width = 0;
                                if !indent.is_empty() {
                                    let indent_span = Span::raw(indent.clone());
                                    let indent_w = indent_span.width();
                                    current_spans.push(indent_span);
                                    current_width += indent_w;
                                }
                            }
                            sub.push(ch);
                            current_width += ch_w;
                        }
                        if !sub.is_empty() {
                            current_spans.push(Span::styled(sub, style));
                        }
                    }
                }
            }
        }
    }

    if !current_spans.is_empty() {
        let has_content = current_spans.iter().any(|s| !s.content.trim().is_empty());
        if has_content || wrapped_lines.is_empty() {
            wrapped_lines.push(Line::from(current_spans));
        }
    }

    if wrapped_lines.is_empty() {
        vec![Line::raw("")]
    } else {
        wrapped_lines
    }
}

pub fn wrap_lines(lines: Vec<Line<'static>>, max_width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        out.extend(wrap_line(line, max_width));
    }
    out
}

pub fn compute_history_lines(items: &[UiHistoryItem], max_width: Option<usize>) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let div_width = max_width.unwrap_or(80).max(10);
    for (idx, item) in items.iter().enumerate() {
        if idx > 0 {
            lines.push(Line::from(Span::styled(
                "─".repeat(div_width),
                Style::default().fg(Color::Rgb(60, 60, 60)),
            )));
        }

        let badge = match item.kind {
            UiHistoryKind::User => Span::styled(
                " [USER] ",
                Style::default().bg(Color::Blue).fg(Color::White).bold(),
            ),
            UiHistoryKind::Assistant => Span::styled(
                " [ASSISTANT] ",
                Style::default().bg(Color::Green).fg(Color::Black).bold(),
            ),
            UiHistoryKind::System => {
                if item.commit_hash.is_some() && item.text.to_lowercase().contains("undid") {
                    Span::styled(
                        " [UNDO] ",
                        Style::default().bg(Color::Yellow).fg(Color::Black).bold(),
                    )
                } else {
                    Span::styled(
                        " [SYSTEM] ",
                        Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
                    )
                }
            }
        };

        let id_span = Span::styled(
            format!(" #{}", item.id),
            Style::default().fg(Color::DarkGray).bold(),
        );

        let mut header_spans = vec![badge, id_span];
        if let Some(hash) = &item.commit_hash {
            header_spans.push(Span::raw(" "));
            header_spans.push(Span::styled(
                format!("[COMMIT: {}]", hash),
                Style::default().fg(Color::Cyan).bold(),
            ));
        }
        if let Some(summary) = &item.summary {
            header_spans.push(Span::raw(" - "));
            header_spans.push(Span::styled(summary.clone(), Style::default().bold()));
        }
        lines.push(Line::from(header_spans));

        let text_style = match item.kind {
            UiHistoryKind::User => Style::default().fg(Color::Cyan),
            UiHistoryKind::Assistant => Style::default().fg(Color::White),
            UiHistoryKind::System => Style::default().fg(Color::Yellow),
        };

        if item.kind == UiHistoryKind::Assistant {
            let mut history_theme = crate::markdown::MarkdownTheme::answer();
            history_theme.line_prefix = Some(Span::raw("  "));
            let md_lines = crate::markdown::render_markdown(&item.text, &history_theme);
            if md_lines.is_empty() {
                for line in item.text.lines() {
                    lines.push(Line::from(Span::styled(format!("  {}", line), text_style)));
                }
            } else {
                lines.extend(md_lines);
            }
        } else {
            for line in item.text.lines() {
                lines.push(Line::from(Span::styled(format!("  {}", line), text_style)));
            }
        }

        if !item.files.is_empty() {
            if item.kind == UiHistoryKind::Assistant {
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        format!("Modified files ({}):", item.files.len()),
                        Style::default().fg(Color::Cyan).bold(),
                    ),
                ]));
                for file_entry in &item.files {
                    let (op_label, op_style, path) = if let Some(p) = file_entry.strip_prefix("[NEW] ") {
                        ("[NEW] ", Style::default().fg(Color::Green).bold(), p)
                    } else if let Some(p) = file_entry.strip_prefix("[DEL] ") {
                        ("[DEL] ", Style::default().fg(Color::Red).bold(), p)
                    } else if let Some(p) = file_entry.strip_prefix("[MOVE] ") {
                        ("[MOVE] ", Style::default().fg(Color::Cyan).bold(), p)
                    } else if let Some(p) = file_entry.strip_prefix("[EDIT] ") {
                        ("[EDIT] ", Style::default().fg(Color::Magenta).bold(), p)
                    } else {
                        ("• ", Style::default().fg(Color::Yellow).bold(), file_entry.as_str())
                    };

                    lines.push(Line::from(vec![
                        Span::raw("    "),
                        Span::styled(op_label, op_style),
                        Span::styled(path.to_string(), Style::default().fg(Color::White)),
                    ]));
                }
            } else {
                let mut file_spans = vec![
                    Span::raw("  "),
                    Span::styled("Context: ", Style::default().fg(Color::DarkGray).bold()),
                ];
                let files_joined = item.files.join(", ");
                file_spans.push(Span::styled(files_joined, Style::default().fg(Color::DarkGray)));
                lines.push(Line::from(file_spans));
            }
        }
    }

    if let Some(width) = max_width {
        wrap_lines(lines, width)
    } else {
        lines
    }
}

pub fn render_ui(frame: &mut ratatui::Frame, state: &mut AppState) {
    let term_height = frame.area().height;
    let term_width = frame.area().width;
    let max_input_height = (term_height * 4 / 10).clamp(6, 16);
    let inner_input_width = (term_width.saturating_sub(2)) as usize;
    let (total_visual_lines, cursor_visual_line) = state
        .input_editor
        .visual_lines_and_cursor(inner_input_width);

    let needed_input_height = (total_visual_lines as u16 + 2).max(3);
    let input_height = needed_input_height.min(max_input_height);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(input_height),
            Constraint::Length(3),
        ])
        .split(frame.area());

    // Top Header
    let (project_name, dirty_status) = match &state.repo_state {
        Some(repo) => {
            let name = Path::new(&repo.root)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&repo.root);
            let dirty = if repo.dirty {
                Span::styled(" [DIRTY]", Style::default().fg(Color::Yellow).bold())
            } else {
                Span::styled(" [CLEAN]", Style::default().fg(Color::Green))
            };
            (name.to_string(), dirty)
        }
        None => ("no repository".to_string(), Span::raw("")),
    };

    let (model_tab_style, context_tab_style, history_tab_style) = match state.view_mode {
        ViewMode::Model => (
            Style::default().bg(Color::Blue).fg(Color::White).bold(),
            Style::default().bg(Color::DarkGray).fg(Color::White),
            Style::default().bg(Color::DarkGray).fg(Color::White),
        ),
        ViewMode::Context => (
            Style::default().bg(Color::DarkGray).fg(Color::White),
            Style::default().bg(Color::Blue).fg(Color::White).bold(),
            Style::default().bg(Color::DarkGray).fg(Color::White),
        ),
        ViewMode::History => (
            Style::default().bg(Color::DarkGray).fg(Color::White),
            Style::default().bg(Color::DarkGray).fg(Color::White),
            Style::default().bg(Color::Blue).fg(Color::White).bold(),
        ),
        ViewMode::Onboarding => (
            Style::default().bg(Color::DarkGray).fg(Color::White),
            Style::default().bg(Color::DarkGray).fg(Color::White),
            Style::default().bg(Color::DarkGray).fg(Color::White),
        ),
    };

    let onboarding_tab_span = if state.view_mode == ViewMode::Onboarding {
        vec![
            Span::raw(" "),
            Span::styled(
                " ! Setup / Onboarding ",
                Style::default().bg(Color::Yellow).fg(Color::Black).bold(),
            ),
        ]
    } else {
        Vec::new()
    };

    let mut header_spans = vec![
        Span::styled(
            " TAUQE ",
            Style::default().bg(Color::Cyan).fg(Color::Black).bold(),
        ),
        Span::raw("  "),
        Span::styled(" 1: Model ", model_tab_style),
        Span::raw(" "),
        Span::styled(
            format!(" 2: Context ({}) ", state.context.items.len()),
            context_tab_style,
        ),
        Span::raw(" "),
        Span::styled(
            format!(" 3: History ({}) ", state.history_view.total_count),
            history_tab_style,
        ),
    ];
    header_spans.extend(onboarding_tab_span);
    header_spans.extend(vec![
        Span::raw(" | Project: "),
        Span::styled(project_name, Style::default().bold()),
        dirty_status,
        Span::raw("  | Wf: "),
        Span::styled(&state.workflow, Style::default().fg(Color::Cyan).bold()),
        Span::raw("  Proto: "),
        Span::styled(
            &state.edit_protocol,
            Style::default().fg(Color::Magenta).bold(),
        ),
        Span::raw("  Model: "),
        Span::styled(
            if state.active_model.is_empty() {
                "-"
            } else {
                state.active_model.as_str()
            },
            Style::default().fg(Color::Green).bold(),
        ),
        Span::raw("  | Session: "),
        Span::styled(
            format!("${:.5}", state.model.session_total_cost),
            if state.model.session_total_cost > 0.0 {
                Style::default().fg(Color::Yellow).bold()
            } else {
                Style::default().fg(Color::DarkGray)
            },
        ),
        Span::raw("  Prev: "),
        Span::styled(
            match state.model.prev_cost {
                Some(cost) => format!("${:.5}", cost),
                None => "-".to_string(),
            },
            if state.model.prev_cost.unwrap_or(0.0) > 0.0 {
                Style::default().fg(Color::Yellow).bold()
            } else {
                Style::default().fg(Color::DarkGray)
            },
        ),
        Span::raw("  Current: "),
        Span::styled(
            match state.model.current_cost {
                Some(cost) => format!("${:.5}", cost),
                None => "-".to_string(),
            },
            if state.model.current_cost.unwrap_or(0.0) > 0.0 {
                Style::default().fg(Color::Yellow).bold()
            } else {
                Style::default().fg(Color::DarkGray)
            },
        ),
    ]);
    let header_line = Line::from(header_spans);

    let header = Paragraph::new(header_line).block(Block::default().borders(Borders::ALL));
    frame.render_widget(header, chunks[0]);

    match state.view_mode {
        ViewMode::Onboarding => {
            render_onboarding_view(frame, state, chunks[1], chunks[2], chunks[3]);
        }
        ViewMode::Model => {
            let content_height = chunks[1].height.saturating_sub(2);
            let content_width = (chunks[1].width.saturating_sub(2) as usize).max(10);
            state.last_model_height = content_height;
            state.model.content_rect = (
                chunks[1].x + 1,
                chunks[1].y + 1,
                chunks[1].width.saturating_sub(2),
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

            let spin = crate::model_view::SPINNER_FRAMES
                [state.model.spinner_frame % crate::model_view::SPINNER_FRAMES.len()];
            let round_suffix = extract_current_round(&state.model.text)
                .map(|r| format!(" [{}]", r))
                .unwrap_or_default();

            let (status_text, status_style) = match state.model.status.as_str() {
                "awaiting" | "starting" => (
                    format!("{} Awaiting...{}", spin, round_suffix),
                    Style::default().bold().fg(Color::Yellow),
                ),
                "thinking" => (
                    format!("{} Thinking...{}", spin, round_suffix),
                    Style::default().bold().fg(Color::Rgb(130, 170, 220)),
                ),
                "responding" | "streaming" => (
                    format!("{} Responding...{}", spin, round_suffix),
                    Style::default().bold().fg(Color::Yellow),
                ),
                "editing" => (
                    format!("{} Editing...{}", spin, round_suffix),
                    Style::default().bold().fg(Color::Magenta),
                ),
                "verifying" => (
                    format!("{} Verifying...{}", spin, round_suffix),
                    Style::default().bold().fg(Color::Cyan),
                ),
                "done" => (
                    format!("Done{}", round_suffix),
                    Style::default().bold().fg(Color::Green),
                ),
                "cancelled" => (
                    format!("Cancelled{}", round_suffix),
                    Style::default().bold().fg(Color::Red),
                ),
                "error" => ("Error".to_string(), Style::default().bold().fg(Color::Red)),
                _ => ("Idle".to_string(), Style::default().bold().fg(Color::Gray)),
            };

            let scroll_indicator = if max_scroll > 0 {
                format!(" [{}/{}]", state.model.scroll + 1, total_lines)
            } else {
                String::new()
            };

            let mut title_spans = vec![
                Span::raw(" Model View: "),
                Span::styled(status_text, status_style),
                Span::styled(scroll_indicator, Style::default().fg(Color::DarkGray)),
            ];

            if let Some((notif, inst)) = &state.model.copy_notification {
                if inst.elapsed().as_secs_f32() < 2.5 {
                    title_spans.push(Span::raw(" "));
                    title_spans.push(Span::styled(
                        format!(" [✓ {}] ", notif),
                        Style::default().bg(Color::Green).fg(Color::Black).bold(),
                    ));
                }
            }

            title_spans.push(Span::raw(" "));
            let title_line = Line::from(title_spans);

            let model_paragraph = Paragraph::new(model_lines)
                .block(Block::default().title(title_line).borders(Borders::ALL))
                .scroll((state.model.scroll, 0));
            frame.render_widget(model_paragraph, chunks[1]);

            let is_busy = state.model.is_busy();
            let editor = &state.input_editor;
            let text = editor.get_text();
            let mut input_lines = Vec::new();
            let (cur_line_idx, cur_col) = editor.cursor_line_col();

            if text.is_empty() {
                let placeholder = if is_busy {
                    format!(
                        " Model is {}... Draft prompt for next turn (Esc to cancel)...",
                        state.model.status
                    )
                } else {
                    " Type your prompt (Enter to send, Shift+Enter or Ctrl+J for newline, '?' for help)...".to_string()
                };
                let prompt_sym_style = if is_busy {
                    Style::default().fg(Color::DarkGray).bold()
                } else {
                    Style::default().fg(Color::Cyan).bold()
                };
                input_lines.push(Line::from(vec![
                    Span::styled(" > ", prompt_sym_style),
                    Span::styled("█", Style::default().fg(Color::Yellow)),
                    Span::styled(placeholder, Style::default().fg(Color::DarkGray)),
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

            let visible_input_lines = chunks[2].height.saturating_sub(2);
            let input_scroll =
                if visible_input_lines > 0 && cursor_visual_line >= visible_input_lines as usize {
                    (cursor_visual_line + 1 - visible_input_lines as usize) as u16
                } else {
                    0
                };

            let (input_title, border_style) = if is_busy {
                let title = if editor.line_count() > 1 {
                    format!(
                        " Intent [{} Busy: {} | Esc to cancel] (Line {}/{}) ",
                        spin,
                        state.model.status,
                        cur_line_idx + 1,
                        editor.line_count()
                    )
                } else {
                    format!(
                        " Intent [{} Busy: {} | Esc to cancel] ",
                        spin,
                        state.model.status
                    )
                };
                (title, Style::default().fg(Color::Yellow))
            } else if editor.line_count() > 1 {
                (
                    format!(
                        " Intent (Line {}/{}, Enter to send) ",
                        cur_line_idx + 1,
                        editor.line_count()
                    ),
                    Style::default(),
                )
            } else {
                (
                    " Intent (Enter to send, Shift+Enter / Ctrl+J for newline) ".to_string(),
                    Style::default(),
                )
            };

            let input_paragraph = Paragraph::new(input_lines)
                .block(
                    Block::default()
                        .title(input_title)
                        .borders(Borders::ALL)
                        .border_style(border_style),
                )
                .wrap(Wrap { trim: false })
                .scroll((input_scroll, 0));
            frame.render_widget(input_paragraph, chunks[2]);

            let footer_line = Line::from(vec![
                Span::styled(
                    " Ctrl+1/2/3 ",
                    Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
                ),
                Span::raw(" Switch  "),
                Span::styled(
                    " Ctrl+W ",
                    Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
                ),
                Span::raw(" Wf  "),
                Span::styled(
                    " Ctrl+P ",
                    Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
                ),
                Span::raw(" Proto  "),
                Span::styled(
                    " Ctrl+M ",
                    Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
                ),
                Span::raw(" Model  "),
                Span::styled(
                    if is_busy { " Esc " } else { " Enter " },
                    if is_busy {
                        Style::default().bg(Color::Red).fg(Color::White).bold()
                    } else {
                        Style::default().bg(Color::DarkGray).fg(Color::White).bold()
                    },
                ),
                Span::raw(if is_busy { " Cancel Gen  " } else { " Send  " }),
                Span::styled(
                    " Shift+Enter / Ctrl+J ",
                    Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
                ),
                Span::raw(" Newline  "),
                Span::styled(
                    " [/] ",
                    Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
                ),
                Span::raw(" Files  "),
                Span::styled(
                    " Space ",
                    Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
                ),
                Span::raw(" Diff  "),
                Span::styled(
                    " u ",
                    Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
                ),
                Span::raw(" Undo  "),
                Span::styled(
                    " s ",
                    Style::default().bg(Color::Cyan).fg(Color::Black).bold(),
                ),
                Span::raw(" Squash  "),
                Span::styled(
                    " Ctrl+L ",
                    Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
                ),
                Span::raw(" Clear Hist  "),
                Span::styled(
                    " ? ",
                    Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
                ),
                Span::raw(" Help"),
            ]);
            let footer = Paragraph::new(footer_line).block(Block::default().borders(Borders::ALL));
            frame.render_widget(footer, chunks[3]);
        }
        ViewMode::Context => {
            let mut context_lines: Vec<Line> = Vec::new();

            let pinned_count = state.context.items.iter().filter(|i| i.layer == ContextLayer::Pinned).count();
            let user_count = state.context.items.iter().filter(|i| i.layer == ContextLayer::User).count();
            let auto_count = state.context.items.iter().filter(|i| i.layer == ContextLayer::Auto).count();

            context_lines.push(Line::from(vec![
                Span::raw("Total size: "),
                Span::styled(
                    format!("~{} tokens", state.context.total_estimated_tokens),
                    Style::default().fg(Color::Cyan).bold(),
                ),
                Span::raw(format!(
                    " | Files: {} (Pinned: {}, User: {}, Auto: {})",
                    state.context.items.len(),
                    pinned_count,
                    user_count,
                    auto_count
                )),
                Span::raw(format!(" | Revision: #{}", state.context.revision)),
            ]));
            context_lines.push(Line::raw(""));

            let rows = state.context_view.compute_rows(&state.context.items);
            let active_row = rows.get(state.context_view.cursor_index).cloned();

            for (idx, row) in rows.iter().enumerate() {
                let is_selected = idx == state.context_view.cursor_index;
                match row {
                    ContextRow::Header(layer) => {
                        let (arrow, name, count, tokens, style_color, hint) = match layer {
                            ContextLayer::Pinned => {
                                let arrow = if state.context_view.pinned_expanded { "▼" } else { "▶" };
                                let count = pinned_count;
                                let tokens: u64 = state.context.items.iter()
                                    .filter(|i| i.layer == ContextLayer::Pinned)
                                    .map(|i| i.estimated_tokens)
                                    .sum();
                                (arrow, "Pinned Files", count, tokens, Color::Cyan, " [Protected / Config]")
                            }
                            ContextLayer::User => {
                                let arrow = if state.context_view.user_expanded { "▼" } else { "▶" };
                                let count = user_count;
                                let tokens: u64 = state.context.items.iter()
                                    .filter(|i| i.layer == ContextLayer::User)
                                    .map(|i| i.estimated_tokens)
                                    .sum();
                                (arrow, "User Files", count, tokens, Color::Yellow, " [Editable / Read-Only]")
                            }
                            ContextLayer::Auto => {
                                let arrow = if state.context_view.auto_expanded { "▼" } else { "▶" };
                                let count = auto_count;
                                let tokens: u64 = state.context.items.iter()
                                    .filter(|i| i.layer == ContextLayer::Auto)
                                    .map(|i| i.estimated_tokens)
                                    .sum();
                                (arrow, "Auto Files", count, tokens, Color::Magenta, " [Model Requested]")
                            }
                        };

                        let prefix = if is_selected { "● " } else { "  " };
                        let line_style = if is_selected {
                            Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD)
                        } else {
                            Style::default()
                        };

                        let header_line = Line::from(vec![
                            Span::styled(prefix, Style::default().fg(style_color).bold()),
                            Span::styled(format!("{} {} ", arrow, name), Style::default().fg(style_color).bold()),
                            Span::styled(
                                format!("({} files, ~{} tokens)", count, tokens),
                                Style::default().fg(Color::White).bold(),
                            ),
                            Span::styled(hint, Style::default().fg(Color::DarkGray)),
                        ]).style(line_style);

                        context_lines.push(header_line);
                    }
                    ContextRow::Item(item) => {
                        let cursor_prefix = if is_selected { "   ▶ " } else { "     " };

                        let (access_badge, access_style) = match item.access {
                            ContextAccess::Editable => {
                                ("[EDITABLE] ", Style::default().fg(Color::Yellow).bold())
                            }
                            ContextAccess::ReadOnly => {
                                ("[READ-ONLY]", Style::default().fg(Color::Green))
                            }
                        };

                        let line_style = if is_selected {
                            Style::default()
                                .bg(Color::DarkGray)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default()
                        };

                        let item_line = Line::from(vec![
                            Span::styled(cursor_prefix, Style::default().fg(Color::Cyan)).bold(),
                            Span::styled(access_badge, access_style),
                            Span::raw(" "),
                            Span::styled(&item.path, Style::default().bold()),
                            Span::styled(
                                format!(
                                    "  (~{} tokens, {} B)",
                                    item.estimated_tokens, item.size_bytes
                                ),
                                Style::default().fg(Color::DarkGray),
                            ),
                        ])
                        .style(line_style);

                        context_lines.push(item_line);
                    }
                }
            }

            let ctx_paragraph = Paragraph::new(context_lines)
                .block(
                    Block::default()
                        .title(" Project Context (Pinned, User, Auto) ")
                        .borders(Borders::ALL),
                )
                .wrap(Wrap { trim: false });
            frame.render_widget(ctx_paragraph, chunks[1]);

            let info_line = if let Some(msg) = &state.context_view.status_message {
                Line::from(Span::styled(msg, Style::default().fg(Color::Green).bold()))
            } else {
                match active_row {
                    Some(ContextRow::Header(ContextLayer::Pinned)) => Line::from(Span::styled(
                        "Pinned section: Architecture & key documents from tauqe.toml (Space/Enter to fold)",
                        Style::default().fg(Color::Cyan),
                    )),
                    Some(ContextRow::Header(ContextLayer::User)) => Line::from(Span::styled(
                        "User section: 'e' Add editable, 'r' Add read-only, Space/Enter to fold",
                        Style::default().fg(Color::Yellow),
                    )),
                    Some(ContextRow::Header(ContextLayer::Auto)) => Line::from(Span::styled(
                        "Auto section: Model-requested context. 'c' Clear all auto, Space/Enter to fold",
                        Style::default().fg(Color::Magenta),
                    )),
                    Some(ContextRow::Item(ref it)) => match it.layer {
                        ContextLayer::Pinned => Line::from(Span::styled(
                            "Pinned file: Protected read-only document (cannot be removed or modified)",
                            Style::default().fg(Color::Cyan),
                        )),
                        ContextLayer::User => Line::from(Span::styled(
                            "User file: 't' Toggle Editable ↔ Read-Only, 'd'/'x' Remove",
                            Style::default().fg(Color::White),
                        )),
                        ContextLayer::Auto => Line::from(Span::styled(
                            "Auto file: Press 'p'/'u' or Enter to promote to User, 'd'/'x' Remove, 'c' Clear all auto",
                            Style::default().fg(Color::Magenta),
                        )),
                    },
                    None => Line::from(Span::styled(
                        "Press 'e' for editable, 'r' for read-only, 'Space' to fold/unfold",
                        Style::default().fg(Color::DarkGray),
                    )),
                }
            };
            let prompt_widget = Paragraph::new(info_line).block(
                Block::default()
                    .title(" Context Actions ")
                    .borders(Borders::ALL),
            );
            frame.render_widget(prompt_widget, chunks[2]);

            let footer_line = Line::from(vec![
                Span::styled(
                    " Ctrl+1/2/3 ",
                    Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
                ),
                Span::raw(" Switch  "),
                Span::styled(
                    " Space ",
                    Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
                ),
                Span::raw(" Fold  "),
                Span::styled(
                    " e ",
                    Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
                ),
                Span::raw(" Add Edit  "),
                Span::styled(
                    " r ",
                    Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
                ),
                Span::raw(" Add Read  "),
                Span::styled(
                    " t ",
                    Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
                ),
                Span::raw(" Toggle  "),
                Span::styled(
                    " p/u ",
                    Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
                ),
                Span::raw(" Promote  "),
                Span::styled(
                    " c ",
                    Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
                ),
                Span::raw(" Clear Auto  "),
                Span::styled(
                    " d/x ",
                    Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
                ),
                Span::raw(" Remove  "),
                Span::styled(
                    " ? ",
                    Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
                ),
                Span::raw(" Help"),
            ]);
            let footer = Paragraph::new(footer_line).block(Block::default().borders(Borders::ALL));
            frame.render_widget(footer, chunks[3]);

            if state.context_view.adding_file {
                render_add_file_picker(frame, state);
            }
        }
        ViewMode::History => {
            let content_height = chunks[1].height.saturating_sub(2);
            let content_width = (chunks[1].width.saturating_sub(2) as usize).max(10);
            state.last_model_height = content_height;

            let history_lines = compute_history_lines(&state.history_view.items, Some(content_width));
            let total_lines = history_lines.len();
            state.history_view.rendered_lines_count = total_lines;

            let max_scroll = (total_lines as u16).saturating_sub(content_height);
            if state.history_view.auto_scroll || state.history_view.scroll > max_scroll {
                state.history_view.scroll = max_scroll;
            }

            let (status_text, status_style) = if state.history_view.loading {
                ("[Loading...]", Style::default().bold().fg(Color::Yellow))
            } else {
                (
                    Box::leak(
                        format!(
                            "[Showing {} of {}]",
                            state.history_view.items.len(),
                            state.history_view.total_count
                        )
                        .into_boxed_str(),
                    ) as &str,
                    Style::default().fg(Color::DarkGray),
                )
            };

            let scroll_indicator = if max_scroll > 0 {
                format!(" [{}/{}]", state.history_view.scroll + 1, total_lines)
            } else {
                String::new()
            };

            let title_line = Line::from(vec![
                Span::raw(" Session Semantic History: "),
                Span::styled(status_text, status_style),
                Span::styled(scroll_indicator, Style::default().fg(Color::DarkGray)),
                Span::raw(" "),
            ]);

            let history_paragraph = Paragraph::new(history_lines)
                .block(Block::default().title(title_line).borders(Borders::ALL))
                .scroll((state.history_view.scroll, 0));
            frame.render_widget(history_paragraph, chunks[1]);

            let info_line = Line::from(vec![
                Span::styled(" Navigation: ", Style::default().fg(Color::Cyan).bold()),
                Span::raw("Scroll up with "),
                Span::styled("↑ / k / PgUp", Style::default().bold().fg(Color::White)),
                Span::raw(" to dynamically fetch older turns. "),
                Span::styled(
                    if state.history_view.auto_scroll {
                        "[Auto-scroll: ON]"
                    } else {
                        "[Auto-scroll: OFF]"
                    },
                    if state.history_view.auto_scroll {
                        Style::default().fg(Color::Green).bold()
                    } else {
                        Style::default().fg(Color::DarkGray)
                    },
                ),
            ]);
            let prompt_widget = Paragraph::new(info_line).block(
                Block::default()
                    .title(" History Info ")
                    .borders(Borders::ALL),
            );
            frame.render_widget(prompt_widget, chunks[2]);

            let footer_line = Line::from(vec![
                Span::styled(
                    " Ctrl+1/2/3 ",
                    Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
                ),
                Span::raw(" Switch  "),
                Span::styled(
                    " ↑/↓ (k/j) ",
                    Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
                ),
                Span::raw(" Scroll  "),
                Span::styled(
                    " PgUp/PgDn ",
                    Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
                ),
                Span::raw(" Page  "),
                Span::styled(
                    " Home/End ",
                    Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
                ),
                Span::raw(" Top/Bottom  "),
                Span::styled(
                    " Ctrl+L ",
                    Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
                ),
                Span::raw(" Clear  "),
                Span::styled(
                    " Esc / q ",
                    Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
                ),
                Span::raw(" Back  "),
                Span::styled(
                    " ? ",
                    Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
                ),
                Span::raw(" Help"),
            ]);
            let footer = Paragraph::new(footer_line).block(Block::default().borders(Borders::ALL));
            frame.render_widget(footer, chunks[3]);
        }
    }

    if let Some(dialog) = &state.squash_dialog {
        render_squash_popup(frame, dialog);
    } else if state.confirm_cancel {
        render_confirm_cancel_popup(frame, state);
    } else if state.confirm_undo {
        render_confirm_undo_popup(frame, state);
    } else if state.confirm_clear_history {
        render_confirm_clear_history_popup(frame, state);
    } else if let Some(dialog) = &state.selection_dialog {
        render_selection_dialog(frame, dialog);
    } else if state.show_help {
        render_help_popup(frame, state.view_mode);
    }
}

pub fn render_squash_popup(frame: &mut ratatui::Frame, dialog: &crate::app::SquashDialogState) {
    let area = centered_rect(75, 75, frame.area());
    frame.render_widget(Clear, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Min(4),
            Constraint::Length(8),
            Constraint::Length(3),
        ])
        .split(area);

    let mut header_lines = Vec::new();
    header_lines.push(Line::from(vec![
        Span::styled(" Upstream Base: ", Style::default().bold().fg(Color::Cyan)),
        Span::styled(
            if dialog.base_ref.is_empty() { "Auto-detecting...".to_string() } else { dialog.base_ref.clone() },
            Style::default().bold().fg(Color::Yellow),
        ),
        Span::raw(" | "),
        Span::styled(format!("Commits to squash: {}", dialog.commits.len()), Style::default().bold()),
        Span::raw(" | "),
        Span::styled(dialog.diff_stat.clone(), Style::default().fg(Color::DarkGray)),
    ]));
    if let Some(status) = &dialog.status_message {
        header_lines.push(Line::from(Span::styled(status, Style::default().fg(Color::Yellow))));
    } else {
        header_lines.push(Line::from(Span::styled(
            "Soft-reset to base ref: keeps all cumulative changes staged and commits cleanly.",
            Style::default().fg(Color::DarkGray),
        )));
    }

    let header_widget = Paragraph::new(header_lines).block(
        Block::default()
            .title(" Squash Commits into Single Feature Commit ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan)),
    );
    frame.render_widget(header_widget, chunks[0]);

    let mut commit_lines = Vec::new();
    if dialog.commits.is_empty() {
        if dialog.loading {
            commit_lines.push(Line::from(Span::styled(
                "  Inspecting commits ahead of upstream...",
                Style::default().fg(Color::DarkGray),
            )));
        } else {
            commit_lines.push(Line::from(Span::styled(
                "  No commits ahead of upstream base branch.",
                Style::default().fg(Color::Red),
            )));
        }
    } else {
        for c in &dialog.commits {
            commit_lines.push(Line::from(vec![
                Span::styled(format!("  • {} ", c.hash), Style::default().fg(Color::Cyan).bold()),
                Span::styled(&c.subject, Style::default().fg(Color::White)),
                Span::styled(format!(" ({}, {})", c.author, c.date), Style::default().fg(Color::DarkGray)),
            ]));
        }
    }
    let commits_widget = Paragraph::new(commit_lines)
        .block(Block::default().title(" Commits Ahead ").borders(Borders::ALL))
        .wrap(Wrap { trim: false });
    frame.render_widget(commits_widget, chunks[1]);

    let mut msg_lines = Vec::new();
    if dialog.loading && dialog.message_buffer.is_empty() {
        msg_lines.push(Line::from(Span::styled(
            "Generating Conventional Commit message via LLM (incorporating pinned project guidelines)...",
            Style::default().fg(Color::Yellow),
        )));
    } else {
        for line in dialog.message_buffer.lines() {
            msg_lines.push(Line::from(Span::raw(line)));
        }
        if dialog.message_buffer.is_empty() {
            msg_lines.push(Line::from(vec![
                Span::styled("█", Style::default().fg(Color::Yellow)),
                Span::styled(" (type commit message or edit generated proposal)", Style::default().fg(Color::DarkGray)),
            ]));
        }
    }
    let msg_widget = Paragraph::new(msg_lines).block(
        Block::default()
            .title(" Commit Message (Conventional Commit) ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Green)),
    );
    frame.render_widget(msg_widget, chunks[2]);

    let footer_line = Line::from(vec![
        Span::styled(
            " Enter ",
            Style::default().bg(Color::Green).fg(Color::Black).bold(),
        ),
        Span::raw(" Apply Squash    "),
        Span::styled(
            " Esc ",
            Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
        ),
        Span::raw(" Cancel"),
    ]);
    let footer_widget = Paragraph::new(footer_line)
        .block(Block::default().borders(Borders::ALL))
        .alignment(ratatui::layout::Alignment::Center);
    frame.render_widget(footer_widget, chunks[3]);
}

pub fn render_onboarding_view(
    frame: &mut ratatui::Frame,
    state: &AppState,
    main_area: Rect,
    info_area: Rect,
    footer_area: Rect,
) {
    let ob = &state.onboarding;

    let step_badge = match ob.step {
        crate::app::OnboardingStep::Config => Line::from(vec![
            Span::styled(" [1. Конфигурация] ", Style::default().bg(Color::Cyan).fg(Color::Black).bold()),
            Span::styled(" ─── 2. API-ключ ─── 3. Запуск", Style::default().fg(Color::DarkGray)),
        ]),
        crate::app::OnboardingStep::Credentials => Line::from(vec![
            Span::styled(" 1. Конфигурация ─── ", Style::default().fg(Color::Green)),
            Span::styled("[2. API-ключ]", Style::default().bg(Color::Yellow).fg(Color::Black).bold()),
            Span::styled(" ─── 3. Запуск", Style::default().fg(Color::DarkGray)),
        ]),
        crate::app::OnboardingStep::Gatekeeper => Line::from(vec![
            Span::styled(" 1. Конфигурация ─── 2. API-ключ ─── ", Style::default().fg(Color::DarkGray)),
            Span::styled("[Внимание: Ключ не настроен]", Style::default().bg(Color::Red).fg(Color::White).bold()),
        ]),
        crate::app::OnboardingStep::Ready => Line::from(vec![
            Span::styled(" 1. Конфигурация ─── 2. API-ключ ─── ", Style::default().fg(Color::Green)),
            Span::styled("[3. Готово к работе]", Style::default().bg(Color::Green).fg(Color::Black).bold()),
        ]),
    };

    let mut lines = Vec::new();
    lines.push(Line::raw(""));
    lines.push(step_badge);
    lines.push(Line::raw(""));

    match ob.step {
        crate::app::OnboardingStep::Config => {
            lines.push(Line::from(Span::styled(
                "Шаг 1: Конфигурация проекта (tauqe.toml)",
                Style::default().bold().fg(Color::Cyan),
            )));
            lines.push(Line::from("Файл конфигурации не найден в корне проекта."));
            lines.push(Line::from(format!(
                "Он будет создан по пути: {}",
                ob.default_config_path
            )));
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                "Выберите модель по умолчанию для проекта (стрелки ↑/↓, затем Enter):",
                Style::default().bold().fg(Color::White),
            )));
            lines.push(Line::raw(""));

            let options = [
                format!("Создать tauqe.toml с моделью {} (Рекомендуется)", ob.models_list[0]),
                format!("Создать tauqe.toml с моделью {}", ob.models_list[1]),
                format!("Создать tauqe.toml с моделью {}", ob.models_list[2]),
                format!("Создать tauqe.toml с моделью {}", ob.models_list[3]),
                "Ввести идентификатор модели вручную...".to_string(),
                "Пропустить (использовать настройки по умолчанию без создания файла)".to_string(),
            ];

            for (idx, opt) in options.iter().enumerate() {
                let is_sel = idx == ob.selected_index;
                let prefix = if is_sel { "  ▶ [●] " } else { "    [ ] " };
                let style = if is_sel {
                    Style::default().bg(Color::DarkGray).fg(Color::White).bold()
                } else {
                    Style::default().fg(Color::Gray)
                };
                lines.push(Line::from(vec![
                    Span::styled(prefix, Style::default().fg(Color::Cyan).bold()),
                    Span::styled(opt.clone(), style),
                ]));
            }
        }
        crate::app::OnboardingStep::Credentials => {
            lines.push(Line::from(Span::styled(
                "Шаг 2: Учётные данные и ключ OpenRouter",
                Style::default().bold().fg(Color::Yellow),
            )));
            lines.push(Line::from("API-ключ OpenRouter не обнаружен ни в окружении (OPENROUTER_API_KEY), ни в файлах credentials."));
            lines.push(Line::from("Ключ необходим для доступа к моделям (получить ключ можно на https://openrouter.ai/keys)."));
            lines.push(Line::from(format!(
                "Рекомендуемое безопасное место: {}",
                ob.default_credentials_path
            )));
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                "Выберите способ настройки ключа (стрелки ↑/↓, затем Enter):",
                Style::default().bold().fg(Color::White),
            )));
            lines.push(Line::raw(""));

            let options = [
                format!("Ввести API-ключ сейчас (сохранить с правами 0600 в {})", ob.default_credentials_path),
                "Создать файл-заглушку credentials.toml (заполнить вручную в редакторе)".to_string(),
                "Я уже задал / задам через переменную OPENROUTER_API_KEY (Проверить снова)".to_string(),
                "Пропустить".to_string(),
            ];

            for (idx, opt) in options.iter().enumerate() {
                let is_sel = idx == ob.selected_index;
                let prefix = if is_sel { "  ▶ [●] " } else { "    [ ] " };
                let style = if is_sel {
                    Style::default().bg(Color::DarkGray).fg(Color::White).bold()
                } else {
                    Style::default().fg(Color::Gray)
                };
                lines.push(Line::from(vec![
                    Span::styled(prefix, Style::default().fg(Color::Yellow).bold()),
                    Span::styled(opt.clone(), style),
                ]));
            }
        }
        crate::app::OnboardingStep::Gatekeeper => {
            lines.push(Line::from(Span::styled(
                "Внимание: Система не готова к работе",
                Style::default().bold().fg(Color::Red),
            )));
            lines.push(Line::from("Без ключа OpenRouter вызовы языковой модели невозможны."));
            lines.push(Line::from("Вы можете ввести ключ прямо сейчас, либо создать файл credentials.toml, либо экспортировать:"));
            lines.push(Line::from(Span::styled("  export OPENROUTER_API_KEY=\"sk-or-v1-...\"", Style::default().fg(Color::Cyan).bold())));
            lines.push(Line::raw(""));

            let options = [
                "Вернуться назад и настроить ключ OpenRouter".to_string(),
                "Проверить снова (перечитать окружение и файлы)".to_string(),
                "Выйти из Tauqe".to_string(),
            ];

            for (idx, opt) in options.iter().enumerate() {
                let is_sel = idx == ob.selected_index;
                let prefix = if is_sel { "  ▶ [●] " } else { "    [ ] " };
                let style = if is_sel {
                    Style::default().bg(Color::DarkGray).fg(Color::White).bold()
                } else {
                    Style::default().fg(Color::Gray)
                };
                lines.push(Line::from(vec![
                    Span::styled(prefix, Style::default().fg(Color::Red).bold()),
                    Span::styled(opt.clone(), style),
                ]));
            }
        }
        crate::app::OnboardingStep::Ready => {
            lines.push(Line::from(Span::styled(
                "Система успешно настроена и готова к работе!",
                Style::default().bold().fg(Color::Green),
            )));
            lines.push(Line::raw(""));
            lines.push(Line::from(vec![
                Span::styled("• Модель: ", Style::default().bold()),
                Span::styled(&state.active_model, Style::default().fg(Color::Cyan)),
            ]));
            lines.push(Line::from(vec![
                Span::styled("• Конфигурация: ", Style::default().bold()),
                Span::styled(
                    ob.config_path.as_deref().unwrap_or("Встроенные умолчания"),
                    Style::default().fg(Color::White),
                ),
            ]));
            lines.push(Line::from(vec![
                Span::styled("• Ключ OpenRouter: ", Style::default().bold()),
                Span::styled("[OK] Настроен", Style::default().fg(Color::Green).bold()),
            ]));
            lines.push(Line::raw(""));
            lines.push(Line::from("Нажмите Enter, чтобы перейти в рабочий интерфейс сессии."));
            lines.push(Line::from(Span::styled(
                "Справка по всем горячим клавишам доступна в любой момент по нажатию '?'.",
                Style::default().fg(Color::DarkGray),
            )));
            lines.push(Line::raw(""));

            let prefix = "  ▶ [●] ";
            let style = Style::default().bg(Color::Green).fg(Color::Black).bold();
            lines.push(Line::from(vec![
                Span::styled(prefix, Style::default().fg(Color::Green).bold()),
                Span::styled(" Начать работу (Перейти в терминал) ", style),
            ]));
        }
    }

    if ob.input_active {
        lines.push(Line::raw(""));
        let (prompt, masked) = match ob.step {
            crate::app::OnboardingStep::Config => ("Введите модель: ", false),
            _ => ("Введите OpenRouter API Key: ", true),
        };

        let displayed = if masked {
            "•".repeat(ob.input_buffer.len())
        } else {
            ob.input_buffer.clone()
        };

        lines.push(Line::from(vec![
            Span::styled(prompt, Style::default().fg(Color::Yellow).bold()),
            Span::styled(displayed, Style::default().fg(Color::White).bold()),
            Span::styled("█", Style::default().fg(Color::Yellow)),
            Span::styled("  (Enter: Сохранить, Esc: Отмена)", Style::default().fg(Color::DarkGray)),
        ]));
    }

    if let Some(err) = &ob.error_message {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            format!("Ошибка: {}", err),
            Style::default().fg(Color::Red).bold(),
        )));
    } else if let Some(msg) = &ob.status_message {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            format!("✓ {}", msg),
            Style::default().fg(Color::Green).bold(),
        )));
    }

    let main_block = Paragraph::new(lines)
        .block(
            Block::default()
                .title(" Первичная настройка Tauqe (Onboarding) ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(main_block, main_area);

    let info_text = match ob.step {
        crate::app::OnboardingStep::Config => "Используйте клавиши ↑/↓ для выбора модели и Enter для подтверждения.",
        crate::app::OnboardingStep::Credentials => "Безопасное хранение ключей: файл создаётся с правами доступа 0600 (только чтение владельцем).",
        crate::app::OnboardingStep::Gatekeeper => "Для работы LLM необходим API-ключ OpenRouter. Выберите действие.",
        crate::app::OnboardingStep::Ready => "Все параметры проверены. Нажмите Enter для старта.",
    };
    let info_block = Paragraph::new(Line::from(Span::styled(info_text, Style::default().fg(Color::DarkGray))))
        .block(Block::default().title(" Информация ").borders(Borders::ALL));
    frame.render_widget(info_block, info_area);

    let footer_line = Line::from(vec![
        Span::styled(
            " ↑ / ↓ (k / j) ",
            Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
        ),
        Span::raw(" Выбор пункта  "),
        Span::styled(
            " Enter ",
            Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
        ),
        Span::raw(" Подтвердить  "),
        Span::styled(
            " Esc / q ",
            Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
        ),
        Span::raw(" Назад / Выход  "),
        Span::styled(
            " Ctrl+O ",
            Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
        ),
        Span::raw(" Перечитать конфиг"),
    ]);
    let footer = Paragraph::new(footer_line).block(Block::default().borders(Borders::ALL));
    frame.render_widget(footer, footer_area);
}

pub fn render_confirm_cancel_popup(frame: &mut ratatui::Frame, _state: &AppState) {
    let area = centered_rect(58, 28, frame.area());
    frame.render_widget(Clear, area);

    let lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            "Прервать текущую операцию модели?",
            Style::default().bold().fg(Color::Yellow),
        )),
        Line::raw(""),
        Line::from("Генерация ответа будет остановлена."),
        Line::from(Span::styled(
            "Все незавершенные изменения и диффы будут отменены.",
            Style::default().fg(Color::DarkGray),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled(
                " [Y] / Enter ",
                Style::default().bg(Color::Red).fg(Color::White).bold(),
            ),
            Span::raw(" Прервать    "),
            Span::styled(
                " [N] / Esc ",
                Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
            ),
            Span::raw(" Продолжить"),
        ]),
    ];

    let block = Paragraph::new(lines)
        .block(
            Block::default()
                .title(" Подтверждение прерывания ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow)),
        )
        .alignment(ratatui::layout::Alignment::Center);

    frame.render_widget(block, area);
}

pub fn render_confirm_undo_popup(frame: &mut ratatui::Frame, state: &AppState) {
    let area = centered_rect(55, 30, frame.area());
    frame.render_widget(Clear, area);

    let mut lines = Vec::new();
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "Are you sure you want to undo the last AI commit?",
        Style::default().bold().fg(Color::Yellow),
    )));
    lines.push(Line::raw(""));

    if let Some(hash) = &state.model.last_commit_hash {
        lines.push(Line::from(vec![
            Span::raw("Commit: "),
            Span::styled(hash.clone(), Style::default().bold().fg(Color::Cyan)),
        ]));
    }
    if let Some(summary) = &state.model.last_commit_summary {
        lines.push(Line::from(vec![
            Span::raw("Summary: "),
            Span::styled(summary.clone(), Style::default().fg(Color::White)),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "Any uncommitted changes from the pre-edit checkpoint will be restored.",
        Style::default().fg(Color::DarkGray),
    )));
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled(
            " [Y] / Enter ",
            Style::default().bg(Color::Red).fg(Color::White).bold(),
        ),
        Span::raw(" Confirm Undo    "),
        Span::styled(
            " [N] / Esc ",
            Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
        ),
        Span::raw(" Cancel"),
    ]));

    let block = Paragraph::new(lines)
        .block(
            Block::default()
                .title(" Confirm Undo ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Red)),
        )
        .alignment(ratatui::layout::Alignment::Center);

    frame.render_widget(block, area);
}

pub fn render_confirm_clear_history_popup(frame: &mut ratatui::Frame, _state: &AppState) {
    let area = centered_rect(58, 30, frame.area());
    frame.render_widget(Clear, area);

    let lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            "Are you sure you want to clear the conversation history?",
            Style::default().bold().fg(Color::Yellow),
        )),
        Line::raw(""),
        Line::from("This will permanently erase the session history:"),
        Line::from(Span::styled(
            "  .tauqe/history.jsonl",
            Style::default().fg(Color::DarkGray).italic(),
        )),
        Line::from(Span::styled(
            "and clear the current model view output.",
            Style::default().fg(Color::DarkGray),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled(
                " [Y] / Enter ",
                Style::default().bg(Color::Red).fg(Color::White).bold(),
            ),
            Span::raw(" Confirm Clear    "),
            Span::styled(
                " [N] / Esc ",
                Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
            ),
            Span::raw(" Cancel"),
        ]),
    ];

    let block = Paragraph::new(lines)
        .block(
            Block::default()
                .title(" Confirm Clear History ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Red)),
        )
        .alignment(ratatui::layout::Alignment::Center);

    frame.render_widget(block, area);
}

pub fn render_selection_dialog(
    frame: &mut ratatui::Frame,
    dialog: &crate::app::SelectionDialogState,
) {
    let item_count = dialog.items.len();
    let height = (item_count as u16 + 4).clamp(5, 16);
    let area = centered_rect(
        50,
        (height * 100 / frame.area().height).clamp(15, 60),
        frame.area(),
    );
    frame.render_widget(Clear, area);

    let (title, title_color) = match dialog.kind {
        crate::app::SelectionDialogKind::Workflow => (" Select Workflow ", Color::Cyan),
        crate::app::SelectionDialogKind::EditProtocol => (" Select Edit Protocol ", Color::Magenta),
        crate::app::SelectionDialogKind::Model => (" Select Model ", Color::Green),
    };

    let mut lines = Vec::new();
    lines.push(Line::raw(""));

    for (idx, item) in dialog.items.iter().enumerate() {
        let is_selected = idx == dialog.selected_index;
        let prefix = if is_selected { " ▶ " } else { "   " };
        let line_style = if is_selected {
            Style::default().bg(Color::DarkGray).fg(Color::White).bold()
        } else {
            Style::default().fg(Color::Gray)
        };

        lines.push(
            Line::from(vec![
                Span::styled(prefix, Style::default().fg(title_color).bold()),
                Span::styled(item.clone(), line_style),
            ])
            .style(line_style),
        );
    }
    lines.push(Line::raw(""));

    let block = Paragraph::new(lines).block(
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(title_color)),
    );

    frame.render_widget(block, area);
}

pub fn render_add_file_picker(frame: &mut ratatui::Frame, state: &AppState) {
    let area = centered_rect(70, 50, frame.area());
    frame.render_widget(Clear, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(3)])
        .split(area);

    let input_line = Line::from(vec![
        Span::styled(" File / Pattern: ", Style::default().fg(Color::Cyan).bold()),
        Span::raw(&state.context_view.add_input),
        Span::styled("█", Style::default().fg(Color::Yellow)),
    ]);
    let (access_title, border_color) = match state.context_view.add_access {
        ContextAccess::Editable => (
            " Add EDITABLE to Context (Path, directory or glob like *.rs) ",
            Color::Yellow,
        ),
        ContextAccess::ReadOnly => (
            " Add READ-ONLY to Context (Path, directory or glob like *.rs) ",
            Color::Cyan,
        ),
    };
    let input_block = Paragraph::new(input_line).block(
        Block::default()
            .title(access_title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color)),
    );
    frame.render_widget(input_block, chunks[0]);

    let mut candidate_lines = Vec::new();
    if state.context_view.filtered_candidates.is_empty() {
        candidate_lines.push(Line::from(Span::styled(
            "  No matching files found.",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for (idx, candidate) in state.context_view.filtered_candidates.iter().enumerate() {
            let is_sel = idx == state.context_view.selected_candidate_index;
            let (prefix, style) = if is_sel {
                (
                    " ▶ ",
                    Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
                )
            } else {
                ("   ", Style::default().fg(Color::Gray))
            };

            let is_pattern_entry = candidate.starts_with("[+] Add all matching '");

            let line = if is_pattern_entry {
                Line::from(vec![
                    Span::styled(prefix, Style::default().fg(Color::Green).bold()),
                    Span::styled(
                        candidate,
                        if is_sel {
                            Style::default().bg(Color::Green).fg(Color::Black).bold()
                        } else {
                            Style::default().fg(Color::Green).bold()
                        },
                    ),
                ])
            } else {
                Line::from(vec![
                    Span::styled(prefix, Style::default().fg(Color::Cyan)).bold(),
                    Span::styled(candidate, style),
                ])
            };

            candidate_lines.push(line);
        }
    }

    let list_title = match state.context_view.add_access {
        ContextAccess::Editable => " Matching Files / Actions (Enter: Add EDITABLE, Tab: Complete, ↑/↓: Navigate, Esc: Cancel) ",
        ContextAccess::ReadOnly => " Matching Files / Actions (Enter: Add READ-ONLY, Tab: Complete, ↑/↓: Navigate, Esc: Cancel) ",
    };
    let list_block = Paragraph::new(candidate_lines).block(
        Block::default()
            .title(list_title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    frame.render_widget(list_block, chunks[1]);
}

pub fn render_help_popup(frame: &mut ratatui::Frame, mode: ViewMode) {
    let area = centered_rect(65, 55, frame.area());
    frame.render_widget(Clear, area);

    let (title, help_lines) = match mode {
        ViewMode::Model => (
            " Help: Model View ",
            vec![
                Line::from(Span::styled(
                    "Global Navigation",
                    Style::default().fg(Color::Cyan).bold(),
                )),
                Line::from("  Ctrl+1        Switch directly to Model view"),
                Line::from("  Ctrl+2        Switch directly to Context view (preserves input)"),
                Line::from("  Ctrl+3        Switch directly to History view"),
                Line::from("  Tab           Switch between views (if input empty)"),
                Line::from("  Esc / q       Close help / Cancel operation / Exit"),
                Line::from("  ?             Toggle this help popup"),
                Line::raw(""),
                Line::from(Span::styled(
                    "Workflow & Settings",
                    Style::default().fg(Color::Green).bold(),
                )),
                Line::from("  Ctrl+W        Cycle workflow (disabled while generating)"),
                Line::from("  Ctrl+P        Cycle edit protocol (disabled while generating)"),
                Line::from("  Ctrl+M        Cycle active model (disabled while generating)"),
                Line::raw(""),
                Line::from(Span::styled(
                    "Editor & Prompt",
                    Style::default().fg(Color::Yellow).bold(),
                )),
                Line::from("  Enter         Send prompt (disabled during generation; Esc to cancel)"),
                Line::from("  Shift+Enter   Insert newline in prompt (also Ctrl+J)"),
                Line::from("  ← / →         Move cursor left / right"),
                Line::from("  ↑ / ↓         Move cursor up / down across lines in prompt"),
                Line::from("  Ctrl+A / E    Move cursor to line start / end"),
                Line::from("  Alt+B / F     Move cursor word backward / forward"),
                Line::from("  Ctrl+K / U    Kill to line end / beginning"),
                Line::from("  Alt+D / Alt+Bksp Kill word forward / backward"),
                Line::from("  Ctrl+Y        Yank (paste) killed text"),
                Line::from("  Tab           Insert 2 spaces in prompt"),
                Line::from("  Esc           Clear prompt / Cancel streaming / Exit"),
                Line::raw(""),
                Line::from(Span::styled(
                    "Model & File Review",
                    Style::default().fg(Color::Yellow).bold(),
                )),
                Line::from("  Space / Enter Fold / Unfold selected file diff (when input empty)"),
                Line::from("  [ / ]         Navigate modified files (when input empty)"),
                Line::from("  u             Undo last AI commit (asks confirmation)"),
                Line::from("  Ctrl+C        Cancel active streaming / thinking"),
                Line::from("  Ctrl+L        Clear conversation history & model view"),
                Line::from("  Ctrl+R        Toggle reasoning / thinking visibility"),
                Line::from("  PgUp / PgDn   Scroll output by page"),
            ],
        ),
        ViewMode::Context => (
            " Help: Context View ",
            vec![
                Line::from(Span::styled(
                    "Global Navigation",
                    Style::default().fg(Color::Cyan).bold(),
                )),
                Line::from("  Ctrl+1        Switch directly to Model view"),
                Line::from("  Ctrl+2        Switch directly to Context view"),
                Line::from("  Ctrl+3        Switch directly to History view"),
                Line::from("  Tab           Cycle views (Model -> Context -> History)"),
                Line::from("  Esc / q       Back to Model view / Close help"),
                Line::from("  ?             Toggle this help popup"),
                Line::raw(""),
                Line::from(Span::styled(
                    "Three-Tier Context Model (Pinned, User, Auto)",
                    Style::default().fg(Color::Yellow).bold(),
                )),
                Line::from("  ↑/↓ or k/j    Navigate through sections and context files"),
                Line::from("  Space / Enter Fold / Unfold active section header"),
                Line::from("  e             Add file(s) to User layer as EDITABLE"),
                Line::from("  r             Add file(s) to User layer as READ-ONLY"),
                Line::from("  t             Toggle active file (Editable ↔ Read-only)"),
                Line::from("  p / u / Enter Promote selected Auto file into User layer"),
                Line::from("  c             Clear all Auto-requested files"),
                Line::from("  d / x / Del   Remove file (Pinned files are protected)"),
                Line::raw(""),
                Line::from(Span::styled(
                    "Add File / Pattern Picker",
                    Style::default().fg(Color::Cyan).bold(),
                )),
                Line::from("  Type pattern  Filter by substring or glob (e.g. *.rs, src/)"),
                Line::from("  ↑ / ↓         Select matching file or '[+] Add all' action"),
                Line::from("  Tab           Autocomplete path into input"),
                Line::from("  Enter         Add highlighted file or all matching files"),
                Line::from("  Esc           Cancel picker"),
            ],
        ),
        ViewMode::History => (
            " Help: History View ",
            vec![
                Line::from(Span::styled(
                    "Global Navigation",
                    Style::default().fg(Color::Cyan).bold(),
                )),
                Line::from("  Ctrl+1        Switch directly to Model view"),
                Line::from("  Ctrl+2        Switch directly to Context view"),
                Line::from("  Ctrl+3        Switch directly to History view"),
                Line::from("  Tab           Cycle views (Model -> Context -> History)"),
                Line::from("  Esc / q       Back to Model view / Close help"),
                Line::from("  ?             Toggle this help popup"),
                Line::raw(""),
                Line::from(Span::styled(
                    "History Navigation",
                    Style::default().fg(Color::Yellow).bold(),
                )),
                Line::from("  ↑ / k         Scroll up (fetches older entries at top)"),
                Line::from("  ↓ / j         Scroll down (resumes auto-scroll at bottom)"),
                Line::from("  PgUp / PgDn   Scroll by page"),
                Line::from("  Home / End    Jump to beginning / end"),
                Line::from("  Ctrl+L        Clear history and screen"),
            ],
        ),
        ViewMode::Onboarding => (
            " Help: Onboarding ",
            vec![
                Line::from(Span::styled(
                    "Навигация мастера настройки",
                    Style::default().fg(Color::Cyan).bold(),
                )),
                Line::from("  ↑ / ↓ (k / j) Выбор варианта"),
                Line::from("  Enter         Подтвердить выбор / продолжить"),
                Line::from("  Esc / q       Назад / Выход"),
                Line::from("  Ctrl+O        Перечитать конфигурацию с диска"),
                Line::from("  ?             Открыть/закрыть эту справку"),
            ],
        ),
    };

    let popup_block = Paragraph::new(help_lines)
        .block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow)),
        )
        .wrap(Wrap { trim: false });

    frame.render_widget(popup_block, area);
}

pub fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_current_round() {
        assert_eq!(extract_current_round("Hello world"), None);
        assert_eq!(
            extract_current_round("Some text\n\n---\n**[Round 2]** Added 1 file\n\nMore text"),
            Some("Round 2".to_string())
        );
        assert_eq!(
            extract_current_round("Text [Round 2] more text [Round 3] now"),
            Some("Round 3".to_string())
        );
        assert_eq!(
            extract_current_round("Text [Round 2] [Patch Retry 1/3] fixing"),
            Some("Patch Retry 1/3".to_string())
        );
    }

    #[test]
    fn test_compute_history_lines_assistant_modified_files() {
        let item = UiHistoryItem {
            id: 2,
            kind: UiHistoryKind::Assistant,
            text: "Updated codebase".to_string(),
            summary: Some("Refactoring".to_string()),
            commit_hash: Some("abcdef1".to_string()),
            files: vec![
                "[EDIT] src/main.rs".to_string(),
                "[NEW] src/util.rs".to_string(),
                "[DEL] src/old.rs".to_string(),
                "[MOVE] src/prev.rs -> src/next.rs".to_string(),
            ],
        };
        let lines = compute_history_lines(&[item], None);
        let rendered: Vec<String> = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .collect();

        assert!(rendered.iter().any(|line| line.contains("[COMMIT: abcdef1]")));
        assert!(rendered.iter().any(|line| line.contains("Refactoring")));
        assert!(rendered.iter().any(|line| line.contains("Updated codebase")));
        assert!(rendered.iter().any(|line| line.contains("Modified files (4):")));
        assert!(rendered.iter().any(|line| line.contains("[EDIT] ") && line.contains("src/main.rs")));
        assert!(rendered.iter().any(|line| line.contains("[NEW] ") && line.contains("src/util.rs")));
        assert!(rendered.iter().any(|line| line.contains("[DEL] ") && line.contains("src/old.rs")));
        assert!(rendered.iter().any(|line| line.contains("[MOVE] ") && line.contains("src/prev.rs -> src/next.rs")));
    }

    #[test]
    fn test_compute_history_lines_wrapping() {
        let long_text = "This is a very long line of text that should definitely be wrapped across multiple lines when rendered in the history view widget with a constrained width constraint.".to_string();
        let item = UiHistoryItem {
            id: 1,
            kind: UiHistoryKind::User,
            text: long_text,
            summary: None,
            commit_hash: None,
            files: Vec::new(),
        };

        let unwrapped = compute_history_lines(std::slice::from_ref(&item), None);
        let wrapped = compute_history_lines(std::slice::from_ref(&item), Some(40));

        assert!(wrapped.len() > unwrapped.len());
        for line in &wrapped {
            assert!(line.width() <= 40, "Line width {} exceeds max_width 40", line.width());
        }
    }
}
