//! The modal-surface subsystem.
//!
//! A modal surface is any dialog, popup, or overlay that owns the keyboard while
//! it is open. This module declares every surface of the interface, and that
//! declaration order is the single source of truth for their stacking priority
//! (Conventions §7). The state a surface keeps while it is open lives beside that
//! declaration in [`ModalState`], so a new dialog extends this subsystem instead
//! of scattering one field across the application state.

use strum::EnumIter;

use crate::theme::selector::ThemeSelectorState;
use crate::views::plans::{PlanCreateDialogState, PlanRefineBlockedState, PlanRefineDialogState};

use super::{BatchCancelButton, CommitAudit, ConfirmDialogButton, DiffInspectorState};
use super::{
    ConfirmExecutePipelineState, ConfirmExecuteReviewState, ConfirmExecuteScopeState,
    ConfirmReviewAnalyzeState, ConfirmReviewTruncationState, HelpTab, LocaleDialogState,
    OuterDocsCacheDialogState, PendingReviewLaunch, PromptModalState, ReviewDialogState,
    SelectionDialogState, SquashDialogState, StatusDialogState, UsageSummaryState,
};

/// State of the overlay surfaces that own the keyboard while they are open.
///
/// Every overlay [`ActiveModal`] ranks keeps its fields here instead of
/// scattering them across [`AppState`](crate::app::AppState): this module
/// already owns the ranking of the modal stack, so a surface and the state it
/// keeps are declared in one subsystem (Conventions §7).
#[derive(Debug, Clone, Default)]
pub struct ModalState {
    /// Fatal transport failure banner: the daemon stopped answering, so the
    /// interface stays read-only until the developer quits.
    pub server_disconnected: Option<String>,
    /// Squash-commits dialog: base selection, commit message, and confirmation.
    pub squash_dialog: Option<SquashDialogState>,
    /// Whether the keybinding Help dialog is open.
    pub show_help: bool,
    /// Vertical scroll offset of the Help dialog body.
    pub help_scroll: u16,
    /// Tab the Help dialog currently presents.
    pub help_tab: HelpTab,
    /// Interactive theme preset selector, when open.
    pub theme_selector: Option<ThemeSelectorState>,
    /// Full usage telemetry breakdown dialog, when open.
    pub usage_summary: Option<UsageSummaryState>,
    /// Unified discussion and group-resolution prompt, when open.
    pub prompt_modal: Option<PromptModalState>,
    /// Interface-language (locale) dialog, when open.
    pub locale_dialog: Option<LocaleDialogState>,
    /// Status picker over a plan step or a review finding, when open.
    pub status_dialog: Option<StatusDialogState>,
    /// Primary model picker over the configured primary tiers, when open.
    pub selection_dialog: Option<SelectionDialogState>,
    /// Hierarchical browser over the durable external documentation cache.
    pub outer_docs_cache: Option<OuterDocsCacheDialogState>,
    /// Logical cache path awaiting the removal confirmation, if any.
    pub confirm_outer_docs_cache_remove: Option<String>,
    /// Fullscreen two-pane audit of the change set the REPL viewport stages.
    pub diff_inspector: Option<DiffInspectorState>,
    /// Finished change set a plan step commit audits, when the inspector was
    /// opened from the Plans tree instead of the Develop viewport (c-3).
    pub diff_audit: Option<CommitAudit>,
    /// Plan refinement form, when open.
    ///
    /// The form belongs to the modal stack, not to the Plans view: it owns the
    /// keyboard while it is open, so it is declared here beside the surface
    /// that ranks it (Conventions §7). Only the turn markers of the view
    /// (`refining` / `refining_plan_id`) stay on the view state.
    pub refine_dialog: Option<PlanRefineDialogState>,
    /// Plan creation form, when open.
    ///
    /// The form belongs to the modal stack, not to the Plans view: it owns the
    /// keyboard while it is open, so it is declared here beside the surface
    /// that ranks it (Conventions §7). Only the turn markers of the view
    /// (`refining` / `refining_plan_id`) stay on the view state
    /// (c-typed-plan-creation-dialog).
    pub create_dialog: Option<PlanCreateDialogState>,
    /// Blocker report of a refinement the model cannot apply, when open.
    pub refine_blocked: Option<PlanRefineBlockedState>,
    /// Whether the Auto-layer clear confirmation is open.
    ///
    /// The card is a surface that owns the keyboard while it is open rather than
    /// browsing state of the Context view, so arming it means
    /// [`ActiveModal::ClearAuto`] outranks the view that raised it
    /// (Conventions §5.4).
    pub confirm_clear_auto: bool,
    /// Whether the User-layer clear confirmation is open.
    pub confirm_clear_user: bool,
    /// Logical path whose removal from a context layer awaits the shared
    /// confirmation card.
    ///
    /// While it is set, `AppState::active_modal` reports
    /// [`ActiveModal::RemoveContextFile`], so the `context/remove` request leaves
    /// the client only after the developer accepts the card and never on the
    /// bare keystroke (Conventions §4.3, §5.4).
    pub confirm_remove_context_file: Option<String>,
    /// Plan whose deletion awaits the first-tier confirmation.
    ///
    /// It outranks the plan tree that raised it, so the decision owns the
    /// keyboard until the developer answers it (Conventions §5.4).
    pub confirm_delete_plan: Option<String>,
    /// Plan whose deletion escalated to the second tier because unfinished work
    /// would be discarded.
    ///
    /// It outranks the first tier it escalates from, so the riskier
    /// confirmation always owns the keyboard (c-1).
    pub confirm_delete_plan_final: Option<String>,
    /// Review session whose deletion awaits the shared confirmation card.
    pub confirm_delete_review: Option<String>,
    /// Plan whose implementation horizon awaits the first-tier clear
    /// confirmation.
    pub confirm_clear_horizon: Option<String>,
    /// Plan whose horizon clear escalated to the second tier because work had
    /// already been recorded in Git.
    ///
    /// It outranks the first tier it escalates from, exactly as the deletion
    /// pair does (c-1).
    pub confirm_clear_horizon_final: Option<String>,
    /// Plan step or scope execution confirmation, when open.
    ///
    /// Executing a step or a whole scope hands control to the Coder role, so the
    /// dialog carries the configured Coder ladder and the action buttons that
    /// own the keyboard while it is open, and the plan stays the single source
    /// of truth for each step's instructions (c-1, c-4).
    pub confirm_execute_scope: Option<ConfirmExecuteScopeState>,
    /// Multi-plan execution pipeline confirmation, when open.
    ///
    /// A pipeline is a super-batch over several plans running under one
    /// registered operation, so the confirmation that launches it is a surface
    /// of the modal stack rather than browsing state of the Plans view (c-1,
    /// c-5).
    pub confirm_execute_pipeline: Option<ConfirmExecutePipelineState>,
    /// Review-execution confirmation, when open.
    ///
    /// One surface carries both shapes the confirmation takes: the binary card
    /// of a session queue, the cross-session batch, and the unified plan of an
    /// architectural group, and the point form of one finding or one group step
    /// with its Coder selector and directives editor. Keeping both in one field
    /// is what lets the capture probe answer "does the focused surface hold an
    /// editor?" from a single place (c-1, c-4, c-5).
    pub confirm_execute_review: Option<ConfirmExecuteReviewState>,
    /// Confirmation of the costly Architect post-analysis over a review session.
    ///
    /// The classification rewrites the whole post-analysis layer of the session,
    /// so the decision is a surface of the modal stack and keeps the safe
    /// `Cancel` default until the developer asks for it (c-1, c-4).
    pub confirm_review_analyze: Option<ConfirmReviewAnalyzeState>,
    /// Review launch awaiting its audit-window projection, when one is in flight.
    ///
    /// Confirming the launch dialog consumes it, so the dialog and the typed
    /// launch parameters wait here until the projection answers: a refused
    /// projection or a declined truncation returns these exact fields, and an
    /// approved window starts exactly these parameters (c-3).
    pub pending_review_launch: Option<PendingReviewLaunch>,
    /// Projection awaiting the developer's consent to a truncated audit window.
    ///
    /// It outranks the launch dialog it returns to, so a pending decision is
    /// never hidden behind a reopened dialog (c-3).
    pub confirm_review_truncation: Option<ConfirmReviewTruncationState>,
    /// Review launch dialog, when open.
    ///
    /// The dialog owns the keyboard while it is open and exposes the reviewer,
    /// preset, target-path, and instructions sections (Conventions §6, §7).
    pub review_dialog: Option<ReviewDialogState>,
    /// Whether the confirmation that interrupts the running turn is open.
    pub confirm_cancel: bool,
    /// Whether the quit confirmation is open.
    pub confirm_quit: bool,
    /// Whether the three-way batch-cancellation dialog is open.
    pub confirm_batch_cancel: bool,
    /// Action button currently focused in the batch-cancellation dialog.
    pub batch_cancel_button: BatchCancelButton,
    /// Whether a running batch has been asked to stop after its current member.
    pub batch_stopping: bool,
    /// Whether the discussion-focus detach confirmation is open.
    pub confirm_detach_focus: bool,
    /// Whether the conversation-history clear confirmation is open.
    pub confirm_clear_history: bool,
    /// Focused button of the confirmation card a surface currently shows.
    ///
    /// The button is presentation state of the answer the developer is giving,
    /// not of the entity the card names, so every confirmation dialog reads and
    /// resets this one value instead of keeping a copy of its own. It always
    /// starts, and returns, on the safe `Cancel` default (Conventions §5.4).
    pub confirm_button: ConfirmDialogButton,
}

