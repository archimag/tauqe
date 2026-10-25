use std::path::Path;

use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::{AppState, ViewMode};
use crate::i18n::I18nEngine;
use crate::theme::engine::{current_engine, ThemeEngine};
use crate::ui::UiAction;

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

/// Resolves the model name and the reasoning depth the header must print.
///
/// While a turn owns the interface the header mirrors the model that is
/// actually running. The background Scribe role never owns the interface, so
/// its work must not replace the developer's visible model. When no turn is
/// active the header shows the concrete model the current selection resolves
/// to. Every branch goes through the single interface formatter, and the
/// semantic role is never part of the text: a role is how a model is selected,
/// not a property of its name (c-2).
///
/// The name and the depth travel as two values because the header spends a
/// narrow budget: only the name may be shortened, while the depth the developer
/// confirmed in the picker must stay readable on screen (c-2).
fn model_selector_parts(state: &AppState) -> (String, Option<tauqe_protocol::ReasoningEffort>) {
    if state.model.is_busy() {
        if let Some(running) = state.running_model.as_ref() {
            if running.role != Some(tauqe_protocol::ModelRole::Scribe) {
                return (
                    crate::model_display::model_label(&running.model, None),
                    running.effort,
                );
            }
        }
    }

    let entry = match &state.model_choice.selection {
        tauqe_protocol::ModelSelection::Specific(model_ref) => state
            .model_choice
            .roles
            .as_ref()
            .and_then(|roles| roles.entries.as_ref())
            .and_then(|entries| entries.primary.iter().find(|entry| &entry.model == model_ref))
            .cloned(),
        // A tier selection carries the full { model, effort } coordinate of
        // the picked role-list entry, so the label is rendered straight from
        // the DTO by the fallback branch below instead of re-deriving an
        // entry by matching the model (c-1).
        tauqe_protocol::ModelSelection::Tier { .. } => None,
        tauqe_protocol::ModelSelection::Role(role) => crate::model_display::role_entry(*role),
    };

    match entry {
        Some(entry) => (
            crate::model_display::model_label(&entry.model, None),
            entry.effort,
        ),
        None => match &state.model_choice.selection {
            tauqe_protocol::ModelSelection::Specific(model_ref) => {
                (crate::model_display::model_label(model_ref, None), None)
            }
            tauqe_protocol::ModelSelection::Tier { model, effort } => {
                (crate::model_display::model_label(model, None), *effort)
            }
            tauqe_protocol::ModelSelection::Role(_) => (
                crate::model_display::model_label(&state.active_model, None),
                None,
            ),
        },
    }
}

