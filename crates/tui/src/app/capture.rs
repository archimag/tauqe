//! The declared text-capture boundary of the interface.
//!
//! Every surface that can own the keyboard answers one question exactly once: does
//! it reserve printable characters for a text buffer, or does it read them as
//! commands? The answer is a typed `CaptureSite` rather than a boolean, so a
//! consumer that has to know *where* a keystroke belongs reads the same declaration
//! the keyboard dispatch gate consults instead of restating a subset of it
//! (c-text-capture-boundary).

use crate::app::{ActiveModal, AppState, SquashDialogFocus, ViewMode};

/// The text field that currently captures printable characters.
///
/// One variant names one capturing field, so a consumer that must deliver text
/// names the exact buffer that owns the keyboard instead of inferring it from the
/// surface that happens to be open (c-text-capture-boundary).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureSite {
    /// Custom base-reference input of the squash-commits dialog.
    SquashCustomBaseRef,
    /// Commit-message editor of the squash-commits dialog.
    SquashCommitMessage,
    /// Target directory or glob row of the review launch dialog.
    ReviewLaunchPath,
    /// Instructions prompt of the review launch dialog.
    ReviewLaunchPrompt,
    /// Target-language field of the locale dialog.
    LocaleLanguage,
    /// Engineering directives of the plan creation form.
    PlanCreateDirectives,
    /// Refinement instructions of the plan refinement form.
    PlanRefineInstructions,
    /// Implementation directives of a single-target review execution.
    ReviewExecuteDirectives,
    /// Discussion prompt of the unified prompt modal.
    DiscussionPrompt,
    /// REPL prompt.
    ReplPrompt,
    /// Inline Context-view path input.
    ContextPathInput,
    /// Onboarding input field.
    OnboardingInput,
}
/// Why a quit would discard work the developer has not committed yet.
///
/// The quit gate asks this question once, so the decision to raise the
/// confirmation and the activity the card reports are the same answer
/// (c-text-capture-boundary, #31).
///
/// A draft in any editor the capture boundary declares is reported through
/// [`QuitGuardReason::Editor`] with the site that holds it, so a dialog can
/// never fall out of a hand-maintained list of text fields
/// (Conventions §3.9, §5.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuitGuardReason {
    /// A batch review is running, and quitting abandons the queue.
    ReviewRunning,
    /// A bounded turn is in flight, and quitting abandons it.
    ModelBusy,
    /// The squash dialog is applying its commit.
    SquashApplying,
    /// The named editor holds a draft that quitting would discard.
    Editor(CaptureSite),
}
impl AppState {
    /// Whether a text buffer currently captures printable characters.
    ///
    /// The answer describes the surface that owns the keyboard. While a modal is
    /// open, only the editor it focuses captures text, because that modal receives
    /// every keystroke and the view behind it receives none; without a modal, the
    /// active view decides. A surface that merely lists rows, toggles buttons, or
    /// confirms an action captures nothing, so a printable key pressed there stays
    /// a command (Conventions §5.1).
    ///
    /// The predicate is derived from `focused_capture_site`, so the keyboard
    /// dispatch gate and the resolver that names the capturing field can never
    /// disagree (c-text-capture-boundary).
    pub fn text_input_capturing(&self) -> bool {
        self.focused_capture_site().is_some()
    }

    /// The capturing field that currently owns the keyboard, if any.
    ///
    /// This is the single declaration of the capture boundary: the modal surface
    /// outranks the view behind it, and every answer names the field itself rather
    /// than a bare boolean, so a consumer that must deliver text reaches the same
    /// buffer the keyboard would (c-text-capture-boundary).
    fn focused_capture_site(&self) -> Option<CaptureSite> {
        match self.active_modal() {
            Some(modal) => self.modal_capture_site(modal),
            None => self.view_capture_site(),
        }
    }

