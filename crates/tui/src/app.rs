use ratatui::layout::Rect;
use tauqe_protocol::{
    ContextState, DiscussionFocus, ModelRef, ModelRole, ModelRolesSummary, ModelSelection,
    ReasoningEffort, RepositoryState, ReviewStartParams, SystemStatusResult,
};

use crate::components::model_selector::ModelSelector;
use crate::components::ListSelector;
use crate::config::TuiConfig;
use crate::editor::InputEditor;
use crate::i18n::{active_t, I18nEngine};
use crate::theme::presets::ThemePreset;
use crate::theme::selector::ThemeSelectorState;
use crate::ui::HitRegistry;
use crate::views::context::ContextViewState;
use crate::views::history::HistoryViewState;
use crate::views::plans::{PlansViewState, RefineDialogFocus};
use crate::views::repl::{ReplView, StreamingFileEdit, TurnStatus};
use crate::views::review::ReviewViewState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyCommand {
    pub key: &'static str,
    pub description: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConfirmDialogButton {
    #[default]
    Cancel,
    Confirm,
}

/// Action selected in the batch-cancellation dialog.
///
/// A batch runs a bounded queue of isolated turns. Distinguishing "keep going"
/// from a graceful stop (finish the current member, then halt) and an immediate
/// abort lets the developer interrupt a long queue without discarding the work
/// already committed (Conventions.md §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BatchCancelButton {
    /// Keep the batch running; the safe default.
    #[default]
    Continue,
    /// Let the current member finish, then stop before the next one.
    SoftStop,
    /// Abort the current member and stop the batch immediately.
    HardStop,
}

impl BatchCancelButton {
    /// Advances focus to the next action button.
    pub fn cycle_next(&mut self) {
        *self = match self {
            Self::Continue => Self::SoftStop,
            Self::SoftStop => Self::HardStop,
            Self::HardStop => Self::Continue,
        };
    }

    /// Moves focus to the previous action button.
    pub fn cycle_prev(&mut self) {
        *self = match self {
            Self::Continue => Self::HardStop,
            Self::SoftStop => Self::Continue,
            Self::HardStop => Self::SoftStop,
        };
    }
}

/// The single source of truth for modal stacking order.
///
/// Every surface that must decide *which* modal is on top — keyboard dispatch,
/// rendering, mouse hit-testing, and paste routing — derives its decision from
/// [`AppState::active_modal`]. The variant order encodes priority: the first
/// active variant wins. Centralizing the list here guarantees that the window a
/// developer *sees* is always the window that receives keystrokes, mouse events,
/// and pasted text, and that adding a modal no longer requires editing several
/// divergent chains (Conventions §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveModal {
    /// Fatal transport state; blocks every other surface.
    ServerDisconnected,
    Squash,
    Review,
    /// Review-finding decomposition dialog opened with `r` in the Review view.
    ReviewRefine,
    ConfirmCancel,
    ConfirmQuit,
    BatchCancel,
    ClearAuto,
    ClearUser,
    ConfirmDetachFocus,
    /// First tier of the implementation-horizon clear confirmation.
    ClearHorizon,
    /// Second tier, raised when completed or in-progress work exists.
    ClearHorizonFinal,
    DeletePlan,
    /// Second tier, raised when the plan still carries unfinished work.
    DeletePlanFinal,
    DeleteReview,
    /// Confirmation of the expensive Architect post-analysis over a session.
    ReviewAnalyze,
    ExecuteReview,
    ExecutePipeline,
    ExecuteScope,
    RefineDialog,
    RefineBlocked,
    Prompt,
    Locale,
    ClearHistory,
    Status,
    Selection,
    /// Fullscreen two-pane audit of the change set staged in the Develop
    /// viewport: a read-only reading surface that outranks the informational
    /// overlays below it (c-3).
    DiffInspector,
    ThemeSelector,
    /// Full usage telemetry breakdown for the session and the operations.
    UsageSummary,
    Help,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ModalFocus {
    #[default]
    Editor,
    Actions,
}

impl ModalFocus {
    pub fn is_editor(&self) -> bool {
        matches!(self, Self::Editor)
    }

    pub fn is_actions(&self) -> bool {
        matches!(self, Self::Actions)
    }

    pub fn toggle(&mut self) {
        *self = match self {
            Self::Editor => Self::Actions,
            Self::Actions => Self::Editor,
        };
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModalActionsState {
    pub focused_button: ConfirmDialogButton,
}

impl Default for ModalActionsState {
    fn default() -> Self {
        Self {
            focused_button: ConfirmDialogButton::Cancel,
        }
    }
}

impl ModalActionsState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel() -> Self {
        Self {
            focused_button: ConfirmDialogButton::Cancel,
        }
    }

    pub fn confirm() -> Self {
        Self {
            focused_button: ConfirmDialogButton::Confirm,
        }
    }

    pub fn toggle(&mut self) {
        self.focused_button = match self.focused_button {
            ConfirmDialogButton::Cancel => ConfirmDialogButton::Confirm,
            ConfirmDialogButton::Confirm => ConfirmDialogButton::Cancel,
        };
    }

    pub fn is_confirm(&self) -> bool {
        self.focused_button == ConfirmDialogButton::Confirm
    }

    pub fn is_cancel(&self) -> bool {
        self.focused_button == ConfirmDialogButton::Cancel
    }

    pub fn reset_to_cancel(&mut self) {
        self.focused_button = ConfirmDialogButton::Cancel;
    }
}

impl From<ConfirmDialogButton> for ModalActionsState {
    fn from(btn: ConfirmDialogButton) -> Self {
        Self {
            focused_button: btn,
        }
    }
}

impl From<ModalActionsState> for ConfirmDialogButton {
    fn from(st: ModalActionsState) -> Self {
        st.focused_button
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum NotificationLevel {
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Debug, Clone)]
pub struct AppNotification {
    pub level: NotificationLevel,
    pub text: String,
    pub created_at: std::time::Instant,
    pub ttl: std::time::Duration,
}

/// The developer's model choice (a semantic role or specific model)
/// together with the semantic role assignments reported by the server.
#[derive(Debug, Clone, Default)]
pub struct ModelChoice {
    pub selection: ModelSelection,
    pub roles: Option<ModelRolesSummary>,
}

impl ModelChoice {
    /// Resolves a selection to a concrete model. A role cannot be resolved until the
    /// server has reported the role assignments.
    pub fn resolve(&self, selection: &ModelSelection) -> Option<ModelRef> {
        match selection {
            ModelSelection::Specific(model) => Some(model.clone()),
            ModelSelection::Tier { model, .. } => Some(model.clone()),
            ModelSelection::Role(role) => self.roles.as_ref().map(|roles| match role {
                ModelRole::Scribe => roles.scribe.clone(),
                ModelRole::Coder => roles.coder.clone(),
                ModelRole::Architect => roles.architect.clone(),
                ModelRole::Primary => roles.primary.clone(),
                ModelRole::Reviewer => roles.reviewer.clone(),
            }),
        }
    }
}

/// Structured identity of the model currently executing a turn.
///
/// Populated from the `model/started` event so the header can render the live
/// model while a turn owns the interface, and cleared once the turn settles.
/// The semantic role is provenance only: it selects the model and is never part
/// of the printed name, so every surface renders the label through
/// [`crate::model_display::model_label`] (c-2).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RunningModel {
    /// Wire model reference.
    pub model: ModelRef,
    /// Semantic role that launched the turn, when it was role-driven.
    pub role: Option<ModelRole>,
    /// Effective reasoning effort, when one was requested.
    pub effort: Option<ReasoningEffort>,
}

impl RunningModel {
    /// Display label of the running model through the single printing rule.
    ///
    /// The alias comes from the provider catalog published once per frame, so a
    /// model configured with a human-readable name is never presented by its
    /// provider identifier, and the reasoning depth is appended when set
    /// (`name [effort]`) exactly like a configured role entry (c-1, c-2).
    pub fn label(&self) -> String {
        crate::model_display::model_label(&self.model, self.effort)
    }
}

/// Canonical Help-open chord, expressed with the abstract primary command modifier
/// (`C-`). The header button and the Help dialog both derive their labels from this
/// single value, so the advertised and documented keybindings cannot diverge
/// (Conventions §4.1). Outside text-input contexts a bare `?` also opens Help.
pub const HELP_OPEN_CHORD: &str = "C-H";

/// Compact rendering of [`HELP_OPEN_CHORD`] for the header button
/// (`C-H` for the Ctrl primary modifier, `M-H` for Alt).
pub fn help_open_hint(primary: crate::config::PrimaryModifier) -> String {
    HELP_OPEN_CHORD.replace("C-", primary.emacs_prefix())
}

/// State of the primary model picker (`C-m`).
///
/// The picker is a thin shell over [`ModelSelector`]: the configured primary
/// rungs, the highlighted one, the cyclic navigation, the digit quick-select,
/// the scroll arithmetic, and the row rendering all live in the single model
/// selection component. The dialog therefore stores nothing but that selector,
/// so it cannot drift from the four role dialogs that share it (c-1).
#[derive(Debug, Clone)]
pub struct SelectionDialogState {
    pub selector: ModelSelector,
}

impl SelectionDialogState {
    pub fn new(entries: Vec<tauqe_protocol::ModelEntry>, selected_index: usize) -> Self {
        Self {
            selector: ModelSelector::new(ModelRole::Primary, entries, selected_index),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusDialogTarget {
    PlanItem {
        plan_id: String,
        item_id: String,
        item_title: String,
        current_status: tauqe_protocol::PlanItemStatus,
    },
    ReviewItem {
        review_id: Option<String>,
        item_id: u32,
        item_title: String,
        current_status: tauqe_protocol::ReviewItemStatus,
    },
}

impl StatusDialogTarget {
    /// Number of selectable status options presented for this target.
    pub fn options_count(&self) -> usize {
        match self {
            Self::PlanItem { .. } => 6,
            Self::ReviewItem { .. } => 7,
        }
    }
}

/// Pending confirmation of a plan step or scope execution.
/// Section of the plan step and scope execution dialog that owns the keyboard.
///
/// Executing a step or a scope hands control to the Coder role, and the plan is
/// the single source of truth for what each step must implement, so the
/// confirmation carries exactly two keyboard targets: the starting Coder rung and
/// the action buttons. There is no directives editor to reach, so two states
/// describe the whole focus model (c-4, Conventions §6, §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExecuteScopeFocus {
    /// The embedded Coder ladder owns digits, arrows, and paging.
    #[default]
    ModelList,
    /// The action bar owns button stepping and activation.
    Actions,
}

impl ExecuteScopeFocus {
    /// Whether the embedded Coder list owns the keyboard.
    pub fn is_model_list(&self) -> bool {
        matches!(self, Self::ModelList)
    }

    /// Whether the action bar owns the keyboard.
    pub fn is_actions(&self) -> bool {
        matches!(self, Self::Actions)
    }

    /// Advances focus to the next section in visual order.
    ///
    /// A dialog offering a single Coder entry has no list to navigate, so the
    /// action bar stays focused instead of cycling through a one-row picker.
    pub fn cycle_next(&mut self, entry_count: usize) {
        *self = match self {
            Self::ModelList => Self::Actions,
            Self::Actions if entry_count > 1 => Self::ModelList,
            Self::Actions => Self::Actions,
        };
    }

    /// Moves focus to the previous section in visual order.
    pub fn cycle_prev(&mut self, entry_count: usize) {
        *self = match self {
            Self::ModelList => Self::Actions,
            Self::Actions if entry_count > 1 => Self::ModelList,
            Self::Actions => Self::Actions,
        };
    }
}

/// Pending confirmation of a plan step or scope execution.
///
/// Executing a step or a scope hands control to the Coder role, so the dialog
/// carries the configured Coder ladder and the section that currently owns the
/// keyboard. The plan states every step's instructions itself, so no directives
/// editor travels with the confirmation: the developer picks only the
/// implementing model before any bounded turn spends tokens (c-4).
#[derive(Debug, Clone)]
pub struct ConfirmExecuteScopeState {
    pub plan_id: String,
    pub plan_title: String,
    pub scope_title: String,
    pub step_id: Option<String>,
    pub steps: Vec<tauqe_protocol::PlanItem>,
    /// Coder-role model selector: configured entries, cyclic navigation, and the
    /// shared embedded-list rendering (c-4).
    pub selector: ModelSelector,
    /// Section of the dialog that currently owns the keyboard.
    pub focus: ExecuteScopeFocus,
    /// Focused action button; the safe `Cancel` holds the default.
    pub confirm_button: ConfirmDialogButton,
}

impl ConfirmExecuteScopeState {
    /// Builds an execution confirmation over the configured Coder entries.
    ///
    /// One constructor serves both shapes — a single step and a whole scope,
    /// including a concept-only plan whose horizon is projected just-in-time —
    /// so the state definition cannot drift between them (c-4).
    pub fn new(
        plan_id: String,
        plan_title: String,
        scope_title: String,
        step_id: Option<String>,
        steps: Vec<tauqe_protocol::PlanItem>,
        coder_entries: Vec<tauqe_protocol::ModelEntry>,
    ) -> Self {
        Self {
            plan_id,
            plan_title,
            scope_title,
            step_id,
            steps,
            selector: ModelSelector::new(ModelRole::Coder, coder_entries, 0),
            focus: ExecuteScopeFocus::default(),
            confirm_button: ConfirmDialogButton::Cancel,
        }
    }

    /// Currently selected Coder entry, if any.
    pub fn selected_entry(&self) -> Option<&tauqe_protocol::ModelEntry> {
        self.selector.selected_entry()
    }
}
/// Pending confirmation of a multi-plan execution pipeline.
///
/// A pipeline is a super-batch: every queued plan runs one after another under a
/// single registered operation. The state keeps the display titles so the
/// confirmation can name the whole queue, and records the plans that carry only
/// their architectural concept so the dialog can announce the just-in-time
/// horizon projection before any bounded turn spends tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmExecutePipelineState {
    pub plan_ids: Vec<String>,
    pub plan_titles: Vec<String>,
    pub steps: Vec<tauqe_protocol::PlanItem>,
    /// Titles of the plans whose horizon the batch projects just-in-time.
    pub pending_synthesis: Vec<String>,
}

/// Pending confirmation of the Architect post-analysis over one review session.
///
/// The classification is a costly LLM operation that replaces the whole
/// post-analysis layer of the session, so launching it requires an explicit
/// confirmation whose safe default keeps the existing layer untouched (c-4).
#[derive(Debug, Clone)]
pub struct ConfirmReviewAnalyzeState {
    /// Session whose findings the Architect classifies.
    pub review_id: String,
    /// Display title of that session, captured when the dialog opens.
    pub review_title: String,
}

#[derive(Debug, Clone)]
pub struct ConfirmExecuteReviewState {
    pub review_id: Option<String>,
    pub review_title: String,
    pub scope_title: String,
    pub item_id: Option<u32>,
    /// Concrete subplan step targeted by point execution, when the developer
    /// confirmed from a step row.
    pub step_id: Option<String>,
    pub items: Vec<tauqe_protocol::ReviewIssue>,
    /// Coder-role model selector: configured entries, cyclic navigation, and the
    /// shared embedded-list rendering (concept #c-3).
    pub selector: ModelSelector,
    /// Optional implementation directives forwarded to the Coder turn.
    pub instructions_editor: InputEditor,
    /// Section of the dialog that currently owns the keyboard.
    pub focus: RefineDialogFocus,
    /// Focused action button; the safe `Cancel` holds the default.
    pub confirm_button: ConfirmDialogButton,
}

impl ConfirmExecuteReviewState {
    /// Builds a review-execution confirmation over the configured Coder entries.
    ///
    /// One constructor serves both shapes: point execution (one finding or one
    /// subplan step, which exposes the Coder selector and the directives editor)
    /// and the batch queue, so the state definition cannot drift between them
    /// (concept #c-3).
    pub fn new(
        review_id: Option<String>,
        review_title: String,
        scope_title: String,
        item_id: Option<u32>,
        step_id: Option<String>,
        items: Vec<tauqe_protocol::ReviewIssue>,
        coder_entries: Vec<tauqe_protocol::ModelEntry>,
    ) -> Self {
        Self {
            review_id,
            review_title,
            scope_title,
            item_id,
            step_id,
            items,
            selector: ModelSelector::new(ModelRole::Coder, coder_entries, 0),
            instructions_editor: InputEditor::default(),
            focus: RefineDialogFocus::default(),
            confirm_button: ConfirmDialogButton::Cancel,
        }
    }