/// The single source of truth for modal stacking order.
///
/// Every surface that must decide *which* modal is on top — keyboard dispatch,
/// rendering, mouse hit-testing, and paste routing — derives its decision from
/// [`AppState::active_modal`](crate::app::AppState::active_modal). The variant
/// order encodes priority: the first open variant wins. Centralizing the list
/// here guarantees that the window a developer *sees* is always the window that
/// receives keystrokes, mouse events, and pasted text (Conventions §7).
///
/// The declaration order *is* the priority. `AppState::modal_is_open` binds each
/// variant to the state probe that observes its surface, and `active_modal`
/// walks the variants in exactly this order through the derived
/// [`strum::IntoEnumIterator`] implementation, so a surface ranks by where it is
/// declared rather than by an arm appended to a scan method (c-1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumIter)]
pub enum ActiveModal {
    /// Fatal transport state; blocks every other surface.
    ServerDisconnected,
    Squash,
    /// Consent to launch a review whose audited window the configured budgets
    /// truncate. It outranks the launch dialog it returns to, so a pending
    /// decision is never hidden behind a reopened dialog (c-3).
    ReviewTruncation,
    Review,
    ConfirmCancel,
    ConfirmQuit,
    BatchCancel,
    ClearAuto,
    ClearUser,
    /// A Context file whose removal awaits the shared confirmation card.
    RemoveContextFile,
    /// One cache removal the browser requested, awaiting the shared card.
    ///
    /// It outranks the browser that raised it, so a pending decision always owns
    /// the keyboard (Conventions §5.4).
    OuterDocsCacheRemove,
    /// Hierarchical browser over the materialized external documentation cache.
    OuterDocsCache,
    ConfirmDetachFocus,
    /// Second tier, raised when completed or in-progress work exists.
    ///
    /// It outranks the first tier it escalates from, so the riskier
    /// confirmation always owns the keyboard (c-1).
    ClearHorizonFinal,
    /// First tier of the implementation-horizon clear confirmation.
    ClearHorizon,
    /// Second tier, raised when the plan still carries unfinished work.
    ///
    /// It outranks the first tier it escalates from, so a deletion that would
    /// discard recorded work always owns the keyboard (c-1).
    DeletePlanFinal,
    DeletePlan,
    DeleteReview,
    /// Confirmation of the expensive Architect post-analysis over a session.
    ReviewAnalyze,
    ExecuteReview,
    ExecutePipeline,
    ExecuteScope,
    /// Two-phase plan creation form.
    ///
    /// It owns the keyboard while it is open, and its Planner ladder, Critic
    /// ladder, directives editor, and action bar are the sections it cycles
    /// through (c-typed-plan-creation-dialog).
    CreatePlan,
    RefineDialog,
    RefineBlocked,
    Prompt,
    Locale,
    ClearHistory,
    Status,
    Selection,
    /// Fullscreen two-pane audit of the change set staged in the REPL
    /// viewport: a read-only reading surface that outranks the informational
    /// overlays below it (c-3).
    DiffInspector,
    ThemeSelector,
    /// Full usage telemetry breakdown for the session and the operations.
    UsageSummary,
    Help,
}