    /// The capturing field of `modal`, or `None` when the surface reads its keys
    /// as commands.
    ///
    /// The match over the modal stack is exhaustive on purpose: a new modal
    /// surface must state whether it captures text, so a dialog holding a text
    /// field can never be silently treated as a command surface
    /// (c-text-capture-boundary).
    fn modal_capture_site(&self, modal: ActiveModal) -> Option<CaptureSite> {
        match modal {
            ActiveModal::Squash => self.modals.squash_dialog.as_ref().and_then(|dialog| {
                if dialog.custom_input_active {
                    Some(CaptureSite::SquashCustomBaseRef)
                } else if dialog.focus == SquashDialogFocus::MessageEditor {
                    Some(CaptureSite::SquashCommitMessage)
                } else {
                    None
                }
            }),
            ActiveModal::Review => self.modals.review_dialog.as_ref().and_then(|dialog| {
                if dialog.focus.is_path_editor() {
                    Some(CaptureSite::ReviewLaunchPath)
                } else if dialog.focus.is_prompt_editor() {
                    Some(CaptureSite::ReviewLaunchPrompt)
                } else {
                    None
                }
            }),
            ActiveModal::Prompt => self
                .modals
                .prompt_modal
                .as_ref()
                .filter(|state| state.focus.is_editor())
                .map(|_| CaptureSite::DiscussionPrompt),
            ActiveModal::Locale => self
                .modals
                .locale_dialog
                .as_ref()
                .filter(|dialog| dialog.focus.is_editor())
                .map(|_| CaptureSite::LocaleLanguage),
            ActiveModal::CreatePlan => self
                .modals
                .create_dialog
                .as_ref()
                .filter(|dialog| dialog.focus.is_instructions_editor())
                .map(|_| CaptureSite::PlanCreateDirectives),
            ActiveModal::RefineDialog => self
                .modals
                .refine_dialog
                .as_ref()
                .filter(|dialog| dialog.focus.is_instructions_editor())
                .map(|_| CaptureSite::PlanRefineInstructions),
            // Only the point-execution form exposes the directives editor; a
            // batch queue confirms as a binary card whose focus never reaches a
            // text field.
            ActiveModal::ExecuteReview => {
                let target = self.modals.confirm_execute_review.as_ref()?;
                if target.is_single_target() && target.focus.is_instructions_editor() {
                    Some(CaptureSite::ReviewExecuteDirectives)
                } else {
                    None
                }
            }
            ActiveModal::ServerDisconnected
            | ActiveModal::ReviewTruncation
            | ActiveModal::ConfirmCancel
            | ActiveModal::ConfirmQuit
            | ActiveModal::BatchCancel
            | ActiveModal::ClearAuto
            | ActiveModal::ClearUser
            | ActiveModal::RemoveContextFile
            | ActiveModal::OuterDocsCacheRemove
            | ActiveModal::OuterDocsCache
            | ActiveModal::ConfirmDetachFocus
            | ActiveModal::ClearHorizonFinal
            | ActiveModal::ClearHorizon
            | ActiveModal::DeletePlanFinal
            | ActiveModal::DeletePlan
            | ActiveModal::DeleteReview
            | ActiveModal::ReviewAnalyze
            | ActiveModal::ExecutePipeline
            | ActiveModal::ExecuteScope
            | ActiveModal::RefineBlocked
            | ActiveModal::ClearHistory
            | ActiveModal::Status
            | ActiveModal::Selection
            | ActiveModal::DiffInspector
            | ActiveModal::ThemeSelector
            | ActiveModal::UsageSummary
            | ActiveModal::Help => None,
        }
    }

    /// The capturing field of the active view, or `None` when the view reads its
    /// keys as commands.
    ///
    /// The REPL prompt is always present, the inline Context path input exists
    /// only while a file path is typed, and the onboarding fields capture only on
    /// the steps that ask for input; every structural view is a command surface.
    fn view_capture_site(&self) -> Option<CaptureSite> {
        match self.view_mode {
            ViewMode::Repl => Some(CaptureSite::ReplPrompt),
            ViewMode::Context => self
                .context_view
                .adding_file
                .then_some(CaptureSite::ContextPathInput),
            ViewMode::Onboarding => self
                .onboarding
                .input_active
                .then_some(CaptureSite::OnboardingInput),
            ViewMode::Plans | ViewMode::Review | ViewMode::History => None,
        }
    }