    /// Currently selected Coder entry, if any.
    pub fn selected_entry(&self) -> Option<&tauqe_protocol::ModelEntry> {
        self.selector.selected_entry()
    }

    /// Whether point execution targets exactly one finding or subplan step.
    ///
    /// Only this shape exposes the Coder selector and the directives editor; a
    /// batch runs autonomously on the configured role defaults (concept #c-3).
    pub fn is_single_target(&self) -> bool {
        self.review_id.is_some() && self.items.len() == 1
    }
}

impl Default for ConfirmExecuteReviewState {
    fn default() -> Self {
        Self::new(
            None,
            String::new(),
            String::new(),
            None,
            None,
            Vec::new(),
            Vec::new(),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptModalTarget {
    Review {
        review_id: String,
        review_title: String,
        focused_items: Vec<(u32, String)>,
    },
    Plan {
        plan_id: String,
        plan_title: String,
        focused_items: Vec<(String, String)>,
    },
}

#[derive(Debug, Clone)]
pub struct PromptModalState {
    pub target: PromptModalTarget,
    pub prompt_editor: InputEditor,
    pub focus: ModalFocus,
    pub actions: ModalActionsState,
}

impl PromptModalState {
    pub fn new(target: PromptModalTarget) -> Self {
        Self {
            target,
            prompt_editor: InputEditor::default(),
            focus: ModalFocus::Editor,
            actions: ModalActionsState::cancel(),
        }
    }
}

/// Focus section of the locale dialog: the language field or the action bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LocaleDialogFocus {
    /// The single-line target-language field owns the keyboard.
    #[default]
    Editor,
    /// The action buttons own the keyboard.
    Actions,
}

impl LocaleDialogFocus {
    pub fn is_editor(&self) -> bool {
        matches!(self, Self::Editor)
    }

    pub fn is_actions(&self) -> bool {
        matches!(self, Self::Actions)
    }
}

/// Action selected in the locale dialog's action bar.
///
/// Synthesizing spends tokens through the daemon and is the primary action;
/// resetting reverts to the built-in English; cancel dismisses without a change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LocaleDialogButton {
    #[default]
    Synthesize,
    ResetEnglish,
    Cancel,
}

impl LocaleDialogButton {
    /// Advances focus to the next action button.
    pub fn cycle_next(&mut self) {
        *self = match self {
            Self::Synthesize => Self::ResetEnglish,
            Self::ResetEnglish => Self::Cancel,
            Self::Cancel => Self::Synthesize,
        };
    }

    /// Moves focus to the previous action button.
    pub fn cycle_prev(&mut self) {
        *self = match self {
            Self::Synthesize => Self::Cancel,
            Self::ResetEnglish => Self::Synthesize,
            Self::Cancel => Self::ResetEnglish,
        };
    }
}

/// State of the interface-language (locale) dialog.
///
/// The developer names a target language in free form; confirming dispatches
/// `locale/generate` to the daemon (c-4). The dialog also offers a one-step
/// reset back to the embedded English (c-5).
#[derive(Debug, Clone, Default)]
pub struct LocaleDialogState {
    /// Single-line target-language field.
    pub language_editor: InputEditor,
    /// Section that currently owns the keyboard.
    pub focus: LocaleDialogFocus,
    /// Focused action button.
    pub button: LocaleDialogButton,
    /// Whether synthesis request is currently in flight.
    pub generating: bool,
}

/// State of the full usage breakdown dialog.
///
/// The daemon owns every counter (c-1), so the dialog stores nothing but its
/// own scroll offset and renders the snapshot the footer mirrors (c-2, c-4).
#[derive(Debug, Clone, Default)]
pub struct UsageSummaryState {
    /// Vertical scroll offset of the breakdown body.
    pub scroll: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusDialogState {
    pub target: StatusDialogTarget,
    pub selector: ListSelector,
}

impl StatusDialogState {
    pub fn new(target: StatusDialogTarget, selected_index: usize) -> Self {
        let selector = ListSelector::new(target.options_count(), selected_index);
        Self { target, selector }
    }
}

/// Snapshot of the change set a developer audits in the fullscreen inspector.
///
/// The inspector is a reading surface, not a live projection of the stream: the
/// staged files are captured when the Develop viewport hands them over, so
/// scrolling, resizing, and re-rendering never depend on edits that are still
/// arriving. The captured list is the session file list the left pane walks,
/// while the scroll offset belongs to the isolated diff pane alone (c-3).
pub struct DiffInspectorState {
    /// Staged files of the audited turn, in Viewport order.
    pub files: Vec<StreamingFileEdit>,
    /// Index of the highlighted file inside [`Self::files`].
    pub selected_file: usize,
    /// Vertical scroll offset of the isolated diff read area.
    pub diff_scroll: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    Repl,
    Plans,
    Review,
    Context,
    History,
    Onboarding,
}

/// Tab currently shown by the Help modal.
///
/// The Help dialog exposes three independent command surfaces: global shortcuts
/// reachable from any view (`Global`), the commands of the active view mode
/// (`CurrentMode`), and the shared [`crate::editor::InputEditor`] Emacs/Readline
/// bindings (`TextEditor`). Keeping them on separate tabs prevents mode help
/// and global shortcuts from colliding or overflowing the screen (Conventions.md §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HelpTab {
    /// Application-wide shortcuts reachable from every view; the default tab.
    #[default]
    Global,
    /// Commands specific to the active view mode.
    CurrentMode,
    /// Shared text-editor (Emacs/Readline) commands.
    TextEditor,
}

impl HelpTab {
    /// Advances to the next tab, wrapping around.
    pub fn next(self) -> Self {
        match self {
            Self::Global => Self::CurrentMode,
            Self::CurrentMode => Self::TextEditor,
            Self::TextEditor => Self::Global,
        }
    }

    /// Moves to the previous tab, wrapping around.
    pub fn prev(self) -> Self {
        match self {
            Self::Global => Self::TextEditor,
            Self::CurrentMode => Self::Global,
            Self::TextEditor => Self::CurrentMode,
        }
    }

    /// Human-readable tab label shown in the popup's tab bar.
    pub fn title(self) -> &'static str {
        match self {
            Self::Global => "Global",
            Self::CurrentMode => "Current Mode",
            Self::TextEditor => "Text Editor",
        }
    }
}

/// Focus section of the review launch dialog.
///
/// The dialog exposes the embedded Reviewer model list, the review-preset
/// list, the two text fields, and the action bar, and `Tab` cycles through
/// them in visual order. Making the section explicit replaces the old
/// heuristic that inferred the target from whether the editor was empty, so
/// list navigation and text editing never fight over the same keys
/// (Conventions §6, §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReviewDialogFocus {
    ModelList,
    /// Selectable list of the project's review presets, led by the explicit
    /// `Custom` row (c-5).
    PresetList,
    /// Single-line field for the target directory or glob pattern.
    PathEditor,
    #[default]
    PromptEditor,
    Actions,
}

impl ReviewDialogFocus {
    /// Visual order of the dialog sections, from the reviewer list to the
    /// action bar. Focus cycling walks this sequence and skips the sections
    /// that cannot receive the keyboard (c-3, c-5).
    const VISUAL_ORDER: [ReviewDialogFocus; 5] = [
        ReviewDialogFocus::ModelList,
        ReviewDialogFocus::PresetList,
        ReviewDialogFocus::PathEditor,
        ReviewDialogFocus::PromptEditor,
        ReviewDialogFocus::Actions,
    ];

    /// Position of the section in the visual order.
    fn position(self) -> usize {
        Self::VISUAL_ORDER
            .iter()
            .position(|section| *section == self)
            .unwrap_or(0)
    }

    pub fn is_model_list(&self) -> bool {
        matches!(self, Self::ModelList)
    }

    /// Whether the focus is on the review-preset section.
    pub fn is_preset_list(&self) -> bool {
        matches!(self, Self::PresetList)
    }

    pub fn is_path_editor(&self) -> bool {
        matches!(self, Self::PathEditor)
    }

    pub fn is_prompt_editor(&self) -> bool {
        matches!(self, Self::PromptEditor)
    }

    pub fn is_actions(&self) -> bool {
        matches!(self, Self::Actions)
    }
}

/// Candidate rows of the contextual directory completion menu kept in state.
///
/// The menu is drawn under the path block, so the list is capped to keep the
/// popup compact: a longer prefix match is narrowed by typing one more
/// character rather than by scrolling an arbitrarily long menu (c-5).
pub const PATH_COMPLETION_MAX_ROWS: usize = 6;

/// Contextual directory completion of the active target-path row (c-5).
///
/// The completion holds the repository directory prefixes that continue the
/// text typed in the active row, together with the highlighted candidate. It is
/// pure presentation state: it selects nothing by itself and never reaches the
/// wire, because only an accepted candidate changes the row text that becomes
/// `ReviewStartParams::path_patterns`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PathCompletion {
    candidates: Vec<String>,
    selected: usize,
}

impl PathCompletion {
    /// Whether the menu currently offers at least one candidate.
    pub fn is_open(&self) -> bool {
        !self.candidates.is_empty()
    }

    /// Candidate prefixes the menu draws, in repository order.
    pub fn candidates(&self) -> &[String] {
        &self.candidates
    }

    /// Index of the highlighted candidate.
    pub fn selected_index(&self) -> usize {
        self.selected
    }

    /// Highlighted candidate, if any.
    pub fn selected_candidate(&self) -> Option<&str> {
        self.candidates.get(self.selected).map(String::as_str)
    }

    /// Index of the first candidate drawn in a window of `visible_rows` rows,
    /// keeping the highlight visible.
    pub fn visible_start(&self, visible_rows: usize) -> usize {
        visible_start(self.selected, self.candidates.len(), visible_rows)
    }

    /// Replaces the candidate list, preserving the highlight while the list is
    /// unchanged and returning it to the first row otherwise, so moving the
    /// cursor inside the row never resets a candidate already chosen.
    pub fn reset(&mut self, candidates: Vec<String>) {
        if self.candidates != candidates {
            self.selected = 0;
        }
        self.candidates = candidates;
        self.clamp();
    }

    /// Highlights the next candidate, wrapping around at the end.
    pub fn select_next(&mut self) {
        if !self.candidates.is_empty() {
            self.selected = (self.selected + 1) % self.candidates.len();
        }
    }

    /// Highlights the previous candidate, wrapping around at the start.
    pub fn select_prev(&mut self) {
        if !self.candidates.is_empty() {
            self.selected = self
                .selected
                .checked_sub(1)
                .unwrap_or(self.candidates.len() - 1);
        }
    }

    /// Highlights the candidate at `index`, clamped into range.
    pub fn select_index(&mut self, index: usize) {
        if self.candidates.is_empty() {
            return;
        }
        self.selected = index.min(self.candidates.len() - 1);
    }

    /// Closes the menu without touching the rows.
    pub fn close(&mut self) {
        self.candidates.clear();
        self.selected = 0;
    }

    fn clamp(&mut self) {
        if self.candidates.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.candidates.len() {
            self.selected = self.candidates.len() - 1;
        }
    }
}

/// Index of the first row of a window of `visible_rows` rows that keeps
/// `selected` visible.
///
/// The completion menu and the path rows share this arithmetic, so neither
/// surface re-derives its own scroll window.
fn visible_start(selected: usize, len: usize, visible_rows: usize) -> usize {
    if len == 0 || visible_rows == 0 {
        return 0;
    }
    let height = visible_rows.min(len);
    let max_start = len - height;
    let mut start = 0;
    if selected >= height {
        start = selected + 1 - height;
    }
    start.min(max_start)
}

/// Directory prefixes of `directories` that continue `prefix` (c-5).
///
/// Every repository directory that starts with the typed text and is not the
/// text itself continues it. A blank prefix offers nothing, because an empty row
/// narrows nothing and the whole directory tree is not a suggestion. The list is
/// capped so the popup stays compact.
fn completion_candidates(prefix: &str, directories: &[String]) -> Vec<String> {
    if prefix.is_empty() {
        return Vec::new();
    }
    directories
        .iter()
        .filter(|directory| directory.starts_with(prefix) && directory.as_str() != prefix)
        .take(PATH_COMPLETION_MAX_ROWS)
        .cloned()
        .collect()
}