#[cfg(test)]
mod tests {
    use super::{ActiveModal, ConfirmDialogButton, HelpTab, ModalState};

    /// The shared state starts closed and keeps the canonical Help tab, so a
    /// surface that seeds nothing opens exactly where the standalone fields put
    /// it before the subsystem existed (Conventions §3).
    #[test]
    fn test_modal_state_defaults_close_every_surface() {
        let state = ModalState::default();
        assert!(!state.show_help);
        assert!(state.server_disconnected.is_none());
        assert!(state.squash_dialog.is_none());
        assert_eq!(state.help_scroll, 0);
        assert_eq!(state.help_tab, HelpTab::Global);
        assert!(state.theme_selector.is_none());
        assert!(state.usage_summary.is_none());
        assert!(state.prompt_modal.is_none());
        assert!(state.locale_dialog.is_none());
        assert!(state.status_dialog.is_none());
        assert!(state.selection_dialog.is_none());
        assert!(state.outer_docs_cache.is_none());
        assert!(state.confirm_outer_docs_cache_remove.is_none());
        assert!(state.diff_inspector.is_none());
        assert!(state.diff_audit.is_none());
        assert!(state.create_dialog.is_none());
        assert!(state.refine_dialog.is_none());
        assert!(state.refine_blocked.is_none());
        assert!(!state.confirm_clear_auto);
        assert!(!state.confirm_clear_user);
        assert!(state.confirm_remove_context_file.is_none());
        assert!(state.confirm_delete_plan.is_none());
        assert!(state.confirm_delete_plan_final.is_none());
        assert!(state.confirm_delete_review.is_none());
        assert!(state.confirm_clear_horizon.is_none());
        assert!(state.confirm_clear_horizon_final.is_none());
        assert!(state.confirm_execute_scope.is_none());
        assert!(state.confirm_execute_pipeline.is_none());
        assert!(state.confirm_execute_review.is_none());
        assert!(state.confirm_review_analyze.is_none());
        assert!(state.pending_review_launch.is_none());
        assert!(state.confirm_review_truncation.is_none());
        assert!(state.review_dialog.is_none());
        assert!(!state.confirm_cancel);
        assert!(!state.confirm_quit);
        assert!(!state.confirm_batch_cancel);
        assert_eq!(
            state.batch_cancel_button,
            crate::app::BatchCancelButton::Continue
        );
        assert!(!state.batch_stopping);
        assert!(!state.confirm_detach_focus);
        assert!(!state.confirm_clear_history);
        assert_eq!(state.confirm_button, ConfirmDialogButton::Cancel);
    }