    /// Routes pasted text into the editor the capture boundary declares.
    ///
    /// The destination is the field `focused_capture_site` resolves, so a payload
    /// reaches exactly the editor the keyboard would type into, and a surface that
    /// captures no text drops it instead of guessing a field (Conventions §5.1,
    /// c-text-capture-boundary).
    ///
    /// The match over [`CaptureSite`] is exhaustive on purpose: a capturing field
    /// cannot exist without a paste destination, so a surface added later either
    /// declares where its text goes or captures nothing at all. Single-line fields
    /// collapse embedded line breaks to spaces while multiline fields keep the
    /// payload verbatim, and the active review-launch target-path row refreshes its
    /// directory completion after the insert, because the pasted prefix changes the
    /// candidates the dialog offers.
    pub fn route_paste(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let Some(site) = self.focused_capture_site() else {
            return;
        };

        match site {
            CaptureSite::SquashCustomBaseRef => {
                if let Some(dialog) = self.modals.squash_dialog.as_mut() {
                    dialog.custom_editor.insert_paste_single_line(text);
                }
            }
            CaptureSite::SquashCommitMessage => {
                if let Some(dialog) = self.modals.squash_dialog.as_mut() {
                    dialog.message_editor.insert_paste(text);
                }
            }
            CaptureSite::ReviewLaunchPath => {
                if let Some(dialog) = self.modals.review_dialog.as_mut() {
                    if let Some(editor) = dialog.active_path_editor_mut() {
                        editor.insert_paste_single_line(text);
                    }
                    dialog.refresh_path_completion();
                }
            }
            CaptureSite::ReviewLaunchPrompt => {
                if let Some(dialog) = self.modals.review_dialog.as_mut() {
                    dialog.prompt_editor.insert_paste(text);
                }
            }
            CaptureSite::LocaleLanguage => {
                if let Some(dialog) = self.modals.locale_dialog.as_mut() {
                    dialog.language_editor.insert_paste_single_line(text);
                }
            }
            CaptureSite::PlanCreateDirectives => {
                if let Some(dialog) = self.modals.create_dialog.as_mut() {
                    dialog.directives_editor.insert_paste(text);
                }
            }
            CaptureSite::PlanRefineInstructions => {
                if let Some(dialog) = self.modals.refine_dialog.as_mut() {
                    dialog.instructions_editor.insert_paste(text);
                }
            }
            CaptureSite::ReviewExecuteDirectives => {
                if let Some(dialog) = self.modals.confirm_execute_review.as_mut() {
                    dialog.instructions_editor.insert_paste(text);
                }
            }
            CaptureSite::DiscussionPrompt => {
                if let Some(dialog) = self.modals.prompt_modal.as_mut() {
                    dialog.prompt_editor.insert_paste(text);
                }
            }
            CaptureSite::ReplPrompt => {
                self.input_editor.insert_paste(text);
            }
            CaptureSite::ContextPathInput => {
                self.context_view.add_input.insert_paste_single_line(text);
            }
            CaptureSite::OnboardingInput => {
                self.onboarding.input_buffer.insert_paste_single_line(text);
            }
        }
    }
    /// The first piece of uncommitted work a quit would discard, if any.
    ///
    /// This is the one scan the quit gate and the quit card both read, so the
    /// decision to ask for confirmation and the activity the card reports can
    /// never disagree (c-text-capture-boundary, #31).
    ///
    /// The order is fixed: running work first, then the prompt draft, then an
    /// in-flight squash, then every editor a populated dialog or view still
    /// holds. An editor is read independently of where the focus rests and of
    /// which modal the stack ranks highest, so a card raised above a form that
    /// still holds text keeps a second quit request guarded
    /// (Conventions §3.9, §5.4).
    pub fn quit_guard_reason(&self) -> Option<QuitGuardReason> {
        if self.review.running {
            return Some(QuitGuardReason::ReviewRunning);
        }
        if self.model.is_busy() {
            return Some(QuitGuardReason::ModelBusy);
        }
        if !self.input_editor.is_empty() {
            return Some(QuitGuardReason::Editor(CaptureSite::ReplPrompt));
        }
        if self
            .modals
            .squash_dialog
            .as_ref()
            .is_some_and(|dialog| dialog.applying)
        {
            return Some(QuitGuardReason::SquashApplying);
        }
        self.editor_holding_text().map(QuitGuardReason::Editor)
    }