#[derive(Debug, Clone)]
pub struct ReviewDialogState {
    pub files_count: usize,
    pub estimated_tokens: usize,
    /// Reviewer-role model selector: configured entries, cyclic navigation,
    /// embedded-list rendering, and the shared scroll arithmetic (c-1, c-2).
    pub selector: ModelSelector,
    /// Repository file list the handshake delivered.
    ///
    /// The live match counter evaluates the target text against exactly this
    /// list with the same `tauqe_protocol::glob::GlobPattern` the server
    /// applies, so a local preview can never predict a selection the server
    /// resolves differently (c-2).
    pub repo_files: Vec<String>,
    /// Review presets the project declares, offered as a selectable section.
    ///
    /// Row zero of [`Self::preset_selector`] is the explicit `Custom` entry, so
    /// a developer can always fall back to a manual launch after inspecting a
    /// preset (c-5). An empty list collapses the section and leaves the dialog
    /// exactly as it was before presets existed.
    pub presets: Vec<tauqe_protocol::ReviewPresetSummary>,
    /// Cursor and scroll window of the preset section. The shared
    /// `ListSelector` owns the navigation arithmetic, so the section carries no
    /// index math of its own (c-5).
    pub preset_selector: ListSelector,
    /// Ordered path and glob rows narrowing the audited set.
    ///
    /// Every non-empty row becomes one entry of `ReviewStartParams::path_patterns`,
    /// which the server resolves as a disjunction: a file is audited when at
    /// least one row selects it. The rows are sent as one list, so the matched
    /// files are merged with the current User Context rather than replacing it:
    /// the developer keeps exclusive ownership of that layer (c-3, c-6).
    pub path_inputs: Vec<InputEditor>,
    /// Index of the path row that currently owns the keyboard.
    pub path_active: usize,
    /// Directory prefixes the repository offers to the active path row.
    ///
    /// Derived once from `repo_files` through `collect_directories`, so typing
    /// never re-walks the whole file list to rebuild the prefix tree (c-5).
    pub path_directories: Vec<String>,
    /// Contextual directory completion of the active path row (c-5).
    pub path_completion: PathCompletion,
    pub prompt_editor: InputEditor,
    /// Section that currently owns the keyboard.
    pub focus: ReviewDialogFocus,
    /// Which action-bar button holds focus. Defaults to the safe `Cancel`.
    pub confirm_button: ConfirmDialogButton,
}

impl ReviewDialogState {
    /// Builds a review launch dialog selecting among the configured
    /// `reviewer_entries` for the Reviewer role.
    ///
    /// `repo_files` is the repository file list the handshake delivered: the
    /// dialog evaluates the target text against it locally, so a launch needs
    /// no matching RPC (c-2).
    pub fn new(
        files_count: usize,
        estimated_tokens: usize,
        reviewer_entries: Vec<tauqe_protocol::ModelEntry>,
        repo_files: Vec<String>,
    ) -> Self {
        let path_directories = crate::dialogs::pattern::collect_directories(&repo_files);
        Self {
            files_count,
            estimated_tokens,
            selector: ModelSelector::new(ModelRole::Reviewer, reviewer_entries, 0),
            repo_files,
            presets: Vec::new(),
            preset_selector: ListSelector::new(0, 0),
            path_inputs: vec![InputEditor::default()],
            path_active: 0,
            path_directories,
            path_completion: PathCompletion::default(),
            prompt_editor: InputEditor::default(),
            focus: ReviewDialogFocus::default(),
            confirm_button: ConfirmDialogButton::Cancel,
        }
    }

    /// Registers the project's review presets as a selectable section.
    ///
    /// The section lists the explicit `Custom` row first and one row per
    /// configured preset, so a manual launch is always one row away after a
    /// preset has been inspected (c-5). A project that declares no preset keeps
    /// the section collapsed and the dialog exactly as it was before.
    pub fn with_presets(mut self, presets: Vec<tauqe_protocol::ReviewPresetSummary>) -> Self {
        if !presets.is_empty() {
            self.preset_selector = ListSelector::new(presets.len() + 1, 0);
        }
        self.presets = presets;
        self
    }

    /// Whether the project declares any review preset.
    pub fn has_presets(&self) -> bool {
        !self.presets.is_empty()
    }

    /// Labels of the selectable preset rows: the explicit `Custom` entry first,
    /// then every configured preset by its human-readable title, or by its name
    /// when the preset declares no title (c-5).
    ///
    /// The list is empty when the project declares no preset, so a caller can
    /// never draw a row that [`Self::preset_selector`] does not own.
    pub fn preset_labels(&self) -> Vec<String> {
        if !self.has_presets() {
            return Vec::new();
        }
        let mut labels = Vec::with_capacity(self.presets.len() + 1);
        labels.push(active_t("dialog.review_launch.preset_custom"));
        for preset in &self.presets {
            let title = preset
                .title
                .as_deref()
                .map(str::trim)
                .filter(|title| !title.is_empty());
            labels.push(match title {
                Some(title) => title.to_string(),
                None => preset.name.clone(),
            });
        }
        labels
    }

    /// Preset the highlighted row selects, or `None` while the explicit `Custom`
    /// row owns the highlight (c-5).
    pub fn selected_preset(&self) -> Option<&tauqe_protocol::ReviewPresetSummary> {
        self.preset_selector
            .selected_index()
            .checked_sub(1)
            .and_then(|index| self.presets.get(index))
    }

    /// Whether the launch is the custom, one-off audit the developer assembles
    /// by hand.
    ///
    /// The explicit `Custom` row of the preset section owns this mode; a
    /// highlighted preset owns the launch instead and supplies its own groups,
    /// paths, and prompts. The mode governs the whole dialog surface: the
    /// target field is its input aid and exists only in this mode, while the
    /// preset section itself is the mode switcher and stays visible in both
    /// (c-5).
    pub fn is_custom_launch(&self) -> bool {
        self.selected_preset().is_none()
    }
    pub fn selected_entry(&self) -> Option<&tauqe_protocol::ModelEntry> {
        self.selector.selected_entry()
    }

    /// Trimmed, non-empty path rows in entry order (c-3, c-6).
    ///
    /// Blank rows narrow nothing, so they are dropped instead of being sent as
    /// worthless patterns that would select the whole repository.
    pub fn path_patterns(&self) -> Vec<String> {
        self.path_inputs
            .iter()
            .map(|editor| editor.get_text().trim().to_string())
            .filter(|pattern| !pattern.is_empty())
            .collect()
    }

    /// Number of path rows the dialog currently lists.
    pub fn path_row_count(&self) -> usize {
        self.path_inputs.len()
    }

    /// Text editor of the row that currently owns the keyboard.
    pub fn active_path_editor(&self) -> Option<&InputEditor> {
        self.path_inputs.get(self.path_active)
    }

    /// Mutable text editor of the row that currently owns the keyboard.
    pub fn active_path_editor_mut(&mut self) -> Option<&mut InputEditor> {
        self.path_inputs.get_mut(self.path_active)
    }

    /// Whether the active row holds no text, which is what makes `Backspace`
    /// close it instead of deleting a character (c-3).
    pub fn active_path_is_empty(&self) -> bool {
        self.active_path_editor().is_some_and(InputEditor::is_empty)
    }

    /// Whether the active row carries a usable path or glob.
    ///
    /// A row is usable once it holds non-whitespace text: the glob matcher is
    /// total and degrades an unterminated character class to a literal bracket
    /// instead of rejecting the pattern, so emptiness is the only defect a row
    /// can carry (c-3).
    pub fn active_path_is_valid(&self) -> bool {
        self.active_path_editor()
            .is_some_and(|editor| !editor.get_text().trim().is_empty())
    }

    /// Appends the next path row when the active one carries a usable pattern.
    ///
    /// `C-j` opens a row below the active one and hands it the keyboard, so a
    /// further pattern is typed without leaving the section. An empty or blank
    /// row refuses the append, which keeps stray empty rows out of the list
    /// (c-3). Returns whether a row was added.
    pub fn append_path_row(&mut self) -> bool {
        if !self.active_path_is_valid() {
            return false;
        }
        self.path_inputs.push(InputEditor::default());
        self.path_active = self.path_inputs.len() - 1;
        self.refresh_path_completion();
        true
    }

    /// Closes the active row and hands the keyboard to the previous one.
    ///
    /// Only an empty row can be closed, and the dialog always keeps at least one
    /// row, so a row holding text is never dropped by a single keystroke and a
    /// multi-row list can never shrink into nothing (c-3). Returns whether a row
    /// was removed.
    pub fn remove_active_path_row(&mut self) -> bool {
        if !self.active_path_is_empty() || self.path_inputs.len() <= 1 {
            return false;
        }
        self.path_inputs.remove(self.path_active);
        self.path_active = self
            .path_active
            .saturating_sub(1)
            .min(self.path_inputs.len() - 1);
        self.refresh_path_completion();
        true
    }

    /// Walks the active path row with `C-n` / `C-p`.
    ///
    /// Returns `true` while the walk stays inside the list and `false` at its
    /// boundary, where the caller hands the keyboard to the neighbouring section
    /// instead. The rows therefore behave like a vertical stack of input fields
    /// without ever trapping the focus (c-4).
    pub fn step_path_row(&mut self, forward: bool) -> bool {
        if forward {
            if self.path_active + 1 >= self.path_inputs.len() {
                return false;
            }
            self.path_active += 1;
        } else {
            match self.path_active.checked_sub(1) {
                Some(previous) => self.path_active = previous,
                None => return false,
            }
        }
        self.refresh_path_completion();
        true
    }

    /// Index of the first path row drawn in a window of `visible_rows` rows,
    /// keeping the active row visible.
    pub fn path_visible_start(&self, visible_rows: usize) -> usize {
        visible_start(self.path_active, self.path_inputs.len(), visible_rows)
    }

    /// Recomputes the directory completion of the active row (c-5).
    pub fn refresh_path_completion(&mut self) {
        let prefix = self
            .active_path_editor()
            .map(|editor| editor.get_text().trim().to_string())
            .unwrap_or_default();
        let candidates = completion_candidates(&prefix, &self.path_directories);
        self.path_completion.reset(candidates);
    }

    /// Accepts the highlighted directory completion into the active row (c-5).
    ///
    /// The candidate replaces the typed text, so the row ends exactly on a
    /// directory prefix the matcher resolves as "every path beneath it". Returns
    /// whether a candidate was accepted.
    pub fn accept_path_completion(&mut self) -> bool {
        let candidate = match self.path_completion.selected_candidate() {
            Some(candidate) => candidate.to_string(),
            None => return false,
        };
        if let Some(editor) = self.active_path_editor_mut() {
            editor.clear();
            editor.insert_str(&candidate);
        }
        self.refresh_path_completion();
        true
    }

    /// Highlights the candidate at `index` and accepts it into the active row.
    pub fn accept_path_completion_at(&mut self, index: usize) -> bool {
        self.path_completion.select_index(index);
        self.accept_path_completion()
    }

    /// Highlights the next directory completion of the active row.
    pub fn next_path_completion(&mut self) -> bool {
        if !self.path_completion.is_open() {
            return false;
        }
        self.path_completion.select_next();
        true
    }

    /// Highlights the previous directory completion of the active row.
    pub fn prev_path_completion(&mut self) -> bool {
        if !self.path_completion.is_open() {
            return false;
        }
        self.path_completion.select_prev();
        true
    }

    /// Closes the directory completion menu without touching the rows.
    pub fn close_path_completion(&mut self) {
        self.path_completion.close();
    }

    /// Candidate rows the completion menu presents while the path section owns
    /// the keyboard, and none while another section holds it.
    pub fn visible_completion_rows(&self) -> usize {
        if self.focus.is_path_editor() {
            self.path_completion.candidates().len()
        } else {
            0
        }
    }

    /// Number of repository files the entered path rows select together.
    ///
    /// The rows form a union: a file is counted once no matter how many rows
    /// select it. The value is always computed from the live row texts, so the
    /// counter can never drift from what a launch would send: `None` means the
    /// rows narrow nothing, `Some(0)` means they match nothing (c-6).
    pub fn pattern_match_count(&self) -> Option<usize> {
        crate::dialogs::pattern::count_multi_matches(&self.path_patterns(), &self.repo_files)
    }

    /// Whether a launch may proceed with the entered path rows.
    ///
    /// A highlighted preset supplies its own paths, so the rows are ignored and
    /// can never block the launch (c-5). On the explicit `Custom` row an empty
    /// list narrows nothing and passes silently, while entered patterns that
    /// select no repository file are refused: the server rejects such a launch
    /// with `INVALID_PARAMS`, and the dialog must not promise an audit over a
    /// selection that does not exist (c-6).
    pub fn is_pattern_launchable(&self) -> bool {
        self.selected_preset().is_some() || !matches!(self.pattern_match_count(), Some(0))
    }

    /// Advances focus to the next selectable section in visual order.
    ///
    /// Sections that cannot receive the keyboard are skipped: the reviewer list
    /// when a single model is configured, and the target field when a
    /// highlighted preset owns the launch (c-5).
    pub fn cycle_focus_next(&mut self) {
        self.focus = self.step_focus(true);
    }

    /// Moves focus to the previous selectable section in visual order.
    pub fn cycle_focus_prev(&mut self) {
        self.focus = self.step_focus(false);
    }

    /// Walks the visual order from the current section until an available one
    /// is reached. The walk is bounded by the number of sections, so a dialog
    /// whose lists are unavailable still terminates on a text field.
    fn step_focus(&self, forward: bool) -> ReviewDialogFocus {
        let order = ReviewDialogFocus::VISUAL_ORDER;
        let len = order.len();
        let mut index = self.focus.position();
        for _ in 0..len {
            index = if forward {
                (index + 1) % len
            } else {
                index.checked_sub(1).unwrap_or(len - 1)
            };
            let candidate = order
                .get(index)
                .copied()
                .unwrap_or(ReviewDialogFocus::PromptEditor);
            if self.focus_is_available(candidate) {
                return candidate;
            }
        }
        ReviewDialogFocus::PromptEditor
    }

    /// Whether a section can take the keyboard in this dialog.
    ///
    /// The target field is an input aid of the custom launch: under a
    /// highlighted preset it is absent from the card and can never hold the
    /// keyboard, so focus never lands on a hidden section (c-5).
    fn focus_is_available(&self, focus: ReviewDialogFocus) -> bool {
        let custom = self.is_custom_launch();
        match focus {
            ReviewDialogFocus::ModelList => self.selector.len() > 1,
            ReviewDialogFocus::PresetList => self.has_presets(),
            ReviewDialogFocus::PathEditor => custom,
            _ => true,
        }
    }

