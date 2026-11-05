use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{ActiveModal, AppState, ViewMode};
use crate::i18n::active_t;
use crate::theme::engine::current_engine;

pub mod footer;
pub mod format;
pub mod geometry;
pub mod header;
pub mod table;
pub mod wrap;

use crate::dialogs::render::{
    render_confirm_batch_cancel_popup, render_confirm_cancel_popup,
    render_confirm_clear_auto_popup, render_confirm_clear_history_popup,
    render_confirm_clear_user_popup, render_confirm_delete_plan_popup,
    render_confirm_delete_review_popup, render_confirm_detach_focus_popup,
    render_confirm_execute_pipeline_popup, render_confirm_execute_review_popup,
    render_confirm_execute_scope_popup, render_confirm_outer_docs_cache_remove_popup,
    render_confirm_quit_popup, render_confirm_remove_context_file_popup,
    render_confirm_review_analyze_popup, render_confirm_review_truncation_popup,
    render_diff_inspector, render_disconnected_popup, render_help_popup, render_locale_dialog,
    render_outer_docs_cache_dialog, render_plan_create_dialog, render_plan_refine_blocked_dialog,
    render_plan_refine_dialog, render_prompt_modal, render_review_dialog, render_selection_dialog,
    render_squash_popup, render_status_dialog, render_usage_summary,
};
use crate::views::context::render_context_view;
use crate::views::history::render_history_view;
use crate::views::onboarding::render_onboarding_view;
use crate::views::plans::render_plans_view;
use crate::views::repl::render::render_repl_view;
use crate::views::review::render_review_view;

pub use footer::render_footer;
pub use geometry::centered_rect;
pub use header::render_header;
pub use wrap::{wrap_line, wrap_lines};

const MIN_TERM_WIDTH: u16 = 60;
const MIN_TERM_HEIGHT: u16 = 12;
/// On terminals shorter than this the footer and the non-REPL info strip
/// are dropped so the main area keeps enough rows to stay usable.
const COMPACT_TERM_HEIGHT: u16 = 20;

/// A typed intention the developer expresses by clicking an interactive region.
///
/// A component declares the rectangle it has just drawn together with one of
/// these actions, so adding, moving, or removing an interactive element touches
/// only the component that renders it and never the input dispatcher (c-1).
///
/// An action may carry owned data — the target of a hyperlink is the address the
/// renderer painted — so the registry clones a declaration instead of copying it
/// (c-3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiAction {
    /// Switch the active tab to another view mode.
    SwitchView(ViewMode),
    /// Open the primary model picker.
    OpenModelSelect,
    /// Toggle the Help modal.
    ToggleHelp,
    /// Copy the source of a rendered code block.
    CopyCodeBlock {
        /// History item owning the block; `None` for the Develop response.
        item: Option<u64>,
        /// Index of the block inside its owning surface.
        block: usize,
    },
    /// A click landed inside a rendered code block, outside its copy button.
    CodeBlockBody,
    /// Open a Markdown hyperlink in the developer's browser.
    ///
    /// The target travels as the address the renderer painted it with, so the
    /// pointer dispatcher never re-derives it from a row, a column, or a scroll
    /// offset (c-3).
    OpenUrl(String),
}

/// One interactive rectangle declared by the component that draws it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HitRegion {
    /// Screen rectangle that receives the pointer.
    pub rect: Rect,
    /// Action dispatched when the rectangle is clicked.
    pub action: UiAction,
}

/// Frame-scoped registry of the interactive regions declared while drawing.
///
/// The registry lives strictly inside one frame: [`render_ui`] empties it before
/// the components declare the new regions, so a hidden or covered surface
/// physically cannot leave a stale clickable area behind (c-3).
#[derive(Debug, Clone, Default)]
pub struct HitRegistry {
    regions: Vec<HitRegion>,
}

impl HitRegistry {
    /// Releases every region of the finished frame.
    pub fn clear(&mut self) {
        self.regions.clear();
    }

