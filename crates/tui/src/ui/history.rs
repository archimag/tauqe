use ratatui::layout::Rect;
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding, Paragraph};
use tauqe_protocol::{UiHistoryItem, UiHistoryKind};

use crate::app::AppState;
use crate::ui::wrap_lines;

#[derive(Debug, Clone)]
pub struct HistoryViewState {
    pub items: Vec<UiHistoryItem>,
    pub scroll: u16,
    pub auto_scroll: bool,
    pub has_more: bool,
    pub total_count: usize,
    pub loading: bool,
    pub rendered_lines_count: usize,
    pub pending_before_id: Option<u64>,
}

impl Default for HistoryViewState {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            scroll: 0,
            auto_scroll: true,
            has_more: false,
            total_count: 0,
            loading: false,
            rendered_lines_count: 0,
            pending_before_id: None,
        }
    }
}

pub fn compute_history_items_line_count(items: &[UiHistoryItem], max_width: Option<usize>) -> usize {
    compute_history_lines(items, max_width).len()
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
                    } else if let Some(p) = file_entry.strip_prefix("[OVERWRITE] ") {
                        ("[OVERWRITE] ", Style::default().fg(Color::Yellow).bold(), p)
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

pub fn render_history_view(
    frame: &mut ratatui::Frame,
    state: &mut AppState,
    main_area: Rect,
    info_area: Rect,
) {
    let hist_block = Block::default().padding(Padding::horizontal(1));
    let inner = hist_block.inner(main_area);
    let content_height = inner.height;
    let content_width = (inner.width as usize).max(10);
    state.last_model_height = content_height;

    let history_lines = compute_history_lines(&state.history_view.items, Some(content_width));
    let total_lines = history_lines.len();
    state.history_view.rendered_lines_count = total_lines;

    let max_scroll = (total_lines as u16).saturating_sub(content_height);
    if state.history_view.auto_scroll || state.history_view.scroll > max_scroll {
        state.history_view.scroll = max_scroll;
    }

    let history_paragraph = Paragraph::new(history_lines)
        .block(hist_block)
        .scroll((state.history_view.scroll, 0));
    frame.render_widget(history_paragraph, main_area);

    let info_line = Line::from(vec![
        Span::styled(" Navigation: ", Style::default().fg(Color::Cyan).bold()),
        Span::raw("Scroll up with "),
        Span::styled("↑ / k / PgUp", Style::default().bold().fg(Color::White)),
        Span::raw(" to dynamically fetch older turns. "),
        Span::styled(
            format!("[Showing {} of {}] ", state.history_view.items.len(), state.history_view.total_count),
            Style::default().fg(Color::DarkGray),
        ),
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
    let prompt_widget = Paragraph::new(info_line)
        .block(Block::default().padding(Padding::horizontal(1)));
    frame.render_widget(prompt_widget, info_area);
}

#[cfg(test)]
mod tests {
    use super::*;

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
