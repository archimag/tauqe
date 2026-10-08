use std::path::Path;

use ratatui::layout::Rect;
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::{AppState, HeaderClickAreas, ViewMode};

/// Helper to truncate a string to a max visual display width, appending `…` if needed.
pub(crate) fn truncate_to_width(s: &str, max_width: usize) -> String {
    let span_w = Span::raw(s).width();
    if span_w <= max_width {
        return s.to_string();
    }
    if max_width <= 1 {
        return "…".to_string();
    }
    let target = max_width - 1;
    let mut truncated = String::new();
    let mut current_w = 0;
    for ch in s.chars() {
        let ch_w = Span::raw(ch.to_string()).width();
        if current_w + ch_w > target {
            break;
        }
        truncated.push(ch);
        current_w += ch_w;
    }
    truncated.push('…');
    truncated
}

/// Top header: project name -> tabs -> model selector & tools (Squash, Help).
#[allow(unused_assignments)]
pub fn render_header(frame: &mut ratatui::Frame, state: &mut AppState, area: Rect) {
    state.header_clicks = HeaderClickAreas::default();
    if area.height < 3 || area.width < 10 {
        return;
    }

    let header_row = area.y + 1;
    state.header_clicks.row = header_row;

    let raw_project_name = match &state.repo_state {
        Some(repo) => Path::new(&repo.root)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&repo.root)
            .to_string(),
        None => "no repository".to_string(),
    };

    let inner_header_width = area.width.saturating_sub(2) as usize;
    let mut current_col: u16 = area.x + 1;
    let max_col = area.x + area.width.saturating_sub(1);

    // If terminal is narrow, truncate project name to at most 18 cells
    let max_proj_w = if inner_header_width < 80 { 12 } else { 24 };
    let project_name = truncate_to_width(&raw_project_name, max_proj_w);

    let active_mode = state.view_mode;
    let active_style = Style::default().bg(Color::Blue).fg(Color::White).bold();
    let inactive_style = Style::default().bg(Color::DarkGray).fg(Color::White);
    let tab_style = |mode: ViewMode| {
        if active_mode == mode {
            active_style
        } else {
            inactive_style
        }
    };
    let develop_tab_style = tab_style(ViewMode::Develop);
    let context_tab_style = tab_style(ViewMode::Context);
    let review_tab_style = tab_style(ViewMode::Review);
    let plans_tab_style = tab_style(ViewMode::Plans);
    let history_tab_style = tab_style(ViewMode::History);

    let mut header_spans = Vec::new();

    // 1. Project name first
    let icon_span = Span::styled(" 📁 ", Style::default().fg(Color::Yellow));
    let icon_w = icon_span.width() as u16;
    header_spans.push(icon_span);
    current_col += icon_w;

    let proj_span = Span::styled(project_name, Style::default().fg(Color::Cyan).bold());
    let proj_w = proj_span.width() as u16;
    header_spans.push(proj_span);
    current_col += proj_w;

    let div1_span = Span::styled(" │ ", Style::default().fg(Color::DarkGray));
    let div1_w = div1_span.width() as u16;
    header_spans.push(div1_span);
    current_col += div1_w;

    // Local macro to append span and accurately track its clickable column range
    macro_rules! push_item {
        ($span:expr, $target:expr) => {{
            let span = $span;
            let w = span.width() as u16;
            let start = current_col;
            let end = start + w.saturating_sub(1);
            if end < max_col {
                $target = (start, end);
            }
            current_col += w;
            header_spans.push(span);
        }};
    }

    // 2. Tabs
    push_item!(
        Span::styled(" 1: Develop ", develop_tab_style),
        state.header_clicks.develop_tab
    );
    header_spans.push(Span::raw(" "));
    current_col += 1;

    let tab_ctx_text = format!(" 2: Context ({}) ", state.context.items.len());
    push_item!(
        Span::styled(tab_ctx_text, context_tab_style),
        state.header_clicks.context_tab
    );
    header_spans.push(Span::raw(" "));
    current_col += 1;

    push_item!(
        Span::styled(" 3: Review ", review_tab_style),
        state.header_clicks.review_tab
    );
    header_spans.push(Span::raw(" "));
    current_col += 1;

    let focused_count = state.plans_view.current_plan.as_ref().map(|p| p.stats().checked).unwrap_or(0);
    let tab_plans_text = if focused_count > 0 {
        format!(" 4: Plans ({}) ", focused_count)
    } else {
        " 4: Plans ".to_string()
    };
    push_item!(
        Span::styled(tab_plans_text, plans_tab_style),
        state.header_clicks.plans_tab
    );
    header_spans.push(Span::raw(" "));
    current_col += 1;

    push_item!(
        Span::styled(" 5: History ", history_tab_style),
        state.header_clicks.history_tab
    );

    if state.view_mode == ViewMode::Onboarding {
        let ob_span = Span::styled(
            " ! Setup ",
            Style::default().bg(Color::Yellow).fg(Color::Black).bold(),
        );
        header_spans.push(Span::raw(" "));
        current_col += 1;
        let ob_w = ob_span.width() as u16;
        header_spans.push(ob_span);
        current_col += ob_w;
    }

    // 3. Right side items: Model Selector & Tools (Squash) & Help action
    let raw_model_str = &state.active_model.name;
    let max_model_len = if inner_header_width < 90 { 14 } else { 28 };
    let truncated_model = truncate_to_width(raw_model_str, max_model_len);
    let model_text = format!(" 🧠 {} ", truncated_model);
    let model_span = Span::styled(model_text, Style::default().fg(Color::Green).bold());
    let model_len = model_span.width() as u16;

    let squash_span = Span::styled(
        " [F6 Squash] ",
        Style::default().bg(Color::Cyan).fg(Color::Black).bold(),
    );
    let squash_len = squash_span.width() as u16;

    let help_span = Span::styled(
        " [?] Help ",
        Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
    );
    let help_len = help_span.width() as u16;

    let right_width = model_len as usize + 3 + squash_len as usize + 3 + help_len as usize;
    let left_width = (current_col.saturating_sub(area.x + 1)) as usize;

    if left_width + right_width + 1 < inner_header_width {
        let padding = inner_header_width - left_width - right_width;
        header_spans.push(Span::raw(" ".repeat(padding)));
        current_col += padding as u16;
    } else {
        header_spans.push(Span::raw("  "));
        current_col += 2;
    }

    push_item!(model_span, state.header_clicks.model_select);

    let div2_span = Span::styled(" │ ", Style::default().fg(Color::DarkGray));
    let div2_w = div2_span.width() as u16;
    header_spans.push(div2_span);
    current_col += div2_w;

    push_item!(squash_span, state.header_clicks.squash_button);

    let div3_span = Span::styled(" │ ", Style::default().fg(Color::DarkGray));
    let div3_w = div3_span.width() as u16;
    header_spans.push(div3_span);
    current_col += div3_w;

    push_item!(help_span, state.header_clicks.help_button);

    let header_line = Line::from(header_spans);
    let header = Paragraph::new(header_line).block(Block::default().borders(Borders::ALL));
    frame.render_widget(header, area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_to_width_ascii() {
        assert_eq!(truncate_to_width("hello", 10), "hello");
        assert_eq!(truncate_to_width("hello world", 6), "hello…");
    }

    #[test]
    fn test_truncate_to_width_cyrillic_no_panic() {
        let cyrillic = "Исправление ошибки сжатия коммитов в Git репозитории";
        let truncated = truncate_to_width(cyrillic, 25);
        assert!(truncated.ends_with('…'));
        assert!(Span::raw(&truncated).width() <= 25);
    }
}
