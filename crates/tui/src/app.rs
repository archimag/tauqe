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
use crate::views::context::ContextViewState;
use crate::views::history::HistoryViewState;
use crate::views::plans::{PlansViewState, RefineDialogFocus};
use crate::views::repl::{ReplView, TurnStatus};
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
    DeleteReview,
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
/// model together with its semantic role while a turn owns the interface, and
/// cleared once the turn settles. This keeps "what runs right now" in the
/// header (an action surface) without duplicating server-side state.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RunningModel {
    /// Wire model reference.
    pub model: ModelRef,
    /// Semantic role that launched the turn, when it was role-driven.
    pub role: Option<ModelRole>,
    /// Configured human-readable alias, when one exists.
    pub alias: Option<String>,
    /// Effective reasoning effort, when one was requested.
    pub effort: Option<ReasoningEffort>,
}

impl RunningModel {
    /// Human-readable label for the header.
    ///
    /// The configured alias wins; otherwise the redundant `provider:` prefix is
    /// dropped and the reasoning effort is appended when set (`name [effort]`).
    pub fn label(&self) -> String {
        if let Some(alias) = self.alias.as_deref() {
            let trimmed = alias.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
        let short = compact_model_name(&self.model);
        match self.effort {
            Some(effort) => format!("{} [{}]", short, effort),
            None => short.to_string(),
        }
    }
}

/// Compact display name of a concrete model, dropping the redundant
/// `provider:` prefix so the pickers and the header share one formatting rule.
pub fn compact_model_name(model: &ModelRef) -> &str {
    &model.name
}

pub fn role_title(role: ModelRole) -> &'static str {
    match role {
        ModelRole::Scribe => "Scribe",
        ModelRole::Coder => "Coder",
        ModelRole::Architect => "Architect",
        ModelRole::Primary => "Primary",
        ModelRole::Reviewer => "Reviewer",
    }
}