    /// The first text buffer holding unsubmitted content, in the fixed order
    /// the quit guard reports.
    ///
    /// Every editor is visited whatever the focus and whatever the modal stack
    /// ranks, so a dialog the top-most card covers still guards the exit while
    /// its text is present. The directives editor of a batch-shaped
    /// review-execution confirmation is skipped, because that shape never
    /// exposes it and its text therefore cannot be lost
    /// (c-text-capture-boundary, #31).
    fn editor_holding_text(&self) -> Option<CaptureSite> {
        if self
            .modals
            .squash_dialog
            .as_ref()
            .is_some_and(|dialog| !dialog.custom_editor.is_empty())
        {
            return Some(CaptureSite::SquashCustomBaseRef);
        }
        if self
            .modals
            .squash_dialog
            .as_ref()
            .is_some_and(|dialog| !dialog.message_editor.is_empty())
        {
            return Some(CaptureSite::SquashCommitMessage);
        }
        if self
            .modals
            .review_dialog
            .as_ref()
            .is_some_and(|dialog| !dialog.prompt_editor.is_empty())
        {
            return Some(CaptureSite::ReviewLaunchPrompt);
        }
        if self
            .modals
            .review_dialog
            .as_ref()
            .is_some_and(|dialog| dialog.path_inputs.iter().any(|row| !row.is_empty()))
        {
            return Some(CaptureSite::ReviewLaunchPath);
        }
        if self
            .modals
            .refine_dialog
            .as_ref()
            .is_some_and(|dialog| !dialog.instructions_editor.is_empty())
        {
            return Some(CaptureSite::PlanRefineInstructions);
        }
        if self
            .modals
            .create_dialog
            .as_ref()
            .is_some_and(|dialog| !dialog.directives_editor.is_empty())
        {
            return Some(CaptureSite::PlanCreateDirectives);
        }

        let review_execution = self.modals.confirm_execute_review.as_ref();
        let single_target = review_execution.is_some_and(|state| state.is_single_target());
        if single_target
            && review_execution.is_some_and(|state| !state.instructions_editor.is_empty())
        {
            return Some(CaptureSite::ReviewExecuteDirectives);
        }

        if self
            .modals
            .prompt_modal
            .as_ref()
            .is_some_and(|dialog| !dialog.prompt_editor.is_empty())
        {
            return Some(CaptureSite::DiscussionPrompt);
        }
        if self
            .modals
            .locale_dialog
            .as_ref()
            .is_some_and(|dialog| !dialog.language_editor.is_empty())
        {
            return Some(CaptureSite::LocaleLanguage);
        }
        if self.context_view.adding_file && !self.context_view.add_input.is_empty() {
            return Some(CaptureSite::ContextPathInput);
        }
        if self.onboarding.input_active && !self.onboarding.input_buffer.is_empty() {
            return Some(CaptureSite::OnboardingInput);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{
        ConfirmExecuteReviewState, LocaleDialogFocus, LocaleDialogState, ModalFocus,
        PromptModalState, PromptModalTarget, ReviewDialogFocus, ReviewDialogState,
        SquashDialogState,
    };
    use crate::views::plans::{PlanCreateDialogState, PlanFormFocus, PlanRefineDialogState};

    /// A fully constructed application state the resolver tests adjust field by
    /// field.
    fn test_state() -> AppState {
        crate::app::test_state()
    }

    /// Asserts that `st` resolves to `site` and that the public predicate the
    /// keyboard dispatcher consults agrees with the resolver
    /// (c-text-capture-boundary).
    fn assert_site(st: &AppState, site: Option<CaptureSite>) {
        assert_eq!(st.focused_capture_site(), site);
        assert_eq!(
            st.text_input_capturing(),
            site.is_some(),
            "the keyboard dispatch gate must agree with the resolver"
        );
    }

    /// The squash dialog captures text in its custom base-ref input and in its
    /// commit-message editor, and nowhere else. The open modal outranks the REPL
    /// prompt behind it (c-text-capture-boundary).
    #[test]
    fn test_capture_site_resolves_the_squash_editors() {
        let mut st = test_state();
        assert_site(&st, Some(CaptureSite::ReplPrompt));

        st.modals.squash_dialog = Some(SquashDialogState::default());
        assert_site(&st, None);

        st.modals.squash_dialog = Some(SquashDialogState {
            focus: SquashDialogFocus::MessageEditor,
            ..Default::default()
        });
        assert_site(&st, Some(CaptureSite::SquashCommitMessage));

        // The custom base-ref input captures text whatever the list focus is.
        st.modals.squash_dialog = Some(SquashDialogState {
            focus: SquashDialogFocus::FileList,
            custom_input_active: true,
            ..Default::default()
        });
        assert_site(&st, Some(CaptureSite::SquashCustomBaseRef));
    }

    /// The review launch dialog captures text in its target-path rows and in its
    /// instructions prompt, while its lists and action bar stay command surfaces
    /// (c-text-capture-boundary).
    #[test]
    fn test_capture_site_resolves_the_review_launch_editors() {
        let mut st = test_state();
        let mut dialog = ReviewDialogState::new(0, 0, Vec::new(), Vec::new());
        assert_eq!(dialog.focus, ReviewDialogFocus::PromptEditor);

        st.modals.review_dialog = Some(dialog.clone());
        assert_site(&st, Some(CaptureSite::ReviewLaunchPrompt));

        dialog.focus = ReviewDialogFocus::PathEditor;
        st.modals.review_dialog = Some(dialog.clone());
        assert_site(&st, Some(CaptureSite::ReviewLaunchPath));

        for focus in [
            ReviewDialogFocus::ModelList,
            ReviewDialogFocus::PresetList,
            ReviewDialogFocus::Actions,
        ] {
            dialog.focus = focus;
            st.modals.review_dialog = Some(dialog.clone());
            assert_site(&st, None);
        }
    }

    /// The locale dialog captures text in its target-language field only
    /// (c-text-capture-boundary).
    #[test]
    fn test_capture_site_resolves_the_locale_field() {
        let mut st = test_state();
        st.modals.locale_dialog = Some(LocaleDialogState::default());
        assert_site(&st, Some(CaptureSite::LocaleLanguage));

        st.modals.locale_dialog = Some(LocaleDialogState {
            focus: LocaleDialogFocus::Actions,
            ..Default::default()
        });
        assert_site(&st, None);
    }

    /// The discussion prompt captures text while its editor holds the keyboard
    /// and stops as soon as its action bar does (c-text-capture-boundary).
    #[test]
    fn test_capture_site_resolves_the_discussion_prompt() {
        let mut st = test_state();
        let prompt = PromptModalState::new(PromptModalTarget::Plan {
            plan_id: "plan-1".to_string(),
            plan_title: "Plan 1".to_string(),
            focused_items: Vec::new(),
        });
        st.modals.prompt_modal = Some(prompt);
        assert_site(&st, Some(CaptureSite::DiscussionPrompt));

        if let Some(modal) = st.modals.prompt_modal.as_mut() {
            modal.focus = ModalFocus::Actions;
        }
        assert_site(&st, None);
    }

    /// Both plan-synthesis forms capture text in their instructions editor and
    /// nowhere else (c-text-capture-boundary).
    #[test]
    fn test_capture_site_resolves_the_plan_form_editors() {
        let mut st = test_state();

        st.modals.create_dialog = Some(PlanCreateDialogState::new(Vec::new()));
        assert_site(&st, Some(CaptureSite::PlanCreateDirectives));
        if let Some(dialog) = st.modals.create_dialog.as_mut() {
            dialog.focus = PlanFormFocus::Actions;
        }
        assert_site(&st, None);
        st.modals.create_dialog = None;

        let refine =
            PlanRefineDialogState::new("plan-1".to_string(), "Plan 1".to_string(), Vec::new());
        st.modals.refine_dialog = Some(refine);
        assert_site(&st, Some(CaptureSite::PlanRefineInstructions));
        if let Some(dialog) = st.modals.refine_dialog.as_mut() {
            dialog.focus = PlanFormFocus::PlannerList;
        }
        assert_site(&st, None);
    }

    /// Only the point-execution form exposes the directives editor; a batch queue
    /// confirms as a binary card, so no field of it captures text even though its
    /// focus still rests on the (absent) editor row (c-text-capture-boundary).
    #[test]
    fn test_capture_site_resolves_only_a_single_target_review_execution() {
        let mut st = test_state();
        let finding = tauqe_protocol::ReviewIssue {
            id: 1,
            title: "Finding 1".to_string(),
            uuid: None,
            commit: None,
            group: None,
            model: None,
            issue_type: tauqe_protocol::IssueType::Bug,
            status: tauqe_protocol::IssueStatus::Todo,
            description: String::new(),
            recommendations: Vec::new(),
            files: Vec::new(),
            readonly_files: Vec::new(),
            note: None,
        };

        st.modals.confirm_execute_review = Some(ConfirmExecuteReviewState::new(
            Some("rev-1".to_string()),
            "Session rev-1".to_string(),
            "Finding 1".to_string(),
            Some(1),
            vec![finding],
            Vec::new(),
        ));
        assert_site(&st, Some(CaptureSite::ReviewExecuteDirectives));

        st.modals.confirm_execute_review = Some(ConfirmExecuteReviewState::new(
            Some("rev-1".to_string()),
            "Session rev-1".to_string(),
            "All remaining findings".to_string(),
            None,
            Vec::new(),
            Vec::new(),
        ));
        assert_site(&st, None);
    }

    /// Every view buffer the boundary declares resolves its own site, and the
    /// structural views stay command surfaces (c-text-capture-boundary).
    #[test]
    fn test_capture_site_resolves_every_view_buffer() {
        let mut st = test_state();
        assert_eq!(st.view_mode, ViewMode::Repl);
        assert_site(&st, Some(CaptureSite::ReplPrompt));

        st.view_mode = ViewMode::Context;
        assert_site(&st, None);
        st.context_view.adding_file = true;
        assert_site(&st, Some(CaptureSite::ContextPathInput));
        st.context_view.adding_file = false;

        st.view_mode = ViewMode::Onboarding;
        assert_site(&st, None);
        st.onboarding.input_active = true;
        assert_site(&st, Some(CaptureSite::OnboardingInput));
        st.onboarding.input_active = false;

        for mode in [ViewMode::Plans, ViewMode::Review, ViewMode::History] {
            st.view_mode = mode;
            assert_site(&st, None);
        }
    }

    /// A surface that only lists rows, toggles buttons, or confirms an action
    /// resolves no capture site even while it owns the keyboard
    /// (c-text-capture-boundary).
    #[test]
    fn test_command_only_surfaces_resolve_no_capture_site() {
        let mut st = test_state();

        st.modals.show_help = true;
        assert_site(&st, None);
        st.modals.show_help = false;

        st.open_usage_summary();
        assert_site(&st, None);
        st.modals.usage_summary = None;

        st.modals.confirm_execute_scope = Some(crate::app::ConfirmExecuteScopeState::new(
            "plan-1".to_string(),
            "Plan 1".to_string(),
            "Step 1".to_string(),
            Some("1".to_string()),
            Vec::new(),
            Vec::new(),
        ));
        assert_site(&st, None);
    }
    /// A text editor holding `text`.
    fn editor_with(text: &str) -> crate::editor::InputEditor {
        let mut editor = crate::editor::InputEditor::default();
        editor.insert_str(text);
        editor
    }

    /// One review finding with the given identity.
    fn review_issue(id: u32, title: &str) -> tauqe_protocol::ReviewIssue {
        tauqe_protocol::ReviewIssue {
            id,
            title: title.to_string(),
            uuid: None,
            commit: None,
            group: None,
            model: None,
            issue_type: tauqe_protocol::IssueType::Bug,
            status: tauqe_protocol::IssueStatus::Todo,
            description: String::new(),
            recommendations: Vec::new(),
            files: Vec::new(),
            readonly_files: Vec::new(),
            note: None,
        }
    }

    /// Every editor the capture boundary declares guards the exit while it
    /// holds a draft, so a dialog can never be omitted from a hand-maintained
    /// subset again (c-text-capture-boundary, #31).
    #[test]
    fn test_quit_guard_reason_covers_every_capturing_editor() {
        let mut st = test_state();
        assert_eq!(st.quit_guard_reason(), None);
        assert!(!st.has_lossy_work());

        st.input_editor.insert_str("draft prompt");
        assert_eq!(
            st.quit_guard_reason(),
            Some(QuitGuardReason::Editor(CaptureSite::ReplPrompt))
        );
        st.input_editor.clear();

        st.modals.squash_dialog = Some(SquashDialogState {
            custom_editor: editor_with("origin/main"),
            ..Default::default()
        });
        assert_eq!(
            st.quit_guard_reason(),
            Some(QuitGuardReason::Editor(CaptureSite::SquashCustomBaseRef))
        );
        st.modals.squash_dialog = Some(SquashDialogState {
            message_editor: editor_with("feat(tui): guard the quit"),
            ..Default::default()
        });
        assert_eq!(
            st.quit_guard_reason(),
            Some(QuitGuardReason::Editor(CaptureSite::SquashCommitMessage))
        );
        st.modals.squash_dialog = None;

        let mut review = ReviewDialogState::new(0, 0, Vec::new(), Vec::new());
        review.prompt_editor.insert_str("focus on error handling");
        st.modals.review_dialog = Some(review);
        assert_eq!(
            st.quit_guard_reason(),
            Some(QuitGuardReason::Editor(CaptureSite::ReviewLaunchPrompt))
        );
        let mut review = ReviewDialogState::new(0, 0, Vec::new(), Vec::new());
        review.path_inputs[0].insert_str("crates/tui/");
        st.modals.review_dialog = Some(review);
        assert_eq!(
            st.quit_guard_reason(),
            Some(QuitGuardReason::Editor(CaptureSite::ReviewLaunchPath))
        );
        st.modals.review_dialog = None;

        let mut refine =
            PlanRefineDialogState::new("plan-1".to_string(), "Plan 1".to_string(), Vec::new());
        refine
            .instructions_editor
            .insert_str("split the second step");
        st.modals.refine_dialog = Some(refine);
        assert_eq!(
            st.quit_guard_reason(),
            Some(QuitGuardReason::Editor(CaptureSite::PlanRefineInstructions))
        );
        st.modals.refine_dialog = None;

        let mut create = PlanCreateDialogState::new(Vec::new());
        create.directives_editor.insert_str("add a retry budget");
        st.modals.create_dialog = Some(create);
        assert_eq!(
            st.quit_guard_reason(),
            Some(QuitGuardReason::Editor(CaptureSite::PlanCreateDirectives))
        );
        st.modals.create_dialog = None;

        let mut execute = ConfirmExecuteReviewState::new(
            Some("rev-1".to_string()),
            "Session rev-1".to_string(),
            "Finding 1".to_string(),
            Some(1),
            vec![review_issue(1, "Finding 1")],
            Vec::new(),
        );
        execute.instructions_editor.insert_str("guard the header");
        st.modals.confirm_execute_review = Some(execute);
        assert_eq!(
            st.quit_guard_reason(),
            Some(QuitGuardReason::Editor(
                CaptureSite::ReviewExecuteDirectives
            ))
        );
        st.modals.confirm_execute_review = None;

        let mut prompt = PromptModalState::new(PromptModalTarget::Plan {
            plan_id: "plan-1".to_string(),
            plan_title: "Plan 1".to_string(),
            focused_items: Vec::new(),
        });
        prompt
            .prompt_editor
            .insert_str("what about the retry budget?");
        st.modals.prompt_modal = Some(prompt);
        assert_eq!(
            st.quit_guard_reason(),
            Some(QuitGuardReason::Editor(CaptureSite::DiscussionPrompt))
        );
        st.modals.prompt_modal = None;

        st.modals.locale_dialog = Some(LocaleDialogState {
            language_editor: editor_with("French"),
            ..Default::default()
        });
        assert_eq!(
            st.quit_guard_reason(),
            Some(QuitGuardReason::Editor(CaptureSite::LocaleLanguage))
        );
        st.modals.locale_dialog = None;

        st.context_view.adding_file = true;
        st.context_view.add_input.insert_str("crates/");
        assert_eq!(
            st.quit_guard_reason(),
            Some(QuitGuardReason::Editor(CaptureSite::ContextPathInput))
        );
        st.context_view.add_input.clear();
        assert_eq!(
            st.quit_guard_reason(),
            None,
            "an active but empty path input discards nothing"
        );
        st.context_view.adding_file = false;

        st.onboarding.input_active = true;
        st.onboarding.input_buffer.insert_str("sk-or-secret");
        assert_eq!(
            st.quit_guard_reason(),
            Some(QuitGuardReason::Editor(CaptureSite::OnboardingInput))
        );
        st.onboarding.input_buffer.clear();
        assert_eq!(st.quit_guard_reason(), None);
        st.onboarding.input_active = false;

        assert_eq!(st.quit_guard_reason(), None);
        assert!(!st.has_lossy_work());
    }

    /// The guard reads the editors themselves, never the focus or the modal
    /// stack: typed text survives a card raised above its dialog, so a second
    /// quit request over the confirmation stays guarded
    /// (c-text-capture-boundary, #31).
    #[test]
    fn test_quit_guard_reads_editors_independently_of_focus_and_modal_stack() {
        let mut st = test_state();
        let mut refine =
            PlanRefineDialogState::new("plan-1".to_string(), "Plan 1".to_string(), Vec::new());
        refine
            .instructions_editor
            .insert_str("split the second step");
        refine.focus = PlanFormFocus::Actions;
        st.modals.refine_dialog = Some(refine);

        assert_eq!(
            st.quit_guard_reason(),
            Some(QuitGuardReason::Editor(CaptureSite::PlanRefineInstructions)),
            "the action bar claiming the keyboard does not release the draft"
        );

        // The quit card the first request raised owns the keyboard, while the
        // text it covers is still exactly what a confirm would discard.
        st.modals.confirm_quit = true;
        assert_eq!(st.active_modal(), Some(ActiveModal::ConfirmQuit));
        assert_eq!(
            st.quit_guard_reason(),
            Some(QuitGuardReason::Editor(CaptureSite::PlanRefineInstructions))
        );
        assert!(st.has_lossy_work());
    }

    /// A batch queue confirms as a binary card without a directives editor, so
    /// the shape alone never guards a quit and text left in the shared state
    /// cannot make it guard one (c-text-capture-boundary, #31).
    #[test]
    fn test_batch_shaped_review_execution_confirmation_is_not_lossy() {
        let mut st = test_state();
        st.modals.confirm_execute_review = Some(ConfirmExecuteReviewState::new(
            Some("rev-1".to_string()),
            "Session rev-1".to_string(),
            "All remaining findings".to_string(),
            None,
            Vec::new(),
            Vec::new(),
        ));
        assert_eq!(st.quit_guard_reason(), None);
        assert!(!st.has_lossy_work());

        if let Some(state) = st.modals.confirm_execute_review.as_mut() {
            state
                .instructions_editor
                .insert_str("ignored by the batch shape");
        }
        assert_eq!(st.quit_guard_reason(), None);
        assert!(!st.has_lossy_work());
    }

    /// Running work outranks a draft, and the scan reports the first reason in
    /// its fixed order so the card always names the same activity
    /// (c-text-capture-boundary, #31).
    #[test]
    fn test_quit_guard_reports_running_work_before_a_draft() {
        let mut st = test_state();
        st.input_editor.insert_str("draft prompt");
        assert_eq!(
            st.quit_guard_reason(),
            Some(QuitGuardReason::Editor(CaptureSite::ReplPrompt))
        );

        st.model.status = crate::views::repl::TurnStatus::Thinking;
        assert_eq!(st.quit_guard_reason(), Some(QuitGuardReason::ModelBusy));

        st.review.running = true;
        assert_eq!(st.quit_guard_reason(), Some(QuitGuardReason::ReviewRunning));

        st.review.running = false;
        st.model.status = crate::views::repl::TurnStatus::Idle;
        st.input_editor.clear();
        st.modals.squash_dialog = Some(SquashDialogState {
            applying: true,
            ..Default::default()
        });
        assert_eq!(
            st.quit_guard_reason(),
            Some(QuitGuardReason::SquashApplying)
        );
    }
}