    /// Records a rectangle a component has just drawn, bound to its action.
    pub fn register(&mut self, rect: Rect, action: UiAction) {
        self.regions.push(HitRegion { rect, action });
    }

    /// Resolves a pointer position to the action of the top-most region.
    ///
    /// Regions are consulted in reverse registration order, so a surface drawn
    /// later — a dialog over a view — wins over whatever it covers.
    ///
    /// The declaration is cloned rather than copied because a hyperlink target is
    /// owned text, so the registry hands back exactly what it was given (c-3).
    pub fn action_at(&self, column: u16, row: u16) -> Option<UiAction> {
        self.regions
            .iter()
            .rev()
            .find(|region| crate::input::mouse::rect_contains(region.rect, column, row))
            .map(|region| region.action.clone())
    }

    /// Regions declared by the components of the current frame, in draw order.
    #[cfg(test)]
    pub fn regions(&self) -> &[HitRegion] {
        &self.regions
    }
}

/// Solid background filler that unconditionally sets symbol and background color
/// for every grid cell, preventing unstyled gaps and terminal default background leakage.
struct CanvasBackground(ratatui::style::Color);

impl ratatui::widgets::Widget for CanvasBackground {
    fn render(self, area: Rect, buf: &mut ratatui::buffer::Buffer) {
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                let cell = &mut buf[(x, y)];
                cell.set_symbol(" ");
                cell.set_bg(self.0);
            }
        }
    }
}

fn render_terminal_too_small(frame: &mut ratatui::Frame) {
    let engine = current_engine();
    let area = frame.area();
    let current_size = active_t("ui.terminal_current")
        .replacen("{}", &area.width.to_string(), 1)
        .replacen("{}", &area.height.to_string(), 1);
    let required_size = active_t("ui.terminal_required")
        .replacen("{}", &MIN_TERM_WIDTH.to_string(), 1)
        .replacen("{}", &MIN_TERM_HEIGHT.to_string(), 1);
    let lines = vec![
        Line::from(Span::styled(
            active_t("ui.terminal_too_small"),
            engine.fg(engine.palette().status_warning).bold(),
        )),
        Line::from(current_size),
        Line::from(required_size),
    ];
    let height = area.height.min(3);
    let rect = Rect::new(
        area.x,
        area.y + area.height.saturating_sub(height) / 2,
        area.width,
        height,
    );
    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), rect);
}

/// Height the prompt pane occupies for a wrapped draft of `line_count` visual
/// lines: the two border rows plus the draft, clamped to the terminal budget.
///
/// The wrapped count is narrowed through the saturating conversion the crate
/// uses for every line count, so a draft long enough to overflow the `u16`
/// viewport unit can neither wrap around in release nor panic under the debug
/// overflow checks, and the clamp below bounds the result either way (c-2).
fn prompt_pane_height(line_count: usize, max_input_height: u16) -> u16 {
    let lines = u16::try_from(line_count).unwrap_or(u16::MAX);
    lines.saturating_add(2).max(3).min(max_input_height)
}