pub fn role_badge(role: ModelRole) -> &'static str {
    match role {
        ModelRole::Scribe => "[Scr]",
        ModelRole::Coder => "[Cod]",
        ModelRole::Architect => "[Arc]",
        ModelRole::Primary => "[Pri]",
        ModelRole::Reviewer => "[Rev]",
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

/// Compact label for a model entry, preferring its alias when configured
/// and falling back to `name [effort]` or bare `name`.
pub fn compact_entry_label(entry: &tauqe_protocol::ModelEntry) -> String {
    if let Some(alias) = entry.alias.as_deref() {
        let trimmed = alias.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    let short = compact_model_name(&entry.model);
    match entry.effort {
        Some(effort) => format!("{} [{}]", short, effort),
        None => short.to_string(),
    }
}

/// Numbered row label for the embedded model lists: `[N] <label>`, where the
/// label follows the shared model presentation (alias when configured,
/// otherwise `provider:model`) with the reasoning effort appended.
///
/// The reusable [`crate::components::model_selector::ModelSelector`] list and
/// the primary model picker share this one numbering rule so their rows cannot
/// drift apart.
pub fn numbered_entry_label(index: usize, entry: &tauqe_protocol::ModelEntry) -> String {
    let mut label = format!("[{}] {}", index + 1, entry.display_label());
    let aliased = entry
        .alias
        .as_deref()
        .is_some_and(|alias| !alias.trim().is_empty());
    if aliased {
        if let Some(effort) = entry.effort {
            label.push_str(&format!(" [{}]", effort));
        }
    }
    label
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionDialogKind {
    /// List of models configured for the Primary role.
    PrimaryModels,
    /// Full list of available models.
    ModelCatalog,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionItem {
    Primary(tauqe_protocol::ModelEntry),
    Model(ModelRef),
    /// Pseudo-entry appended to the primary picker that opens the full model
    /// catalog, so configured primary models never lock the developer out of
    /// the complete provider list.
    Catalog,
}

#[derive(Debug, Clone)]
pub struct SelectionDialogState {
    pub kind: SelectionDialogKind,
    pub items: Vec<SelectionItem>,
    pub selector: ListSelector,
}

impl SelectionDialogState {
    pub fn new(
        kind: SelectionDialogKind,
        items: Vec<SelectionItem>,
        selected_index: usize,
    ) -> Self {
        let selector = ListSelector::new(items.len(), selected_index);
        Self {
            kind,
            items,
            selector,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmExecuteScopeState {
    pub plan_id: String,
    pub plan_title: String,
    pub scope_title: String,
    pub step_id: Option<String>,
    pub steps: Vec<tauqe_protocol::PlanItem>,
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
/// The dialog exposes three keyboard targets — the embedded Reviewer model
/// list, the instruction editor, and the action buttons — and `Tab` cycles
/// through them in visual order. Making the section explicit replaces the old
/// heuristic that inferred the target from whether the editor was empty, so
/// list navigation and text editing never fight over the same keys
/// (Conventions §6, §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReviewDialogFocus {
    ModelList,
    /// Single-line field for the target directory or glob pattern.
    PathEditor,
    #[default]
    PromptEditor,
    Actions,
}

impl ReviewDialogFocus {
    /// Advances focus to the next section in visual order.
    ///
    /// When only one model is configured, `ModelList` is omitted from cycling so
    /// navigation moves directly between the path field, the prompt editor, and
    /// the action buttons.
    pub fn cycle_next(&mut self, entry_count: usize) {
        *self = match self {
            Self::ModelList => Self::PathEditor,
            Self::PathEditor => Self::PromptEditor,
            Self::PromptEditor => Self::Actions,
            Self::Actions => {
                if entry_count > 1 {
                    Self::ModelList
                } else {
                    Self::PathEditor
                }
            }
        };
    }

    /// Moves focus to the previous section in visual order.
    pub fn cycle_prev(&mut self, entry_count: usize) {
        *self = match self {
            Self::ModelList => Self::Actions,
            Self::PathEditor => {
                if entry_count > 1 {
                    Self::ModelList
                } else {
                    Self::Actions
                }
            }
            Self::PromptEditor => Self::PathEditor,
            Self::Actions => Self::PromptEditor,
        };
    }

    pub fn is_model_list(&self) -> bool {
        matches!(self, Self::ModelList)
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

#[derive(Debug, Clone)]
pub struct ReviewDialogState {
    pub files_count: usize,
    pub estimated_tokens: usize,
    /// Reviewer-role model selector: configured entries, cyclic navigation,
    /// embedded-list rendering, and the shared scroll arithmetic (c-1, c-2).
    pub selector: ModelSelector,
    /// Single-line target directory or glob pattern narrowing the reviewed set.
    ///
    /// The value is sent as `ReviewStartParams::path_pattern`, so the matched
    /// files are merged alongside the User Context on the server instead of
    /// replacing it: the developer keeps exclusive ownership of that layer
    /// (c-review-context-input).
    pub path_editor: InputEditor,
    pub prompt_editor: InputEditor,
    /// Section that currently owns the keyboard.
    pub focus: ReviewDialogFocus,
    /// Which action-bar button holds focus. Defaults to the safe `Cancel`.
    pub confirm_button: ConfirmDialogButton,
}

impl ReviewDialogState {
    /// Builds a review launch dialog selecting among the configured
    /// `reviewer_entries` for the Reviewer role.
    pub fn new(
        files_count: usize,
        estimated_tokens: usize,
        reviewer_entries: Vec<tauqe_protocol::ModelEntry>,
    ) -> Self {
        Self {
            files_count,
            estimated_tokens,
            selector: ModelSelector::new(ModelRole::Reviewer, reviewer_entries, 0),
            path_editor: InputEditor::default(),
            prompt_editor: InputEditor::default(),
            focus: ReviewDialogFocus::default(),
            confirm_button: ConfirmDialogButton::Cancel,
        }
    }

    /// Currently selected reviewer entry, if any.
    pub fn selected_entry(&self) -> Option<&tauqe_protocol::ModelEntry> {
        self.selector.selected_entry()
    }

    /// Builds the RPC parameters that launch this review.
    ///
    /// Both text fields are trimmed and collapse to `None` when blank, so an
    /// empty prompt or path never reaches the server as a meaningless value.
    /// The optional directory or glob becomes `path_pattern`, which the server
    /// applies by merging the matched files with the current User Context
    /// without mutating the developer's layer (c-review-context-input).
    pub fn start_params(&self) -> ReviewStartParams {
        let prompt = self.prompt_editor.get_text().trim().to_string();
        let pattern = self.path_editor.get_text().trim().to_string();
        ReviewStartParams {
            model: self.selected_entry().map(|entry| entry.model.clone()),
            user_prompt: if prompt.is_empty() {
                None
            } else {
                Some(prompt)
            },
            path_pattern: if pattern.is_empty() {
                None
            } else {
                Some(pattern)
            },
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

#[derive(Debug, Clone, Copy, Default)]
pub struct HeaderClickAreas {
    pub row: u16,
    pub repl_tab: (u16, u16),
    pub context_tab: (u16, u16),
    pub review_tab: (u16, u16),
    pub plans_tab: (u16, u16),
    pub history_tab: (u16, u16),
    pub model_select: (u16, u16),
    pub squash_button: (u16, u16),
    pub help_button: (u16, u16),
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
            status_message: Some("Inspecting repository history...".to_string()),
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
    pub available_models: Vec<ModelRef>,
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
    pub available_models: Vec<ModelRef>,
    pub available_workflows: Vec<tauqe_protocol::WorkflowKind>,
    pub available_edit_protocols: Vec<tauqe_protocol::EditProtocolKind>,
    pub model: ReplView,
    pub context: ContextState,
    pub context_view: ContextViewState,
    pub history_view: HistoryViewState,
    pub review: ReviewViewState,
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
    pub confirm_delete_review: Option<String>,
    pub confirm_execute_scope: Option<ConfirmExecuteScopeState>,
    pub confirm_execute_pipeline: Option<ConfirmExecutePipelineState>,
    pub confirm_execute_review: Option<ConfirmExecuteReviewState>,
    pub confirm_button: ConfirmDialogButton,
    pub selection_dialog: Option<SelectionDialogState>,
    pub status_dialog: Option<StatusDialogState>,
    pub squash_dialog: Option<SquashDialogState>,
    pub review_dialog: Option<ReviewDialogState>,
    pub prompt_modal: Option<PromptModalState>,
    pub notification: Option<AppNotification>,
    pub turn_started_at: Option<std::time::Instant>,
    pub server_disconnected: Option<String>,
    pub server_log_path: std::path::PathBuf,
    pub last_model_height: u16,
    pub last_frame_area: Rect,
    pub header_clicks: HeaderClickAreas,
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
            available_models: init.available_models,
            available_workflows: init.available_workflows,
            available_edit_protocols: init.available_edit_protocols,
            model: ReplView::default(),
            context: init.context,
            context_view: ContextViewState::default(),
            history_view: HistoryViewState::default(),
            review: ReviewViewState::default(),
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
            confirm_delete_review: None,
            confirm_execute_scope: None,
            confirm_execute_pipeline: None,
            confirm_execute_review: None,
            confirm_button: ConfirmDialogButton::Cancel,
            selection_dialog: None,
            status_dialog: None,
            squash_dialog: None,
            review_dialog: None,
            prompt_modal: None,
            notification: None,
            turn_started_at: None,
            server_disconnected: None,
            server_log_path: init.server_log_path,
            last_model_height: 10,
            last_frame_area: Rect::new(0, 0, 80, 24),
            header_clicks: HeaderClickAreas::default(),
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
    /// `[models.primary]`. Falls back to the full model catalog when no primary
    /// entries are configured.
    pub fn open_model_dialog(&mut self) -> bool {
        let entries = self
            .model_choice
            .roles
            .as_ref()
            .and_then(|r| r.entries.as_ref())
            .map(|e| e.primary.clone())
            .unwrap_or_default();

        if entries.is_empty() {
            return self.open_model_catalog();
        }

        let selected = match &self.model_choice.selection {
            ModelSelection::Specific(m) => entries.iter().position(|e| &e.model == m).unwrap_or(0),
            ModelSelection::Role(ModelRole::Primary) => 0,
            _ => 0,
        };

        let mut items: Vec<SelectionItem> =
            entries.into_iter().map(SelectionItem::Primary).collect();
        // The catalog escape hatch keeps every available provider model reachable
        // even when primary models are configured. It is omitted only when the
        // catalog is empty, which would otherwise produce an inert row.
        if !self.available_models.is_empty() {
            items.push(SelectionItem::Catalog);
        }
        self.selection_dialog = Some(SelectionDialogState::new(
            SelectionDialogKind::PrimaryModels,
            items,
            selected,
        ));
        true
    }

    /// Opens the full list of available models. Returns false when the list is empty.
    pub fn open_model_catalog(&mut self) -> bool {
        if self.available_models.is_empty() {
            return false;
        }
        let selected = self
            .available_models
            .iter()
            .position(|m| m == &self.active_model)
            .unwrap_or(0);
        let items = self
            .available_models
            .iter()
            .cloned()
            .map(SelectionItem::Model)
            .collect();
        self.selection_dialog = Some(SelectionDialogState::new(
            SelectionDialogKind::ModelCatalog,
            items,
            selected,
        ));
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
                    None,
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
        if self.confirm_delete_plan.is_some() {
            return Some(ActiveModal::DeletePlan);
        }
        if self.confirm_delete_review.is_some() {
            return Some(ActiveModal::DeleteReview);
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
        available_models: Vec::new(),
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
    fn test_selection_dialog_scrolling_and_bounds() {
        let items: Vec<SelectionItem> = (0..20)
            .map(|i| SelectionItem::Model(ModelRef::openrouter(format!("model-{}", i))))
            .collect();
        let mut dialog = SelectionDialogState::new(SelectionDialogKind::ModelCatalog, items, 0);
        let visible_height = 5;

        assert_eq!(dialog.selector.selected_index(), 0);
        assert_eq!(dialog.selector.scroll_offset(), 0);

        for _ in 0..4 {
            dialog.selector.select_next(visible_height);
        }
        assert_eq!(dialog.selector.selected_index(), 4);
        assert_eq!(dialog.selector.scroll_offset(), 0);

        dialog.selector.select_next(visible_height);
        assert_eq!(dialog.selector.selected_index(), 5);
        assert_eq!(dialog.selector.scroll_offset(), 1);

        for _ in 0..20 {
            dialog.selector.select_next(visible_height);
        }
        assert_eq!(dialog.selector.selected_index(), 19);
        assert_eq!(dialog.selector.scroll_offset(), 15);

        dialog.selector.select_prev(visible_height);
        assert_eq!(dialog.selector.selected_index(), 18);
        assert_eq!(dialog.selector.scroll_offset(), 15);

        for _ in 0..5 {
            dialog.selector.select_prev(visible_height);
        }
        assert_eq!(dialog.selector.selected_index(), 13);
        assert_eq!(dialog.selector.scroll_offset(), 13);
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
        let primary_entry = tauqe_protocol::ModelEntry::new(
            ModelRef::openrouter("primary-model"),
            None,
            Some("Primary Alias".to_string()),
        );
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
        assert_eq!(dialog.kind, SelectionDialogKind::PrimaryModels);
        assert_eq!(dialog.items.len(), 1);
        assert_eq!(dialog.items[0], SelectionItem::Primary(primary_entry));
        assert_eq!(dialog.selector.selected_index(), 0);
    }

    #[test]
    fn test_open_model_dialog_appends_catalog_escape_hatch() {
        let mut state = test_state();
        let primary_entry =
            tauqe_protocol::ModelEntry::new(ModelRef::openrouter("primary-model"), None, None);
        state.available_models = vec![ModelRef::openrouter("catalog-model")];
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
        assert_eq!(dialog.kind, SelectionDialogKind::PrimaryModels);
        assert_eq!(dialog.items.len(), 2);
        assert_eq!(dialog.items[0], SelectionItem::Primary(primary_entry));
        assert_eq!(dialog.items[1], SelectionItem::Catalog);
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

    #[test]
    fn test_review_dialog_focus_cycles_through_path_editor() {
        // More than one reviewer configured: the model list participates.
        let mut focus = ReviewDialogFocus::PromptEditor;
        focus.cycle_next(3);
        assert_eq!(focus, ReviewDialogFocus::Actions);
        focus.cycle_next(3);
        assert_eq!(focus, ReviewDialogFocus::ModelList);
        focus.cycle_next(3);
        assert_eq!(focus, ReviewDialogFocus::PathEditor);
        focus.cycle_next(3);
        assert_eq!(focus, ReviewDialogFocus::PromptEditor);

        // Backward order mirrors the forward order exactly.
        focus.cycle_prev(3);
        assert_eq!(focus, ReviewDialogFocus::PathEditor);
        focus.cycle_prev(3);
        assert_eq!(focus, ReviewDialogFocus::ModelList);
        focus.cycle_prev(3);
        assert_eq!(focus, ReviewDialogFocus::Actions);
        focus.cycle_prev(3);
        assert_eq!(focus, ReviewDialogFocus::PromptEditor);
    }

    #[test]
    fn test_review_dialog_focus_skips_model_list_when_single_reviewer() {
        let mut focus = ReviewDialogFocus::PromptEditor;
        focus.cycle_next(1);
        assert_eq!(focus, ReviewDialogFocus::Actions);
        focus.cycle_next(1);
        assert_eq!(focus, ReviewDialogFocus::PathEditor);
        focus.cycle_next(1);
        assert_eq!(focus, ReviewDialogFocus::PromptEditor);

        focus.cycle_prev(1);
        assert_eq!(focus, ReviewDialogFocus::PathEditor);
        focus.cycle_prev(1);
        assert_eq!(focus, ReviewDialogFocus::Actions);
        focus.cycle_prev(1);
        assert_eq!(focus, ReviewDialogFocus::PromptEditor);
    }

    #[test]
    fn test_review_dialog_start_params_folds_path_and_prompt() {
        let mut dialog = ReviewDialogState::new(0, 0, Vec::new());

        // Both fields start blank: neither reaches the wire.
        let params = dialog.start_params();
        assert!(params.user_prompt.is_none());
        assert!(params.path_pattern.is_none());
        assert_eq!(serde_json::to_string(&params).unwrap(), "{}");

        dialog
            .prompt_editor
            .insert_str("  focus on error handling  ");
        dialog.path_editor.insert_str(" crates/core/src/**/*.rs ");
        let params = dialog.start_params();
        assert_eq!(
            params.user_prompt.as_deref(),
            Some("focus on error handling")
        );
        assert_eq!(
            params.path_pattern.as_deref(),
            Some("crates/core/src/**/*.rs")
        );
    }
}
