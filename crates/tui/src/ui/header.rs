use std::path::Path;

use ratatui::layout::Rect;
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::{AppState, ViewMode};

/// Top header: project name -> tabs -> model selector & tools (Squash, Help).
pub fn render_header(frame: &mut ratatui::Frame, state: &mut AppState, area: Rect) {
    let project_name = match &state.repo_state {
        Some(repo) => Path::new(&repo.root)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&repo.root)
            .to_string(),
        None => "no repository".to_string(),
    };

    let inner_header_width = area.width.saturating_sub(2) as usize;
    let mut current_col: u16 = 1;

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
    let history_tab_style = tab_style(ViewMode::History);
    let review_tab_style = tab_style(ViewMode::Review);

    let mut header_spans = Vec::new();

    // 1. Project name first
    header_spans.push(Span::styled(" 📁 ", Style::default().fg(Color::Yellow)));
    header_spans.push(Span::styled(
        project_name.clone(),
        Style::default().fg(Color::Cyan).bold(),
    ));
    header_spans.push(Span::styled(" │ ", Style::default().fg(Color::DarkGray)));
    current_col += 3 + (project_name.chars().count() as u16) + 3;

    // 2. Tabs
    let tab_dev_text = " 1: Develop ";
    let dev_start = current_col;
    let dev_len = tab_dev_text.chars().count() as u16;
    header_spans.push(Span::styled(tab_dev_text, develop_tab_style));
    header_spans.push(Span::raw(" "));
    state.header_clicks.develop_tab = (dev_start, dev_start + dev_len.saturating_sub(1));
    current_col += dev_len + 1;

    let tab_ctx_text = format!(" 2: Context ({}) ", state.context.items.len());
    let ctx_start = current_col;
    let ctx_len = tab_ctx_text.chars().count() as u16;
    header_spans.push(Span::styled(&tab_ctx_text, context_tab_style));
    header_spans.push(Span::raw(" "));
    state.header_clicks.context_tab = (ctx_start, ctx_start + ctx_len.saturating_sub(1));
    current_col += ctx_len + 1;

    let tab_review_text = " 3: Review ";
    let review_start = current_col;
    let review_len = tab_review_text.chars().count() as u16;
    header_spans.push(Span::styled(tab_review_text, review_tab_style));
    header_spans.push(Span::raw(" "));
    state.header_clicks.review_tab = (review_start, review_start + review_len.saturating_sub(1));
    current_col += review_len + 1;

    let tab_hist_text = format!(" 4: History ({}) ", state.history_view.total_count);
    let hist_start = current_col;
    let hist_len = tab_hist_text.chars().count() as u16;
    header_spans.push(Span::styled(&tab_hist_text, history_tab_style));
    state.header_clicks.history_tab = (hist_start, hist_start + hist_len.saturating_sub(1));
    current_col += hist_len;

    if state.view_mode == ViewMode::Onboarding {
        let ob_text = " ! Setup ";
        header_spans.push(Span::raw(" "));
        header_spans.push(Span::styled(
            ob_text,
            Style::default().bg(Color::Yellow).fg(Color::Black).bold(),
        ));
        current_col += 1 + ob_text.chars().count() as u16;
    }

    // 3. Right side items: Model Selector & Tools (Squash) & Help action
    let model_text = format!(" 🧠 {} ", state.active_model);
    let squash_text = " [F6 Squash] ";
    let help_text = " [?] Help ";

    // Emoji 🧠 occupies 2 terminal cells
    let model_len = (model_text.chars().count() + 1) as u16;
    let right_width = model_len as usize + 3 + squash_text.chars().count() + 3 + help_text.chars().count();
    let left_width = current_col as usize - 1;

    if left_width + right_width + 1 < inner_header_width {
        let padding = inner_header_width - left_width - right_width;
        header_spans.push(Span::raw(" ".repeat(padding)));
        current_col += padding as u16;
    } else {
        header_spans.push(Span::raw("  "));
        current_col += 2;
    }

    let model_start = current_col;
    header_spans.push(Span::styled(
        model_text,
        Style::default().fg(Color::Green).bold(),
    ));
    state.header_clicks.model_select = (model_start, model_start + model_len.saturating_sub(1));
    current_col += model_len;

    header_spans.push(Span::styled(" │ ", Style::default().fg(Color::DarkGray)));
    current_col += 3;

    let squash_start = current_col;
    let squash_len = squash_text.chars().count() as u16;
    header_spans.push(Span::styled(
        squash_text,
        Style::default().bg(Color::Cyan).fg(Color::Black).bold(),
    ));
    state.header_clicks.squash_button = (squash_start, squash_start + squash_len.saturating_sub(1));
    current_col += squash_len;

    header_spans.push(Span::styled(" │ ", Style::default().fg(Color::DarkGray)));
    current_col += 3;

    let help_start = current_col;
    let help_len = help_text.chars().count() as u16;
    header_spans.push(Span::styled(
        help_text,
        Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
    ));
    state.header_clicks.help_button = (help_start, help_start + help_len.saturating_sub(1));

    let header_line = Line::from(header_spans);
    let header = Paragraph::new(header_line).block(Block::default().borders(Borders::ALL));
    frame.render_widget(header, area);
}