pub fn render_ui(frame: &mut ratatui::Frame, state: &mut AppState) {
    let area = frame.area();
    state.last_frame_area = area;
    // The registry lives strictly inside one frame: every interactive region is
    // declared by the component that draws it, and a stale region of a hidden or
    // resized surface can never survive into the next frame (c-3).
    state.hit_regions.clear();
    // Publish the provider catalog and the resolved role entries the single
    // model-name printer reads. The snapshot is fully determined by the
    // mirrored model choice, so the frame republishes it only once that choice
    // has advanced: a streaming redraw between two mutations reuses the
    // published labels instead of re-walking the catalog and taking the
    // projection write lock on every frame (c-1, c-3).
    state.publish_model_display();
    if area.width < MIN_TERM_WIDTH || area.height < MIN_TERM_HEIGHT {
        render_terminal_too_small(frame);
        return;
    }
    let engine = current_engine();
    let palette = engine.palette();

    // Synchronize host terminal window margins with the theme canvas.
    crate::terminal::update_terminal_background(palette.canvas);

    // Paint the theme canvas across the entire grid buffer so every cell is initialized
    // with the active palette background, eliminating terminal color bars.
    frame.render_widget(CanvasBackground(engine.color(palette.canvas)), area);

    let term_height = area.height;
    let term_width = area.width;
    let compact = term_height < COMPACT_TERM_HEIGHT;

    // The bottom area is the prompt editor in REPL and a fixed telemetry
    // strip elsewhere. Only REPL sizes it from the prompt draft, so a draft
    // never inflates an unrelated view's info row (Conventions.md §2).
    let is_busy = state.model.is_busy();
    let (prompt_lines, cursor_visual_line, bottom_height) = if state.view_mode == ViewMode::Repl {
        let max_input_height = (term_height * 4 / 10).clamp(6, 16);
        let inner_input_width = (term_width.saturating_sub(2)) as usize;
        let (lines, cursor) = state
            .input_editor
            .build_prompt_lines(inner_input_width, is_busy);
        let height = prompt_pane_height(lines.len(), max_input_height);
        (lines, cursor, height)
    } else if compact {
        (Vec::new(), 0, 0)
    } else {
        (Vec::new(), 0, 3)
    };

    // Degrade gracefully on short terminals instead of refusing to render.
    let footer_height = if compact { 0 } else { 3 };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(bottom_height),
            Constraint::Length(footer_height),
        ])
        .split(area);

    render_header(frame, state, chunks[0]);

    match state.view_mode {
        ViewMode::Onboarding => {
            render_onboarding_view(frame, state, chunks[1], chunks[2], chunks[3]);
        }
        ViewMode::Repl => {
            render_repl_view(
                frame,
                state,
                chunks[1],
                chunks[2],
                prompt_lines,
                cursor_visual_line,
            );
        }
        ViewMode::Context => {
            render_context_view(frame, state, chunks[1], chunks[2]);
        }
        ViewMode::History => {
            render_history_view(frame, state, chunks[1], chunks[2]);
        }
        ViewMode::Review => {
            render_review_view(frame, state, chunks[1], chunks[2]);
        }
        ViewMode::Plans => {
            render_plans_view(frame, state, chunks[1], chunks[2]);
        }
    }

    if state.view_mode != ViewMode::Onboarding {
        render_footer(frame, state, chunks[3]);
    }

    // Render exactly the modal the canonical ordering reports as top-most, so the
    // visible window is always the window that receives input.
    match state.active_modal() {
        Some(ActiveModal::ServerDisconnected) => {
            if let Some(msg) = &state.modals.server_disconnected {
                render_disconnected_popup(frame, msg);
            }
        }
        Some(ActiveModal::Squash) => {
            if let Some(dialog) = &mut state.modals.squash_dialog {
                render_squash_popup(frame, dialog);
            }
        }
        Some(ActiveModal::Review) => {
            if let Some(dialog) = &state.modals.review_dialog {
                render_review_dialog(frame, dialog, state.tui_config.input.primary_modifier);
            }
        }
        Some(ActiveModal::ConfirmCancel) => render_confirm_cancel_popup(frame, state),
        Some(ActiveModal::ConfirmQuit) => render_confirm_quit_popup(frame, state),
        Some(ActiveModal::BatchCancel) => render_confirm_batch_cancel_popup(frame, state),
        Some(ActiveModal::ClearAuto) => render_confirm_clear_auto_popup(frame, state),
        Some(ActiveModal::ClearUser) => render_confirm_clear_user_popup(frame, state),
        Some(ActiveModal::RemoveContextFile) => {
            render_confirm_remove_context_file_popup(frame, state);
        }
        Some(ActiveModal::OuterDocsCacheRemove) => {
            render_confirm_outer_docs_cache_remove_popup(frame, state);
        }
        Some(ActiveModal::OuterDocsCache) => {
            if let Some(dialog) = state.modals.outer_docs_cache.as_mut() {
                render_outer_docs_cache_dialog(frame, dialog);
            }
        }
        Some(ActiveModal::ConfirmDetachFocus) => render_confirm_detach_focus_popup(frame, state),
        Some(ActiveModal::ClearHorizon) => {
            crate::dialogs::render::render_confirm_clear_horizon_popup(frame, state);
        }
        Some(ActiveModal::ClearHorizonFinal) => {
            crate::dialogs::render::render_confirm_clear_horizon_final_popup(frame, state);
        }
        Some(ActiveModal::DeletePlan) => {
            render_confirm_delete_plan_popup(frame, state);
        }
        Some(ActiveModal::DeletePlanFinal) => {
            crate::dialogs::render::render_confirm_delete_plan_final_popup(frame, state);
        }
        Some(ActiveModal::DeleteReview) => {
            render_confirm_delete_review_popup(frame, state);
        }
        Some(ActiveModal::ReviewAnalyze) => render_confirm_review_analyze_popup(frame, state),
        Some(ActiveModal::ReviewTruncation) => render_confirm_review_truncation_popup(frame, state),
        Some(ActiveModal::ExecuteReview) => render_confirm_execute_review_popup(frame, state),
        Some(ActiveModal::ExecutePipeline) => render_confirm_execute_pipeline_popup(frame, state),
        Some(ActiveModal::ExecuteScope) => render_confirm_execute_scope_popup(frame, state),
        Some(ActiveModal::CreatePlan) => {
            if let Some(dialog) = &state.modals.create_dialog {
                render_plan_create_dialog(frame, dialog, state.tui_config.input.primary_modifier);
            }
        }
        Some(ActiveModal::RefineDialog) => {
            if let Some(dialog) = &state.modals.refine_dialog {
                render_plan_refine_dialog(frame, dialog, state.tui_config.input.primary_modifier);
            }
        }
        Some(ActiveModal::RefineBlocked) => {
            if let Some(blocked) = &state.modals.refine_blocked {
                render_plan_refine_blocked_dialog(frame, blocked, state.modals.confirm_button);
            }
        }
        Some(ActiveModal::Prompt) => {
            if let Some(modal) = &state.modals.prompt_modal {
                render_prompt_modal(frame, modal);
            }
        }
        Some(ActiveModal::Locale) => {
            render_locale_dialog(frame, state);
        }
        Some(ActiveModal::ClearHistory) => render_confirm_clear_history_popup(frame, state),
        Some(ActiveModal::Status) => {
            if let Some(dialog) = &state.modals.status_dialog {
                render_status_dialog(frame, dialog);
            }
        }
        Some(ActiveModal::Selection) => {
            if let Some(dialog) = &state.modals.selection_dialog {
                render_selection_dialog(frame, dialog);
            }
        }
        // The inspector is a live projection of the change set: every
        // edit-stream event refreshes it, and it paints the rows through the same
        // diff cache the REPL viewport already filled, so the audit and the
        // REPL can never disagree about a diff body (c-1, c-3).
        Some(ActiveModal::DiffInspector) => {
            // The animation frame is read before the inspector borrows the shared
            // state, so the file the stream currently writes carries the same
            // spinner here as it does in the Viewport (c-3).
            let spinner_frame = state.model.spinner_frame;
            // A plan step audits the finished change set of its commit, while the
            // Develop viewport audits the live staged set it owns. The two
            // borrows are disjoint fields of the shared state (c-3).
            let (audited, cache) = match state.modals.diff_audit.as_mut() {
                Some(audit) => (audit.files.as_slice(), &mut audit.cache),
                None => (state.model.files.as_slice(), &mut state.model.diff_cache),
            };
            if let Some(inspector) = state.modals.diff_inspector.as_mut() {
                render_diff_inspector(frame, inspector, audited, cache, spinner_frame);
            }
        }
        Some(ActiveModal::ThemeSelector) => {
            if let Some(dialog) = &state.modals.theme_selector {
                crate::theme::selector::render_theme_selector(frame, dialog);
            }
        }
        Some(ActiveModal::UsageSummary) => {
            if let Some(summary) = state.modals.usage_summary.as_mut() {
                render_usage_summary(
                    frame,
                    &state.model.usage_totals,
                    state.model.repomap_tokens,
                    &mut summary.scroll,
                );
            }
        }
        Some(ActiveModal::Help) => {
            render_help_popup(
                frame,
                state.view_mode,
                state.modals.help_tab,
                &mut state.modals.help_scroll,
                state.tui_config.input.primary_modifier,
                state.tui_config.input.meta_modifier,
            );
        }
        None => {}
    }
}