    /// The declaration order of [`ActiveModal`] is the modal stacking priority,
    /// because `AppState::active_modal` walks exactly this sequence. The whole
    /// order is pinned here, so a variant inserted in the wrong place cannot
    /// silently change which surface owns the keyboard (c-1).
    #[test]
    fn test_active_modal_declaration_order_is_the_stacking_priority() {
        use strum::IntoEnumIterator;

        let observed: Vec<ActiveModal> = ActiveModal::iter().collect();

        // The fatal transport state outranks every other surface and Help is
        // the lowest one, so the stack opens and closes exactly there.
        assert_eq!(observed.first(), Some(&ActiveModal::ServerDisconnected));
        assert_eq!(observed.last(), Some(&ActiveModal::Help));

        // A destructive confirmation escalates to a stricter tier, so the
        // escalated variant must outrank the first tier it replaces (c-1).
        let rank = |modal: ActiveModal| {
            observed
                .iter()
                .position(|entry| *entry == modal)
                .expect("every declared surface is iterated")
        };
        assert!(rank(ActiveModal::ClearHorizonFinal) < rank(ActiveModal::ClearHorizon));
        assert!(rank(ActiveModal::DeletePlanFinal) < rank(ActiveModal::DeletePlan));

        let expected = [
            ActiveModal::ServerDisconnected,
            ActiveModal::Squash,
            ActiveModal::ReviewTruncation,
            ActiveModal::Review,
            ActiveModal::ConfirmCancel,
            ActiveModal::ConfirmQuit,
            ActiveModal::BatchCancel,
            ActiveModal::ClearAuto,
            ActiveModal::ClearUser,
            ActiveModal::RemoveContextFile,
            ActiveModal::OuterDocsCacheRemove,
            ActiveModal::OuterDocsCache,
            ActiveModal::ConfirmDetachFocus,
            ActiveModal::ClearHorizonFinal,
            ActiveModal::ClearHorizon,
            ActiveModal::DeletePlanFinal,
            ActiveModal::DeletePlan,
            ActiveModal::DeleteReview,
            ActiveModal::ReviewAnalyze,
            ActiveModal::ExecuteReview,
            ActiveModal::ExecutePipeline,
            ActiveModal::ExecuteScope,
            ActiveModal::CreatePlan,
            ActiveModal::RefineDialog,
            ActiveModal::RefineBlocked,
            ActiveModal::Prompt,
            ActiveModal::Locale,
            ActiveModal::ClearHistory,
            ActiveModal::Status,
            ActiveModal::Selection,
            ActiveModal::DiffInspector,
            ActiveModal::ThemeSelector,
            ActiveModal::UsageSummary,
            ActiveModal::Help,
        ];

        assert_eq!(observed.as_slice(), expected.as_slice());
    }
}
