use std::path::Path;

use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::{role_badge, role_title, AppState, HeaderClickAreas, ViewMode};
use crate::i18n::I18nEngine;
use crate::theme::engine::{current_engine, ThemeEngine};

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

/// Terminal display width of a string, measured in columns rather than bytes so
/// a localized label selects the header tier its visual width deserves (c-3).
fn display_width(s: &str) -> usize {
    Span::raw(s).width()
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
    ReplTab,
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
        ClickTarget::ReplTab => clicks.repl_tab = range,
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
    i18n: &I18nEngine,
) -> String {
    // Tab names come from the canonical catalog, so a hot-swapped locale keeps
    // the header in step with the rest of the interface (c-1, c-5).
    if mode == ViewMode::Onboarding {
        let name = i18n.t("tab.onboarding");
        return match tier {
            HeaderTier::Compact if mode != active_mode => " ! ".to_string(),
            _ => format!(" ! {} ", name),
        };
    }

    let (number, name, count): (&str, &str, Option<usize>) = match mode {
        ViewMode::Repl => ("1", i18n.t("tab.repl"), None),
        ViewMode::Plans => (
            "2",
            i18n.t("tab.plans"),
            (plans_count > 0).then_some(plans_count),
        ),
        ViewMode::Review => (
            "3",
            i18n.t("tab.review"),
            (review_count > 0).then_some(review_count),
        ),
        ViewMode::Context => ("4", i18n.t("tab.context"), Some(ctx_count)),
        ViewMode::History => ("5", i18n.t("tab.history"), None),
        ViewMode::Onboarding => ("!", i18n.t("tab.onboarding"), None),
    };

    // The three tiers trade count detail for horizontal space: the full tier
    // shows the count, the medium tier the name only, and the compact tier
    // collapses inactive tabs to their number.
    match tier {
        HeaderTier::Full => match count {
            Some(c) => format!(" {}: {} ({}) ", number, name, c),
            None => format!(" {}: {} ", number, name),
        },
        HeaderTier::Medium => format!(" {}: {} ", number, name),
        HeaderTier::Compact => {
            if mode == active_mode {
                format!(" {}: {} ", number, name)
            } else {
                format!(" {} ", number)
            }
        }
    }
}

/// Top header: project name -> tabs -> model selector & tools (Squash, Help).
pub fn render_header(frame: &mut ratatui::Frame, state: &mut AppState, area: Rect) {
    let engine = current_engine();
    render_header_with_engine(frame, state, area, &engine);
}