#[cfg(test)]
mod hit_tests {
    use super::*;

    #[test]
    fn later_regions_win_over_the_surfaces_they_cover() {
        let mut registry = HitRegistry::default();
        registry.register(
            Rect::new(0, 0, 10, 2),
            UiAction::SwitchView(ViewMode::Plans),
        );
        registry.register(Rect::new(4, 0, 3, 2), UiAction::ToggleHelp);

        assert_eq!(registry.action_at(5, 1), Some(UiAction::ToggleHelp));
        assert_eq!(
            registry.action_at(1, 1),
            Some(UiAction::SwitchView(ViewMode::Plans))
        );
        // The right and bottom edges are exclusive, and empty space resolves to
        // no action at all instead of the nearest region.
        assert_eq!(registry.action_at(10, 1), None);
        assert_eq!(registry.action_at(20, 1), None);
    }

    #[test]
    fn clearing_the_registry_drops_every_region_of_the_finished_frame() {
        let mut registry = HitRegistry::default();
        registry.register(Rect::new(0, 0, 10, 2), UiAction::OpenModelSelect);
        assert_eq!(registry.action_at(0, 0), Some(UiAction::OpenModelSelect));

        registry.clear();
        assert_eq!(registry.action_at(0, 0), None);
    }

    #[test]
    fn code_block_action_separates_history_blocks_from_the_response() {
        let mut registry = HitRegistry::default();
        registry.register(
            Rect::new(2, 5, 8, 1),
            UiAction::CopyCodeBlock {
                item: Some(7),
                block: 3,
            },
        );

        assert_eq!(
            registry.action_at(3, 5),
            Some(UiAction::CopyCodeBlock {
                item: Some(7),
                block: 3,
            })
        );
    }
    #[test]
    fn a_link_region_resolves_to_the_address_it_opens() {
        let mut registry = HitRegistry::default();
        registry.register(
            Rect::new(4, 6, 12, 1),
            UiAction::OpenUrl("https://example.com".to_string()),
        );

        assert_eq!(
            registry.action_at(5, 6),
            Some(UiAction::OpenUrl("https://example.com".to_string()))
        );
        // The right edge is exclusive and empty space resolves to no action, so
        // a link never swallows a click that belongs to its neighbour.
        assert_eq!(registry.action_at(16, 6), None);
        assert_eq!(registry.action_at(20, 6), None);
    }
}

#[cfg(test)]
mod prompt_height_tests {
    use super::*;

    /// A draft whose wrapped visual lines reach the top of the `u16` range
    /// would overflow the border-row addition if the count were narrowed first,
    /// and a longer one would truncate into an arbitrary height. Both stay
    /// inside the terminal budget instead (c-2).
    #[test]
    fn prompt_pane_height_saturates_instead_of_wrapping_the_wrapped_line_count() {
        assert_eq!(prompt_pane_height(usize::from(u16::MAX) - 1, 16), 16);
        assert_eq!(prompt_pane_height(usize::from(u16::MAX), 16), 16);
        assert_eq!(prompt_pane_height(usize::MAX, 16), 16);

        // A short draft keeps its natural height: two border rows plus four
        // lines, and an empty draft never falls below the three-row minimum.
        assert_eq!(prompt_pane_height(4, 16), 6);
        assert_eq!(prompt_pane_height(0, 16), 3);
        assert_eq!(prompt_pane_height(20, 16), 16);
    }
}