    /// Builds the RPC parameters that launch this review.
    ///
    /// A highlighted preset owns the launch: the server expands it into the
    /// groups, paths, and prompts it declares, so the dialog sends only its name
    /// and never a target pattern that would contradict the preset's own paths
    /// (c-5). The explicit `Custom` row keeps the previous behaviour — the
    /// entered rows become the `path_patterns` list, which the server resolves
    /// as a union and merges into the matched files of the current User Context
    /// without mutating the developer's layer
    /// (c-review-context-input).
    ///
    /// Both text fields are trimmed and collapse to `None` when blank, so an
    /// empty prompt or path never reaches the server as a meaningless value.
    pub fn start_params(&self) -> ReviewStartParams {
        let prompt = self.prompt_editor.get_text().trim().to_string();
        let patterns = self.path_patterns();
        let preset = self.selected_preset();
        ReviewStartParams {
            // The selected reviewer rung travels whole — the model together
            // with the reasoning depth it was configured with — so the reviewer
            // the developer sees on screen is the reviewer that actually runs
            // and no consumer re-derives the depth from the configured entries.
            model: self.selected_entry().map(|entry| ModelSelection::Tier {
                model: entry.model.clone(),
                effort: entry.effort,
            }),
            user_prompt: if prompt.is_empty() {
                None
            } else {
                Some(prompt)
            },
            // A preset supplies its own paths, so the typed rows are dropped
            // exactly as the ignored target field was (c-5, c-6).
            path_patterns: if preset.is_some() {
                Vec::new()
            } else {
                patterns
            },
            // A preset name launches the groups the project configured; its
            // absence is the custom, one-off launch the developer assembled by
            // hand (c-5).
            preset: preset.map(|preset| preset.name.clone()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnboardingStep {
    Git,
    Config,
    Credentials,
    Workstation,
    Modifier,
    Ready,
    Gatekeeper,
}

#[derive(Clone)]
pub struct OnboardingState {
    pub step: OnboardingStep,
    pub has_git: bool,
    pub has_config: bool,
    pub config_skipped: bool,
    pub config_path: Option<String>,
    pub default_config_path: String,
    pub has_api_key: bool,
    pub credentials_path: Option<String>,
    pub default_credentials_path: String,
    pub has_tui_config: bool,
    pub workstation_skipped: bool,
    pub default_tui_config_path: String,
    pub chosen_layout: crate::config::LayoutPreset,
    pub chosen_langmap: Option<String>,
    pub selected_index: usize,
    pub input_buffer: InputEditor,
    pub input_active: bool,
    pub input_langmap: bool,
    pub show_key: bool,
    pub status_message: Option<String>,
    pub error_message: Option<String>,
}

/// Redacts the credential buffer: the onboarding input holds the API key while
/// the developer is typing it, so a raw `{:?}` must never reveal it.
impl std::fmt::Debug for OnboardingState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let input_repr = format!(
            "<redacted: {} chars>",
            self.input_buffer.get_text().chars().count()
        );
        f.debug_struct("OnboardingState")
            .field("step", &self.step)
            .field("has_git", &self.has_git)
            .field("has_config", &self.has_config)
            .field("config_skipped", &self.config_skipped)
            .field("config_path", &self.config_path)
            .field("default_config_path", &self.default_config_path)
            .field("has_api_key", &self.has_api_key)
            .field("credentials_path", &self.credentials_path)
            .field("default_credentials_path", &self.default_credentials_path)
            .field("has_tui_config", &self.has_tui_config)
            .field("workstation_skipped", &self.workstation_skipped)
            .field("default_tui_config_path", &self.default_tui_config_path)
            .field("chosen_layout", &self.chosen_layout)
            .field("chosen_langmap", &self.chosen_langmap)
            .field("selected_index", &self.selected_index)
            .field("input_buffer", &input_repr)
            .field("input_active", &self.input_active)
            .field("input_langmap", &self.input_langmap)
            .field("show_key", &self.show_key)
            .field("status_message", &self.status_message)
            .field("error_message", &self.error_message)
            .finish()
    }
}

impl Default for OnboardingState {
    fn default() -> Self {
        let has_tui = crate::config::find_tui_config_path().is_some();
        let default_tui = crate::config::default_tui_config_path()
            .display()
            .to_string();
        Self {
            step: OnboardingStep::Config,
            has_git: true,
            has_config: false,
            config_skipped: false,
            config_path: None,
            default_config_path: "tauqe.toml".to_string(),
            has_api_key: false,
            credentials_path: None,
            default_credentials_path: "~/.config/tauqe/credentials.toml".to_string(),
            has_tui_config: has_tui,
            workstation_skipped: false,
            default_tui_config_path: default_tui,
            chosen_layout: crate::config::LayoutPreset::None,
            chosen_langmap: None,
            selected_index: 0,
            input_buffer: InputEditor::default(),
            input_active: false,
            input_langmap: false,
            show_key: false,
            status_message: None,
            error_message: None,
        }
    }
}

impl OnboardingState {
    /// Determines the next onboarding step from the `has_*` completion flags.
    ///
    /// Canonical order: Git -> Config -> Credentials -> Workstation -> Ready.
    /// The Modifier sub-step is entered explicitly after a layout choice on the
    /// Workstation step. Optional phases the developer deliberately dismissed
    /// are recorded via `config_skipped` / `workstation_skipped`, so the flow
    /// never loops back to a phase the developer already left.
    pub fn next_step(&self) -> OnboardingStep {
        if !self.has_git {
            return OnboardingStep::Git;
        }
        if !self.has_config && !self.config_skipped {
            return OnboardingStep::Config;
        }
        if !self.has_api_key {
            return OnboardingStep::Credentials;
        }
        if !self.has_tui_config && !self.workstation_skipped {
            return OnboardingStep::Workstation;
        }
        OnboardingStep::Ready
    }

    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status_message = Some(msg.into());
        self.error_message = None;
    }

    pub fn set_error(&mut self, err: impl Into<String>) {
        self.error_message = Some(err.into());
        self.status_message = None;
    }

    pub fn clear_messages(&mut self) {
        self.status_message = None;
        self.error_message = None;
    }

    /// Applies the authoritative flags and paths reported by the server.
    pub fn apply_status(&mut self, status: &SystemStatusResult) {
        self.has_git = status.has_git;
        self.has_config = status.has_config;
        self.has_api_key = status.has_api_key;
        if status.config_path.is_some() {
            self.config_path = status.config_path.clone();
        }
        if status.credentials_path.is_some() {
            self.credentials_path = status.credentials_path.clone();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SquashBaseMode {
    #[default]
    Session,
    Upstream,
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SquashDialogFocus {
    #[default]
    FileList,
    DiffView,
    MessageEditor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SquashConfirm {
    Apply,
    Discard,
}

#[derive(Debug, Clone)]
pub struct SquashFileItem {
    pub path: String,
    pub diff: String,
    pub expanded: bool,
}

#[derive(Debug, Clone)]
pub struct SquashDialogState {
    pub loading: bool,
    pub generating_message: bool,
    pub applying: bool,
    pub base_mode: SquashBaseMode,
    pub base_ref: String,
    pub pending_base: Option<(SquashBaseMode, String)>,
    pub session_base: Option<String>,
    pub upstream_base: Option<String>,
    pub custom_editor: InputEditor,
    pub custom_input_active: bool,
    pub commits: Vec<tauqe_protocol::GitSquashCommitItem>,
    pub diff_stat: String,
    pub files: Vec<SquashFileItem>,
    pub selected_file_index: usize,
    pub diff_scroll: u16,
    pub message_editor: InputEditor,
    pub status_message: Option<String>,
    pub focus: SquashDialogFocus,
    pub confirm: Option<SquashConfirm>,
    pub confirm_button: ConfirmDialogButton,
}

impl Default for SquashDialogState {
    fn default() -> Self {
        Self {
            loading: true,
            generating_message: false,
            applying: false,
            base_mode: SquashBaseMode::Session,
            base_ref: String::new(),
            pending_base: None,
            session_base: None,
            upstream_base: None,
            custom_editor: InputEditor::default(),
            custom_input_active: false,
            commits: Vec::new(),
            diff_stat: String::new(),
            files: Vec::new(),
            selected_file_index: 0,
            diff_scroll: 0,
            message_editor: InputEditor::default(),
            status_message: Some(active_t("dialog.squash.status_inspecting")),
            focus: SquashDialogFocus::FileList,
            confirm: None,
            confirm_button: ConfirmDialogButton::Cancel,
        }
    }
}

/// Inputs required to construct an [`AppState`].
///
/// Only the fields that carry initialization signal from the handshake are
/// listed; [`AppState::new`] fills every transient or default-able field. This
/// keeps one construction path shared by production and tests instead of a
/// fifty-field struct literal duplicated across the crate.
pub struct AppInit {
    pub view_mode: ViewMode,
    pub protocol_version: String,
    pub repo_state: Option<RepositoryState>,
    pub all_repo_files: Vec<String>,
    pub workflow: tauqe_protocol::WorkflowKind,
    pub edit_protocol: tauqe_protocol::EditProtocolKind,
    pub active_model: ModelRef,
    pub available_workflows: Vec<tauqe_protocol::WorkflowKind>,
    pub available_edit_protocols: Vec<tauqe_protocol::EditProtocolKind>,
    pub context: ContextState,
    pub onboarding: OnboardingState,
    pub input_editor: InputEditor,
    pub tui_config: TuiConfig,
    pub i18n: I18nEngine,
    pub server_log_path: std::path::PathBuf,
}

pub struct AppState {
    pub view_mode: ViewMode,
    pub protocol_version: String,
    pub repo_state: Option<RepositoryState>,
    pub all_repo_files: Vec<String>,
    pub workflow: tauqe_protocol::WorkflowKind,
    pub edit_protocol: tauqe_protocol::EditProtocolKind,
    pub active_model: ModelRef,
    pub available_workflows: Vec<tauqe_protocol::WorkflowKind>,
    pub available_edit_protocols: Vec<tauqe_protocol::EditProtocolKind>,
    pub model: ReplView,
    pub context: ContextState,
    pub context_view: ContextViewState,
    pub history_view: HistoryViewState,
    pub review: ReviewViewState,
    /// Review presets declared by the project, mirrored from the configuration
    /// snapshot so the launch dialog can offer them without re-reading
    /// `tauqe.toml` (c-5).
    pub review_presets: Vec<tauqe_protocol::ReviewPresetSummary>,
    pub plans_view: PlansViewState,
    pub onboarding: OnboardingState,
    pub input_editor: InputEditor,
    pub tui_config: TuiConfig,
    pub show_help: bool,
    pub help_scroll: u16,
    /// Tab currently shown by the Help modal (`Mode Shortcuts` vs `Text Editor`).
    pub help_tab: HelpTab,
    pub confirm_cancel: bool,
    pub confirm_quit: bool,
    /// Whether the batch-cancellation dialog is open.
    pub confirm_batch_cancel: bool,
    /// Action button currently focused in the batch-cancellation dialog.
    pub batch_cancel_button: BatchCancelButton,
    /// Whether a running batch has been asked to stop after its current member.
    pub batch_stopping: bool,
    pub confirm_detach_focus: bool,
    /// Plan awaiting the first-tier implementation-horizon clear confirmation.
    pub confirm_clear_horizon: Option<String>,
    /// Plan whose horizon clear escalated to the second tier because work was
    /// already recorded in Git.
    pub confirm_clear_horizon_final: Option<String>,
    pub confirm_clear_history: bool,
    pub confirm_delete_plan: Option<String>,
    /// Plan whose deletion escalated to the second tier because unfinished work
    /// would be discarded.
    pub confirm_delete_plan_final: Option<String>,
    pub confirm_delete_review: Option<String>,
    /// Session awaiting confirmation of the costly Architect post-analysis.
    pub confirm_review_analyze: Option<ConfirmReviewAnalyzeState>,
    pub confirm_execute_scope: Option<ConfirmExecuteScopeState>,
    pub confirm_execute_pipeline: Option<ConfirmExecutePipelineState>,
    pub confirm_execute_review: Option<ConfirmExecuteReviewState>,
    pub confirm_button: ConfirmDialogButton,
    pub selection_dialog: Option<SelectionDialogState>,
    pub status_dialog: Option<StatusDialogState>,
    pub squash_dialog: Option<SquashDialogState>,
    pub review_dialog: Option<ReviewDialogState>,
    pub prompt_modal: Option<PromptModalState>,
    /// Fullscreen two-pane audit of the change set the Develop viewport stages.
    pub diff_inspector: Option<DiffInspectorState>,
    pub notification: Option<AppNotification>,
    pub turn_started_at: Option<std::time::Instant>,
    pub server_disconnected: Option<String>,
    pub server_log_path: std::path::PathBuf,
    pub last_model_height: u16,
    pub last_frame_area: Rect,
    /// Interactive regions declared by the components of the current frame.
    ///
    /// The registry is emptied before every frame and filled exclusively by the
    /// components that draw, so pointer dispatch never depends on a hard-coded
    /// coordinate table and no stale region outlives the frame that declared it
    /// (c-1, c-3).
    pub hit_regions: HitRegistry,
    pub terminal_focused: bool,
    pub shift_tip_shown: bool,
    pub model_choice: ModelChoice,
    /// Structured identity of the model currently executing a turn, if any.
    pub running_model: Option<RunningModel>,
    /// Active discussion focus anchor mirrored from the server.
    ///
    /// Drives the REPL focus banner and the empty-prompt `Esc` detach shortcut.
    /// The server owns the authoritative value and broadcasts every change via
    /// `discussion/focusChanged`; the client only mirrors it for rendering.
    pub discussion_focus: DiscussionFocus,
    /// Active theme preset currently painted across the interface.
    pub active_theme_preset: ThemePreset,
    /// Interactive theme preset selector, when open.
    pub theme_selector: Option<ThemeSelectorState>,
    /// Runtime localization engine: canonical English plus the active custom locale.
    pub i18n: I18nEngine,
    /// Interface-language (locale) dialog, when open.
    pub locale_dialog: Option<LocaleDialogState>,
    /// Full usage telemetry breakdown dialog, when open.
    pub usage_summary: Option<UsageSummaryState>,
}

impl AppState {
    /// Builds an application state from its handshake-derived inputs, filling
    /// every transient field with a default. Shared by production startup and
    /// tests so the field set is defined in exactly one place.
    pub fn new(init: AppInit) -> Self {
        let active_theme_preset = ThemePreset::resolve(
            init.tui_config.theme.name.as_deref(),
            init.tui_config.theme.mode.resolve(),
        );
        let state = Self {
            view_mode: init.view_mode,
            protocol_version: init.protocol_version,
            repo_state: init.repo_state,
            all_repo_files: init.all_repo_files,
            workflow: init.workflow,
            edit_protocol: init.edit_protocol,
            active_model: init.active_model,
            available_workflows: init.available_workflows,
            available_edit_protocols: init.available_edit_protocols,
            model: ReplView::default(),
            context: init.context,
            context_view: ContextViewState::default(),
            history_view: HistoryViewState::default(),
            review: ReviewViewState::default(),
            review_presets: Vec::new(),
            plans_view: PlansViewState::default(),
            onboarding: init.onboarding,
            input_editor: init.input_editor,
            tui_config: init.tui_config,
            show_help: false,
            help_scroll: 0,
            help_tab: HelpTab::default(),
            confirm_cancel: false,
            confirm_quit: false,
            confirm_batch_cancel: false,
            batch_cancel_button: BatchCancelButton::Continue,
            batch_stopping: false,
            confirm_detach_focus: false,
            confirm_clear_horizon: None,
            confirm_clear_horizon_final: None,
            confirm_clear_history: false,
            confirm_delete_plan: None,
            confirm_delete_plan_final: None,
            confirm_delete_review: None,
            confirm_review_analyze: None,
            confirm_execute_scope: None,
            confirm_execute_pipeline: None,
            confirm_execute_review: None,
            confirm_button: ConfirmDialogButton::Cancel,
            selection_dialog: None,
            status_dialog: None,
            squash_dialog: None,
            review_dialog: None,
            prompt_modal: None,
            diff_inspector: None,
            notification: None,
            turn_started_at: None,
            server_disconnected: None,
            server_log_path: init.server_log_path,
            last_model_height: 10,
            last_frame_area: Rect::new(0, 0, 80, 24),
            hit_regions: HitRegistry::default(),
            terminal_focused: true,
            shift_tip_shown: false,
            model_choice: ModelChoice::default(),
            running_model: None,
            discussion_focus: DiscussionFocus::None,
            active_theme_preset,
            theme_selector: None,
            i18n: init.i18n,
            locale_dialog: None,
            usage_summary: None,
        };
        // Publish the resolved catalog to the process-wide locale view so chrome
        // rendered without `&AppState` (modal footers) shares one source of
        // truth and a hot-swapped locale reaches it too (c-1, c-5).
        crate::i18n::install_active_locale(&state.i18n);
        state
    }

    pub fn notify(
        &mut self,
        text: impl Into<String>,
        level: NotificationLevel,
        ttl: std::time::Duration,
    ) {
        if let Some(ref current) = self.notification {
            if current.created_at.elapsed() < current.ttl
                && level == NotificationLevel::Info
                && current.level > level
            {
                return;
            }
        }
        self.notification = Some(AppNotification {
            level,
            text: text.into(),
            created_at: std::time::Instant::now(),
            ttl,
        });
    }

    pub fn notify_info(&mut self, text: impl Into<String>) {
        self.notify(
            text,
            NotificationLevel::Info,
            std::time::Duration::from_secs(3),
        );
    }

    pub fn notify_success(&mut self, text: impl Into<String>) {
        self.notify(
            text,
            NotificationLevel::Success,
            std::time::Duration::from_secs(4),
        );
    }

    pub fn notify_warning(&mut self, text: impl Into<String>) {
        self.notify(
            text,
            NotificationLevel::Warning,
            std::time::Duration::from_secs(4),
        );
    }

    pub fn notify_error(&mut self, text: impl Into<String>) {
        self.notify(
            text,
            NotificationLevel::Error,
            std::time::Duration::from_secs(6),
        );
    }

    /// Opens the primary model picker presenting models configured under
    /// `[models.primary]`.
    ///
    /// The picker presents strictly the configured primary tiers: `primary` is
    /// mandatory in the configuration contract, so an empty list means the role
    /// summary has not reached the client yet. The picker then stays closed and
    /// reports that condition instead of offering an alternative model source
    /// (c-2).
    pub fn open_model_dialog(&mut self) -> bool {
        let entries = self
            .model_choice
            .roles
            .as_ref()
            .and_then(|r| r.entries.as_ref())
            .map(|e| e.primary.clone())
            .unwrap_or_default();

        if entries.is_empty() {
            self.notify_warning(active_t("notify.model_select_empty"));
            return false;
        }

        // A tier is highlighted by both of its coordinates: the same model may
        // be configured twice at different reasoning depths, so matching only
        // the model would re-open the picker on the first rung and hide the
        // depth the developer actually picked. When the exact rung has left the
        // configuration, the first entry of the same model is the closest
        // surviving highlight (c-2).
        let selected = match &self.model_choice.selection {
            ModelSelection::Tier { model, effort } => entries
                .iter()
                .position(|entry| &entry.model == model && &entry.effort == effort)
                .or_else(|| entries.iter().position(|entry| &entry.model == model))
                .unwrap_or(0),
            ModelSelection::Specific(model) => entries
                .iter()
                .position(|entry| &entry.model == model)
                .unwrap_or(0),
            ModelSelection::Role(_) => 0,
        };

        self.selection_dialog = Some(SelectionDialogState::new(entries, selected));
        true
    }

    /// Resolves the Coder model entries a review-execution dialog offers.
    ///
    /// Configured Coder entries win; otherwise the resolved Coder role (or the
    /// active model) backs the dialog so it always presents a valid selection.
    /// This mirrors the Architect ladder lookup of the decomposition dialogs, so
    /// the Coder role can never silently disappear from the picker.
    pub fn coder_entries(&self) -> Vec<tauqe_protocol::ModelEntry> {
        self.model_choice
            .roles
            .as_ref()
            .and_then(|roles| roles.entries.as_ref())
            .map(|entries| entries.coder.clone())
            .filter(|entries| !entries.is_empty())
            .unwrap_or_else(|| {
                let fallback_model = self
                    .model_choice
                    .resolve(&ModelSelection::Role(ModelRole::Coder))
                    .unwrap_or_else(|| self.active_model.clone());
                let fallback_effort = self
                    .model_choice
                    .roles
                    .as_ref()
                    .and_then(|roles| roles.efforts.as_ref())
                    .and_then(|efforts| efforts.coder);
                vec![tauqe_protocol::ModelEntry::new(
                    fallback_model,
                    fallback_effort,
                )]
            })
    }

    /// Returns the highest-priority active modal surface, or `None` when no modal
    /// is open. This is the canonical ordering consulted by keyboard dispatch,
    /// rendering, mouse hit-testing, and paste routing.
    pub fn active_modal(&self) -> Option<ActiveModal> {
        if self.server_disconnected.is_some() {
            return Some(ActiveModal::ServerDisconnected);
        }
        if self.squash_dialog.is_some() {
            return Some(ActiveModal::Squash);
        }
        if self.review_dialog.is_some() {
            return Some(ActiveModal::Review);
        }
        if self.review.refine_dialog.is_some() {
            return Some(ActiveModal::ReviewRefine);
        }
        if self.confirm_cancel {
            return Some(ActiveModal::ConfirmCancel);
        }
        if self.confirm_quit {
            return Some(ActiveModal::ConfirmQuit);
        }
        if self.confirm_batch_cancel {
            return Some(ActiveModal::BatchCancel);
        }
        if self.context_view.confirm_clear_auto {
            return Some(ActiveModal::ClearAuto);
        }
        if self.context_view.confirm_clear_user {
            return Some(ActiveModal::ClearUser);
        }
        if self.confirm_detach_focus {
            return Some(ActiveModal::ConfirmDetachFocus);
        }
        // The stricter second tier outranks the first tier, so a horizon that
        // escalated to the dangerous confirmation always owns the keyboard.
        if self.confirm_clear_horizon_final.is_some() {
            return Some(ActiveModal::ClearHorizonFinal);
        }
        if self.confirm_clear_horizon.is_some() {
            return Some(ActiveModal::ClearHorizon);
        }
        // The stricter second tier outranks the first tier, so a plan whose
        // deletion would discard unfinished work always owns the keyboard.
        if self.confirm_delete_plan_final.is_some() {
            return Some(ActiveModal::DeletePlanFinal);
        }
        if self.confirm_delete_plan.is_some() {
            return Some(ActiveModal::DeletePlan);
        }
        if self.confirm_delete_review.is_some() {
            return Some(ActiveModal::DeleteReview);
        }
        if self.confirm_review_analyze.is_some() {
            return Some(ActiveModal::ReviewAnalyze);
        }
        if self.confirm_execute_review.is_some() {
            return Some(ActiveModal::ExecuteReview);
        }
        if self.confirm_execute_pipeline.is_some() {
            return Some(ActiveModal::ExecutePipeline);
        }
        if self.confirm_execute_scope.is_some() {
            return Some(ActiveModal::ExecuteScope);
        }
        if self.plans_view.refine_dialog.is_some() {
            return Some(ActiveModal::RefineDialog);
        }
        if self.plans_view.refine_blocked.is_some() {
            return Some(ActiveModal::RefineBlocked);
        }
        if self.prompt_modal.is_some() {
            return Some(ActiveModal::Prompt);
        }
        if self.locale_dialog.is_some() {
            return Some(ActiveModal::Locale);
        }
        if self.confirm_clear_history {
            return Some(ActiveModal::ClearHistory);
        }
        if self.status_dialog.is_some() {
            return Some(ActiveModal::Status);
        }
        if self.selection_dialog.is_some() {
            return Some(ActiveModal::Selection);
        }
        // The inspector is a reading surface opened over the Develop viewport:
        // every confirmation above outranks it, while the informational
        // overlays below never displace an audit in progress (c-3).
        if self.diff_inspector.is_some() {
            return Some(ActiveModal::DiffInspector);
        }
        if self.theme_selector.is_some() {
            return Some(ActiveModal::ThemeSelector);
        }
        // The breakdown outranks Help but never displaces a confirmation, so a
        // read-only telemetry surface can never swallow a pending decision.
        if self.usage_summary.is_some() {
            return Some(ActiveModal::UsageSummary);
        }
        if self.show_help {
            return Some(ActiveModal::Help);
        }
        None
    }

    /// Whether any modal surface (or the inline Context-view file input)
    /// currently suppresses background interaction.
    pub fn has_active_modal(&self) -> bool {
        // `adding_file` is an inline Context-view input; it does not participate
        // in the modal stack, yet it still blocks background interaction.
        self.active_modal().is_some() || self.context_view.adding_file
    }

    pub fn active_notification(&self) -> Option<&AppNotification> {
        if let Some(ref notif) = self.notification {
            if notif.created_at.elapsed() < notif.ttl {
                return Some(notif);
            }
        }
        None
    }

    pub fn take_prompt(&mut self) -> Option<String> {
        if self.input_editor.is_empty() {
            return None;
        }
        if self.model.is_busy() || self.review.running {
            self.notify_warning(active_t("notify.model_busy_esc"));
            return None;
        }
        let prompt = self.input_editor.get_text().trim().to_string();
        self.input_editor.record_history(&prompt);
        self.input_editor.clear();
        self.confirm_clear_history = false;
        self.confirm_cancel = false;
        self.confirm_button = ConfirmDialogButton::Cancel;
        if prompt.is_empty() {
            return None;
        }

        self.model.reasoning.clear();
        self.model.text.clear();
        self.model.markdown_lines.clear();
        self.model.error = None;
        self.model.result = None;
        self.model.scroll = 0;
        self.model.status = TurnStatus::Awaiting;
        self.model.auto_scroll = true;

        self.model.edits_active = false;
        self.model.files.clear();
        self.model.selected_file_index = 0;
        self.model.edit_final_applied = None;
        self.model.edit_final_error = None;
        self.model.last_commit_hash = None;
        self.model.last_commit_summary = None;
        self.model.toolchain = None;
        self.model.copy_flash = None;
        self.model.code_blocks.clear();
        self.model.esc_clear_armed = None;
        self.turn_started_at = Some(std::time::Instant::now());

        Some(prompt)
    }

    /// Raises the rollback warning and arms the persistent recall hint for a
    /// prompt that never reached the model history.
    ///
    /// A failed or cancelled turn rolls its history record back, so the text
    /// survives only in the editor history. The warning states that fact and
    /// names the recall chords, and the Develop view keeps the same hint on
    /// screen until the next turn starts (c-3).
    pub fn notify_prompt_rolled_back(&mut self) {
        self.model.prompt_rolled_back = true;
        let hint = self.prompt_recall_hint();
        self.notify_warning(hint);
    }

    /// Localized hint naming the editor chords that recall a rolled-back prompt.
    ///
    /// The meta chord is projected through the configured modifiers, so the hint
    /// never advertises a key the developer did not configure (Conventions §5.2).
    pub fn prompt_recall_hint(&self) -> String {
        active_t("notify.prompt_rolled_back").replacen(
            "{}",
            self.tui_config.input.meta_modifier.emacs_prefix(),
            1,
        )
    }

    pub fn update_filtered_candidates(&mut self) {
        self.context_view
            .update_filtered_candidates(&self.context.items, &self.all_repo_files);
    }

    /// Propagates the configured theme to every view that renders Markdown,
    /// resolving `Auto` against the terminal background and applying the
    /// configured preset. Called on startup, on configuration reload, and
    /// whenever a view is rebuilt from defaults.
    pub fn sync_theme_mode(&mut self) {
        crate::markdown::set_color_depth_override(self.tui_config.theme.color_depth);
        let preset = ThemePreset::resolve(
            self.tui_config.theme.name.as_deref(),
            self.tui_config.theme.mode.resolve(),
        );
        self.apply_theme_preset(preset);
    }

    /// Applies a theme preset to the live interface (hot reload).
    ///
    /// The global projection engine and every Markdown-bearing surface are
    /// refreshed in place, so a preset swap — from a configuration reload or
    /// the interactive selector — takes effect immediately without a restart
    /// and without disturbing the prompt draft or any in-flight turn (`#c-4`).
    pub fn apply_theme_preset(&mut self, preset: ThemePreset) {
        self.active_theme_preset = preset;
        crate::theme::engine::set_active_preset(Some(preset));
        let mode = self.tui_config.theme.mode.resolve();
        self.model.set_theme_mode(mode);
        self.model.refresh_theme();
        self.history_view.theme_mode = mode;
        self.history_view.refresh_theme();
        self.plans_view.theme_mode = mode;
        self.review.theme_mode = mode;
    }

    /// Opens the interactive theme preset selector, seeded on the active preset.
    pub fn open_theme_selector(&mut self) {
        self.theme_selector = Some(ThemeSelectorState::new(self.active_theme_preset));
    }

    /// Opens the full usage breakdown dialog at its top.
    ///
    /// Reading telemetry never spends tokens and never mutates domain state, so
    /// the dialog opens regardless of a running turn and only mirrors the latest
    /// daemon snapshot (c-4).
    pub fn open_usage_summary(&mut self) {
        self.usage_summary = Some(UsageSummaryState::default());
    }

    /// Opens the interface-language (locale) dialog.
    ///
    /// Blocked when a model turn or review is already running to avoid
    /// conflicting with active operations, and preserves in-flight locale
    /// synthesis if one is already generating.
    pub fn open_locale_dialog(&mut self) {
        if self.model.is_busy() || self.review.running {
            self.notify_warning(active_t("notify.locale_busy"));
            return;
        }
        if self.locale_dialog.as_ref().is_some_and(|d| d.generating) {
            return;
        }
        self.locale_dialog = Some(LocaleDialogState::default());
    }

    /// Hot-swaps the active custom locale, taking effect immediately (c-5).
    pub fn apply_custom_locale(&mut self, locale: crate::i18n::CustomLocale) {
        self.i18n.apply_custom_locale(locale);
        crate::i18n::install_active_locale(&self.i18n);
    }

    /// Deactivates the custom locale, reverting to embedded English (c-5).
    pub fn reset_locale_to_english(&mut self) {
        self.i18n.reset_locale_to_english();
        crate::i18n::install_active_locale(&self.i18n);
    }
}

/// Shared test fixture: a fully constructed [`AppState`] with deterministic
/// defaults that every unit test can adjust field by field.
#[cfg(test)]
pub(crate) fn test_state() -> AppState {
    AppState::new(AppInit {
        view_mode: ViewMode::Repl,
        protocol_version: "1.0".to_string(),
        repo_state: None,
        all_repo_files: Vec::new(),
        workflow: tauqe_protocol::WorkflowKind::Git,
        edit_protocol: tauqe_protocol::EditProtocolKind::Xml,
        active_model: ModelRef::openrouter("test-model"),
        available_workflows: Vec::new(),
        available_edit_protocols: Vec::new(),
        context: ContextState::default(),
        onboarding: OnboardingState::default(),
        input_editor: InputEditor::default(),
        tui_config: TuiConfig::default(),
        i18n: I18nEngine::english(),
        server_log_path: std::path::PathBuf::from(".tauqe/server.log"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_apply_theme_preset_hot_swaps_without_touching_draft() {
        let mut st = test_state();
        st.input_editor.insert_str("unsubmitted draft");

        st.apply_theme_preset(ThemePreset::Gruvbox);
        assert_eq!(st.active_theme_preset, ThemePreset::Gruvbox);
        // A hot theme swap never disturbs the developer's unsubmitted prompt.
        assert_eq!(st.input_editor.get_text(), "unsubmitted draft");
    }

    #[test]
    fn test_sync_theme_mode_reads_preset_from_configuration() {
        let mut st = test_state();
        st.tui_config.theme.name = Some("catppuccin".to_string());
        st.sync_theme_mode();
        assert_eq!(st.active_theme_preset, ThemePreset::Catppuccin);
    }

    /// The full usage breakdown outranks Help but never displaces a
    /// confirmation, and reopening always seeds a fresh scroll offset (c-4).
    #[test]
    fn test_usage_summary_owns_the_keyboard_above_help() {
        let mut st = test_state();
        assert!(st.usage_summary.is_none());

        st.open_usage_summary();
        assert_eq!(st.active_modal(), Some(ActiveModal::UsageSummary));
        assert!(st.has_active_modal());

        // Help is the lowest-priority surface and never displaces the breakdown.
        st.show_help = true;
        assert_eq!(st.active_modal(), Some(ActiveModal::UsageSummary));

        st.usage_summary = None;
        assert_eq!(st.active_modal(), Some(ActiveModal::Help));

        st.show_help = false;
        assert_eq!(st.active_modal(), None);

        // A dialog left scrolled reopens at its top.
        st.usage_summary = Some(UsageSummaryState { scroll: 42 });
        st.open_usage_summary();
        assert_eq!(st.usage_summary.as_ref().unwrap().scroll, 0);
    }

    #[test]
    fn test_open_locale_dialog_blocked_when_model_busy() {
        let mut st = test_state();
        st.model.status = TurnStatus::Responding;

        st.open_locale_dialog();
        assert!(st.locale_dialog.is_none());
        let notif = st.active_notification().expect("warning notification");
        assert!(notif.text.contains("Cannot change locale"));
    }

    #[test]
    fn test_open_locale_dialog_does_not_wipe_generating_state() {
        let mut st = test_state();
        let mut dialog = LocaleDialogState {
            generating: true,
            ..Default::default()
        };
        dialog.language_editor.insert_str("French");
        st.locale_dialog = Some(dialog);

        st.open_locale_dialog();
        let current = st.locale_dialog.as_ref().expect("dialog remains present");
        assert!(current.generating);
        assert_eq!(current.language_editor.get_text(), "French");
    }

    #[test]
    fn test_onboarding_next_step_deterministic_order() {
        let mut state = OnboardingState {
            has_git: false,
            has_config: false,
            has_api_key: false,
            has_tui_config: false,
            ..OnboardingState::default()
        };
        assert_eq!(state.next_step(), OnboardingStep::Git);

        state.has_git = true;
        assert_eq!(state.next_step(), OnboardingStep::Config);

        state.has_config = true;
        assert_eq!(state.next_step(), OnboardingStep::Credentials);

        state.has_api_key = true;
        assert_eq!(state.next_step(), OnboardingStep::Workstation);

        state.has_tui_config = true;
        assert_eq!(state.next_step(), OnboardingStep::Ready);
    }

    #[test]
    fn test_onboarding_next_step_honors_skipped_optional_phases() {
        let mut state = OnboardingState {
            has_git: true,
            has_config: false,
            config_skipped: true,
            has_api_key: true,
            has_tui_config: false,
            ..OnboardingState::default()
        };
        assert_eq!(state.next_step(), OnboardingStep::Workstation);

        state.workstation_skipped = true;
        assert_eq!(state.next_step(), OnboardingStep::Ready);
    }

    #[test]
    fn test_modal_actions_state_defaults_to_cancel() {
        let state = ModalActionsState::new();
        assert_eq!(state.focused_button, ConfirmDialogButton::Cancel);
        assert!(state.is_cancel());
        assert!(!state.is_confirm());

        let mut toggled = state;
        toggled.toggle();
        assert_eq!(toggled.focused_button, ConfirmDialogButton::Confirm);
        assert!(toggled.is_confirm());

        toggled.reset_to_cancel();
        assert_eq!(toggled.focused_button, ConfirmDialogButton::Cancel);
    }

    #[test]
    fn test_modal_focus_toggle() {
        let mut focus = ModalFocus::default();
        assert_eq!(focus, ModalFocus::Editor);
        assert!(focus.is_editor());
        assert!(!focus.is_actions());

        focus.toggle();
        assert_eq!(focus, ModalFocus::Actions);
        assert!(focus.is_actions());

        focus.toggle();
        assert_eq!(focus, ModalFocus::Editor);
    }

    #[test]
    fn test_help_open_hint_tracks_primary_modifier() {
        assert_eq!(HELP_OPEN_CHORD, "C-H");
        assert_eq!(help_open_hint(crate::config::PrimaryModifier::Ctrl), "C-H");
        assert_eq!(help_open_hint(crate::config::PrimaryModifier::Alt), "M-H");
    }

    #[test]
    fn test_take_prompt_when_busy_preserves_input() {
        let mut state = test_state();
        state.model.status = TurnStatus::Thinking;

        state.input_editor.insert_str("next planned prompt");
        assert!(state.model.is_busy());

        let result = state.take_prompt();
        assert_eq!(result, None);
        assert_eq!(state.input_editor.get_text(), "next planned prompt");
        assert!(state.active_notification().is_some());

        state.model.status = TurnStatus::Done;
        assert!(!state.model.is_busy());

        let result = state.take_prompt();
        assert_eq!(result, Some("next planned prompt".to_string()));
        assert!(state.input_editor.is_empty());
    }

    #[test]
    fn test_notify_prompt_rolled_back_names_the_configured_recall_chord() {
        let mut st = test_state();
        assert!(!st.model.prompt_rolled_back);

        st.notify_prompt_rolled_back();

        assert!(
            st.model.prompt_rolled_back,
            "the Develop view keeps the hint until the next turn starts"
        );
        let notif = st.active_notification().expect("warning notification");
        assert_eq!(notif.level, NotificationLevel::Warning);
        assert!(notif.text.contains("[↑]"));
        assert!(
            notif.text.contains("M-P"),
            "the default meta chord is Alt (M-)"
        );

        // Re-projecting through the configured meta modifier swaps the chord.
        st.tui_config.input.meta_modifier = crate::config::MetaModifier::Ctrl;
        let hint = st.prompt_recall_hint();
        assert!(hint.contains("C-P"));
        assert!(!hint.contains("M-P"));
    }

    #[test]
    fn test_notification_level_priority_preserves_error() {
        let mut state = test_state();

        state.notify_error("Critical failure");
        assert_eq!(
            state.active_notification().unwrap().text,
            "Critical failure"
        );

        // Lower-priority info should not overwrite active error
        state.notify_info("Minor info");
        assert_eq!(
            state.active_notification().unwrap().text,
            "Critical failure"
        );

        // Success feedback is never swallowed by an active error
        state.notify_success("Copied");
        assert_eq!(state.active_notification().unwrap().text, "Copied");

        // Equal or higher priority replaces it
        state.notify_error("New critical failure");
        assert_eq!(
            state.active_notification().unwrap().text,
            "New critical failure"
        );
    }

    #[test]
    fn test_selection_dialog_navigation_keeps_the_highlighted_rung_visible() {
        let entries: Vec<tauqe_protocol::ModelEntry> = (0..20)
            .map(|i| {
                tauqe_protocol::ModelEntry::new(ModelRef::openrouter(format!("model-{}", i)), None)
            })
            .collect();
        let mut dialog = SelectionDialogState::new(entries, 0);
        let visible_height = 5;

        assert_eq!(dialog.selector.selected_index(), 0);
        assert_eq!(dialog.selector.visible_start(visible_height), 0);

        for _ in 0..4 {
            dialog.selector.select_next();
        }
        assert_eq!(dialog.selector.selected_index(), 4);
        assert_eq!(dialog.selector.visible_start(visible_height), 0);

        dialog.selector.select_next();
        assert_eq!(dialog.selector.selected_index(), 5);
        assert_eq!(dialog.selector.visible_start(visible_height), 1);

        // Past the last rung the highlight wraps to the first one and the window
        // follows it, exactly as in the role dialogs (c-1).
        dialog.selector.select_index(19);
        assert_eq!(dialog.selector.visible_start(visible_height), 15);
        dialog.selector.select_next();
        assert_eq!(dialog.selector.selected_index(), 0);
        assert_eq!(dialog.selector.visible_start(visible_height), 0);

        dialog.selector.select_prev();
        assert_eq!(dialog.selector.selected_index(), 19);
        assert_eq!(dialog.selector.visible_start(visible_height), 15);
    }

    #[test]
    fn test_model_choice_resolve_semantic_roles() {
        let choice = ModelChoice {
            selection: ModelSelection::Role(ModelRole::Architect),
            roles: Some(ModelRolesSummary {
                scribe: ModelRef::openrouter("scribe-model"),
                coder: ModelRef::openrouter("coder-model"),
                architect: ModelRef::openrouter("architect-model"),
                primary: ModelRef::openrouter("primary-model"),
                reviewer: ModelRef::openrouter("reviewer-model"),
                efforts: None,
                entries: None,
            }),
        };

        assert_eq!(
            choice.resolve(&ModelSelection::Role(ModelRole::Scribe)),
            Some(ModelRef::openrouter("scribe-model"))
        );
        assert_eq!(
            choice.resolve(&ModelSelection::Role(ModelRole::Coder)),
            Some(ModelRef::openrouter("coder-model"))
        );
        assert_eq!(
            choice.resolve(&ModelSelection::Role(ModelRole::Architect)),
            Some(ModelRef::openrouter("architect-model"))
        );
        assert_eq!(
            choice.resolve(&ModelSelection::Role(ModelRole::Primary)),
            Some(ModelRef::openrouter("primary-model"))
        );
        assert_eq!(
            choice.resolve(&ModelSelection::Role(ModelRole::Reviewer)),
            Some(ModelRef::openrouter("reviewer-model"))
        );
    }

    #[test]
    fn test_open_model_dialog_presents_primary_entries() {
        let mut state = test_state();
        let primary_entry =
            tauqe_protocol::ModelEntry::new(ModelRef::openrouter("primary-model"), None);
        state.model_choice = ModelChoice {
            selection: ModelSelection::Role(ModelRole::Primary),
            roles: Some(ModelRolesSummary {
                scribe: ModelRef::openrouter("scribe-model"),
                coder: ModelRef::openrouter("coder-model"),
                architect: ModelRef::openrouter("architect-model"),
                primary: ModelRef::openrouter("primary-model"),
                reviewer: ModelRef::openrouter("reviewer-model"),
                efforts: None,
                entries: Some(tauqe_protocol::ModelRolesEntries {
                    primary: vec![primary_entry.clone()],
                    ..Default::default()
                }),
            }),
        };

        assert!(state.open_model_dialog());
        let dialog = state.selection_dialog.expect("dialog should be opened");
        assert_eq!(dialog.selector.len(), 1);
        assert_eq!(dialog.selector.selected_entry(), Some(&primary_entry));
        assert_eq!(dialog.selector.selected_index(), 0);
    }

    /// The picker is built strictly from `[models.primary]`: entries configured
    /// for another role never leak in, and no provider-catalog rung exists to
    /// widen the list beyond the configured ladder (c-1, c-2).
    #[test]
    fn test_open_model_dialog_presents_only_the_primary_ladder() {
        let primary = vec![
            tauqe_protocol::ModelEntry::new(ModelRef::openrouter("primary-a"), None),
            tauqe_protocol::ModelEntry::new(
                ModelRef::openrouter("primary-b"),
                Some(ReasoningEffort::High),
            ),
        ];
        let mut state = test_state();
        state.model_choice = ModelChoice {
            selection: ModelSelection::Role(ModelRole::Primary),
            roles: Some(ModelRolesSummary {
                scribe: ModelRef::openrouter("scribe-model"),
                coder: ModelRef::openrouter("coder-model"),
                architect: ModelRef::openrouter("architect-model"),
                primary: ModelRef::openrouter("primary-a"),
                reviewer: ModelRef::openrouter("reviewer-model"),
                efforts: None,
                entries: Some(tauqe_protocol::ModelRolesEntries {
                    primary: primary.clone(),
                    coder: vec![tauqe_protocol::ModelEntry::new(
                        ModelRef::openrouter("coder-model"),
                        None,
                    )],
                    ..Default::default()
                }),
            }),
        };

        assert!(state.open_model_dialog());
        let mut dialog = state.selection_dialog.expect("dialog should be opened");

        // Exactly the primary rungs are selectable: the coder entry configured
        // alongside them is not a picker row.
        assert_eq!(dialog.selector.len(), primary.len());
        for (index, entry) in primary.iter().enumerate() {
            dialog.selector.select_index(index);
            assert_eq!(dialog.selector.selected_entry(), Some(entry));
        }
    }

    /// The picker presents strictly the configured primary tiers: an empty list
    /// keeps it closed and reports the condition instead of offering an
    /// alternative model source (c-2).
    #[test]
    fn test_open_model_dialog_reports_an_empty_primary_list() {
        let mut state = test_state();

        assert!(!state.open_model_dialog());
        assert!(
            state.selection_dialog.is_none(),
            "the picker must not open without configured primary tiers"
        );
        let notif = state
            .active_notification()
            .expect("the closed picker explains why");
        assert_eq!(notif.level, NotificationLevel::Warning);
        assert!(notif.text.contains("[models.primary]"));
    }

    /// Reopening the picker highlights the rung the developer picked, not the
    /// first entry that happens to share its model (c-2).
    #[test]
    fn test_open_model_dialog_highlights_the_selected_tier() {
        let model = ModelRef::openrouter("primary-model");
        let weak = tauqe_protocol::ModelEntry::new(model.clone(), Some(ReasoningEffort::Low));
        let strong = tauqe_protocol::ModelEntry::new(model.clone(), Some(ReasoningEffort::High));

        let mut state = test_state();
        state.model_choice = ModelChoice {
            selection: ModelSelection::Role(ModelRole::Primary),
            roles: Some(ModelRolesSummary {
                scribe: ModelRef::openrouter("scribe-model"),
                coder: ModelRef::openrouter("coder-model"),
                architect: ModelRef::openrouter("architect-model"),
                primary: model.clone(),
                reviewer: ModelRef::openrouter("reviewer-model"),
                efforts: None,
                entries: Some(tauqe_protocol::ModelRolesEntries {
                    primary: vec![weak.clone(), strong.clone()],
                    ..Default::default()
                }),
            }),
        };

        // The second rung of the same model owns the highlight, so the depth on
        // screen never disagrees with the depth that will be sent.
        state.model_choice.selection = ModelSelection::Tier {
            model: model.clone(),
            effort: Some(ReasoningEffort::High),
        };
        assert!(state.open_model_dialog());
        let dialog = state
            .selection_dialog
            .clone()
            .expect("dialog should be opened");
        assert_eq!(dialog.selector.selected_index(), 1);
        assert_eq!(dialog.selector.selected_entry(), Some(&strong));

        // The first rung stays reachable for the same resolved model.
        state.model_choice.selection = ModelSelection::Tier {
            model: model.clone(),
            effort: Some(ReasoningEffort::Low),
        };
        assert!(state.open_model_dialog());
        let dialog = state
            .selection_dialog
            .clone()
            .expect("dialog should be opened");
        assert_eq!(dialog.selector.selected_index(), 0);
        assert_eq!(dialog.selector.selected_entry(), Some(&weak));

        // A rung whose depth left the configuration still highlights its model
        // instead of falling back to an unrelated entry.
        state.model_choice.selection = ModelSelection::Tier {
            model: model.clone(),
            effort: Some(ReasoningEffort::Max),
        };
        assert!(state.open_model_dialog());
        let dialog = state
            .selection_dialog
            .clone()
            .expect("dialog should be opened");
        assert_eq!(dialog.selector.selected_index(), 0);
        assert_eq!(dialog.selector.selected_entry(), Some(&weak));
    }

    #[test]
    fn test_onboarding_debug_redacts_credential_buffer() {
        let mut state = OnboardingState::default();
        state.input_buffer.insert_str("sk-or-secret-key");

        let rendered = format!("{:?}", state);
        assert!(!rendered.contains("sk-or-secret-key"));
        assert!(rendered.contains("redacted"));
    }

    #[test]
    fn test_active_modal_single_source_of_truth() {
        let mut st = test_state();

        // No modal open: nothing is active.
        assert_eq!(st.active_modal(), None);
        assert!(!st.has_active_modal());

        // Help is the lowest-priority surface.
        st.show_help = true;
        assert_eq!(st.active_modal(), Some(ActiveModal::Help));
        assert!(st.has_active_modal());

        // A confirmation outranks Help, so the visible window receives input.
        st.confirm_quit = true;
        assert_eq!(st.active_modal(), Some(ActiveModal::ConfirmQuit));

        // A disconnected transport blocks every other surface.
        st.server_disconnected = Some("closed".to_string());
        assert_eq!(st.active_modal(), Some(ActiveModal::ServerDisconnected));

        st.server_disconnected = None;
        st.confirm_quit = false;
        st.show_help = false;
        assert_eq!(st.active_modal(), None);

        // The inline Context input still suppresses background interaction.
        st.context_view.adding_file = true;
        assert_eq!(st.active_modal(), None);
        assert!(st.has_active_modal());
    }

    /// The diff inspector is a read-only audit surface: it owns the keyboard
    /// while it is open, above every informational overlay, and a pending
    /// confirmation still outranks it so an interrupt is never swallowed (c-3).
    #[test]
    fn test_diff_inspector_stacks_below_confirmations() {
        let mut st = test_state();
        st.diff_inspector = Some(DiffInspectorState {
            files: Vec::new(),
            selected_file: 0,
            diff_scroll: 0,
        });

        assert_eq!(st.active_modal(), Some(ActiveModal::DiffInspector));
        assert!(st.has_active_modal());

        // Reading surfaces below it never displace an audit in progress.
        st.show_help = true;
        st.open_usage_summary();
        assert_eq!(st.active_modal(), Some(ActiveModal::DiffInspector));

        // A pending confirmation always outranks the inspector.
        st.confirm_quit = true;
        assert_eq!(st.active_modal(), Some(ActiveModal::ConfirmQuit));

        st.confirm_quit = false;
        st.diff_inspector = None;
        assert_eq!(st.active_modal(), Some(ActiveModal::UsageSummary));
    }

    #[test]
    fn test_clear_user_modal_priority() {
        let mut st = test_state();

        // The User-context clear confirmation owns its own surface.
        st.context_view.confirm_clear_user = true;
        assert_eq!(st.active_modal(), Some(ActiveModal::ClearUser));

        // Help stays a lower-priority surface than the confirmation.
        st.show_help = true;
        assert_eq!(st.active_modal(), Some(ActiveModal::ClearUser));
    }

    #[test]
    fn test_clear_horizon_modal_priority() {
        let mut st = test_state();

        st.confirm_clear_horizon = Some("plan-1".to_string());
        assert_eq!(st.active_modal(), Some(ActiveModal::ClearHorizon));
        assert!(st.has_active_modal());

        // Both tiers armed at once: the stricter second (dangerous) tier wins
        // so the riskier confirmation always owns the keyboard.
        st.confirm_clear_horizon_final = Some("plan-1".to_string());
        assert_eq!(st.active_modal(), Some(ActiveModal::ClearHorizonFinal));

        st.confirm_clear_horizon = None;
        assert_eq!(st.active_modal(), Some(ActiveModal::ClearHorizonFinal));

        st.confirm_clear_horizon_final = None;
        assert_eq!(st.active_modal(), None);
    }

    /// Destructive plan deletion keeps the two-tier model: the escalated target
    /// outranks the first tier, so the stricter confirmation always owns the
    /// keyboard (c-1).
    #[test]
    fn test_delete_plan_final_modal_priority() {
        let mut st = test_state();

        st.confirm_delete_plan = Some("plan-1".to_string());
        assert_eq!(st.active_modal(), Some(ActiveModal::DeletePlan));

        st.confirm_delete_plan_final = Some("plan-1".to_string());
        assert_eq!(st.active_modal(), Some(ActiveModal::DeletePlanFinal));

        st.confirm_delete_plan = None;
        assert_eq!(st.active_modal(), Some(ActiveModal::DeletePlanFinal));

        st.confirm_delete_plan_final = None;
        assert_eq!(st.active_modal(), None);
    }

    #[test]
    fn test_batch_cancel_dialog_priority_and_defaults() {
        let mut st = test_state();
        assert_eq!(st.batch_cancel_button, BatchCancelButton::Continue);

        st.confirm_batch_cancel = true;
        assert_eq!(st.active_modal(), Some(ActiveModal::BatchCancel));
        assert!(st.has_active_modal());

        // A later confirmation outranks it so the top-most surface always owns
        // the keyboard.
        st.confirm_quit = true;
        assert_eq!(st.active_modal(), Some(ActiveModal::ConfirmQuit));
    }

    #[test]
    fn test_batch_cancel_button_cycles_through_all_actions() {
        let mut button = BatchCancelButton::Continue;
        button.cycle_next();
        assert_eq!(button, BatchCancelButton::SoftStop);
        button.cycle_next();
        assert_eq!(button, BatchCancelButton::HardStop);
        button.cycle_next();
        assert_eq!(button, BatchCancelButton::Continue);

        button.cycle_prev();
        assert_eq!(button, BatchCancelButton::HardStop);
        button.cycle_prev();
        assert_eq!(button, BatchCancelButton::SoftStop);
    }

    /// Builds a review dialog over the given repository files.
    fn review_dialog_with(repo_files: &[&str], reviewers: usize) -> ReviewDialogState {
        let entries = (0..reviewers)
            .map(|i| {
                let model = ModelRef::openrouter(format!("reviewer-{i}"));
                tauqe_protocol::ModelEntry::new(model, None)
            })
            .collect();
        ReviewDialogState::new(
            0,
            0,
            entries,
            repo_files.iter().map(|path| path.to_string()).collect(),
        )
    }

    #[test]
    fn test_review_dialog_match_counter_follows_the_target_paths() {
        let mut dialog =
            review_dialog_with(&["src/lib.rs", "crates/tui/src/main.rs", "README.md"], 1);

        // An empty list narrows nothing, so the counter stays silent.
        assert_eq!(dialog.pattern_match_count(), None);
        assert!(dialog.is_pattern_launchable());

        dialog.path_inputs[0].insert_str("*.rs");
        assert_eq!(dialog.pattern_match_count(), Some(2));
        assert!(dialog.is_pattern_launchable());

        // A second row joins the union: `README.md` is selected by the new row
        // while the first one keeps its two source files, so the audit covers
        // three files and no file is counted twice (c-6).
        assert!(dialog.append_path_row());
        assert_eq!(dialog.path_active, 1);
        dialog.path_inputs[1].insert_str("README.md");
        assert_eq!(dialog.pattern_match_count(), Some(3));

        // Editing a row re-evaluates the counter, so the displayed number always
        // describes the rows a launch would send (c-6).
        dialog.path_inputs[0].clear();
        assert_eq!(dialog.pattern_match_count(), Some(1));

        dialog.path_inputs[1].clear();
        assert_eq!(dialog.pattern_match_count(), None);
        assert!(dialog.is_pattern_launchable());
    }

    #[test]
    fn test_review_dialog_gate_refuses_a_pattern_without_matches() {
        let mut dialog = review_dialog_with(&["src/lib.rs"], 1);
        dialog.path_inputs[0].insert_str("srx/**/*.rs");

        assert_eq!(dialog.pattern_match_count(), Some(0));
        assert!(
            !dialog.is_pattern_launchable(),
            "a pattern that selects no file must block the launch"
        );
    }

    #[test]
    fn test_review_dialog_focus_cycles_through_every_section() {
        // More than one reviewer: every section takes part in the cycle, in
        // visual order, and the target field stays reachable on the explicit
        // `Custom` row (c-3, c-5).
        let mut dialog = review_dialog_with(&["src/lib.rs"], 2);
        assert_eq!(dialog.focus, ReviewDialogFocus::PromptEditor);

        dialog.cycle_focus_next();
        assert_eq!(dialog.focus, ReviewDialogFocus::Actions);
        dialog.cycle_focus_next();
        assert_eq!(dialog.focus, ReviewDialogFocus::ModelList);
        dialog.cycle_focus_next();
        assert_eq!(dialog.focus, ReviewDialogFocus::PathEditor);
        dialog.cycle_focus_next();
        assert_eq!(dialog.focus, ReviewDialogFocus::PromptEditor);

        // Backward order mirrors the forward order exactly.
        dialog.cycle_focus_prev();
        assert_eq!(dialog.focus, ReviewDialogFocus::PathEditor);
        dialog.cycle_focus_prev();
        assert_eq!(dialog.focus, ReviewDialogFocus::ModelList);
        dialog.cycle_focus_prev();
        assert_eq!(dialog.focus, ReviewDialogFocus::Actions);
        dialog.cycle_focus_prev();
        assert_eq!(dialog.focus, ReviewDialogFocus::PromptEditor);
    }

    #[test]
    fn test_review_dialog_focus_skips_unavailable_sections() {
        // A single reviewer: only the two text fields and the action bar can
        // take the keyboard.
        let mut dialog = review_dialog_with(&[], 1);

        dialog.cycle_focus_next();
        assert_eq!(dialog.focus, ReviewDialogFocus::Actions);
        dialog.cycle_focus_next();
        assert_eq!(dialog.focus, ReviewDialogFocus::PathEditor);
        dialog.cycle_focus_next();
        assert_eq!(dialog.focus, ReviewDialogFocus::PromptEditor);

        dialog.cycle_focus_prev();
        assert_eq!(dialog.focus, ReviewDialogFocus::PathEditor);
        dialog.cycle_focus_prev();
        assert_eq!(dialog.focus, ReviewDialogFocus::Actions);
        dialog.cycle_focus_prev();
        assert_eq!(dialog.focus, ReviewDialogFocus::PromptEditor);
    }
    /// A highlighted preset owns the launch, so its input aid — the target
    /// field — leaves the focus cycle, while the preset section stays reachable
    /// as the mode switcher. Returning to the `Custom` row restores the full
    /// cycle (c-5).
    #[test]
    fn test_review_dialog_focus_skips_custom_sections_under_a_preset() {
        let mut dialog = review_dialog_with(&["src/lib.rs"], 1).with_presets(vec![
            tauqe_protocol::ReviewPresetSummary {
                name: "backend".to_string(),
                title: None,
            },
        ]);
        let rows = dialog.preset_selector.item_count();
        dialog.preset_selector.select_index(1, rows);
        assert!(dialog.selected_preset().is_some());
        assert!(!dialog.is_custom_launch());

        // The forward and backward walks visit only the sections that exist in
        // the preset mode.
        for _ in 0..ReviewDialogFocus::VISUAL_ORDER.len() {
            dialog.cycle_focus_next();
            assert_ne!(dialog.focus, ReviewDialogFocus::PathEditor);
        }
        for _ in 0..ReviewDialogFocus::VISUAL_ORDER.len() {
            dialog.cycle_focus_prev();
            assert_ne!(dialog.focus, ReviewDialogFocus::PathEditor);
        }

        // Returning to the `Custom` row restores the target field to the cycle.
        dialog.preset_selector.select_index(0, rows);
        assert!(dialog.is_custom_launch());
        dialog.cycle_focus_next();
        assert_eq!(dialog.focus, ReviewDialogFocus::Actions);
        dialog.cycle_focus_next();
        assert_eq!(dialog.focus, ReviewDialogFocus::PresetList);
        dialog.cycle_focus_next();
        assert_eq!(dialog.focus, ReviewDialogFocus::PathEditor);
        dialog.cycle_focus_next();
        assert_eq!(dialog.focus, ReviewDialogFocus::PromptEditor);
    }
    #[test]
    fn test_review_dialog_start_params_folds_path_and_prompt() {
        let mut dialog = ReviewDialogState::new(0, 0, Vec::new(), Vec::new());

        // Both fields start blank: neither reaches the wire.
        let params = dialog.start_params();
        assert!(params.user_prompt.is_none());
        assert!(params.path_patterns.is_empty());
        assert_eq!(serde_json::to_string(&params).unwrap(), "{}");

        dialog
            .prompt_editor
            .insert_str("  focus on error handling  ");
        dialog.path_inputs[0].insert_str(" crates/core/src/**/*.rs ");
        let params = dialog.start_params();
        assert_eq!(
            params.user_prompt.as_deref(),
            Some("focus on error handling")
        );
        assert_eq!(
            params.path_patterns,
            vec!["crates/core/src/**/*.rs".to_string()]
        );
    }
    /// The preset section joins the focus cycle only when the project declares
    /// presets, so a project without them keeps the previous dialog, and a manual
    /// (`Custom`) launch stays one row away after a preset is inspected (c-5).
    #[test]
    fn test_review_dialog_focus_cycles_through_the_preset_section() {
        let mut dialog = review_dialog_with(&["src/lib.rs"], 1).with_presets(vec![
            tauqe_protocol::ReviewPresetSummary {
                name: "backend".to_string(),
                title: Some("Backend audit".to_string()),
            },
            tauqe_protocol::ReviewPresetSummary {
                name: "frontend".to_string(),
                title: None,
            },
        ]);

        assert!(dialog.has_presets());
        assert_eq!(dialog.preset_selector.item_count(), 3);
        assert_eq!(
            dialog.preset_labels(),
            vec![
                active_t("dialog.review_launch.preset_custom"),
                "Backend audit".to_string(),
                "frontend".to_string(),
            ],
            "the Custom row leads, then every preset by its title or name"
        );
        assert!(
            dialog.selected_preset().is_none(),
            "the Custom row owns the initial highlight"
        );

        // One reviewer: the reviewer list is unavailable, so the cycle walks
        // prompt -> actions -> preset -> path.
        assert_eq!(dialog.focus, ReviewDialogFocus::PromptEditor);
        dialog.cycle_focus_next();
        assert_eq!(dialog.focus, ReviewDialogFocus::Actions);
        dialog.cycle_focus_next();
        assert_eq!(dialog.focus, ReviewDialogFocus::PresetList);
        dialog.cycle_focus_next();
        assert_eq!(dialog.focus, ReviewDialogFocus::PathEditor);
        dialog.cycle_focus_next();
        assert_eq!(dialog.focus, ReviewDialogFocus::PromptEditor);

        // The backward walk mirrors the forward one.
        dialog.cycle_focus_prev();
        assert_eq!(dialog.focus, ReviewDialogFocus::PathEditor);
        dialog.cycle_focus_prev();
        assert_eq!(dialog.focus, ReviewDialogFocus::PresetList);
        dialog.cycle_focus_prev();
        assert_eq!(dialog.focus, ReviewDialogFocus::Actions);
    }

    /// A highlighted preset owns the launch parameters: its name travels, the
    /// target pattern is dropped, and returning to `Custom` restores the previous
    /// pattern-driven launch (c-5).
    #[test]
    fn test_review_dialog_start_params_carry_the_selected_preset() {
        let mut dialog = ReviewDialogState::new(0, 0, Vec::new(), Vec::new()).with_presets(vec![
            tauqe_protocol::ReviewPresetSummary {
                name: "backend".to_string(),
                title: None,
            },
        ]);
        dialog.path_inputs[0].insert_str("crates/**/*.rs");

        // Row zero is the explicit `Custom` launch: the pattern still travels.
        let params = dialog.start_params();
        assert_eq!(params.preset, None);
        assert_eq!(params.path_patterns, vec!["crates/**/*.rs".to_string()]);

        let rows = dialog.preset_selector.item_count();
        dialog.preset_selector.select_index(1, rows);
        assert!(dialog.selected_preset().is_some());

        let params = dialog.start_params();
        assert_eq!(params.preset.as_deref(), Some("backend"));
        assert!(
            params.path_patterns.is_empty(),
            "a preset supplies its own paths, so the target field is ignored"
        );
        assert!(
            dialog.is_pattern_launchable(),
            "a preset launch is never blocked by the ignored pattern field"
        );
    }

    /// A project without presets keeps the previous dialog: the section stays
    /// collapsed, the selector owns no row, and the manual launch is unchanged
    /// (c-5).
    #[test]
    fn test_review_dialog_without_presets_keeps_the_manual_launch() {
        let mut dialog = ReviewDialogState::new(0, 0, Vec::new(), Vec::new());
        assert!(!dialog.has_presets());
        assert_eq!(dialog.preset_selector.item_count(), 0);
        assert!(dialog.preset_labels().is_empty());
        assert!(dialog.selected_preset().is_none());
        assert_eq!(dialog.start_params().preset, None);

        // The section is skipped by the focus cycle entirely.
        for _ in 0..ReviewDialogFocus::VISUAL_ORDER.len() {
            dialog.cycle_focus_next();
            assert_ne!(dialog.focus, ReviewDialogFocus::PresetList);
        }
    }
    /// The post-analysis confirmation owns the keyboard while it is open and
    /// outranks every informational overlay below it (c-4).
    #[test]
    fn test_review_analyze_confirmation_owns_the_keyboard() {
        let mut st = test_state();
        st.confirm_review_analyze = Some(ConfirmReviewAnalyzeState {
            review_id: "rev-1".to_string(),
            review_title: "Session rev-1".to_string(),
        });
        assert_eq!(st.active_modal(), Some(ActiveModal::ReviewAnalyze));
        assert!(st.has_active_modal());

        st.show_help = true;
        assert_eq!(st.active_modal(), Some(ActiveModal::ReviewAnalyze));

        st.confirm_review_analyze = None;
        assert_eq!(st.active_modal(), Some(ActiveModal::Help));
    }

    /// A row is appended only after a usable pattern, so stray empty rows never
    /// enter the list (c-3).
    #[test]
    fn test_path_rows_append_only_after_a_usable_pattern() {
        let mut dialog = review_dialog_with(&["src/lib.rs"], 1);
        assert_eq!(dialog.path_row_count(), 1);

        assert!(!dialog.append_path_row(), "an empty row refuses the append");
        dialog.path_inputs[0].insert_str("   ");
        assert!(!dialog.append_path_row(), "a blank row refuses the append");
        assert_eq!(dialog.path_row_count(), 1);

        dialog.path_inputs[0].clear();
        dialog.path_inputs[0].insert_str("src/");
        assert!(dialog.append_path_row());
        assert_eq!(dialog.path_row_count(), 2);
        assert_eq!(dialog.path_active, 1, "the new row takes the keyboard");
        assert!(dialog.active_path_editor().unwrap().is_empty());
    }

    /// `Backspace` closes an empty row and returns the keyboard to the previous
    /// one; a row holding text and the last remaining row are never removed
    /// (c-3).
    #[test]
    fn test_path_rows_remove_only_an_empty_row_and_keep_one() {
        let mut dialog = review_dialog_with(&["src/lib.rs"], 1);
        dialog.path_inputs[0].insert_str("src/");
        assert!(dialog.append_path_row());

        assert!(dialog.active_path_is_empty());
        assert!(dialog.remove_active_path_row());
        assert_eq!(dialog.path_row_count(), 1);
        assert_eq!(dialog.path_active, 0);

        dialog.path_inputs[0].clear();
        assert!(dialog.active_path_is_empty());
        assert!(!dialog.remove_active_path_row(), "one row always survives");
        assert_eq!(dialog.path_row_count(), 1);

        dialog.path_inputs[0].insert_str("crates/");
        assert!(!dialog.remove_active_path_row(), "a filled row is edited");
        assert_eq!(dialog.path_row_count(), 1);
    }

    /// `C-n` / `C-p` walk the rows and report the list boundary, where the
    /// caller hands the keyboard to the neighbouring section (c-4).
    #[test]
    fn test_path_row_navigation_reports_the_list_boundary() {
        let mut dialog = review_dialog_with(&["src/lib.rs"], 1);
        dialog.path_inputs[0].insert_str("src/");
        assert!(dialog.append_path_row());
        assert_eq!(dialog.path_active, 1);

        assert!(dialog.step_path_row(false));
        assert_eq!(dialog.path_active, 0);
        assert!(
            !dialog.step_path_row(false),
            "the first row is the top edge"
        );
        assert!(dialog.step_path_row(true));
        assert_eq!(dialog.path_active, 1);
        assert!(
            !dialog.step_path_row(true),
            "the last row is the bottom edge"
        );
    }

    /// The rows generate the `path_patterns` list a launch sends, and their
    /// union drives both the live counter and the launch gate (c-6).
    #[test]
    fn test_path_rows_form_one_union_scope() {
        let mut dialog = review_dialog_with(&["src/lib.rs", "docs/guide.md"], 1);
        dialog.path_inputs[0].insert_str("*.rs");
        assert_eq!(dialog.pattern_match_count(), Some(1));

        assert!(dialog.append_path_row());
        dialog.path_inputs[1].insert_str("  docs/  ");
        assert_eq!(dialog.pattern_match_count(), Some(2));

        let params = dialog.start_params();
        assert_eq!(
            params.path_patterns,
            vec!["*.rs".to_string(), "docs/".to_string()],
            "each row travels whole, trimmed of surrounding blanks"
        );

        // A row that selects nothing no longer zeroes the union on its own, but
        // a union that selects no file at all blocks the launch (c-6).
        dialog.path_inputs[0].clear();
        dialog.path_inputs[0].insert_str("missing/");
        dialog.path_inputs[1].clear();
        dialog.path_inputs[1].insert_str("absent/");
        assert_eq!(dialog.pattern_match_count(), Some(0));
        assert!(!dialog.is_pattern_launchable());
    }

    /// Typing narrows the repository directory completions of the active row and
    /// accepting one fills the row with the full prefix (c-5).
    #[test]
    fn test_path_completion_offers_repository_directories() {
        let mut dialog = review_dialog_with(
            &[
                "src/lib.rs",
                "crates/core/src/lib.rs",
                "crates/tui/src/main.rs",
                "README.md",
            ],
            1,
        );
        assert!(
            !dialog.path_completion.is_open(),
            "an empty row offers nothing"
        );

        dialog.path_inputs[0].insert_str("cra");
        dialog.refresh_path_completion();
        assert_eq!(
            dialog.path_completion.candidates(),
            &[
                "crates/".to_string(),
                "crates/core/".to_string(),
                "crates/core/src/".to_string(),
                "crates/tui/".to_string(),
                "crates/tui/src/".to_string(),
            ]
        );

        assert!(dialog.accept_path_completion());
        assert_eq!(dialog.path_inputs[0].get_text(), "crates/");
        // Accepting a candidate narrows the menu to its children, so repeated
        // `Tab` presses walk deeper into the tree.
        assert_eq!(
            dialog.path_completion.candidates(),
            &[
                "crates/core/".to_string(),
                "crates/core/src/".to_string(),
                "crates/tui/".to_string(),
                "crates/tui/src/".to_string(),
            ]
        );
    }

    /// The rendered window follows the active row, so a list longer than the
    /// drawn rows never hides the row being typed (c-3).
    #[test]
    fn test_path_visible_start_keeps_the_active_row_in_the_window() {
        let mut dialog = review_dialog_with(&["src/lib.rs"], 1);
        for _ in 0..5 {
            if let Some(last) = dialog.path_inputs.last_mut() {
                last.insert_str("src/");
            }
            assert!(dialog.append_path_row());
        }
        assert_eq!(dialog.path_row_count(), 6);

        assert_eq!(dialog.path_active, 5);
        assert_eq!(dialog.path_visible_start(3), 3);
        dialog.path_active = 0;
        assert_eq!(dialog.path_visible_start(3), 0);
        assert_eq!(dialog.path_visible_start(0), 0);
    }
}