fn render_header_with_engine(
    frame: &mut ratatui::Frame,
    state: &mut AppState,
    area: Rect,
    engine: &ThemeEngine,
) {
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
    // The squash button advertises the portable C-S / M-S chord, following the
    // configured primary modifier instead of the former F6 hardware key.
    let squash_hint = format!("{}S", state.tui_config.input.primary_modifier.emacs_prefix());
    let (max_model_len, squash_text, help_text): (usize, String, String) =
        if inner_header_width >= 125 {
            (20, format!(" [{} Squash] ", squash_hint), format!(" [{}] Help ", help_hint))
        } else if inner_header_width >= 95 {
            (12, format!(" [{} Squash] ", squash_hint), format!(" [{}] Help ", help_hint))
        } else if inner_header_width >= 75 {
            (8, format!(" [{}] ", squash_hint), format!(" [{}] Help ", help_hint))
        } else {
            (6, format!(" [{}] ", squash_hint), format!(" [{}] ", help_hint))
        };

    let palette = engine.palette();

    // 2. Budget for Left-Side (tabs & project). The right group is measured with
    // the tentative model budget below and shrunk if the tab row would otherwise
    // push the buttons past the inner width and into the border.
    let div_width = Span::raw(" │ ").width();
    let right_chrome_width = div_width * 2
        + Span::raw(squash_text.as_str()).width()
        + Span::raw(help_text.as_str()).width();
    let mut right_width =
        Span::raw(format!(" 🧠 {} ", model_selector_label(state, max_model_len))).width()
            + right_chrome_width;

    let avail_for_left = inner_header_width.saturating_sub(right_width + 1);

    let active_mode = state.view_mode;
    let ctx_count = state.context.items.len();
    let review_count = if state.review.sessions.is_empty() && state.review.session.is_some() {
        1
    } else {
        state.review.sessions.len()
    };
    let plans_count = state.plans_view.plans.len();

    let tabs = [
        (ViewMode::Repl, ClickTarget::ReplTab),
        (ViewMode::Plans, ClickTarget::PlansTab),
        (ViewMode::Review, ClickTarget::ReviewTab),
        (ViewMode::Context, ClickTarget::ContextTab),
        (ViewMode::History, ClickTarget::HistoryTab),
    ];

    // Tab widths are measured in terminal columns (not bytes), so a localized
    // label made of multi-byte glyphs selects the same tier its visual width
    // deserves and the header never desynchronizes from its drawn geometry when
    // the locale is hot-swapped (c-1, c-3, c-5).
    let calc_tabs_width = |tier: HeaderTier, i18n: &I18nEngine| -> usize {
        let mut w: usize = tabs
            .iter()
            .map(|(mode, _)| {
                display_width(&tab_label(
                    tier,
                    *mode,
                    active_mode,
                    ctx_count,
                    review_count,
                    plans_count,
                    i18n,
                ))
            })
            .sum();
        w += tabs.len().saturating_sub(1);
        if active_mode == ViewMode::Onboarding {
            w += 1
                + display_width(&tab_label(
                    tier,
                    ViewMode::Onboarding,
                    active_mode,
                    ctx_count,
                    review_count,
                    plans_count,
                    i18n,
                ));
        }
        w
    };

    let full_tabs_w = calc_tabs_width(HeaderTier::Full, &state.i18n);
    let med_tabs_w = calc_tabs_width(HeaderTier::Medium, &state.i18n);

    let (tier, tabs_w) = if avail_for_left >= full_tabs_w + 14 {
        (HeaderTier::Full, full_tabs_w)
    } else if avail_for_left >= med_tabs_w + 10 {
        (HeaderTier::Medium, med_tabs_w)
    } else {
        (
            HeaderTier::Compact,
            calc_tabs_width(HeaderTier::Compact, &state.i18n),
        )
    };

    // Guarantee the right-side group (model chip, Squash, Help) stays inside the
    // header. Localized tab names widen the left group and a role title is a
    // fixed-width label, so shrinking only the model-length budget cannot always
    // recover the space. Truncating the composed chip to whatever remains after
    // the tabs and the fixed right chrome keeps every tab and button visible and
    // hit-testable instead of spilling into the border (Conventions §6, §7).
    let model_budget = inner_header_width
        .saturating_sub(tabs_w + 1 + right_chrome_width)
        .max(1);
    let model_label = model_selector_label(state, max_model_len);
    let model_text = truncate_to_width(&format!(" 🧠 {} ", model_label), model_budget);
    right_width = Span::raw(model_text.as_str()).width() + right_chrome_width;
    let model_span = Span::styled(model_text, engine.fg(palette.status_success).bold());

    let squash_span = Span::styled(
        squash_text,
        engine
            .bg(palette.button_active_bg)
            .fg(engine.color(palette.text_inverse))
            .bold(),
    );

    let help_span = Span::styled(
        help_text,
        engine
            .bg(palette.button_bg)
            .fg(engine.color(palette.button_fg))
            .bold(),
    );

    let div_style = engine.fg(palette.text_muted);
    let div_span = Span::styled(" │ ", div_style);

    let right_items: Vec<(Span, Option<ClickTarget>)> = vec![
        (model_span, Some(ClickTarget::ModelSelect)),
        (div_span.clone(), None),
        (squash_span, Some(ClickTarget::SquashButton)),
        (div_span, None),
        (help_span, Some(ClickTarget::HelpButton)),
    ];

    let active_style = engine
        .bg(palette.button_active_bg)
        .fg(engine.color(palette.text_inverse))
        .bold();
    let inactive_style = engine
        .bg(palette.button_bg)
        .fg(engine.color(palette.button_fg));

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
        left_items.push((Span::styled(" 📁 ", engine.fg(palette.text_secondary)), None));
        left_items.push((
            Span::styled(proj_name, engine.fg(palette.status_info).bold()),
            None,
        ));
        left_items.push((Span::styled(" │ ", engine.fg(palette.text_muted)), None));
    } else if avail_proj_w >= 4 {
        left_items.push((Span::styled(" 📁 ", engine.fg(palette.text_secondary)), None));
    }

    // Add tabs
    for (i, (mode, target)) in tabs.iter().enumerate() {
        if i > 0 {
            left_items.push((Span::raw(" "), None));
        }
        let label = tab_label(
            tier,
            *mode,
            active_mode,
            ctx_count,
            review_count,
            plans_count,
            &state.i18n,
        );
        let style = if active_mode == *mode {
            active_style
        } else {
            inactive_style
        };
        left_items.push((Span::styled(label, style), Some(*target)));
    }

    if active_mode == ViewMode::Onboarding {
        left_items.push((Span::raw(" "), None));
        let ob_label = tab_label(
            tier,
            ViewMode::Onboarding,
            active_mode,
            ctx_count,
            review_count,
            plans_count,
            &state.i18n,
        );
        left_items.push((
            Span::styled(
                ob_label,
                engine
                    .bg(palette.status_warning)
                    .fg(engine.color(palette.text_inverse))
                    .bold(),
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
    let header = Paragraph::new(header_line)
        .style(ratatui::style::Style::default().bg(engine.color(palette.canvas)))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(engine.fg(palette.border))
                .style(ratatui::style::Style::default().bg(engine.color(palette.canvas))),
        );
    frame.render_widget(header, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use tauqe_protocol::{ModelRef, ModelRole, ModelSelection};

    use crate::theme::color::ColorDepth;
    use crate::theme::engine::ThemeEngine;
    use crate::theme::presets::ThemePreset;

    fn test_app_state() -> AppState {
        AppState {
            view_mode: ViewMode::Repl,
            protocol_version: "1.0".to_string(),
            repo_state: Some(tauqe_protocol::RepositoryState {
                root: "/projects/tauqe-awesome-workspace".to_string(),
                branch: "master".to_string(),
                head: "dd60a47".to_string(),
                dirty: false,
            }),
            all_repo_files: Vec::new(),
            workflow: tauqe_protocol::WorkflowKind::Git,
            edit_protocol: tauqe_protocol::EditProtocolKind::Xml,
            active_model: ModelRef::openrouter("anthropic/claude-3-7-sonnet"),
            available_models: Vec::new(),
            available_workflows: Vec::new(),
            available_edit_protocols: Vec::new(),
            model: crate::views::repl::ReplView::default(),
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
            help_tab: crate::app::HelpTab::default(),
            confirm_cancel: false,
            confirm_quit: false,
            confirm_batch_cancel: false,
            batch_cancel_button: crate::app::BatchCancelButton::Continue,
            batch_stopping: false,
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
            active_theme_preset: crate::theme::presets::ThemePreset::TauqeDark,
            theme_selector: None,
            i18n: crate::i18n::I18nEngine::english(),
            locale_dialog: None,
        }
    }

    /// Extracts the rendered header row as a string so tests can assert on the
    /// dynamic labels (for example the squash chord).
    fn header_row_text(terminal: &Terminal<TestBackend>, row: u16) -> String {
        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for x in 0..buffer.area().width {
            text.push_str(buffer[(x, row)].symbol());
        }
        text
    }

    #[test]
    fn test_tab_label_review_count() {
        let i18n = I18nEngine::english();
        let label = |tier: HeaderTier,
                     mode: ViewMode,
                     active: ViewMode,
                     ctx: usize,
                     review: usize,
                     plans: usize| {
            tab_label(tier, mode, active, ctx, review, plans, &i18n)
        };

        assert_eq!(
            label(HeaderTier::Full, ViewMode::Repl, ViewMode::Repl, 0, 0, 0),
            " 1: REPL "
        );
        assert_eq!(
            label(HeaderTier::Medium, ViewMode::Repl, ViewMode::Repl, 0, 0, 0),
            " 1: REPL "
        );
        assert_eq!(
            label(HeaderTier::Compact, ViewMode::Repl, ViewMode::Repl, 0, 0, 0),
            " 1: REPL "
        );
        assert_eq!(
            label(HeaderTier::Compact, ViewMode::Repl, ViewMode::Context, 0, 0, 0),
            " 1 "
        );
        assert_eq!(
            label(HeaderTier::Full, ViewMode::Plans, ViewMode::Repl, 0, 0, 0),
            " 2: Plans "
        );
        assert_eq!(
            label(HeaderTier::Full, ViewMode::Plans, ViewMode::Repl, 0, 0, 3),
            " 2: Plans (3) "
        );
        assert_eq!(
            label(HeaderTier::Full, ViewMode::Review, ViewMode::Repl, 0, 0, 0),
            " 3: Review "
        );
        assert_eq!(
            label(HeaderTier::Full, ViewMode::Review, ViewMode::Repl, 0, 2, 0),
            " 3: Review (2) "
        );
        assert_eq!(
            label(HeaderTier::Medium, ViewMode::Review, ViewMode::Repl, 0, 3, 0),
            " 3: Review "
        );
        assert_eq!(
            label(HeaderTier::Compact, ViewMode::Review, ViewMode::Review, 0, 1, 0),
            " 3: Review "
        );
        assert_eq!(
            label(HeaderTier::Compact, ViewMode::Review, ViewMode::Repl, 0, 1, 0),
            " 3 "
        );
    }

    /// Synthesized locale with tab names that fit the catalog column budget
    /// while differing in visual width from their English originals.
    fn russian_locale() -> crate::i18n::CustomLocale {
        let mut translations = std::collections::BTreeMap::new();
        for (key, value) in [
            ("tab.repl", "Разраб"),
            ("tab.plans", "Планы"),
            ("tab.review", "Обзор"),
            ("tab.context", "Контекст"),
            ("tab.history", "История"),
            ("tab.onboarding", "Настройка"),
        ] {
            translations.insert(key.to_string(), value.to_string());
        }
        crate::i18n::CustomLocale::new("Russian", translations)
    }

    /// Asserts every registered header hit area is in bounds and that neither
    /// the tab group nor the right-side button group overlaps.
    fn assert_ranges_within_bounds(state: &AppState, width: u16) {
        let max_col = width.saturating_sub(1);
        let tab_ranges = [
            state.header_clicks.repl_tab,
            state.header_clicks.plans_tab,
            state.header_clicks.review_tab,
            state.header_clicks.context_tab,
            state.header_clicks.history_tab,
        ];
        let action_ranges = [
            state.header_clicks.model_select,
            state.header_clicks.squash_button,
            state.header_clicks.help_button,
        ];
        let groups: [&[(u16, u16)]; 2] = [&tab_ranges, &action_ranges];

        for group in groups {
            let mut last_end: Option<u16> = None;
            for &range in group {
                if range == (0, 0) {
                    continue;
                }
                assert!(range.0 <= range.1, "malformed range {range:?} at width {width}");
                assert!(range.1 < max_col, "range {range:?} overflows width {width}");
                if let Some(prev) = last_end {
                    assert!(
                        prev < range.0,
                        "ranges overlap at width {width}: {prev} vs {range:?}"
                    );
                }
                last_end = Some(range.1);
            }
        }
    }

    #[test]
    fn test_header_geometry_holds_across_locale_hot_swap() {
        for &w in &[80u16, 100, 120, 160] {
            let backend = TestBackend::new(w, 3);
            let mut terminal = Terminal::new(backend).expect("terminal created");
            let mut state = test_app_state();

            terminal
                .draw(|frame| render_header(frame, &mut state, frame.area()))
                .expect("english draw succeeded");
            assert_ranges_within_bounds(&state, w);

            // Hot-swap to a synthesized locale; the header must re-render with
            // the same non-overlapping, in-bounds geometry (c-1, c-5).
            state.i18n = crate::i18n::I18nEngine::new(Some(russian_locale()));
            terminal
                .draw(|frame| render_header(frame, &mut state, frame.area()))
                .expect("localized draw succeeded");
            assert_ranges_within_bounds(&state, w);
            assert!(
                header_row_text(&terminal, state.header_clicks.row).contains("Разраб"),
                "localized tab label must render at width {w}"
            );
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
                clicks.repl_tab,
                (0, 0),
                "REPL tab must be visible and registered at width {}",
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
            assert!(clicks.repl_tab.1 < clicks.plans_tab.0, "REPL and plans tab overlap");

            assert!(
                header_row_text(&terminal, clicks.row).contains("C-S"),
                "the squash button must advertise the C-S chord at width {}",
                w
            );
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
        state.model.status = crate::views::repl::TurnStatus::Thinking;

        // The live model and its semantic role replace the idle selection.
        state.running_model = Some(crate::app::RunningModel {
            model: ModelRef::openrouter("anthropic/claude-3-7-sonnet"),
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
            model: ModelRef::openrouter("anthropic/claude-3-7-sonnet"),
            role: Some(ModelRole::Architect),
            alias: Some("Sonnet".to_string()),
            effort: None,
        });
        assert_eq!(model_selector_label(&state, 40), "Sonnet [Arc]");

        // The background Scribe role never replaces the visible model.
        state.running_model = Some(crate::app::RunningModel {
            model: ModelRef::openrouter("scribe"),
            role: Some(ModelRole::Scribe),
            alias: None,
            effort: None,
        });
        assert_eq!(model_selector_label(&state, 40), "Primary");

        // Once the turn settles the header returns to the developer's selection.
        state.model.status = crate::views::repl::TurnStatus::Done;
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
        assert!(
            header_row_text(&terminal, state.header_clicks.row).contains("M-S"),
            "the squash button must follow the Alt primary modifier"
        );
    }

    #[test]
    fn header_chrome_styles_track_theme_tokens() {
        for preset in [ThemePreset::TauqeDark, ThemePreset::TauqeLight] {
            let engine = ThemeEngine::new(preset, ColorDepth::Truecolor);
            let palette = engine.palette();

            let backend = TestBackend::new(120, 3);
            let mut terminal = Terminal::new(backend).expect("terminal created");
            let mut state = test_app_state();
            state.view_mode = ViewMode::Repl;

            terminal
                .draw(|frame| {
                    render_header_with_engine(frame, &mut state, frame.area(), &engine);
                })
                .expect("draw succeeded");

            let row = state.header_clicks.row;
            let (start, end) = state.header_clicks.repl_tab;
            assert!(end >= start);
            let cell = &terminal.backend().buffer()[(start, row)];
            assert_eq!(cell.bg, engine.color(palette.button_active_bg));
            assert_eq!(cell.fg, engine.color(palette.text_inverse));

            let (help_start, _) = state.header_clicks.help_button;
            let help_cell = &terminal.backend().buffer()[(help_start, row)];
            assert_eq!(help_cell.bg, engine.color(palette.button_bg));
            assert_eq!(help_cell.fg, engine.color(palette.button_fg));
        }
    }
}
