use std::path::Path;

use ratatui::layout::Rect;
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::{tier_title, AppState, HeaderClickAreas, ViewMode};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeaderTier {
    Full,
    Medium,
    Compact,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClickTarget {
    DevelopTab,
    ContextTab,
    ReviewTab,
    PlansTab,
    HistoryTab,
    ModelSelect,
    SquashButton,
    HelpButton,
}

fn record_click(clicks: &mut HeaderClickAreas, target: ClickTarget, range: (u16, u16)) {
    match target {
        ClickTarget::DevelopTab => clicks.develop_tab = range,
        ClickTarget::ContextTab => clicks.context_tab = range,
        ClickTarget::ReviewTab => clicks.review_tab = range,
        ClickTarget::PlansTab => clicks.plans_tab = range,
        ClickTarget::HistoryTab => clicks.history_tab = range,
        ClickTarget::ModelSelect => clicks.model_select = range,
        ClickTarget::SquashButton => clicks.squash_button = range,
        ClickTarget::HelpButton => clicks.help_button = range,
    }
}

fn tab_label(
    tier: HeaderTier,
    mode: ViewMode,
    active_mode: ViewMode,
    ctx_count: usize,
    plans_count: usize,
) -> String {
    match tier {
        HeaderTier::Full => match mode {
            ViewMode::Develop => " 1: Develop ".to_string(),
            ViewMode::Context => format!(" 2: Context ({}) ", ctx_count),
            ViewMode::Review => " 3: Review ".to_string(),
            ViewMode::Plans => {
                if plans_count > 0 {
                    format!(" 4: Plans ({}) ", plans_count)
                } else {
                    " 4: Plans ".to_string()
                }
            }
            ViewMode::History => " 5: History ".to_string(),
            ViewMode::Onboarding => " ! Setup ".to_string(),
        },
        HeaderTier::Medium => match mode {
            ViewMode::Develop => " 1: Dev ".to_string(),
            ViewMode::Context => format!(" 2: Ctx ({}) ", ctx_count),
            ViewMode::Review => " 3: Rev ".to_string(),
            ViewMode::Plans => {
                if plans_count > 0 {
                    format!(" 4: Plan ({}) ", plans_count)
                } else {
                    " 4: Plan ".to_string()
                }
            }
            ViewMode::History => " 5: Hist ".to_string(),
            ViewMode::Onboarding => " ! Setup ".to_string(),
        },
        HeaderTier::Compact => {
            let is_active = mode == active_mode;
            match mode {
                ViewMode::Develop => {
                    if is_active {
                        " 1: Dev ".to_string()
                    } else {
                        " 1 ".to_string()
                    }
                }
                ViewMode::Context => {
                    if is_active {
                        format!(" 2: Ctx ({}) ", ctx_count)
                    } else {
                        " 2 ".to_string()
                    }
                }
                ViewMode::Review => {
                    if is_active {
                        " 3: Rev ".to_string()
                    } else {
                        " 3 ".to_string()
                    }
                }
                ViewMode::Plans => {
                    if is_active {
                        if plans_count > 0 {
                            format!(" 4: Plan ({}) ", plans_count)
                        } else {
                            " 4: Plan ".to_string()
                        }
                    } else {
                        " 4 ".to_string()
                    }
                }
                ViewMode::History => {
                    if is_active {
                        " 5: Hist ".to_string()
                    } else {
                        " 5 ".to_string()
                    }
                }
                ViewMode::Onboarding => " ! ".to_string(),
            }
        }
    }
}

/// Top header: project name -> tabs -> model selector & tools (Squash, Help).
pub fn render_header(frame: &mut ratatui::Frame, state: &mut AppState, area: Rect) {
    state.header_clicks = HeaderClickAreas::default();
    if area.height < 3 || area.width < 10 {
        return;
    }

    let header_row = area.y + 1;
    state.header_clicks.row = header_row;

    let inner_header_width = area.width.saturating_sub(2) as usize;
    let max_col = area.x + area.width.saturating_sub(1);

    // 1. Determine Right-Side configuration with guaranteed priority
    let (max_model_len, squash_text, help_text) = if inner_header_width >= 125 {
        (20, " [F6 Squash] ", " [?] Help ")
    } else if inner_header_width >= 95 {
        (12, " [F6 Squash] ", " [?] Help ")
    } else if inner_header_width >= 75 {
        (8, " [F6] ", " [?] Help ")
    } else {
        (6, " [F6] ", " [?] ")
    };

    let raw_model_str = &state.active_model.name;
    let model_label = match &state.model_choice.selection {
        tauqe_protocol::ModelSelection::Tier(tier) => tier_title(*tier).to_string(),
        tauqe_protocol::ModelSelection::Specific(_) => {
            truncate_to_width(raw_model_str, max_model_len)
        }
    };
    let model_text = format!(" 🧠 {} ", model_label);
    let model_span = Span::styled(model_text, Style::default().fg(Color::Green).bold());

    let squash_span = Span::styled(
        squash_text,
        Style::default().bg(Color::Cyan).fg(Color::Black).bold(),
    );

    let help_span = Span::styled(
        help_text,
        Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
    );

    let div_style = Style::default().fg(Color::DarkGray);
    let div_span = Span::styled(" │ ", div_style);

    let right_items: Vec<(Span, Option<ClickTarget>)> = vec![
        (model_span, Some(ClickTarget::ModelSelect)),
        (div_span.clone(), None),
        (squash_span, Some(ClickTarget::SquashButton)),
        (div_span, None),
        (help_span, Some(ClickTarget::HelpButton)),
    ];

    let right_width: usize = right_items.iter().map(|(s, _)| s.width()).sum();

    // 2. Budget for Left-Side (tabs & project)
    let avail_for_left = inner_header_width.saturating_sub(right_width + 1);

    let ctx_count = state.context.items.len();
    let plans_count = state.plans_view.plans.len();

    let tabs = [
        (ViewMode::Develop, ClickTarget::DevelopTab),
        (ViewMode::Context, ClickTarget::ContextTab),
        (ViewMode::Review, ClickTarget::ReviewTab),
        (ViewMode::Plans, ClickTarget::PlansTab),
        (ViewMode::History, ClickTarget::HistoryTab),
    ];

    let calc_tabs_width = |tier: HeaderTier| -> usize {
        let mut w: usize = tabs
            .iter()
            .map(|(mode, _)| tab_label(tier, *mode, state.view_mode, ctx_count, plans_count).len())
            .sum();
        w += tabs.len().saturating_sub(1);
        if state.view_mode == ViewMode::Onboarding {
            w += 1 + tab_label(tier, ViewMode::Onboarding, state.view_mode, ctx_count, plans_count).len();
        }
        w
    };

    let full_tabs_w = calc_tabs_width(HeaderTier::Full);
    let med_tabs_w = calc_tabs_width(HeaderTier::Medium);

    let (tier, tabs_w) = if avail_for_left >= full_tabs_w + 14 {
        (HeaderTier::Full, full_tabs_w)
    } else if avail_for_left >= med_tabs_w + 10 {
        (HeaderTier::Medium, med_tabs_w)
    } else {
        (HeaderTier::Compact, calc_tabs_width(HeaderTier::Compact))
    };

    let active_style = Style::default().bg(Color::Blue).fg(Color::White).bold();
    let inactive_style = Style::default().bg(Color::DarkGray).fg(Color::White);

    let raw_project_name = match &state.repo_state {
        Some(repo) => Path::new(&repo.root)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&repo.root)
            .to_string(),
        None => "no repository".to_string(),
    };

    let mut left_items: Vec<(Span, Option<ClickTarget>)> = Vec::new();

    // Determine how much width is available for the project name and icon
    let avail_proj_w = avail_for_left.saturating_sub(tabs_w + 1);
    if avail_proj_w >= 8 {
        let max_name = avail_proj_w.saturating_sub(7).clamp(1, 20);
        let proj_name = truncate_to_width(&raw_project_name, max_name);
        left_items.push((Span::styled(" 📁 ", Style::default().fg(Color::Yellow)), None));
        left_items.push((
            Span::styled(proj_name, Style::default().fg(Color::Cyan).bold()),
            None,
        ));
        left_items.push((Span::styled(" │ ", Style::default().fg(Color::DarkGray)), None));
    } else if avail_proj_w >= 4 {
        left_items.push((Span::styled(" 📁 ", Style::default().fg(Color::Yellow)), None));
    }

    // Add tabs
    for (i, (mode, target)) in tabs.iter().enumerate() {
        if i > 0 {
            left_items.push((Span::raw(" "), None));
        }
        let label = tab_label(tier, *mode, state.view_mode, ctx_count, plans_count);
        let style = if state.view_mode == *mode {
            active_style
        } else {
            inactive_style
        };
        left_items.push((Span::styled(label, style), Some(*target)));
    }

    if state.view_mode == ViewMode::Onboarding {
        left_items.push((Span::raw(" "), None));
        let ob_label = tab_label(tier, ViewMode::Onboarding, state.view_mode, ctx_count, plans_count);
        left_items.push((
            Span::styled(
                ob_label,
                Style::default().bg(Color::Yellow).fg(Color::Black).bold(),
            ),
            None,
        ));
    }

    let left_width: usize = left_items.iter().map(|(s, _)| s.width()).sum();

    // Calculate padding between left and right groups
    let padding = inner_header_width.saturating_sub(left_width + right_width);

    let mut header_spans = Vec::new();
    let mut current_col: u16 = area.x + 1;

    // Render left items
    for (span, target) in left_items {
        let w = span.width() as u16;
        let start = current_col;
        let end = start + w.saturating_sub(1);
        if end < max_col {
            if let Some(t) = target {
                record_click(&mut state.header_clicks, t, (start, end));
            }
        }
        current_col += w;
        header_spans.push(span);
    }

    // Render middle padding
    if padding > 0 {
        header_spans.push(Span::raw(" ".repeat(padding)));
        current_col += padding as u16;
    }

    // Render right items with guaranteed visibility
    for (span, target) in right_items {
        let w = span.width() as u16;
        let start = current_col;
        let end = start + w.saturating_sub(1);
        if end < max_col {
            if let Some(t) = target {
                record_click(&mut state.header_clicks, t, (start, end));
            }
        }
        current_col += w;
        header_spans.push(span);
    }

    let header_line = Line::from(header_spans);
    let header = Paragraph::new(header_line).block(Block::default().borders(Borders::ALL));
    frame.render_widget(header, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use tauqe_protocol::ModelRef;

    fn test_app_state() -> AppState {
        AppState {
            view_mode: ViewMode::Develop,
            protocol_version: "1.0".to_string(),
            repo_state: Some(tauqe_protocol::RepositoryState {
                root: "/projects/tauqe-awesome-workspace".to_string(),
                branch: "master".to_string(),
                head: "dd60a47".to_string(),
                dirty: false,
            }),
            all_repo_files: Vec::new(),
            workflow: "git".to_string(),
            edit_protocol: "xml".to_string(),
            active_model: ModelRef::openrouter("anthropic/claude-3-7-sonnet"),
            available_models: Vec::new(),
            available_workflows: Vec::new(),
            available_edit_protocols: Vec::new(),
            model: crate::ui::develop::DevelopView::default(),
            context: tauqe_protocol::ContextState::default(),
            context_view: crate::ui::context::ContextViewState::default(),
            history_view: crate::ui::history::HistoryViewState::default(),
            review: crate::ui::review::ReviewViewState::default(),
            plans_view: crate::ui::plans::PlansViewState::default(),
            onboarding: crate::app::OnboardingState::default(),
            input_editor: crate::editor::InputEditor::default(),
            tui_config: crate::config::TuiConfig::default(),
            show_help: false,
            help_scroll: 0,
            confirm_cancel: false,
            confirm_quit: false,
            confirm_undo: false,
            confirm_clear_history: false,
            confirm_delete_plan: None,
            confirm_delete_review: None,
            confirm_execute_scope: None,
            confirm_execute_review: None,
            plan_batch_queue: None,
            review_batch_queue: None,
            active_review_step: None,
            confirm_button: crate::app::ConfirmDialogButton::Cancel,
            selection_dialog: None,
            status_dialog: None,
            squash_dialog: None,
            review_dialog: None,
            discuss_plan_dialog: None,
            discuss_review_dialog: None,
            notification: None,
            turn_started_at: None,
            server_disconnected: None,
            server_log_path: std::path::PathBuf::from(".tauqe/server.log"),
            last_model_height: 10,
            header_clicks: HeaderClickAreas::default(),
            terminal_focused: true,
            shift_tip_shown: false,
            model_choice: crate::app::ModelChoice::default(),
        }
    }

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

    #[test]
    fn test_header_renders_and_registers_clicks_across_widths() {
        let widths = [60, 70, 80, 100, 120, 160];
        for &w in &widths {
            let backend = TestBackend::new(w, 3);
            let mut terminal = Terminal::new(backend).expect("terminal created");
            let mut state = test_app_state();

            terminal
                .draw(|frame| {
                    render_header(frame, &mut state, frame.area());
                })
                .expect("draw succeeded");

            let clicks = state.header_clicks;
            assert_ne!(
                clicks.help_button,
                (0, 0),
                "Help button must be visible and registered at width {}",
                w
            );
            assert_ne!(
                clicks.squash_button,
                (0, 0),
                "Squash button must be visible and registered at width {}",
                w
            );
            assert_ne!(
                clicks.model_select,
                (0, 0),
                "Model selector must be visible and registered at width {}",
                w
            );
            assert_ne!(
                clicks.develop_tab,
                (0, 0),
                "Develop tab must be visible and registered at width {}",
                w
            );
            assert_ne!(
                clicks.history_tab,
                (0, 0),
                "History tab must be visible and registered at width {}",
                w
            );

            // Click ranges must be valid and within the terminal bounds
            let max_col = w - 1;
            assert!(clicks.help_button.1 < max_col, "Help button exceeds width {} ({:?})", w, clicks.help_button);
            assert!(clicks.squash_button.1 < clicks.help_button.0, "Squash and help button overlap");
            assert!(clicks.model_select.1 < clicks.squash_button.0, "Model and squash button overlap");
            assert!(clicks.develop_tab.1 < clicks.context_tab.0, "Develop and context tab overlap");
        }
    }
}