/// Renders the model selector label, keeping the selected reasoning depth.
///
/// Only the name is truncated to `max_model_len` columns; the depth suffix is
/// always appended in full, so a long provider name can never push the depth the
/// developer picked out of the header (c-2).
fn model_selector_label(state: &AppState, max_model_len: usize) -> String {
    let (name, effort) = model_selector_parts(state);
    match effort {
        Some(effort) => format!("{} [{}]", truncate_to_width(&name, max_model_len), effort),
        None => truncate_to_width(&name, max_model_len),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeaderTier {
    Full,
    Medium,
    Compact,
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

/// Top header: project name -> tabs -> model selector & tools (Help).
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
    if area.height < 3 || area.width < 10 {
        return;
    }

    let header_row = area.y + 1;

    let inner_header_width = area.width.saturating_sub(2) as usize;
    let max_col = area.x + area.width.saturating_sub(1);

    // 1. Determine Right-Side configuration with guaranteed priority. The Help
    // button advertises the real binding (primary-modifier chord) rather than `?`,
    // which would be typed literally into the Develop prompt.
    let help_hint = crate::app::help_open_hint(state.tui_config.input.primary_modifier);
    let help_label = state.i18n.t("header.help").to_string();
    let (max_model_len, help_text): (usize, String) = if inner_header_width >= 100 {
        (20, format!(" [{}] {} ", help_hint, help_label))
    } else if inner_header_width >= 80 {
        (12, format!(" [{}] {} ", help_hint, help_label))
    } else if inner_header_width >= 60 {
        (8, format!(" [{}] {} ", help_hint, help_label))
    } else {
        (6, format!(" [{}] ", help_hint))
    };

    let palette = engine.palette();

    // 2. Budget for Left-Side (tabs & project). The right group is measured with
    // a nominal model chip below, so the tier is chosen against a right group of
    // realistic width and a wide help label can never push the buttons past the
    // inner width and into the border.
    let div_width = Span::raw(" │ ").width();
    let right_chrome_width = div_width + Span::raw(help_text.as_str()).width();
    let nominal_right_width = Span::raw(format!(
        " 🧠 {} ",
        model_selector_label(state, max_model_len)
    ))
    .width()
        + right_chrome_width;

    let avail_for_left = inner_header_width.saturating_sub(nominal_right_width + 1);

    let active_mode = state.view_mode;
    let ctx_count = state.context.items.len();
    let review_count = if state.review.sessions.is_empty() && state.review.session.is_some() {
        1
    } else {
        state.review.sessions.len()
    };
    let plans_count = state.plans_view.plans.len();

    let tabs = [
        ViewMode::Repl,
        ViewMode::Plans,
        ViewMode::Review,
        ViewMode::Context,
        ViewMode::History,
    ];

    // Tab widths are measured in terminal columns (not bytes), so a localized
    // label made of multi-byte glyphs selects the same tier its visual width
    // deserves and the header never desynchronizes from its drawn geometry when
    // the locale is hot-swapped (c-1, c-3, c-5).
    let calc_tabs_width = |tier: HeaderTier, i18n: &I18nEngine| -> usize {
        let mut w: usize = tabs
            .iter()
            .map(|mode| {
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
            w += 1 + display_width(&tab_label(
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

    // The tabs, the fixed right chrome, the project tile, and the model chip all
    // draw from one shared budget. The minimum chip is reserved first, the
    // project tile is admitted only from what remains above it, and the chip then
    // takes the exact remainder, so the three groups can never sum past the inner
    // width and push the model chip or the Help button out of the header
    // (Conventions §6, §7).
    let chip_min_width = display_width(" 🧠 x ");
    let slack = inner_header_width
        .saturating_sub(tabs_w + 1 + right_chrome_width)
        .max(chip_min_width + 1);
    let project_budget = slack.saturating_sub(chip_min_width);
    let max_name_budget = project_budget.saturating_sub(7);
    let show_project_name = project_budget >= 8;
    let show_project_icon = !show_project_name && project_budget >= 4;
    let project_width = if show_project_name {
        4 + max_name_budget.clamp(1, 20) + 3
    } else if show_project_icon {
        4
    } else {
        0
    };
    let model_budget = slack.saturating_sub(project_width).max(1);
    // The chip spends its budget on the name first: the icon and the reasoning
    // depth keep their full width, so the depth the developer picked stays on
    // screen even when the provider name has to be shortened (c-2).
    let (model_name, model_effort) = model_selector_parts(state);
    let effort_suffix = model_effort
        .map(|effort| format!(" [{}]", effort))
        .unwrap_or_default();
    let chip_chrome = display_width(" 🧠 ") + 1;
    let name_cap = model_budget
        .saturating_sub(chip_chrome + display_width(&effort_suffix))
        .max(1);
    let model_text = truncate_to_width(
        &format!(
            " 🧠 {}{} ",
            truncate_to_width(&model_name, name_cap),
            effort_suffix
        ),
        model_budget,
    );
    let right_width = Span::raw(model_text.as_str()).width() + right_chrome_width;

    let model_span = Span::styled(model_text, engine.fg(palette.status_success).bold());

    let help_span = Span::styled(
        help_text,
        engine
            .bg(palette.button_bg)
            .fg(engine.color(palette.button_fg))
            .bold(),
    );

    let div_style = engine.fg(palette.text_muted);
    let div_span = Span::styled(" │ ", div_style);

    let right_items: Vec<(Span, Option<UiAction>)> = vec![
        (model_span, Some(UiAction::OpenModelSelect)),
        (div_span, None),
        (help_span, Some(UiAction::ToggleHelp)),
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
        None => state.i18n.t("header.no_repository").to_string(),
    };

    let mut left_items: Vec<(Span, Option<UiAction>)> = Vec::new();

    // The project tile was admitted from the shared budget above, so it draws
    // exactly the width reserved for it and never starves the model chip or the
    // Help button.
    if show_project_name {
        let proj_name = truncate_to_width(&raw_project_name, max_name_budget.clamp(1, 20));
        left_items.push((
            Span::styled(" 📁 ", engine.fg(palette.text_secondary)),
            None,
        ));
        left_items.push((
            Span::styled(proj_name, engine.fg(palette.status_info).bold()),
            None,
        ));
        left_items.push((Span::styled(" │ ", engine.fg(palette.text_muted)), None));
    } else if show_project_icon {
        left_items.push((
            Span::styled(" 📁 ", engine.fg(palette.text_secondary)),
            None,
        ));
    }

    // Add tabs
    for (i, mode) in tabs.iter().enumerate() {
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
        left_items.push((
            Span::styled(label, style),
            Some(UiAction::SwitchView(*mode)),
        ));
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

    // Render left items. Each drawn control registers its own rectangle with a
    // typed action, so the pointer dispatcher never learns which widget lives
    // where and adding or moving a tab touches only this renderer (c-1, c-2).
    for (span, action) in left_items {
        let w = span.width() as u16;
        let start = current_col;
        let end = start + w.saturating_sub(1);
        if end < max_col {
            if let Some(action) = action {
                state
                    .hit_regions
                    .register(Rect::new(start, header_row, w, 1), action);
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

    // Render right items with guaranteed visibility, registering each one the
    // same way the left group does.
    for (span, action) in right_items {
        let w = span.width() as u16;
        let start = current_col;
        let end = start + w.saturating_sub(1);
        if end < max_col {
            if let Some(action) = action {
                state
                    .hit_regions
                    .register(Rect::new(start, header_row, w, 1), action);
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
            confirm_detach_focus: false,
            confirm_clear_horizon: None,
            confirm_clear_horizon_final: None,
            confirm_clear_history: false,
            confirm_delete_plan: None,
            confirm_delete_plan_final: None,
            confirm_delete_review: None,
            confirm_execute_scope: None,
            confirm_execute_pipeline: None,
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
            hit_regions: crate::ui::HitRegistry::default(),
            terminal_focused: true,
            shift_tip_shown: false,
            model_choice: crate::app::ModelChoice::default(),
            running_model: None,
            discussion_focus: tauqe_protocol::DiscussionFocus::None,
            active_theme_preset: crate::theme::presets::ThemePreset::TauqeDark,
            theme_selector: None,
            i18n: crate::i18n::I18nEngine::english(),
            locale_dialog: None,
            usage_summary: None,
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
        let label =
            |tier: HeaderTier,
             mode: ViewMode,
             active: ViewMode,
             ctx: usize,
             review: usize,
             plans: usize| { tab_label(tier, mode, active, ctx, review, plans, &i18n) };

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
            label(
                HeaderTier::Compact,
                ViewMode::Repl,
                ViewMode::Context,
                0,
                0,
                0
            ),
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
            label(
                HeaderTier::Medium,
                ViewMode::Review,
                ViewMode::Repl,
                0,
                3,
                0
            ),
            " 3: Review "
        );
        assert_eq!(
            label(
                HeaderTier::Compact,
                ViewMode::Review,
                ViewMode::Review,
                0,
                1,
                0
            ),
            " 3: Review "
        );
        assert_eq!(
            label(
                HeaderTier::Compact,
                ViewMode::Review,
                ViewMode::Repl,
                0,
                1,
                0
            ),
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

    /// Row of the rendered frame that carries the header controls: row 0 is the
    /// top border, so the inner header row is the first drawn row.
    const HEADER_ROW: u16 = 1;

    /// Draws the header once, resetting the frame-scoped registry exactly as
    /// `render_ui` does before the components declare their regions (c-1, c-3).
    fn draw_header(
        terminal: &mut Terminal<TestBackend>,
        state: &mut AppState,
        engine: &ThemeEngine,
    ) {
        state.hit_regions.clear();
        // Mirror the production frame: the catalog is published before the
        // header draws, so its labels read the current configuration (c-3).
        crate::model_display::install_snapshot(state.model_choice.roles.as_ref());
        terminal
            .draw(|frame| render_header_with_engine(frame, state, frame.area(), engine))
            .expect("draw succeeded");
    }

    /// Rectangle the header registered for `action`, if it drew that control.
    fn header_region(state: &AppState, action: UiAction) -> Option<Rect> {
        state
            .hit_regions
            .regions()
            .iter()
            .find(|region| region.action == action)
            .map(|region| region.rect)
    }

    /// Asserts every region the header declared lies inside the drawn frame,
    /// never overlaps a neighbour, and resolves back to its own action (c-1).
    fn assert_header_geometry(state: &AppState, width: u16) {
        let max_col = width.saturating_sub(1);
        let regions = state.hit_regions.regions();
        assert!(!regions.is_empty(), "the header must register its controls");

        let mut ordered: Vec<Rect> = regions.iter().map(|region| region.rect).collect();
        ordered.sort_by_key(|rect| rect.x);

        for region in regions {
            assert_eq!(region.rect.y, HEADER_ROW, "controls live on the header row");
            assert_eq!(region.rect.height, 1, "a control occupies one row");
            assert!(
                region.rect.right() <= max_col,
                "region {:?} overflows width {width}",
                region.rect
            );
            assert_eq!(
                state.hit_regions.action_at(region.rect.x, HEADER_ROW),
                Some(region.action),
                "a drawn control must resolve to its own action"
            );
        }
        for pair in ordered.windows(2) {
            assert!(
                pair[0].right() <= pair[1].x,
                "header controls overlap at width {width}: {:?} vs {:?}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn test_header_geometry_holds_across_locale_hot_swap() {
        for &w in &[80u16, 100, 120, 160] {
            let backend = TestBackend::new(w, 3);
            let mut terminal = Terminal::new(backend).expect("terminal created");
            let mut state = test_app_state();

            draw_header(&mut terminal, &mut state, &current_engine());
            assert_header_geometry(&state, w);

            // Hot-swap to a synthesized locale; the header must re-render with
            // the same non-overlapping, in-bounds geometry (c-1, c-5).
            state.i18n = crate::i18n::I18nEngine::new(Some(russian_locale()));
            draw_header(&mut terminal, &mut state, &current_engine());
            assert_header_geometry(&state, w);
            assert!(
                header_row_text(&terminal, HEADER_ROW).contains("Разраб"),
                "localized tab label must render at width {w}"
            );
        }
    }

    /// A customized locale must reach the header action buttons and the
    /// no-repository placeholder; the English literals must never render in
    /// their place (Conventions §9, c-1).
    #[test]
    fn test_header_actions_and_placeholder_follow_custom_locale() {
        let mut translations = std::collections::BTreeMap::new();
        for (key, value) in [
            ("tab.repl", "Разраб"),
            ("header.help", "Справка"),
            ("header.no_repository", "нет репозитория"),
        ] {
            translations.insert(key.to_string(), value.to_string());
        }

        let backend = TestBackend::new(160, 3);
        let mut terminal = Terminal::new(backend).expect("terminal created");
        let mut state = test_app_state();
        state.repo_state = None;
        state.i18n = crate::i18n::I18nEngine::new(Some(crate::i18n::CustomLocale::new(
            "Russian",
            translations,
        )));

        draw_header(&mut terminal, &mut state, &current_engine());

        let row = header_row_text(&terminal, HEADER_ROW);
        assert_header_geometry(&state, 160);
        assert!(
            row.contains("Справка"),
            "the help label must be localized: {row}"
        );
        assert!(
            row.contains("нет репозитория"),
            "the repository placeholder must be localized: {row}"
        );
        assert!(
            !row.contains("Help"),
            "english action literals must not render: {row}"
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
    fn test_header_registers_every_control_across_widths() {
        let expected = [
            (UiAction::SwitchView(ViewMode::Repl), "REPL tab"),
            (UiAction::SwitchView(ViewMode::Plans), "Plans tab"),
            (UiAction::SwitchView(ViewMode::Review), "Review tab"),
            (UiAction::SwitchView(ViewMode::Context), "Context tab"),
            (UiAction::SwitchView(ViewMode::History), "History tab"),
            (UiAction::OpenModelSelect, "model selector"),
            (UiAction::ToggleHelp, "Help button"),
        ];

        for &w in &[60u16, 70, 80, 100, 120, 160] {
            let backend = TestBackend::new(w, 3);
            let mut terminal = Terminal::new(backend).expect("terminal created");
            let mut state = test_app_state();

            draw_header(&mut terminal, &mut state, &current_engine());

            for (action, label) in expected {
                let rect = header_region(&state, action)
                    .unwrap_or_else(|| panic!("{label} must be drawn and registered at width {w}"));
                assert!(rect.right() < w, "{label} exceeds width {w} ({rect:?})");
                assert_eq!(
                    state.hit_regions.action_at(rect.x, HEADER_ROW),
                    Some(action),
                    "{label} must resolve at width {w}"
                );
            }

            // Controls keep their drawn left-to-right order.
            let repl = header_region(&state, UiAction::SwitchView(ViewMode::Repl))
                .expect("REPL tab registered");
            let plans = header_region(&state, UiAction::SwitchView(ViewMode::Plans))
                .expect("Plans tab registered");
            assert!(repl.right() <= plans.x, "REPL and Plans overlap");
            let model = header_region(&state, UiAction::OpenModelSelect)
                .expect("model selector registered");
            let help = header_region(&state, UiAction::ToggleHelp).expect("help registered");
            assert!(model.right() <= help.x, "model and help button overlap");

            // Only the header row carries header controls.
            assert_eq!(state.hit_regions.action_at(0, 0), None);
            assert_header_geometry(&state, w);
        }
    }

    #[test]
    fn test_model_selector_label_role_resolves_to_the_concrete_model() {
        let mut state = test_app_state();
        state.model_choice.roles = Some(tauqe_protocol::ModelRolesSummary {
            scribe: ModelRef::openrouter("scribe-model"),
            coder: ModelRef::openrouter("coder-model"),
            architect: ModelRef::openrouter("architect-model"),
            primary: ModelRef::openrouter("primary-model"),
            reviewer: ModelRef::openrouter("reviewer-model"),
            efforts: None,
            entries: None,
        });
        // The role resolves through the catalog published once per frame, so
        // the idle header prints the concrete model behind the role (c-2, c-3).
        crate::model_display::install_snapshot(state.model_choice.roles.as_ref());

        state.model_choice.selection = ModelSelection::Role(ModelRole::Coder);
        assert_eq!(model_selector_label(&state, 20), "coder-model");

        state.model_choice.selection = ModelSelection::Role(ModelRole::Architect);
        assert_eq!(model_selector_label(&state, 20), "architect-model");

        // The semantic role is a selection mechanism: the label prints the
        // concrete model it resolves to, never the role itself (c-2).
        state.model_choice.selection = ModelSelection::Role(ModelRole::Reviewer);
        assert_eq!(model_selector_label(&state, 40), "reviewer-model");
    }

    #[test]
    fn test_model_selector_label_idle_uses_the_primary_entry_label() {
        let mut state = test_app_state();
        let entry =
            tauqe_protocol::ModelEntry::new(ModelRef::openrouter("google/gemini-2.0-flash"), None);
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
        crate::model_display::install_snapshot(state.model_choice.roles.as_ref());

        assert_eq!(model_selector_label(&state, 40), "google/gemini-2.0-flash");
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
        let summary = tauqe_protocol::ModelRolesSummary {
            scribe: ModelRef::openrouter("scribe-model"),
            coder: ModelRef::openrouter("coder-model"),
            architect: ModelRef::openrouter("architect-model"),
            primary: ModelRef::openrouter("primary-model"),
            reviewer: ModelRef::openrouter("reviewer-model"),
            efforts: None,
            entries: Some(tauqe_protocol::ModelRolesEntries {
                catalog: Some(vec![tauqe_protocol::ModelCatalogEntry {
                    model: ModelRef::openrouter("anthropic/claude-3-7-sonnet"),
                    alias: Some("Sonnet".to_string()),
                }]),
                ..Default::default()
            }),
        };
        state.model_choice.roles = Some(summary.clone());
        crate::model_display::install_snapshot(Some(&summary));

        // The live model replaces the idle selection, and its role is never
        // appended to the name (c-2).
        state.running_model = Some(crate::app::RunningModel {
            model: ModelRef::openrouter("anthropic/claude-3-7-sonnet"),
            role: Some(ModelRole::Coder),
            effort: None,
        });
        assert_eq!(model_selector_label(&state, 40), "Sonnet");

        // The catalog alias wins over the provider identifier, and the reasoning
        // depth is appended by the single printing rule.
        state.running_model = Some(crate::app::RunningModel {
            model: ModelRef::openrouter("anthropic/claude-3-7-sonnet"),
            role: Some(ModelRole::Architect),
            effort: Some(tauqe_protocol::ReasoningEffort::High),
        });
        assert_eq!(model_selector_label(&state, 40), "Sonnet [high]");

        // The background Scribe role never replaces the visible model; the idle
        // branch prints the concrete model the selection resolves to.
        state.running_model = Some(crate::app::RunningModel {
            model: ModelRef::openrouter("scribe-model"),
            role: Some(ModelRole::Scribe),
            effort: None,
        });
        assert_eq!(model_selector_label(&state, 40), "primary-model");

        // Once the turn settles the header returns to the developer's selection.
        state.model.status = crate::views::repl::TurnStatus::Done;
        state.running_model = None;
        assert_eq!(model_selector_label(&state, 40), "primary-model");
    }

    #[test]
    fn test_header_renders_with_alt_primary_modifier() {
        // The Help button label follows the configured primary modifier, so the
        // header must still render and register its click targets under `alt`.
        let backend = TestBackend::new(80, 3);
        let mut terminal = Terminal::new(backend).expect("terminal created");
        let mut state = test_app_state();
        state.tui_config.input.primary_modifier = crate::config::PrimaryModifier::Alt;

        draw_header(&mut terminal, &mut state, &current_engine());

        assert!(header_region(&state, UiAction::ToggleHelp).is_some());
        assert!(
            header_row_text(&terminal, HEADER_ROW).contains("M-H"),
            "the help button must follow the Alt primary modifier"
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

            draw_header(&mut terminal, &mut state, &engine);

            let repl = header_region(&state, UiAction::SwitchView(ViewMode::Repl))
                .expect("REPL tab registered");
            let cell = &terminal.backend().buffer()[(repl.x, HEADER_ROW)];
            assert_eq!(cell.bg, engine.color(palette.button_active_bg));
            assert_eq!(cell.fg, engine.color(palette.text_inverse));

            let help = header_region(&state, UiAction::ToggleHelp).expect("help registered");
            let help_cell = &terminal.backend().buffer()[(help.x, HEADER_ROW)];
            assert_eq!(help_cell.bg, engine.color(palette.button_bg));
            assert_eq!(help_cell.fg, engine.color(palette.button_fg));
        }
    }

    /// A tier selection prints the reasoning depth the developer picked instead
    /// of the depth of whichever primary entry happens to share the model, so
    /// the header label is derived from the DTO pair and never from a lookup
    /// that drops the effort (c-2).
    #[test]
    fn test_model_selector_label_tier_renders_the_chosen_effort() {
        let mut state = test_app_state();
        let summary = tauqe_protocol::ModelRolesSummary {
            scribe: ModelRef::openrouter("scribe-model"),
            coder: ModelRef::openrouter("coder-model"),
            architect: ModelRef::openrouter("architect-model"),
            primary: ModelRef::openrouter("google/gemini-2.0-flash"),
            reviewer: ModelRef::openrouter("reviewer-model"),
            efforts: None,
            entries: Some(tauqe_protocol::ModelRolesEntries {
                catalog: Some(vec![tauqe_protocol::ModelCatalogEntry {
                    model: ModelRef::openrouter("google/gemini-2.0-flash"),
                    alias: Some("Gemini Flash".to_string()),
                }]),
                ..Default::default()
            }),
        };
        state.model_choice = crate::app::ModelChoice {
            selection: ModelSelection::Tier {
                model: ModelRef::openrouter("google/gemini-2.0-flash"),
                effort: Some(tauqe_protocol::ReasoningEffort::Max),
            },
            roles: Some(summary.clone()),
        };
        crate::model_display::install_snapshot(Some(&summary));

        assert_eq!(model_selector_label(&state, 40), "Gemini Flash [max]");

        // The same model at another depth prints that depth, which proves the
        // label follows the effort carried by the selection itself.
        state.model_choice.selection = ModelSelection::Tier {
            model: ModelRef::openrouter("google/gemini-2.0-flash"),
            effort: Some(tauqe_protocol::ReasoningEffort::Low),
        };
        assert_eq!(model_selector_label(&state, 40), "Gemini Flash [low]");
    }

    /// The drawn header must carry the tier depth, not only the model name: the
    /// depth is the coordinate the developer confirmed in the picker (c-2).
    #[test]
    fn test_header_renders_the_selected_tier_depth() {
        let backend = TestBackend::new(160, 3);
        let mut terminal = Terminal::new(backend).expect("terminal created");
        let mut state = test_app_state();
        let summary = tauqe_protocol::ModelRolesSummary {
            scribe: ModelRef::openrouter("scribe-model"),
            coder: ModelRef::openrouter("coder-model"),
            architect: ModelRef::openrouter("architect-model"),
            primary: ModelRef::openrouter("google/gemini-2.0-flash"),
            reviewer: ModelRef::openrouter("reviewer-model"),
            efforts: None,
            entries: None,
        };
        state.model_choice = crate::app::ModelChoice {
            selection: ModelSelection::Tier {
                model: ModelRef::openrouter("google/gemini-2.0-flash"),
                effort: Some(tauqe_protocol::ReasoningEffort::High),
            },
            roles: Some(summary),
        };

        draw_header(&mut terminal, &mut state, &current_engine());

        let row = header_row_text(&terminal, HEADER_ROW);
        assert!(
            row.contains("google/gemini-2.0-flash [high]"),
            "the header must print the selected tier depth: {row}"
        );
        assert_header_geometry(&state, 160);
    }
}
