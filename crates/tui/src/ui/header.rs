use std::path::Path;

use ratatui::layout::Rect;
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::{role_badge, role_title, AppState, HeaderClickAreas, ViewMode};

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

/// Builds the model selector label strictly from the active selection.
/// Semantic roles render cleanly as the role title (e.g. `Coder`),
/// while an explicit model selection renders the concrete model name.
/// Builds the model selector label from the live turn state.
///
/// While a turn owns the interface the header mirrors the model that is
/// actually running together with its semantic role. The background Scribe role
/// never owns the interface, so its work must not replace the developer's
/// visible model. When no turn is active the header shows the current selection.
fn model_selector_label(state: &AppState, max_model_len: usize) -> String {
    if state.model.is_busy() {
        if let Some(running) = state.running_model.as_ref() {
            if running.role != Some(tauqe_protocol::ModelRole::Scribe) {
                let label = truncate_to_width(&running.label(), max_model_len);
                return match running.role {
                    Some(role) => format!("{} {}", label, role_badge(role)),
                    None => label,
                };
            }
        }
    }

    match &state.model_choice.selection {
        tauqe_protocol::ModelSelection::Specific(model_ref) => {
            let primary_entry = state
                .model_choice
                .roles
                .as_ref()
                .and_then(|r| r.entries.as_ref())
                .and_then(|e| e.primary.iter().find(|entry| &entry.model == model_ref));

            let name = if let Some(entry) = primary_entry {
                crate::app::compact_entry_label(entry)
            } else {
                crate::app::compact_model_name(model_ref).to_string()
            };
            truncate_to_width(&name, max_model_len)
        }
        tauqe_protocol::ModelSelection::Role(role) => {
            let role_entry = state
                .model_choice
                .roles
                .as_ref()
                .and_then(|r| r.entries.as_ref())
                .and_then(|e| match role {
                    tauqe_protocol::ModelRole::Primary => e.primary.first(),
                    tauqe_protocol::ModelRole::Coder => e.coder.first(),
                    tauqe_protocol::ModelRole::Architect => e.architect.first(),
                    tauqe_protocol::ModelRole::Reviewer => e.reviewer.first(),
                    tauqe_protocol::ModelRole::Scribe => e.scribe.first(),
                });

            if let Some(entry) = role_entry {
                truncate_to_width(&crate::app::compact_entry_label(entry), max_model_len)
            } else if let Some(model_ref) =
                state.model_choice.resolve(&tauqe_protocol::ModelSelection::Role(*role))
            {
                truncate_to_width(crate::app::compact_model_name(&model_ref), max_model_len)
            } else {
                role_title(*role).to_string()
            }
        }
    }
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
    review_count: usize,
    plans_count: usize,
) -> String {
    match tier {
        HeaderTier::Full => match mode {
            ViewMode::Develop => " 1: REPL ".to_string(),
            ViewMode::Context => format!(" 2: Context ({}) ", ctx_count),
            ViewMode::Review => {
                if review_count > 0 {
                    format!(" 3: Review ({}) ", review_count)
                } else {
                    " 3: Review ".to_string()
                }
            }
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
            ViewMode::Develop => " 1: REPL ".to_string(),
            ViewMode::Context => format!(" 2: Ctx ({}) ", ctx_count),
            ViewMode::Review => {
                if review_count > 0 {
                    format!(" 3: Rev ({}) ", review_count)
                } else {
                    " 3: Rev ".to_string()
                }
            }
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
                        " 1: REPL ".to_string()
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
                        if review_count > 0 {
                            format!(" 3: Rev ({}) ", review_count)
                        } else {
                            " 3: Rev ".to_string()
                        }
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

    // 1. Determine Right-Side configuration with guaranteed priority. The Help
    // button advertises the real binding (primary-modifier chord) rather than `?`,
    // which would be typed literally into the Develop prompt.
    let help_hint = crate::app::help_open_hint(state.tui_config.input.primary_modifier);
    let (max_model_len, squash_text, help_text): (usize, &str, String) =
        if inner_header_width >= 125 {
            (20, " [F6 Squash] ", format!(" [{}] Help ", help_hint))
        } else if inner_header_width >= 95 {
            (12, " [F6 Squash] ", format!(" [{}] Help ", help_hint))
        } else if inner_header_width >= 75 {
            (8, " [F6] ", format!(" [{}] Help ", help_hint))
        } else {
            (6, " [F6] ", format!(" [{}] ", help_hint))
        };

    let model_label = model_selector_label(state, max_model_len);
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
    let review_count = if state.review.sessions.is_empty() && state.review.session.is_some() {
        1
    } else {
        state.review.sessions.len()
    };
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
            .map(|(mode, _)| {
                tab_label(
                    tier,
                    *mode,
                    state.view_mode,
                    ctx_count,
                    review_count,
                    plans_count,
                )
                .len()
            })
            .sum();
        w += tabs.len().saturating_sub(1);
        if state.view_mode == ViewMode::Onboarding {
            w += 1
                + tab_label(
                    tier,
                    ViewMode::Onboarding,
                    state.view_mode,
                    ctx_count,
                    review_count,
                    plans_count,
                )
                .len();
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
        let label = tab_label(
            tier,
            *mode,
            state.view_mode,
            ctx_count,
            review_count,
            plans_count,
        );
        let style = if state.view_mode == *mode {
            active_style
        } else {
            inactive_style
        };
        left_items.push((Span::styled(label, style), Some(*target)));
    }

    if state.view_mode == ViewMode::Onboarding {
        left_items.push((Span::raw(" "), None));
        let ob_label = tab_label(
            tier,
            ViewMode::Onboarding,
            state.view_mode,
            ctx_count,
            review_count,
            plans_count,
        );
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
    use tauqe_protocol::{ModelRef, ModelRole, ModelSelection};

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
            model: crate::views::develop::DevelopView::default(),
            context: tauqe_protocol::ContextState::default(),
            context_view: crate::views::context::ContextViewState::default(),
            history_view: crate::views::history::HistoryViewState::default(),
            review: crate::views::review::ReviewViewState::default(),
            plans_view: crate::views::plans::PlansViewState::default(),
            onboarding: crate::app::OnboardingState::default(),
            input_editor: crate::editor::InputEditor::default(),
            tui_config: crate::config::TuiConfig::default(),
            show_help: false,
            help_scroll: 0,
            confirm_cancel: false,
            confirm_quit: false,
            confirm_undo: false,
            confirm_detach_focus: false,
            confirm_clear_history: false,
            confirm_delete_plan: None,
            confirm_delete_review: None,
            confirm_execute_scope: None,
            confirm_execute_review: None,
            confirm_button: crate::app::ConfirmDialogButton::Cancel,
            selection_dialog: None,
            status_dialog: None,
            squash_dialog: None,
            review_dialog: None,
            prompt_modal: None,
            discuss_plan_dialog: None,
            discuss_review_dialog: None,
            notification: None,
            turn_started_at: None,
            server_disconnected: None,
            server_log_path: std::path::PathBuf::from(".tauqe/server.log"),
            last_model_height: 10,
            last_frame_area: Rect::new(0, 0, 80, 24),
            header_clicks: HeaderClickAreas::default(),
            terminal_focused: true,
            shift_tip_shown: false,
            model_choice: crate::app::ModelChoice::default(),
            running_model: None,
            discussion_focus: tauqe_protocol::DiscussionFocus::None,
        }
    }

    #[test]
    fn test_tab_label_review_count() {
        assert_eq!(
            tab_label(HeaderTier::Full, ViewMode::Develop, ViewMode::Develop, 0, 0, 0),
            " 1: REPL "
        );
        assert_eq!(
            tab_label(HeaderTier::Medium, ViewMode::Develop, ViewMode::Develop, 0, 0, 0),
            " 1: REPL "
        );
        assert_eq!(
            tab_label(HeaderTier::Compact, ViewMode::Develop, ViewMode::Develop, 0, 0, 0),
            " 1: REPL "
        );
        assert_eq!(
            tab_label(HeaderTier::Compact, ViewMode::Develop, ViewMode::Context, 0, 0, 0),
            " 1 "
        );
        assert_eq!(
            tab_label(HeaderTier::Full, ViewMode::Review, ViewMode::Develop, 0, 0, 0),
            " 3: Review "
        );
        assert_eq!(
            tab_label(HeaderTier::Full, ViewMode::Review, ViewMode::Develop, 0, 2, 0),
            " 3: Review (2) "
        );
        assert_eq!(
            tab_label(HeaderTier::Medium, ViewMode::Review, ViewMode::Develop, 0, 3, 0),
            " 3: Rev (3) "
        );
        assert_eq!(
            tab_label(HeaderTier::Compact, ViewMode::Review, ViewMode::Review, 0, 1, 0),
            " 3: Rev (1) "
        );
        assert_eq!(
            tab_label(HeaderTier::Compact, ViewMode::Review, ViewMode::Develop, 0, 1, 0),
            " 3 "
        );
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

    #[test]
    fn test_model_selector_label_role_shows_title_only() {
        let mut state = test_app_state();
        state.model_choice.selection = ModelSelection::Role(ModelRole::Coder);
        assert_eq!(model_selector_label(&state, 20), "Coder");

        state.model_choice.selection = ModelSelection::Role(ModelRole::Architect);
        assert_eq!(model_selector_label(&state, 20), "Architect");

        // Roles are never truncated by the model-length budget.
        state.model_choice.selection = ModelSelection::Role(ModelRole::Reviewer);
        assert_eq!(model_selector_label(&state, 6), "Reviewer");
    }

    #[test]
    fn test_model_selector_label_idle_prefers_primary_entry_label() {
        let mut state = test_app_state();
        let entry = tauqe_protocol::ModelEntry::new(
            ModelRef::openrouter("google/gemini-2.0-flash"),
            None,
            Some("Gemini Flash".to_string()),
        );
        state.model_choice = crate::app::ModelChoice {
            selection: ModelSelection::Role(ModelRole::Primary),
            roles: Some(tauqe_protocol::ModelRolesSummary {
                scribe: ModelRef::openrouter("scribe-model"),
                coder: ModelRef::openrouter("coder-model"),
                architect: ModelRef::openrouter("architect-model"),
                primary: ModelRef::openrouter("google/gemini-2.0-flash"),
                reviewer: ModelRef::openrouter("reviewer-model"),
                efforts: None,
                entries: Some(tauqe_protocol::ModelRolesEntries {
                    primary: vec![entry],
                    ..Default::default()
                }),
            }),
        };

        assert_eq!(model_selector_label(&state, 20), "Gemini Flash");
    }

    #[test]
    fn test_model_selector_label_specific_shows_model_name() {
        let mut state = test_app_state();
        state.model_choice.selection =
            ModelSelection::Specific(ModelRef::openrouter("anthropic/claude-3-7-sonnet"));
        let label = model_selector_label(&state, 20);
        assert!(label.starts_with("anthropic/claude-3"));
        assert!(label.ends_with('…'));
        assert!(Span::raw(&label).width() <= 20);

        state.model_choice.selection = ModelSelection::Specific(ModelRef::openrouter("gpt-4o"));
        assert_eq!(model_selector_label(&state, 20), "gpt-4o");
    }

    #[test]
    fn test_model_selector_label_tracks_running_turn() {
        let mut state = test_app_state();
        state.model_choice.selection = ModelSelection::Role(ModelRole::Primary);
        state.model.status = crate::views::develop::TurnStatus::Thinking;

        // The live model and its semantic role replace the idle selection.
        state.running_model = Some(crate::app::RunningModel {
            model: "openrouter:anthropic/claude-3-7-sonnet".to_string(),
            role: Some(ModelRole::Coder),
            alias: None,
            effort: None,
        });
        assert_eq!(
            model_selector_label(&state, 40),
            "anthropic/claude-3-7-sonnet [Cod]"
        );

        // A configured alias wins over the provider-qualified name.
        state.running_model = Some(crate::app::RunningModel {
            model: "openrouter:anthropic/claude-3-7-sonnet".to_string(),
            role: Some(ModelRole::Architect),
            alias: Some("Sonnet".to_string()),
            effort: None,
        });
        assert_eq!(model_selector_label(&state, 40), "Sonnet [Arc]");

        // The background Scribe role never replaces the visible model.
        state.running_model = Some(crate::app::RunningModel {
            model: "openrouter:scribe".to_string(),
            role: Some(ModelRole::Scribe),
            alias: None,
            effort: None,
        });
        assert_eq!(model_selector_label(&state, 40), "Primary");

        // Once the turn settles the header returns to the developer's selection.
        state.model.status = crate::views::develop::TurnStatus::Done;
        state.running_model = None;
        assert_eq!(model_selector_label(&state, 40), "Primary");
    }

    #[test]
    fn test_header_renders_with_alt_primary_modifier() {
        // The Help button label follows the configured primary modifier, so the
        // header must still render and register its click targets under `alt`.
        let backend = TestBackend::new(80, 3);
        let mut terminal = Terminal::new(backend).expect("terminal created");
        let mut state = test_app_state();
        state.tui_config.input.primary_modifier = crate::config::PrimaryModifier::Alt;

        terminal
            .draw(|frame| {
                render_header(frame, &mut state, frame.area());
            })
            .expect("draw succeeded");

        assert_ne!(state.header_clicks.help_button, (0, 0));
        assert_ne!(state.header_clicks.squash_button, (0, 0));
    }
}
