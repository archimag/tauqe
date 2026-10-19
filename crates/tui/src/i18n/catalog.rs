use tauqe_protocol::locale::LocaleCatalogItem;

/// Returns the canonical English interface catalog.
///
/// English is the sole embedded source of truth (c-1). Every catalog entry pairs
/// a stable machine key with the canonical English text, semantic usage context
/// to guide LLM translation synthesis (c-2), and an optional terminal display
/// width limit in columns to protect the UI geometry (c-3).
///
/// The catalog enumerates every user-facing surface of the TUI: navigation tabs,
/// header chrome, footer telemetry, bounded-turn phases, modal dialog titles and
/// buttons, view section headers, help vocabulary, command descriptions, and
/// system notifications. Keys are grouped by subsystem so later localization
/// steps resolve strings exclusively through `i18n.t(...)` / `active_t(...)`
/// (c-1) and never hardcode a visible literal.
pub fn canonical_catalog() -> Vec<LocaleCatalogItem> {
    vec![
        // ------------------------------------------------------------------
        // Navigation tabs (tight width budget to fit the top bar).
        // ------------------------------------------------------------------
        item("tab.repl", "REPL", "Main REPL and chat tab title", Some(12)),
        item("tab.context", "Context", "Context and file management tab title", Some(12)),
        item("tab.review", "Review", "Code review sessions tab title", Some(12)),
        item("tab.plans", "Plans", "Hierarchical plans tab title", Some(12)),
        item("tab.history", "History", "Session history and audit tab title", Some(12)),
        item("tab.onboarding", "Setup", "Initial repository setup tab title", Some(12)),

        // ------------------------------------------------------------------
        // Header chrome (project name, model chip, and action buttons).
        // ------------------------------------------------------------------
        item("header.squash", "Squash", "Header button opening the squash commits dialog", Some(12)),
        item("header.help", "Help", "Header button opening the Help dialog", Some(10)),
        item("header.model_chip", "🧠 {}", "Header chip showing the active or running model", None),
        item("header.no_repository", "no repository", "Header placeholder when no Git repository is detected", None),

        // ------------------------------------------------------------------
        // Footer telemetry indicators.
        // ------------------------------------------------------------------
        item("footer.ready", "Ready", "Footer idle indicator", Some(10)),
        item("footer.done", "Done", "Footer indicator for a completed turn", Some(10)),
        item("footer.cancelled", "Cancelled", "Footer indicator for a cancelled turn", Some(12)),
        item("footer.error", "Error", "Footer indicator for a failed turn", Some(10)),
        item("footer.reviewing", "Reviewing", "Footer indicator while a code review runs", Some(12)),
        item("footer.stopping", "[Stopping after current step]", "Footer badge while a soft batch stop is pending", None),
        item("footer.retrying", "⟳ Reconnecting {}/{} in {}s", "Footer indicator counting down the seconds until the next provider attempt", None),
        item("footer.retrying_cancel", "C-g to cancel", "Footer hint naming the chord that aborts a reconnect pause", None),
        item("footer.history_stats", "{} entries (~{} tokens)", "Footer history size indicator", None),

        // ------------------------------------------------------------------
        // Footer usage summary labels (session total, last operation, and the
        // in-flight operation slot) plus the token unit shown when no cost is
        // known. They are unbounded because the whole usage field drops out of
        // the footer as a unit once it no longer fits (c-4).
        // ------------------------------------------------------------------
        item("footer.usage.session", "session", "Footer usage label for the accumulated session total", None),
        item("footer.usage.last", "last", "Footer usage label for the last completed operation", None),
        item("footer.usage.now", "now", "Footer usage label for the operation currently in flight", None),
        item("footer.usage.tok", "tok", "Footer usage unit suffix shown for token counts when no cost is known", None),

        // ------------------------------------------------------------------
        // Bounded-turn phase pipeline (footer FSM telemetry).
        // ------------------------------------------------------------------
        item("phase.discovery", "Discovery", "Bounded-turn discovery phase label", Some(12)),
        item("phase.proposal", "Proposal", "Bounded-turn proposal phase label", Some(12)),
        item("phase.staging", "Staging", "Bounded-turn staging phase label", Some(12)),
        item("phase.verification", "Verify", "Bounded-turn verification phase label", Some(12)),
        item("phase.healing", "Heal", "Bounded-turn healing phase label", Some(12)),

        // ------------------------------------------------------------------
        // Modal action buttons (tight width budget to fit button rows).
        // ------------------------------------------------------------------
        item("modal.confirm", "Confirm", "Confirmation button in modal dialogs", Some(14)),
        item("modal.cancel", "Cancel", "Cancel or safe dismissal button in modal dialogs", Some(14)),
        item("modal.apply", "Apply", "Apply changes button in modal dialogs", Some(14)),
        item("modal.discard", "Discard", "Discard changes button in modal dialogs", Some(14)),
        item("modal.close", "Close", "Close modal button", Some(12)),
        item("modal.delete", "Delete", "Destructive delete action button", Some(12)),
        item("modal.continue", "Continue", "Continue batch execution button", Some(14)),
        item("modal.soft_stop", "Soft Stop", "Stop batch after current step button", Some(16)),
        item("modal.hard_stop", "Hard Stop", "Stop batch immediately button", Some(16)),
        item("modal.synthesize", "Synthesize", "Synthesize a locale button in the locale dialog", Some(14)),
        item("modal.reset_english", "Reset to English", "Reset the interface to English button", Some(18)),
        item("modal.send", "Send to AI", "Send the discussion prompt button", Some(14)),
        item("modal.start_review", "Start Review", "Start a code review button", Some(14)),
        item("modal.start_refinement", "Start Refinement", "Start an architect plan refinement button", Some(18)),

        // ------------------------------------------------------------------
        // Shared modal dialog vocabulary (footer interaction verbs).
        // ------------------------------------------------------------------
        item("dialog.action.focus", "focus", "Verb: move focus between dialog controls", Some(12)),
        item("dialog.action.activate", "activate", "Verb: activate the focused dialog control", Some(12)),
        item("dialog.action.fast_select", "fast select", "Verb: quick confirm or cancel a dialog", Some(14)),
        item("dialog.action.dismiss", "dismiss", "Verb: dismiss the dialog safely", Some(12)),

        // ------------------------------------------------------------------
        // Bounded-turn / FSM telemetry statuses.
        // ------------------------------------------------------------------
        item("status.idle", "Idle", "System idle state indicator", Some(12)),
        item("status.awaiting", "Awaiting", "Awaiting user input indicator", Some(14)),
        item("status.thinking", "Thinking", "Model reasoning in progress indicator", Some(14)),
        item("status.responding", "Responding", "Model streaming response indicator", Some(16)),
        item("status.editing", "Editing", "Staging code edits indicator", Some(14)),
        item("status.verifying", "Verifying", "Running compiler verification pipeline", Some(16)),
        item("status.discovery", "Discovery", "Discovering repository context files", Some(16)),
        item("status.staging", "Staging", "Applying and validating patches in memory", Some(16)),
        item("status.healing", "Healing", "Compiler error healing loop", Some(16)),
        item("status.done", "Done", "Turn completed successfully indicator", Some(10)),
        item("status.cancelled", "Cancelled", "Turn cancelled indicator", Some(14)),
        item("status.error", "Error", "Turn failed with error indicator", Some(12)),

        // ------------------------------------------------------------------
        // Generic interactive verbs and actions.
        // ------------------------------------------------------------------
        item("action.save", "Save", "Save action verb", Some(10)),
        item("action.refresh", "Refresh", "Refresh view action verb", Some(12)),
        item("action.copy", "Copy", "Copy item or block action verb", Some(10)),
        item("action.retry", "Retry", "Retry failed action verb", Some(12)),
        item("action.discuss", "Discuss", "Open discussion modal verb", Some(14)),
        item("action.execute", "Execute", "Execute step or batch action verb", Some(14)),
        item("action.replan", "Replan", "Replan blocked horizon action verb", Some(14)),

        // ------------------------------------------------------------------
        // Modal dialog titles.
        // ------------------------------------------------------------------
        item("dialog.confirm_cancel.title", "Confirm Cancellation", "Title for active turn cancellation popup", None),
        item("dialog.confirm_quit.title", "Confirm Quit", "Title for unsaved changes exit dialog", None),
        item("dialog.confirm_detach_focus.title", "Confirm Detach Focus", "Title for discussion focus detach dialog", None),
        item("dialog.delete_plan.title", "Delete Plan", "Title for plan deletion confirmation dialog", None),
        item("dialog.delete_review.title", "Delete Review", "Title for review session deletion confirmation dialog", None),
        item("dialog.clear_history.title", "Clear Session History", "Title for session history wipe confirmation", None),
        item("dialog.clear_auto.title", "Clear Auto Context", "Title for clearing discovered files confirmation", None),
        item("dialog.clear_user.title", "Clear User Context", "Title for clearing user-added files confirmation", None),
        item("dialog.batch_cancel.title", "Stop Batch Execution", "Title for running batch cancellation dialog", None),
        item("dialog.model_select.title", "Select Model", "Title for model selector modal dialog", None),
        item("dialog.theme_select.title", "Select Color Theme", "Title for theme selector modal dialog", None),
        item("dialog.help.title", "Keyboard Commands", "Title for help modal dialog", None),
        item("dialog.execute_review.title_all", "Execute All Review Sessions", "Title when executing every review session", None),
        item("dialog.execute_review.title_single", "Execute Review Finding", "Title when executing a single review finding", None),
        item("dialog.execute_review.title_session", "Execute Review Session", "Title when executing a review session queue", None),
        item("dialog.execute_scope.title_single", "Execute Plan Step", "Title when executing a single plan step", None),
        item("dialog.execute_scope.title_scope", "Execute Plan Scope", "Title when executing a plan scope queue", None),
        item("dialog.squash.title", "Squash Commits into Developer Feature Commit", "Title for the squash commits dialog", None),
        item("dialog.squash_discard.title", "Discard Message", "Title for discarding a squash message dialog", None),
        item("dialog.squash_confirm.title", "Confirm Git Squash", "Title for the squash apply confirmation dialog", None),
        item("dialog.refine.title", "Deep Plan Refinement (AI Architect)", "Title for the architect plan refinement dialog", None),
        item("dialog.refine_blocked.title", "Plan Refinement Blocked", "Title for the architect refusal dialog", None),
        item("dialog.review_launch.title", "Launch Code Review", "Title for the review launch dialog", None),
        item("dialog.locale.title", "Interface Language", "Title for the interface language dialog", None),
        item("dialog.status.plan.title", "Change Task Status", "Title for the plan item status dialog", None),
        item("dialog.status.review.title", "Change Finding Status", "Title for the review item status dialog", None),
        item("dialog.server_disconnected.title", "Server Disconnected", "Title for the server disconnection modal", None),

        // ------------------------------------------------------------------
        // Dialog action button verbs (chords are appended at render time).
        // ------------------------------------------------------------------
        item("dialog.button.interrupt", "Interrupt", "Confirm turn interruption button", Some(14)),
        item("dialog.button.quit", "Quit TAUQE", "Confirm application quit button", Some(14)),
        item("dialog.button.detach", "Detach", "Confirm discussion focus detach button", Some(12)),
        item("dialog.button.replan_horizon", "Replan Horizon", "Replan the plan horizon button", Some(16)),
        item("dialog.button.dismiss", "Dismiss", "Dismiss dialog button", Some(12)),
        item("dialog.button.confirm_delete", "Confirm Delete", "Confirm destructive deletion button", Some(16)),
        item("dialog.button.confirm_clear", "Confirm Clear", "Confirm clearing a context layer button", Some(16)),
        item("dialog.button.resolve", "Resolve", "Resolve a single review finding button", Some(12)),
        item("dialog.button.resolve_queue", "Resolve Queue", "Resolve a review findings queue button", Some(16)),
        item("dialog.button.execute", "Execute", "Execute a single plan step button", Some(12)),
        item("dialog.button.execute_scope", "Execute Scope", "Execute a plan scope queue button", Some(16)),
        item("dialog.button.execute_pipeline", "Execute Pipeline", "Execute a multi-plan pipeline queue button", Some(18)),
        item("dialog.button.discard", "Discard", "Discard a squash message button", Some(12)),
        item("dialog.button.apply_squash", "Apply Squash", "Apply the squashed commit button", Some(14)),
        item("dialog.button.generate", "Generate via AI", "Generate a commit message with the model button", Some(18)),
        item("dialog.button.select", "Select", "Select the highlighted list item button", Some(10)),
        item("dialog.button.apply", "Apply", "Apply the selected status button", Some(10)),

        // ------------------------------------------------------------------
        // Dialog body text: questions, notes, and field labels for the
        // confirmation surfaces, the disconnection card, the status picker,
        // and the theme selector. Keeping every visible phrase here lets a
        // hot-swapped locale reach the dialog bodies without touching the
        // renderers (c-1, c-2).
        // ------------------------------------------------------------------
        item("dialog.confirm_cancel.question", "Interrupt active model operation?", "Confirm-cancel dialog question line", None),
        item("dialog.confirm_cancel.will_stop", "Generation will be stopped immediately.", "Confirm-cancel note that generation stops", None),
        item("dialog.confirm_cancel.discard_note", "All in-progress edits and uncommitted diffs will be discarded.", "Confirm-cancel note that edits are discarded", None),

        item("dialog.confirm_quit.question", "Quit TAUQE?", "Confirm-quit dialog question line", None),
        item("dialog.confirm_quit.activity_review", "Code review is currently in progress.", "Confirm-quit activity line while a review runs", None),
        item("dialog.confirm_quit.activity_model", "Model generation / turn is currently in progress.", "Confirm-quit activity line while a turn runs", None),
        item("dialog.confirm_quit.activity_draft", "You have an unsubmitted prompt draft in the editor.", "Confirm-quit activity line with a prompt draft", None),
        item("dialog.confirm_quit.activity_squash_applying", "Squash operation is currently being applied.", "Confirm-quit activity line while a squash applies", None),
        item("dialog.confirm_quit.activity_squash_message", "You have an uncommitted squash message in progress.", "Confirm-quit activity line with a squash message", None),
        item("dialog.confirm_quit.activity_default", "Active session work may be lost.", "Confirm-quit default activity line", None),
        item("dialog.confirm_quit.exit_note", "Exiting now will terminate the session and discard active work.", "Confirm-quit note about discarding active work", None),

        item("dialog.confirm_detach_focus.question", "Detach the active discussion focus?", "Confirm-detach-focus dialog question line", None),
        item("dialog.confirm_detach_focus.repl_note", "The REPL will return to the free codebase context.", "Confirm-detach-focus note about returning to the REPL", None),
        item("dialog.confirm_detach_focus.anchor_note", "The plan or review anchor is released until you focus on it again.", "Confirm-detach-focus note about releasing the anchor", None),

        item("dialog.execute_review.heading_all", "Resolve remaining findings sequentially across all sessions with automated verification?", "Execute-all-reviews dialog heading", None),
        item("dialog.execute_review.heading_single", "Resolve target review finding autonomously?", "Execute-single-finding dialog heading", None),
        item("dialog.execute_review.heading_session", "Resolve remaining findings sequentially with automated verification?", "Execute-review-session dialog heading", None),
        item("dialog.execute_review.review_label", " Review: ", "Execute-review review field label", None),
        item("dialog.execute_review.scope_label", " Scope: ", "Execute-review scope field label", None),
        item("dialog.execute_review.scope_count", " ({} finding(s) to resolve)", "Execute-review scope finding count", None),
        item("dialog.execute_review.files_label", " Files: ", "Execute-review files field label", None),
        item("dialog.execute_review.readonly_label", " Readonly: ", "Execute-review read-only files field label", None),
        item("dialog.execute_review.description_label", " Description: ", "Execute-review description field label", None),
        item("dialog.execute_review.recommendation_label", " Recommendation: ", "Execute-review recommendation field label", None),
        item("dialog.execute_review.queue_header", " Findings in execution queue:", "Execute-review queue header", None),
        item("dialog.execute_review.queue_more", "   ... and {} more finding(s)", "Execute-review queue overflow line", None),
        item("dialog.execute_review.note", "TAUQE will resolve each finding in an isolated turn with compiler verification gates.", "Execute-review note about isolated turns", None),
        item("dialog.execute_review.fail_fast", "Fail-Fast Policy: execution stops and rolls back immediately if any finding fails verification.", "Execute-review fail-fast policy note", None),
        item("dialog.execute_review.title_point", "Resolve Review Finding", "Title for point execution of a single finding or subplan step", None),
        item("dialog.execute_review.finding_label", " Finding: ", "Execute-review finding field label", None),
        item("dialog.execute_review.step_label", " Step: ", "Execute-review subplan step field label", None),
        item("dialog.execute_review.model_list_title", " Coder Model ", "Execute-review coder model list title with a single entry", None),
        item("dialog.execute_review.model_list_counter", " Coder Model  [{} of {}] ", "Execute-review coder model list title with a position counter", None),
        item("dialog.execute_review.no_models", "  (no coder models configured)", "Execute-review empty coder model list placeholder", None),
        item("dialog.execute_review.editor_title", "Coder Instructions (optional)", "Execute-review instructions editor block title", None),
        item("dialog.execute_review.editor_placeholder", " Additional implementation directives [optional]...", "Execute-review empty instructions placeholder", None),
        item("dialog.execute_review.hint_actions", "Enter resolve • ← / → button • Esc cancel", "Execute-review dialog footer hint while the action bar has focus", None),
        item("dialog.execute_review.hint_fields", "{}Enter resolve • Tab focus • {} model • Esc cancel", "Execute-review dialog footer hint while a field has focus", None),

        item("dialog.execute_scope.heading_single", "Execute target plan step autonomously?", "Execute-single-step dialog heading", None),
        item("dialog.execute_scope.heading_scope", "Execute plan steps sequentially with automated verification?", "Execute-scope dialog heading", None),
        item("dialog.execute_scope.plan_label", " Plan: ", "Execute-scope plan field label", None),
        item("dialog.execute_scope.scope_label", " Scope: ", "Execute-scope scope field label", None),
        item("dialog.execute_scope.scope_count", " ({} step(s) to execute)", "Execute-scope step count", None),
        item("dialog.execute_scope.details_label", " Details: ", "Execute-scope details field label", None),
        item("dialog.execute_scope.queue_header", " Steps in execution queue:", "Execute-scope queue header", None),
        item("dialog.execute_scope.queue_more", "   ... and {} more step(s)", "Execute-scope queue overflow line", None),
        item("dialog.execute_scope.note", "TAUQE will execute each step in an isolated turn with compiler verification gates.", "Execute-scope note about isolated turns", None),
        item("dialog.execute_scope.fail_fast", "Execution stops immediately if any step fails or is interrupted.", "Execute-scope stop-on-failure note", None),
        item("dialog.execute_scope.title_jit", "Execute Concept Plan", "Title when executing a concept-only plan whose horizon is projected just-in-time", None),
        item("dialog.execute_scope.heading_jit", "Project and execute the first implementation horizon autonomously?", "Execute-concept-plan dialog heading", None),
        item("dialog.execute_scope.jit_note", "No implementation horizon has been synthesized yet; the Architect projects the first one before the first step runs.", "Execute-concept-plan note about just-in-time horizon synthesis", None),

        item("dialog.execute_pipeline.title", "Execute Plan Pipeline", "Title for the multi-plan execution pipeline confirmation dialog", None),
        item("dialog.execute_pipeline.heading", "Execute every ready plan as one sequential pipeline?", "Execute-pipeline dialog heading", None),
        item("dialog.execute_pipeline.plans_label", " Plans: ", "Execute-pipeline plans field label", None),
        item("dialog.execute_pipeline.plans_count", "{} plan(s)", "Execute-pipeline styled plan count", None),
        item("dialog.execute_pipeline.steps_count", " ({} step(s) queued)", "Execute-pipeline queued step count", None),
        item("dialog.execute_pipeline.queue_header", " Plans in the pipeline:", "Execute-pipeline queue header", None),
        item("dialog.execute_pipeline.queue_more", "   ... and {} more plan(s)", "Execute-pipeline queue overflow line", None),
        item("dialog.execute_pipeline.jit_note", " {} plan(s) carry only their architectural concept and are projected just-in-time before the queue starts.", "Execute-pipeline note about just-in-time horizon projection", None),
        item("dialog.execute_pipeline.note", "TAUQE runs one plan at a time under a single operation and stops the queue if any plan fails.", "Execute-pipeline note about sequential execution", None),

        item("dialog.delete_plan.question", "Are you sure you want to delete this plan?", "Delete-plan dialog question line", None),
        item("dialog.delete_plan.plan_label", "Plan: ", "Delete-plan plan field label", None),
        item("dialog.delete_plan.note", "The plan file in .tauqe/plans/ will be permanently removed.", "Delete-plan note about removing the file", None),

        item("dialog.delete_review.question", "Are you sure you want to delete this review session?", "Delete-review dialog question line", None),
        item("dialog.delete_review.review_label", "Review: ", "Delete-review review field label", None),
        item("dialog.delete_review.note", "The review file in .tauqe/reviews/ will be permanently removed.", "Delete-review note about removing the file", None),

        item("dialog.clear_auto.question", "Clear all Auto-context files?", "Clear-auto-context dialog question line", None),
        item("dialog.clear_auto.body_prefix", "Are you sure you want to remove all ", "Clear-auto-context body prefix before the count", None),
        item("dialog.clear_auto.body_count", "{} file(s)", "Clear-auto-context styled file count", None),
        item("dialog.clear_auto.body_suffix", " from the Auto layer?", "Clear-auto-context body suffix after the count", None),
        item("dialog.clear_auto.note", "Persistent User files and Pinned files will not be affected.", "Clear-auto-context note about untouched layers", None),

        item("dialog.clear_user.question", "Clear all User-context files?", "Clear-user-context dialog question line", None),
        item("dialog.clear_user.body_prefix", "Are you sure you want to remove all ", "Clear-user-context body prefix before the count", None),
        item("dialog.clear_user.body_count", "{} file(s)", "Clear-user-context styled file count", None),
        item("dialog.clear_user.body_suffix", " from the User layer?", "Clear-user-context body suffix after the count", None),
        item("dialog.clear_user.note", "Protected Pinned files and Auto-requested files will not be affected.", "Clear-user-context note about untouched layers", None),

        item("dialog.clear_history.question", "Are you sure you want to clear conversation history?", "Clear-history dialog question line", None),
        item("dialog.clear_history.erase_note", "This will permanently erase session history:", "Clear-history note about erasing history", None),
        item("dialog.clear_history.view_note", "and clear the current model view output.", "Clear-history note about clearing the view", None),

        // ------------------------------------------------------------------
        // Clear implementation horizon (two-tier confirmation, c-2). The
        // concept tree survives the reset, so only the implementation steps
        // are described as removed; the second tier warns that the reset is
        // not a Git operation and cannot undo commits already recorded.
        // ------------------------------------------------------------------
        item("dialog.clear_horizon.title", "Clear Implementation Horizon", "Title for clearing implementation horizon confirmation dialog", None),
        item("dialog.clear_horizon.question", "Clear the implementation horizon of this plan?", "Question in clear horizon confirmation dialog", None),
        item("dialog.clear_horizon.note", "All implementation steps will be removed. The architectural concept tree will remain intact.", "Note in clear horizon confirmation dialog explaining concept is preserved", None),
        item("dialog.clear_horizon_final.title", "Confirm Clearing Active Horizon", "Title for two-tier clear horizon confirmation dialog with active or done work", None),
        item("dialog.clear_horizon_final.question", "Are you sure you want to clear completed or in-progress steps?", "Question in tier 2 clear horizon confirmation dialog", None),
        item("dialog.clear_horizon_final.git_note", "Warning: Completed or in-progress steps have already been recorded in Git commits. Clearing the horizon does NOT undo Git commits and may cause desynchronization between Git history and the plan.", "Warning note about Git commits desync in tier 2 clear horizon dialog", None),
        item("dialog.button.clear_horizon", "Clear Horizon", "Button to confirm clearing implementation horizon", Some(16)),
        item("dialog.button.confirm_clear_horizon", "Confirm Clear", "Button to confirm tier 2 clearing of implementation horizon", Some(16)),
        item("notify.plan_horizon_cleared", "Implementation horizon cleared; concept preserved", "Notification after successfully clearing plan horizon", None),
        item("notify.plan_horizon_already_empty", "Plan has no implementation steps to clear", "Notification when clear horizon is invoked on plan with empty horizon", None),

        item("dialog.batch_cancel.question", "Cancel batch execution?", "Batch-cancel dialog question line", None),
        item("dialog.batch_cancel.continue_note", "Continue keeps the batch running.", "Batch-cancel continue action note", None),
        item("dialog.batch_cancel.soft_stop_note", "Soft Stop finishes the current step, then stops the queue.", "Batch-cancel soft-stop action note", None),
        item("dialog.batch_cancel.hard_stop_note", "Hard Stop aborts the current step immediately.", "Batch-cancel hard-stop action note", None),

        // ------------------------------------------------------------------
        // Multi-round continuation dialog (raised between bounded batch rounds,
        // c-4). It reuses the three-way Continue / Soft Stop / Hard Stop
        // vocabulary of the batch-cancel dialog, so a developer who stops a
        // multi-round run makes the same explicit choice as one who interrupts
        // a single round.
        // ------------------------------------------------------------------
        item("dialog.batch_round.title", "Continue Multi-Round Execution", "Title for the dialog offering the next bounded batch round", None),
        item("dialog.batch_round.question", "Start the next bounded round?", "Question in the multi-round continuation dialog", None),
        item("dialog.batch_round.progress", "Rounds completed: {} of {} ({} available)", "Progress line naming the completed, total, and available rounds", None),
        item("dialog.batch_round.continue_note", "Continue launches the next round over the remaining work.", "Multi-round continue action note", None),
        item("dialog.batch_round.soft_stop_note", "Soft Stop keeps every result and ends the loop here.", "Multi-round soft-stop action note", None),
        item("dialog.batch_round.hard_stop_note", "Hard Stop ends the loop without discarding committed work.", "Multi-round hard-stop action note", None),
        item("notify.batch_round_launched", "Round {} launched", "Notification when the next bounded round is launched", None),
        item("notify.batch_round_stopped", "Multi-round execution stopped after {} round(s)", "Notification when the multi-round loop ends", None),

        item("dialog.server_disconnected.terminated", "Connection to tauqe-server was terminated", "Server-disconnected card heading", None),
        item("dialog.server_disconnected.footer", "q / Esc / Ctrl+Q quit", "Server-disconnected footer hint", None),

        item("dialog.theme_select.preview_note", "Preview applies live.", "Theme selector caption that previews live", None),
        item("dialog.theme_select.active_badge", "[active]", "Theme selector badge marking the active preset", None),
        item("dialog.theme_select.footer", "n/p navigate • Enter save • Esc cancel", "Theme selector footer hint", None),

        item("dialog.status.item_label", " Item: ", "Status dialog target item label", None),
        item("dialog.status.option.discussion", "Under review / requires discussion", "Status option description for DISCUSSION", None),
        item("dialog.status.plan.option.todo", "Approved / ready for execution", "Plan status option description for TODO", None),
        item("dialog.status.plan.option.in_progress", "Currently in active development", "Plan status option description for IN_PROGRESS", None),
        item("dialog.status.plan.option.done", "Completed and verified", "Plan status option description for DONE", None),
        item("dialog.status.plan.option.blocked", "Blocked by unmet prerequisites / needs replanning", "Plan status option description for BLOCKED", None),
        item("dialog.status.plan.option.cancelled", "Superseded or cancelled", "Plan status option description for CANCELLED", None),
        item("dialog.status.review.option.todo", "Approved / ready for implementation", "Review status option description for TODO", None),
        item("dialog.status.review.option.in_progress", "Currently being fixed", "Review status option description for IN_PROGRESS", None),
        item("dialog.status.review.option.refine", "Needs architectural decomposition before coding", "Review status option description for REFINE", None),
        item("dialog.status.review.option.fixed", "Resolved and addressed in code", "Review status option description for FIXED", None),
        item("dialog.status.review.option.skipped", "Skipped / no code changes needed", "Review status option description for SKIPPED", None),
        item("dialog.status.review.option.rejected", "False positive / won't fix", "Review status option description for REJECTED", None),
        item("dialog.status.fast_select_hint", "{} fast select • n / p or j / k navigate • Enter apply • Esc cancel", "Status dialog footer hint", None),

        // ------------------------------------------------------------------
        // Help subsystem: tabs, cognitive tiers, and footer legend.
        // ------------------------------------------------------------------
        item("help.tab.global", "Global", "Help tab listing application-wide shortcuts", Some(12)),
        item("help.tab.current_mode", "Current Mode", "Help tab listing active view shortcuts", Some(14)),
        item("help.tab.text_editor", "Text Editor", "Help tab listing shared editor shortcuts", Some(14)),
        item("help.category.essential", "Essential", "Help tier of costly work-cycle commands", Some(12)),
        item("help.category.interface", "Interface", "Help tier of navigation and view commands", Some(12)),
        item("help.category.helpers", "Helpers", "Help tier of auxiliary commands", Some(12)),
        item("help.close", "Close Help", "Close the Help dialog button", Some(14)),
        item("help.footer", "Tab / Shift+Tab · 1 / 2 / 3 switch tabs • n/p or j/k scroll • PgUp/PgDn page • Home top", "Help dialog navigation legend", None),

        // ------------------------------------------------------------------
        // Context view section headers and picker labels.
        // ------------------------------------------------------------------
        item("context.total_size", "Total size:", "Context summary total-size label", None),
        item("context.section.pinned", "Pinned Files", "Context Pinned layer section header", None),
        item("context.section.user", "User Files", "Context User layer section header", None),
        item("context.section.auto", "Auto Files", "Context Auto layer section header", None),
        item("context.section.pinned_hint", " [Protected / Config]", "Context Pinned section header hint", None),
        item("context.section.user_hint", " [Editable / Read-Only]", "Context User section header hint", None),
        item("context.section.auto_hint", " [Model Requested]", "Context Auto section header hint", None),
        item("context.badge.editable", "[EDITABLE]", "Badge marking an editable context file", Some(12)),
        item("context.badge.readonly", "[READ-ONLY]", "Badge marking a read-only context file", Some(14)),
        item("context.info.pinned", "Pinned layer: Architecture & reference documents specified in tauqe.toml", "Context info line for the Pinned layer", None),
        item("context.info.user", "User layer: Persistent files curated by the developer (Editable or Read-Only)", "Context info line for the User layer", None),
        item("context.info.auto", "Auto layer: Files autonomously requested by the model during discovery rounds", "Context info line for the Auto layer", None),
        item("context.info.default", "Context state: 3 layers (Pinned, User, Auto) | Press '?' for commands", "Context info line when no row is selected", None),
        item("context.picker.title_editable", "Add EDITABLE to Context (Path, directory or glob like *.rs)", "Context file picker title for editable mode", None),
        item("context.picker.title_readonly", "Add READ-ONLY to Context (Path, directory or glob like *.rs)", "Context file picker title for read-only mode", None),
        item("context.picker.file_pattern", "File / Pattern:", "Context file picker input label", None),
        item("context.picker.loading", "Loading repository files from server...", "Context file picker loading message", None),
        item("context.picker.no_match", "No matching files found.", "Context file picker empty-result message", None),

        // ------------------------------------------------------------------
        // History view labels.
        // ------------------------------------------------------------------
        item("history.item_counter", "History | Item {}/{}", "History info line item counter", None),
        item("history.auto_scroll_on", "[Auto-scroll: ON]", "History auto-scroll enabled indicator", None),
        item("history.auto_scroll_off", "[Auto-scroll: OFF]", "History auto-scroll disabled indicator", None),

        // ------------------------------------------------------------------
        // Plans view headers and empty state.
        // ------------------------------------------------------------------
        item("plans.empty_title", "No local engineering plans yet.", "Plans view empty-state heading", None),
        item("plans.empty_hint", "Ask Tauqe AI in Develop (Ctrl+1) to formulate a multi-step task plan, or press 'g' to refresh.", "Plans view empty-state hint", None),
        item("plans.refining", "[REFINING: AI Architect is analyzing codebase...]", "Plans view refinement-in-progress indicator", None),
        item("plans.plan_label", "Plan:", "Plans info line plan label", None),
        item("plans.concept_summary", "Concept: {} item(s) of architectural intent", "Plans info line concept section summary", None),
        item("plans.execution_blueprint", "Execution blueprint: {}/{} done", "Plans info line implementation progress", None),
        item("plans.files_prefix", "files:", "Plans info line editable file list prefix", None),
        item("plans.ref_prefix", "ref:", "Plans info line reference file list prefix", None),

        // ------------------------------------------------------------------
        // Review view headers and empty state.
        // ------------------------------------------------------------------
        item("review.session_label", "Review:", "Review info line session label", None),
        item("review.executive_summary", "Executive Summary", "Review session executive summary block heading", None),
        item("review.refining", "[Refining: Architect is decomposing the finding...]", "Review view refinement-in-progress indicator", None),
        item("review.in_progress", "Review in progress...", "Review view running indicator", None),
        item("review.running_hint", "Streaming findings and summary in Develop view (Ctrl+1).", "Review view hint while a review streams", None),
        item("review.empty_title", "No code reviews yet.", "Review view empty-state heading", None),
        item("review.empty_hint", "Add files to Context (Ctrl+4), then press 'N' to run a code review.", "Review view empty-state hint", None),
        item("review.filter_open", "Filter: Open only", "Review view open-findings filter indicator", None),

        // ------------------------------------------------------------------
        // REPL (Develop) startup greeting. The greeting is a transient welcome
        // surface shown only while the response area is empty, so its motto and
        // quick-start verbs are localized like every other interface text.
        // ------------------------------------------------------------------
        item("splash.motto", "The Answer to the Ultimate Question of Engineering", "Develop splash motto revealed by the typewriter effect", None),
        item("splash.hint.help", "Help", "Develop splash quick-start hint verb for opening Help", None),
        item("splash.hint.send", "Send prompt", "Develop splash quick-start hint verb for sending a prompt", None),

        // ------------------------------------------------------------------
        // REPL (Develop) view badges and hints.
        // ------------------------------------------------------------------
        item("repl.focus_badge_plan", "[PLAN]", "Develop discussion focus badge for a plan", Some(12)),
        item("repl.focus_badge_review", "[REVIEW]", "Develop discussion focus badge for a review", Some(14)),
        item("repl.detach_hint", "[Esc to detach]", "Develop hint for detaching the discussion focus", None),
        item("repl.viewport_title", "[VIEWPORT]", "Develop viewport focus title", Some(14)),
        item("repl.stepper_plan", "Plan:", "Develop plan stepper header label", None),

        // ------------------------------------------------------------------
        // Onboarding (Setup) view headings.
        // ------------------------------------------------------------------
        item("onboarding.title", "Tauqe Initial Setup (Onboarding)", "Onboarding view border title", None),
        item("onboarding.step.git_title", "Step 0: Initialize Git Repository", "Onboarding Git initialization step heading", None),
        item("onboarding.step.config_title", "Step 1: Project Configuration (tauqe.toml)", "Onboarding configuration step heading", None),
        item("onboarding.step.credentials_title", "Step 2: Credentials and OpenRouter API Key", "Onboarding credentials step heading", None),
        item("onboarding.step.workstation_title", "Step 3: Workstation Keyboard Layout and Mapping (tui.toml)", "Onboarding workstation step heading", None),
        item("onboarding.step.modifier_title", "Step 4: Primary Command Modifier (C-)", "Onboarding command modifier step heading", None),
        item("onboarding.step.gatekeeper_title", "Warning: System Not Ready", "Onboarding gatekeeper step heading", None),
        item("onboarding.step.ready_title", "Step 5: System is successfully configured and ready to use!", "Onboarding ready step heading", None),

        // ------------------------------------------------------------------
        // Onboarding (Setup) chrome: the step badge that replaces the
        // credentials step when the API key is missing, the per-step
        // information panel, the status and error line prefixes, the ready
        // summary values, and the custom langmap input hint.
        // ------------------------------------------------------------------
        item("onboarding.gatekeeper_badge", "Warning: API Key Missing", "Onboarding step badge replacing the credentials step when the API key is missing", Some(28)),
        item("onboarding.info_title", "Information", "Onboarding information panel title", Some(16)),
        item("onboarding.info.git", "Tauqe relies on Git for checkpoints, atomic step commits, and safe undo.", "Onboarding information line for the Git step", None),
        item("onboarding.info.config", "Create default tauqe.toml or skip to use built-in defaults.", "Onboarding information line for the configuration step", None),
        item("onboarding.info.credentials", "Secure credential storage: created with 0600 file permissions (owner-only read/write).", "Onboarding information line for the credentials step", None),
        item("onboarding.info.workstation", "Workstation configuration: personal settings stored in ~/.config/tauqe/tui.toml.", "Onboarding information line for the workstation step", None),
        item("onboarding.info.modifier", "Primary command modifier for text input: choose Ctrl (default) or Alt for legacy/macOS terminals.", "Onboarding information line for the command modifier step", None),
        item("onboarding.info.gatekeeper", "OpenRouter API key is required for LLM calls. Select an action.", "Onboarding information line for the gatekeeper step", None),
        item("onboarding.info.ready", "All parameters verified. Press Enter to start.", "Onboarding information line for the ready step", None),
        item("onboarding.error_prefix", "Error: {}", "Onboarding error line prefix before the diagnostic message", None),
        item("onboarding.status_prefix", "✓ {}", "Onboarding status line prefix before the confirmation message", None),
        item("onboarding.ready.builtin_defaults", "Built-in defaults", "Onboarding ready summary value when no configuration file exists", None),
        item("onboarding.ready.modifier_ctrl", "Ctrl (Default)", "Onboarding ready summary value for the Ctrl command modifier", Some(20)),
        item("onboarding.ready.modifier_alt", "Alt", "Onboarding ready summary value for the Alt command modifier", Some(20)),
        item("onboarding.input.langmap_hint", "  (Enter: Save, Esc: Cancel)", "Onboarding custom langmap input hint", None),

        // ------------------------------------------------------------------
        // Command descriptions for the global command map.
        // ------------------------------------------------------------------
        item("cmd.global.cancel_generation", "Cancel the active generation", "Global command description", None),
        item("cmd.global.squash_commits", "Open the squash AI commits dialog", "Global command description", None),
        item("cmd.global.switch_repl", "Switch to the REPL view", "Global command description", None),
        item("cmd.global.switch_plans", "Switch to the Plans view", "Global command description", None),
        item("cmd.global.switch_review", "Switch to the Review view", "Global command description", None),
        item("cmd.global.switch_context", "Switch to the Context view", "Global command description", None),
        item("cmd.global.switch_history", "Switch to the History view", "Global command description", None),
        item("cmd.global.select_model", "Select the active model", "Global command description", None),
        item("cmd.global.select_theme", "Select the color theme", "Global command description", None),
        item("cmd.global.select_locale", "Change the interface language (locale)", "Global command description", None),
        item("cmd.global.open_usage_summary", "Open the session and operation usage breakdown", "Global command description", None),
        item("cmd.global.toggle_reasoning", "Fold or unfold the reasoning stream", "Global command description", None),
        item("cmd.global.open_help", "Open the Help dialog", "Global command description", None),
        item("cmd.global.reload_config", "Reload configuration from disk", "Global command description", None),
        item("cmd.global.quit", "Quit the application", "Global command description", None),
        item("cmd.global.keyboard_quit", "Abort the active operation", "Global command description", None),

        // ------------------------------------------------------------------
        // Command descriptions for the shared text editor map.
        // ------------------------------------------------------------------
        item("cmd.editor.move_char_backward", "Move one character backward", "Text editor command description", None),
        item("cmd.editor.move_char_forward", "Move one character forward", "Text editor command description", None),
        item("cmd.editor.move_word_backward", "Move one word backward", "Text editor command description", None),
        item("cmd.editor.move_word_forward", "Move one word forward", "Text editor command description", None),
        item("cmd.editor.move_beginning_of_line", "Move to the beginning of the line", "Text editor command description", None),
        item("cmd.editor.move_end_of_line", "Move to the end of the line", "Text editor command description", None),
        item("cmd.editor.move_buffer_start", "Move to the beginning of the buffer", "Text editor command description", None),
        item("cmd.editor.move_buffer_end", "Move to the end of the buffer", "Text editor command description", None),
        item("cmd.editor.move_line_up", "Move the cursor one line up", "Text editor command description", None),
        item("cmd.editor.move_line_down", "Move the cursor one line down", "Text editor command description", None),
        item("cmd.editor.delete_forward", "Delete the character forward", "Text editor command description", None),
        item("cmd.editor.insert_newline", "Insert a newline", "Text editor command description", None),
        item("cmd.editor.kill_word_backward", "Kill the word before the cursor", "Text editor command description", None),
        item("cmd.editor.kill_word_forward", "Kill the word after the cursor", "Text editor command description", None),
        item("cmd.editor.kill_to_beginning", "Kill to the beginning of the line", "Text editor command description", None),
        item("cmd.editor.kill_line", "Kill to the end of the line", "Text editor command description", None),
        item("cmd.editor.yank", "Yank from the kill ring or the clipboard", "Text editor command description", None),

        // ------------------------------------------------------------------
        // Command descriptions for the shared dialog command map.
        // ------------------------------------------------------------------
        item("cmd.dialog.dismiss", "Dismiss the dialog", "Dialog command description", None),
        item("cmd.dialog.focus_prev", "Focus the previous control", "Dialog command description", None),
        item("cmd.dialog.focus_next", "Focus the next control", "Dialog command description", None),
        item("cmd.dialog.activate", "Activate the focused control", "Dialog command description", None),
        item("cmd.dialog.confirm_fast", "Confirm immediately", "Dialog command description", None),
        item("cmd.dialog.cancel_fast", "Cancel immediately", "Dialog command description", None),

        // ------------------------------------------------------------------
        // Command descriptions for the Context view.
        // ------------------------------------------------------------------
        item("cmd.context.clear_user", "Clear all persistent User files", "Context command description", None),
        item("cmd.context.clear_auto", "Clear all Auto-requested files", "Context command description", None),
        item("cmd.context.add_editable", "Add a file to context as editable", "Context command description", None),
        item("cmd.context.add_readonly", "Add a file to context as read-only", "Context command description", None),
        item("cmd.context.toggle_access", "Toggle access between Editable and Read-Only", "Context command description", None),
        item("cmd.context.promote_user", "Promote an Auto file to the persistent User layer", "Context command description", None),
        item("cmd.context.activate", "Promote an Auto file or fold / unfold the selected section", "Context command description", None),
        item("cmd.context.return", "Return to the REPL", "Context command description", None),
        item("cmd.context.nav_prev", "Select the previous row", "Context command description", None),
        item("cmd.context.nav_next", "Select the next row", "Context command description", None),
        item("cmd.context.select_first", "Select the first row", "Context command description", None),
        item("cmd.context.select_last", "Select the last row", "Context command description", None),
        item("cmd.context.page_up", "Scroll the context view up a page", "Context command description", None),
        item("cmd.context.page_down", "Scroll the context view down a page", "Context command description", None),
        item("cmd.context.fold_toggle", "Fold or unfold the selected section", "Context command description", None),
        item("cmd.context.fold_all", "Toggle fold or unfold for all context sections", "Context command description", None),
        item("cmd.context.copy", "Copy file path to clipboard", "Context command description", None),
        item("cmd.context.refresh", "Refresh context from the server", "Context command description", None),
        item("cmd.context.remove", "Remove the selected file from context", "Context command description", None),

        // ------------------------------------------------------------------
        // Command descriptions for the History view.
        // ------------------------------------------------------------------
        item("cmd.history.clear", "Clear the conversation history (with confirmation)", "History command description", None),
        item("cmd.history.fold_toggle", "Fold or unfold the active history item", "History command description", None),
        item("cmd.history.copy_block", "Copy the code block of the active item", "History command description", None),
        item("cmd.history.copy_item", "Copy the selected item text to the clipboard", "History command description", None),
        item("cmd.history.return", "Return to the REPL", "History command description", None),
        item("cmd.history.nav_prev", "Select the previous item, fetching older turns at the top", "History command description", None),
        item("cmd.history.nav_next", "Select the next history item", "History command description", None),
        item("cmd.history.recenter", "Recenter the active history item", "History command description", None),
        item("cmd.history.scroll_up", "Scroll the history up one line", "History command description", None),
        item("cmd.history.scroll_down", "Scroll the history down one line", "History command description", None),
        item("cmd.history.page_up", "Scroll the history up one page", "History command description", None),
        item("cmd.history.page_down", "Scroll the history down one page", "History command description", None),
        item("cmd.history.select_first", "Jump to the oldest message", "History command description", None),
        item("cmd.history.select_last", "Jump to the newest message", "History command description", None),

        // ------------------------------------------------------------------
        // Command descriptions for the Plans view.
        // ------------------------------------------------------------------
        item("cmd.plans.activate", "Execute a leaf step, or fold groups and headers", "Plans command description", None),
        item("cmd.plans.execute", "Execute the selected step, group, or entire plan", "Plans command description", None),
        item("cmd.plans.execute_all", "Execute every ready step of the selected plan as one batch (with confirmation)", "Plans command description", None),
        item("cmd.plans.replan", "Refine or replan the selected plan horizon with the Architect", "Plans command description", None),
        item("cmd.plans.replan_all", "Replan the horizon to resolve every blocked step with the Architect", "Plans command description", None),
        item("cmd.plans.discuss", "Discuss the selection with AI in the REPL", "Plans command description", None),
        item("cmd.plans.change_status", "Change the step status", "Plans command description", None),
        item("cmd.plans.return", "Return to the REPL", "Plans command description", None),
        item("cmd.plans.accordion_next", "Walk to the next item (accordion)", "Plans command description", None),
        item("cmd.plans.accordion_prev", "Walk to the previous item (accordion)", "Plans command description", None),
        item("cmd.plans.nav_next", "Select the next row", "Plans command description", None),
        item("cmd.plans.nav_prev", "Select the previous row", "Plans command description", None),
        item("cmd.plans.select_first", "Select the first row", "Plans command description", None),
        item("cmd.plans.select_last", "Select the last row", "Plans command description", None),
        item("cmd.plans.fold_toggle", "Fold or unfold the selected plan, section, or step", "Plans command description", None),
        item("cmd.plans.fold_all", "Toggle fold or unfold for all plans, sections, and items", "Plans command description", None),
        item("cmd.plans.page_up", "Scroll the plan view up a page", "Plans command description", None),
        item("cmd.plans.page_down", "Scroll the plan view down a page", "Plans command description", None),
        item("cmd.plans.set_focus", "Set the discussion focus to the selection without leaving the view", "Plans command description", None),
        item("cmd.plans.copy_markdown", "Copy the plan as Markdown to the clipboard", "Plans command description", None),
        item("cmd.plans.clear_horizon", "Clear the implementation horizon of the selected plan (with confirmation)", "Plans command description", None),
        item("cmd.plans.refresh", "Refresh plans from the server", "Plans command description", None),
        item("cmd.plans.delete", "Delete the plan", "Plans command description", None),

        // ------------------------------------------------------------------
        // Command descriptions for the Review view.
        // ------------------------------------------------------------------
        item("cmd.review.execute_all", "Execute all Todo findings across all sessions (with confirmation)", "Review command description", None),
        item("cmd.review.execute", "Execute the selected finding or all Todo findings in the session", "Review command description", None),
        item("cmd.review.refine", "Decompose the selected finding into steps with the Architect", "Review command description", None),
        item("cmd.review.refine_all", "Decompose every finding awaiting architectural refinement", "Review command description", None),
        item("cmd.review.run", "Run a code review on the current context", "Review command description", None),
        item("cmd.review.toggle_hide_closed", "Toggle the filter between all findings and open findings only", "Review command description", None),
        item("cmd.review.set_focus", "Set the discussion focus to the selection without leaving the view", "Review command description", None),
        item("cmd.review.activate", "Execute the selected finding; fold or unfold on session headers", "Review command description", None),
        item("cmd.review.discuss", "Discuss the selected finding or the entire session in the REPL", "Review command description", None),
        item("cmd.review.change_status", "Change the finding status", "Review command description", None),
        item("cmd.review.return", "Return to the REPL", "Review command description", None),
        item("cmd.review.nav_prev", "Select the previous row", "Review command description", None),
        item("cmd.review.nav_next", "Select the next row", "Review command description", None),
        item("cmd.review.accordion_next", "Inspect the next finding (accordion walk)", "Review command description", None),
        item("cmd.review.accordion_prev", "Inspect the previous finding (accordion walk)", "Review command description", None),
        item("cmd.review.select_first", "Select the first row", "Review command description", None),
        item("cmd.review.select_last", "Select the last row", "Review command description", None),
        item("cmd.review.page_up", "Scroll the review view up one page", "Review command description", None),
        item("cmd.review.page_down", "Scroll the review view down one page", "Review command description", None),
        item("cmd.review.fold_toggle", "Fold or unfold the session, summary, or finding details", "Review command description", None),
        item("cmd.review.fold_all", "Toggle fold or unfold for all sessions and findings", "Review command description", None),
        item("cmd.review.copy", "Copy the selected finding or session to the clipboard", "Review command description", None),
        item("cmd.review.delete", "Delete the review session (with confirmation)", "Review command description", None),
        item("cmd.review.refresh", "Refresh review sessions from the server", "Review command description", None),

        // ------------------------------------------------------------------
        // Command descriptions for the REPL (Develop) view.
        // ------------------------------------------------------------------
        item("cmd.repl.send", "Send the prompt", "REPL command description", None),
        item("cmd.repl.squash", "Open the squash AI commits dialog", "REPL command description", None),
        item("cmd.repl.toggle_focus", "Switch focus between the prompt and the Viewport", "REPL command description", None),
        item("cmd.repl.toggle_file_fold", "Fold or unfold the selected file diff", "REPL command description", None),
        item("cmd.repl.prev_file", "Select the previous modified file", "REPL command description", None),
        item("cmd.repl.next_file", "Select the next modified file", "REPL command description", None),
        item("cmd.repl.scroll_up", "Scroll the response up", "REPL command description", None),
        item("cmd.repl.scroll_down", "Scroll the response down", "REPL command description", None),
        item("cmd.repl.page_up", "Scroll the response up one page", "REPL command description", None),
        item("cmd.repl.page_down", "Scroll the response down one page", "REPL command description", None),
        item("cmd.repl.line_up_or_history_prev", "Move the cursor up one line; navigate history at the first line", "REPL command description", None),
        item("cmd.repl.line_down_or_history_next", "Move the cursor down one line; navigate history at the last line", "REPL command description", None),
        item("cmd.repl.move_line_up", "Move the cursor up one line", "REPL command description", None),
        item("cmd.repl.move_line_down", "Move the cursor down one line", "REPL command description", None),
        item("cmd.repl.history_prev", "Navigate the prompt history backward", "REPL command description", None),
        item("cmd.repl.history_next", "Navigate the prompt history forward", "REPL command description", None),
        item("cmd.repl.move_beginning_of_line", "Move the cursor to the beginning of the line", "REPL command description", None),
        item("cmd.repl.move_end_of_line", "Move the cursor to the end of the line", "REPL command description", None),
        item("cmd.repl.move_char_backward", "Move the cursor one character backward", "REPL command description", None),
        item("cmd.repl.move_char_forward", "Move the cursor one character forward", "REPL command description", None),
        item("cmd.repl.move_word_backward", "Move the cursor one word backward", "REPL command description", None),
        item("cmd.repl.move_word_forward", "Move the cursor one word forward", "REPL command description", None),
        item("cmd.repl.insert_spaces", "Insert two spaces", "REPL command description", None),
        item("cmd.repl.insert_newline", "Insert a newline", "REPL command description", None),
        item("cmd.repl.delete_backward", "Delete the character backward", "REPL command description", None),
        item("cmd.repl.kill_word_backward", "Kill the word before the cursor", "REPL command description", None),
        item("cmd.repl.delete_forward", "Delete the character forward", "REPL command description", None),
        item("cmd.repl.kill_line", "Kill to the end of the line", "REPL command description", None),
        item("cmd.repl.kill_to_beginning", "Kill to the beginning of the line", "REPL command description", None),
        item("cmd.repl.yank", "Yank from the kill ring or the clipboard", "REPL command description", None),
        item("cmd.repl.copy_response", "Copy the model response to the clipboard", "REPL command description", None),
        item("cmd.repl.clear_prompt", "Interrupt, or clear the prompt (saving it to the kill ring)", "REPL command description", None),

        // ------------------------------------------------------------------
        // Command descriptions for the Onboarding (Setup) view.
        // ------------------------------------------------------------------
        item("cmd.onboarding.execute", "Execute selected onboarding action", "Onboarding command description", None),
        item("cmd.onboarding.exit_help", "Exit help dialog", "Onboarding command description", None),
        item("cmd.onboarding.toggle_key", "Show / hide entered key", "Onboarding command description", None),
        item("cmd.onboarding.paste_key", "Paste API key from clipboard", "Onboarding command description", None),
        item("cmd.onboarding.clear_key_input", "Clear key input buffer", "Onboarding command description", None),
        item("cmd.onboarding.reload_config", "Reload configuration from disk", "Onboarding command description", None),

        // ------------------------------------------------------------------
        // Context view notifications.
        // ------------------------------------------------------------------
        item("notify.promoting_to_user", "Promoting '{}' to User context...", "Context notification when promoting an Auto file", None),
        item("notify.only_auto_promotable", "Only Auto files can be promoted to User context", "Context notification rejecting promotion", None),
        item("notify.no_user_files", "No user files to clear", "Context notification for an empty User layer", None),
        item("notify.no_auto_files", "No auto files to clear", "Context notification for an empty Auto layer", None),
        item("notify.pinned_readonly", "Pinned files are read-only and cannot be changed", "Context notification rejecting a Pinned access change", None),
        item("notify.pinned_protected_remove", "Pinned files are protected and cannot be removed", "Context notification rejecting a Pinned removal", None),
        item("notify.pinned_protected_section", "Pinned section is protected and cannot be removed", "Context notification rejecting a Pinned section removal", None),
        item("notify.context_copied", "File path copied to clipboard", "Context notification after copying file path", None),
        item("notify.context_copied_osc52", "File path sent to terminal clipboard (OSC 52)", "Context notification after copying file path via OSC 52", None),
        item("notify.context_no_files_copy", "No files in section to copy", "Context notification when copying an empty section", None),

        // ------------------------------------------------------------------
        // Plans view notifications.
        // ------------------------------------------------------------------
        item("notify.plan_not_found", "Plan '{}' not found", "Plans notification when a plan is missing", None),
        item("notify.plan_scope_discussion", "Scope contains {} item(s) in DISCUSSION ({}). Review or approve before executing.", "Plans notification blocking a scope with discussion items", None),
        item("notify.plan_scope_blocked", "Scope contains {} step(s) in BLOCKED status ({}). Resolve blockers or press 'R' to replan horizon before executing.", "Plans notification blocking a scope with blocked steps", None),
        item("notify.plan_scope_all_completed", "All steps in this scope are already completed.", "Plans notification for a completed scope", None),
        item("notify.plan_scope_dependencies", "Scope contains {} step(s) with unmet dependencies ({}). Complete prerequisites first.", "Plans notification blocking a scope with unmet dependencies", None),
        item("notify.plan_step_blocked", "Step is BLOCKED; resolve the blocker or press 'R' to replan horizon before executing.", "Plans notification for a blocked step", None),
        item("notify.plan_step_cancelled", "Step is CANCELLED; change status to TODO before executing.", "Plans notification for a cancelled step", None),
        item("notify.plan_item_not_found", "Target item not found in plan.", "Plans notification when a target item is missing", None),
        item("notify.plan_pipeline_empty", "No plan has a ready step or a synthesized horizon to execute", "Plans notification when the pipeline queue is empty", None),
        item("notify.plan_concept_section_not_executable", "Concept section cannot be executed; execute implementation steps below.", "Plans notification rejecting concept-section execution", None),
        item("notify.plan_concept_item_not_executable", "Concept item cannot be executed directly; execute implementation steps or press 'd' to discuss.", "Plans notification rejecting concept-item execution", None),
        item("notify.plan_concept_no_status", "Concept items define architectural requirements and do not have task statuses. Press 'd' to discuss.", "Plans notification explaining concept items have no status", None),
        item("notify.plan_concept_discuss", "Concept items define architectural requirements. Press 'd' to discuss in Develop.", "Plans notification directing concept discussion to Develop", None),
        item("notify.plan_focus_set", "Discussion focus set to plan '{}'", "Plans notification confirming a focus change", None),
        item("notify.plan_focus_item_set", "Discussion focus set to {} in plan '{}'", "Plans notification confirming an item focus change", None),
        item("notify.plan_copied", "Plan copied to clipboard", "Plans notification after copying a plan", None),
        item("notify.plan_copied_osc52", "Plan sent to terminal clipboard (OSC 52)", "Plans notification after an OSC 52 copy", None),
        item("notify.plan_copy_failed", "Failed to copy plan to clipboard", "Plans notification when copying fails", None),
        item("notify.model_busy", "Model is currently busy; wait for operation to finish", "Notification when the model is busy", None),
        item("notify.model_busy_esc", "Model is currently busy; wait for turn to finish or press Esc to cancel", "Notification when the model is busy during a turn", None),

        // ------------------------------------------------------------------
        // Review view notifications.
        // ------------------------------------------------------------------
        item("notify.review_not_found", "Review session '{}' not found", "Review notification when a session is missing", None),
        item("notify.review_item_not_found", "Review item #{} not found", "Review notification when a finding is missing", None),
        item("notify.review_refine_required", "Finding #{} is in REFINE status: decompose it into steps with the Architect (R) before executing.", "Review notification requiring architectural refinement", None),
        item("notify.review_discussion_status", "Finding #{} is in DISCUSSION status; approve or change status to TODO (t) before executing.", "Review notification blocking a discussion finding", None),
        item("notify.review_already_fixed", "Finding #{} is already FIXED.", "Review notification for an already fixed finding", None),
        item("notify.review_skipped_status", "Finding #{} is SKIPPED; change status to TODO (t) before executing.", "Review notification blocking a skipped finding", None),
        item("notify.review_rejected_status", "Finding #{} is REJECTED; change status to TODO (t) before executing.", "Review notification blocking a rejected finding", None),
        item("notify.review_all_fixed_session", "All findings in this review session are already FIXED.", "Review notification for a completed session", None),
        item("notify.review_no_todo_session", "No TODO findings to execute in this review session.", "Review notification when a session has no TODO findings", None),
        item("notify.review_no_sessions", "No review sessions available.", "Review notification when no sessions exist", None),
        item("notify.review_no_findings", "No findings in review sessions.", "Review notification when sessions have no findings", None),
        item("notify.review_discussion_only", "All remaining findings are in DISCUSSION status; approve or change status to TODO (t) before executing.", "Review notification when only discussion findings remain", None),
        item("notify.review_all_fixed", "All findings across all review sessions are already FIXED.", "Review notification when all sessions are complete", None),
        item("notify.review_no_todo_cross", "No TODO findings to execute across review sessions.", "Review notification when no cross-session TODO findings remain", None),
        item("notify.review_refining", "Decomposing finding with the Architect...", "Review notification while refining a finding", None),
        item("notify.review_select_finding", "Select a finding to decompose into steps (r)", "Review notification prompting to select a finding", None),
        item("notify.review_no_refine_needed", "No findings require architectural decomposition", "Review notification when no findings need refinement", None),
        item("notify.review_focus_set", "Discussion focus set to review '{}'", "Review notification confirming a session focus change", None),
        item("notify.review_focus_item_set", "Discussion focus set to {} in review '{}'", "Review notification confirming a finding focus change", None),
        item("notify.review_cannot_start_busy", "Cannot start a review while the model is generating", "Review notification rejecting a review while busy", None),
        item("notify.review_context_empty", "Context is empty: add files in the Context tab (Ctrl+2) first", "Review notification requiring context files", None),
        item("notify.review_copied", "Review findings copied to clipboard", "Review notification after copying findings", None),
        item("notify.review_copied_osc52", "Review findings sent to terminal clipboard (OSC 52)", "Review notification after an OSC 52 copy", None),
        item("notify.review_copy_failed", "Failed to copy to clipboard", "Review notification when copying fails", None),

        // ------------------------------------------------------------------
        // REPL (Develop) view notifications.
        // ------------------------------------------------------------------
        item("notify.prompt_cleared", "Prompt cleared (C-Y to restore)", "REPL notification after clearing the prompt", None),
        item("notify.prompt_clear_arm", "Press Esc again to clear the prompt", "REPL notification arming the prompt clear", None),
        item("notify.prompt_send_failed", "Failed to send prompt: {}", "REPL notification when sending a prompt fails", None),
        item("notify.copy_busy", "Cannot copy while model is generating", "REPL notification rejecting a copy while busy", None),
        item("notify.copy_no_response", "No model response to copy", "REPL notification when there is no response to copy", None),
        item("notify.response_copied", "Model response copied to clipboard", "REPL notification after copying the response", None),
        item("notify.response_copied_osc52", "Model response sent to terminal clipboard (OSC 52)", "REPL notification after an OSC 52 copy", None),
        item("notify.copy_failed", "Failed to copy to clipboard", "REPL notification when copying fails", None),

        // ------------------------------------------------------------------
        // History view notifications.
        // ------------------------------------------------------------------
        item("notify.history_clear_busy", "Cannot clear history while model is generating", "History notification rejecting a clear while busy", None),
        item("notify.history_item_copied", "History item #{} copied to clipboard", "History notification after copying an item", None),
        item("notify.history_item_copied_osc52", "History item #{} sent to terminal clipboard (OSC 52)", "History notification after an OSC 52 copy", None),
        item("notify.history_item_copy_failed", "Failed to copy history item #{} to clipboard", "History notification when copying an item fails", None),
        item("notify.history_item_empty", "History item #{} has no text to copy", "History notification for an empty item", None),
        item("notify.code_copied", "Copied {} lines of code to clipboard", "History notification after copying a code block", None),
        item("notify.code_copied_osc52", "Sent {} lines of code to terminal clipboard (OSC 52)", "History notification after an OSC 52 code copy", None),
        item("notify.code_copy_failed", "Failed to copy code to clipboard", "History notification when copying a code block fails", None),

        // ------------------------------------------------------------------
        // Onboarding (Setup) view notifications.
        // ------------------------------------------------------------------
        item("notify.onboarding_saved", "Workstation configuration saved to {} (Primary: {}, Meta: {})", "Onboarding notification after saving the workstation config", None),
        item("notify.onboarding_load_failed", "Saved {}, but it cannot be loaded: {}", "Onboarding notification when the saved config cannot be loaded", None),
        item("notify.onboarding_save_failed", "Failed to save {}: {}", "Onboarding notification when saving the config fails", None),
        item("notify.onboarding_clipboard_empty", "Clipboard is empty or unavailable. Use your terminal's paste instead.", "Onboarding notification when the clipboard is unavailable", None),
        item("notify.onboarding_key_empty", "API key cannot be empty. Please enter or paste a valid key, or press Esc to cancel.", "Onboarding notification rejecting an empty API key", None),
        item("notify.onboarding_key_detected", "API key detected in clipboard. Press {} or {} to paste it.", "Onboarding notification announcing a clipboard API key", None),

        // ------------------------------------------------------------------
        // Turn and batch lifecycle notifications.
        // ------------------------------------------------------------------
        item("notify.turn_start_failed", "Failed to start turn: {}", "Notification when a turn cannot be started", None),
        item("notify.turn_completed", "Turn completed", "Notification for a completed turn without a commit", None),
        item("notify.turn_completed_hash", "Turn completed ({})", "Notification for a completed turn with a commit hash", None),
        item("notify.turn_completed_summary", "Turn completed: {}", "Notification for a completed turn with a summary", None),
        item("notify.turn_completed_hash_summary", "Turn completed ({}): {}", "Notification for a completed turn with a hash and summary", None),
        item("notify.edit_desync", "Edit state desynchronized: server finished without sending edit/finished", "Notification when the edit state desynchronizes", None),
        item("notify.cancelled_before_edits", "Operation cancelled before edits were applied", "Notification when a turn is cancelled before applying edits", None),
        item("notify.failed_before_edits", "Operation failed before edits were applied", "Notification when a turn fails before applying edits", None),
        item("notify.review_cancelled", "Review cancelled", "Notification when a review is cancelled", None),
        item("notify.review_execution_started", "Executing review finding(s): switched to REPL for coder execution", "Notification when a review execution turn starts", None),
        item("notify.squashed_commits", "Squashed commits into {} ('{}')", "Notification after squashing commits", None),
        item("notify.batch_title", "TAUQE {} batch {}", "Desktop notification title for a finished batch", None),
        item("notify.batch_body", "{} '{}': {}/{} steps in {}", "Desktop notification body for a finished batch", None),

        // ------------------------------------------------------------------
        // Locale (interface language) dialog.
        // ------------------------------------------------------------------
        item("dialog.locale.active_custom", "Active language: {}", "Locale dialog line naming the active custom language", None),
        item("dialog.locale.active_english", "Active language: English (built-in)", "Locale dialog line while English is active", None),
        item("dialog.locale.intro", "Enter a target language in free form (for example Russian, German, Japanese).", "Locale dialog instruction for the target language field", None),
        item("dialog.locale.target_placeholder", "Target language...", "Locale dialog empty-field placeholder", None),
        item("dialog.locale.generating", "Synthesizing locale '{}' with Scribe model...", "Locale dialog line while synthesis runs", None),
        item("dialog.locale.synthesizing", "Synthesizing...", "Locale dialog action button label while synthesis runs", Some(16)),
        item("dialog.locale.footer_generating", "Esc cancel synthesis", "Locale dialog footer hint while synthesis runs", None),
        item("dialog.locale.footer_editor", "Enter synthesize • Tab controls • Esc dismiss", "Locale dialog footer hint while the language field has focus", None),
        item("dialog.locale.footer_actions", "← / → switch • Enter activate • Tab editor • Esc dismiss", "Locale dialog footer hint while the action bar has focus", None),

        // ------------------------------------------------------------------
        // Usage telemetry dialog: the full per-operation breakdown the compact
        // footer deliberately omits (c-4). Every label is localized so a
        // hot-swapped locale reaches the dialog body and its docked hint (c-5).
        // ------------------------------------------------------------------
        item("dialog.usage.title", "Usage Telemetry", "Title for the full usage breakdown dialog", None),
        item("dialog.usage.session", "Session", "Usage dialog heading for the accumulated session totals", None),
        item("dialog.usage.last", "Last Operation", "Usage dialog heading for the last completed operation", None),
        item("dialog.usage.current", "Current Operation", "Usage dialog heading for the operation in flight", None),
        item("dialog.usage.lifetime", "Project Lifetime", "Usage dialog heading for the project-wide totals accumulated across process restarts", None),
        item("dialog.usage.metric", "Metric", "Usage dialog table column header for the metric name", Some(16)),
        item("dialog.usage.operations", "Operations", "Usage dialog row label for the completed operation count", None),
        item("dialog.usage.prompt_tokens", "Prompt tokens", "Usage dialog row label for prompt tokens", None),
        item("dialog.usage.completion_tokens", "Completion tokens", "Usage dialog row label for completion tokens", None),
        item("dialog.usage.reasoning_tokens", "Reasoning tokens", "Usage dialog row label for reasoning tokens", None),
        item("dialog.usage.cached_tokens", "Cached tokens", "Usage dialog row label for cached tokens", None),
        item("dialog.usage.total_tokens", "Total tokens", "Usage dialog row label for total tokens", None),
        item("dialog.usage.cost", "Cost", "Usage dialog row label for the monetary cost", None),
        item("dialog.usage.duration", "Duration", "Usage dialog row label for the harness-measured duration", None),
        item("dialog.usage.map", "Repository Map", "Usage dialog heading for the tokens the repository map consumed", None),
        item("dialog.usage.repomap_tokens", "Map tokens", "Usage dialog row label for the tokens spent on the repository map", None),
        item("dialog.usage.empty", "No usage recorded in this session yet.", "Usage dialog placeholder when no operation has completed", None),
        item("dialog.usage.footer", "j / k or ↑ / ↓ scroll • PgUp / PgDn page • Home top • Shift+C reset • Esc close", "Usage dialog footer hint including the project telemetry reset chord", None),

        // ------------------------------------------------------------------
        // Discussion prompt modal.
        // ------------------------------------------------------------------
        item("dialog.prompt.title_finding", "Discuss Finding: [{}] #{}", "Prompt modal title when discussing one review finding", None),
        item("dialog.prompt.title_review", "Discuss Review Session: [{}]", "Prompt modal title when discussing a whole review session", None),
        item("dialog.prompt.title_step", "Discuss Step: [{}] #{}", "Prompt modal title when discussing one plan step", None),
        item("dialog.prompt.title_plan", "Discuss Plan: [{}]", "Prompt modal title when discussing a whole plan", None),
        item("dialog.prompt.review_label", " Review: ", "Prompt modal review field label", None),
        item("dialog.prompt.plan_label", " Plan: ", "Prompt modal plan field label", None),
        item("dialog.prompt.scope_label", " Scope: ", "Prompt modal scope field label", None),
        item("dialog.prompt.scope_review", "Entire review session", "Prompt modal scope value for a whole review session", None),
        item("dialog.prompt.scope_review_note", " (discussing code quality findings and resolution plan)", "Prompt modal note describing review-session scope", None),
        item("dialog.prompt.scope_plan", "Entire plan", "Prompt modal scope value for a whole plan", None),
        item("dialog.prompt.scope_plan_note", " (discussing architecture and overall strategy)", "Prompt modal note describing plan scope", None),
        item("dialog.prompt.target_finding_label", " Target finding: ", "Prompt modal target finding field label", None),
        item("dialog.prompt.target_step_label", " Target step: ", "Prompt modal target step field label", None),
        item("dialog.prompt.editor_placeholder", "Enter your comments, questions, or directives for Tauqe AI...", "Prompt modal empty-editor placeholder", None),
        item("dialog.prompt.editor_title", "Your Comments / Questions (Enter send, Shift+Enter / Alt+Enter / C-J newline)", "Prompt modal editor block title", None),
        item("dialog.prompt.hint_actions", "← / → switch • Tab focus • Enter send • y / n select • Esc dismiss", "Prompt modal footer hint while the action bar has focus", None),
        item("dialog.prompt.hint_editor", "Enter send • Shift+Enter / Alt+Enter / C-J newline • Tab controls • Esc dismiss", "Prompt modal footer hint while the editor has focus", None),

        // ------------------------------------------------------------------
        // Plan refinement (architect) dialog and its blocked popup.
        // ------------------------------------------------------------------
        item("dialog.refine.plan_label", " Plan: ", "Refine dialog plan field label", None),
        item("dialog.refine.header_note", " Decomposes high-level tasks into verified atomic engineering steps without altering agreed architecture.", "Refine dialog header note describing the architect mandate", None),
        item("dialog.refine.model_list_counter", " Architect Model  [{} of {}] ", "Refine dialog architect list title with a position counter", None),
        item("dialog.refine.model_list_title", " Architect Model ", "Refine dialog architect list title with a single entry", None),
        item("dialog.refine.no_models", "  (no architect models configured)", "Refine dialog empty architect list placeholder", None),
        item("dialog.refine.editor_placeholder", " Additional refinement directives (e.g. 'split step 2 into atomic subtasks') [optional]...", "Refine dialog empty instructions placeholder", None),
        item("dialog.refine.editor_title", "Architect Instructions (optional)", "Refine dialog instructions editor block title", None),
        item("dialog.refine.hint_actions", "Enter confirm • ← / → button • Esc cancel", "Refine dialog footer hint while the action bar has focus", None),
        item("dialog.refine.hint_fields", "{}Enter start • Tab focus • {} model • Esc cancel", "Refine dialog footer hint while a field has focus", None),
        item("dialog.refine_blocked.heading", "AI Architect Refused Decomposition (Blockers Detected)", "Refine-blocked dialog heading", None),
        item("dialog.refine_blocked.plan_label", "Plan: ", "Refine-blocked dialog plan field label", None),
        item("dialog.refine_blocked.reason_label", "Reason / Contradiction:", "Refine-blocked dialog reason field label", None),
        item("dialog.refine_blocked.note", "TAUQE prohibits speculative guessing. Replan uncompleted horizon addressing architectural blockers.", "Refine-blocked dialog note about replanning", None),

        // ------------------------------------------------------------------
        // Review finding decomposition (architect) dialog.
        // ------------------------------------------------------------------
        item("dialog.review_refine.title", "Decompose Review Finding (AI Architect)", "Title for the review finding decomposition dialog", None),
        item("dialog.review_refine.review_label", " Review: ", "Review finding decomposition dialog session field label", None),
        item("dialog.review_refine.finding_label", " Finding: ", "Review finding decomposition dialog finding field label", None),
        item("dialog.review_refine.header_note", " Decomposes the finding into verified atomic engineering steps without altering agreed architecture.", "Review finding decomposition dialog header note", None),

        // ------------------------------------------------------------------
        // Code review launch dialog.
        // ------------------------------------------------------------------
        item("dialog.review_launch.question", "Run automated code review on active context files?", "Review launch dialog question line", None),
        item("dialog.review_launch.files_label", "  Files: ", "Review launch dialog files count label", None),
        item("dialog.review_launch.tokens_label", "    Estimated tokens: ", "Review launch dialog estimated tokens label", None),
        item("dialog.review_launch.note", "  Only file contents are sent: history and repo map are excluded.", "Review launch dialog note about what is sent", None),
        item("dialog.review_launch.model_list_counter", " Reviewer Model  [{} of {}] ", "Review launch dialog reviewer list title with a position counter", None),
        item("dialog.review_launch.model_list_title", " Reviewer Model ", "Review launch dialog reviewer list title with a single entry", None),
        item("dialog.review_launch.no_models", "  (no reviewer models configured)", "Review launch dialog empty reviewer list placeholder", None),
        item("dialog.review_launch.path_placeholder", " target directory or glob, e.g. src/**/*.rs (optional)", "Review launch dialog empty target-paths placeholder", None),
        item("dialog.review_launch.path_title", "Target Paths (optional)", "Review launch dialog target-paths block title", None),
        item("dialog.review_launch.editor_title", "  Additional review instructions (optional):", "Review launch dialog instructions editor title", None),
        item("dialog.review_launch.editor_placeholder", " (type additional review focus or leave empty)...", "Review launch dialog empty instructions placeholder", None),
        item("dialog.review_launch.hint_actions", "Enter confirm • ← / → button • Esc cancel", "Review launch dialog footer hint while the action bar has focus", None),
        item("dialog.review_launch.hint_fields", "{}Enter start • Tab focus • {} model • Esc cancel", "Review launch dialog footer hint while a field has focus", None),

        // ------------------------------------------------------------------
        // Model selection dialog.
        // ------------------------------------------------------------------
        item("dialog.model_select.title_primary", "Select Primary Model", "Model picker title when choosing a configured primary entry", None),
        item("dialog.model_select.title_catalog", "Select Model ({}/{})", "Model picker title while browsing the full catalog", None),
        item("dialog.model_select.intro_primary", " Models configured under [models.primary] in tauqe.toml", "Model picker intro for the configured primary entries", None),
        item("dialog.model_select.intro_catalog", " All available models", "Model picker intro while browsing the full catalog", None),
        item("dialog.model_select.catalog_row", "[{}] Others... (full model catalog)", "Model picker escape-hatch row opening the full catalog", None),
        item("dialog.model_select.footer_primary", "1-9 quick select • n / p navigate • Enter select • Esc dismiss", "Model picker footer hint for the configured primary entries", None),
        item("dialog.model_select.footer_catalog", "n / p navigate • Enter select • Esc dismiss", "Model picker footer hint while browsing the full catalog", None),

        // ------------------------------------------------------------------
        // Squash commits dialog: base selection, diff inspection, message
        // editing, and the apply/discard confirmation popups.
        // ------------------------------------------------------------------
        item("dialog.squash.base_session", "1: Session ({})", "Squash dialog session base label", Some(20)),
        item("dialog.squash.base_session_none", "1: Session (none)", "Squash dialog session base label when absent", Some(20)),
        item("dialog.squash.base_upstream", "2: Upstream ({})", "Squash dialog upstream base label", Some(20)),
        item("dialog.squash.base_upstream_none", "2: Upstream (none)", "Squash dialog upstream base label when absent", Some(20)),
        item("dialog.squash.base_custom", "3: Custom [{}]", "Squash dialog custom base label", Some(20)),
        item("dialog.squash.base_custom_label", "3: Custom ref", "Squash dialog custom base label when inactive", Some(20)),
        item("dialog.squash.base_label", " Base: ", "Squash dialog base row prefix label", None),
        item("dialog.squash.scope_label", " | Scope: ", "Squash dialog scope row prefix label", None),
        item("dialog.squash.scope_value", "{} commit(s) ahead of {}", "Squash dialog scope value naming the commit count and base", None),
        item("dialog.squash.custom_prompt", " Enter custom base ref: ", "Squash dialog custom base ref prompt", None),
        item("dialog.squash.custom_hint", " (Enter to apply, Esc to cancel)", "Squash dialog custom base ref hint", None),
        item("dialog.squash.stats_label", " Stats: ", "Squash dialog diff stats label", None),
        item("dialog.squash.stats_note", " | 100% local inspection: preview diff without sending tokens.", "Squash dialog note that inspection is local", None),
        item("dialog.squash.loading", "  Loading diff...", "Squash dialog file list loading placeholder", None),
        item("dialog.squash.no_files", "  No changed files.", "Squash dialog empty file list placeholder", None),
        item("dialog.squash.files_title", " Files Changed ({}) ", "Squash dialog file list block title", None),
        item("dialog.squash.folded_file", "  [File '{}' is folded. Press Space to expand diff]", "Squash dialog hint for a folded file diff", None),
        item("dialog.squash.no_diff", "  No diff to display.", "Squash dialog empty diff placeholder", None),
        item("dialog.squash.diff_title", "Diff Inspection (Space: Fold/Unfold, PgUp/PgDn: Scroll)", "Squash dialog diff pane block title", None),
        item("dialog.squash.generating_message", "Generating Conventional Commit message via LLM...", "Squash dialog line while a commit message is generated", None),
        item("dialog.squash.msg_hint_focused", " Ctrl+G to generate message via AI, or type manually...", "Squash dialog empty message placeholder while the editor has focus", None),
        item("dialog.squash.msg_hint_unfocused", " Press 'g' to generate message via AI, or 'e' to type manually...", "Squash dialog empty message placeholder while browsing", None),
        item("dialog.squash.msg_title_focused", "Commit Message (Ctrl+G Generate via AI, Tab/Esc File list)", "Squash dialog message block title while the editor has focus", None),
        item("dialog.squash.msg_title_unfocused", "Commit Message ('g' Generate via AI, 'e' Edit)", "Squash dialog message block title while browsing", None),
        item("dialog.squash.footer.generate", "Generate Msg", "Squash dialog footer verb for generating a message", Some(14)),
        item("dialog.squash.footer.apply", "Apply Squash", "Squash dialog footer verb for applying the squash", Some(14)),
        item("dialog.squash.footer.base", "Base", "Squash dialog footer verb for choosing the base", Some(8)),
        item("dialog.squash.footer.fold", "Fold", "Squash dialog footer verb for folding a diff", Some(8)),
        item("dialog.squash.footer.focus", "Focus", "Squash dialog footer verb for switching focus", Some(8)),
        item("dialog.squash.footer.edit", "Edit", "Squash dialog footer verb for editing the message", Some(8)),
        item("dialog.squash.footer.file_list", "File list", "Squash dialog footer verb for focusing the file list", Some(12)),
        item("dialog.squash.footer.close", "Close", "Squash dialog footer verb for closing the dialog", Some(8)),
        item("dialog.squash_discard.question", "Discard the commit message and close the dialog?", "Squash discard dialog question line", None),
        item("dialog.squash_discard.note", "The message in the editor will be permanently lost.", "Squash discard dialog note about losing the message", None),
        item("dialog.squash_confirm.question", "Confirm Git History Squash?", "Squash apply confirmation question line", None),
        item("dialog.squash_confirm.squashing_label", "Squashing ", "Squash apply confirmation prefix before the commit count", None),
        item("dialog.squash_confirm.commit_count", "{} commit(s)", "Squash apply confirmation commit count", None),
        item("dialog.squash_confirm.squash_tail", " into 1 commit on top of ", "Squash apply confirmation tail after the commit count", None),
        item("dialog.squash_confirm.message_label", "Message: ", "Squash apply confirmation message field label", None),
        item("dialog.squash_confirm.note", "This operation rewrites local Git history ahead of the base ref.", "Squash apply confirmation note about rewriting history", None),
        item("dialog.squash_confirm.confirm_button", "Confirm Squash", "Squash apply confirmation button", Some(16)),
        item("dialog.squash_confirm.no_message", "(no message)", "Squash apply confirmation placeholder when the message is empty", None),

        // ------------------------------------------------------------------
        // RPC response and system notifications: configuration diagnostics,
        // pre-flight verification, context mutations, locale switching,
        // architect responses, and the desktop notifications raised when a
        // turn or review settles. The daemon owns the wire payloads; the TUI
        // localizes the text it renders around them (c-1, c-2).
        // ------------------------------------------------------------------
        item("notify.config_error", "Configuration error: {}", "Notification warning that the loaded tauqe.toml is malformed", None),
        item("notify.preflight_failed", "Pre-flight verification failed: the repository does not build or pass tests. Fix the build/test errors before re-running the batch.", "Notification when pre-flight toolchain verification blocks a batch", None),
        item("notify.rpc_error", "Error: {}", "Generic notification wrapping an RPC error message", None),
        item("notify.plan_deleted", "Plan deleted", "Notification after a plan is deleted", None),
        item("notify.review_deleted", "Review session deleted", "Notification after a review session is deleted", None),
        item("notify.squashed_commit_fallback", "Squashed commit", "Fallback commit summary when a squash message is empty", None),
        item("notify.context_added", "Added {} files (~{} tokens)", "Notification when files are added to context by pattern", None),
        item("notify.context_added_truncated", "Added {} files (~{} tokens); the match set exceeded the configured batch limit and remaining files were skipped", "Notification when a context pattern add exceeds the batch limit", None),
        item("notify.context_removed", "Removed {} file(s) from context", "Notification when files are removed from context", None),
        item("notify.context_cleared", "Cleared {} file(s) from context", "Notification when a context layer is cleared", None),
        item("notify.locale_busy", "Cannot change locale while model is generating", "Notification rejecting a locale change while the model is busy", None),
        item("notify.locale_switched", "Interface language switched to {}", "Notification after the interface language is switched", None),
        item("notify.locale_switched_unsaved", "Interface language switched to {} for this session, but it could not be saved: {}", "Notification when a locale switch cannot be persisted", None),
        item("notify.finding_decomposed", "Finding decomposed into steps: {}", "Notification after the Architect decomposes a finding", None),
        item("notify.finding_decomposed_default", "Finding decomposed into steps by the Architect", "Notification after the Architect decomposes a finding without a note", None),
        item("notify.finding_decompose_blocked", "Decomposition blocked: {}", "Notification when the Architect refuses to decompose a finding", None),
        item("notify.plan_horizon_updated", "Plan '{}' horizon updated ({}/{} done): {}", "Notification after the Architect updates a plan horizon", None),
        item("notify.plan_horizon_updated_default", "Plan '{}' horizon updated successfully by architect ({}/{} done)", "Notification after the Architect updates a plan horizon without a note", None),
        item("notify.plan_horizon_blocked", "Plan horizon synthesis blocked by architect: manual intervention or replanning required", "Notification when the Architect refuses a plan horizon synthesis", None),
        item("notify.turn_desktop_title", "TAUQE", "Desktop notification title for a settled turn", None),
        item("notify.turn_error_desktop_title", "TAUQE Error", "Desktop notification title for a failed turn", None),
        item("notify.turn_error_desktop_body", "Turn failed with error", "Desktop notification body for a failed turn", None),
        item("notify.review_desktop_title", "TAUQE Review", "Desktop notification title for a settled review", None),
        item("notify.review_completed_desktop_body", "Code review completed ({} findings)", "Desktop notification body for a completed review", None),
        item("notify.review_error_desktop_title", "TAUQE Review Error", "Desktop notification title for a failed review", None),
        item("notify.review_error_desktop_body", "Code review failed", "Desktop notification body for a failed review", None),

        // ------------------------------------------------------------------
        // REPL command descriptions continued: buffer boundaries and forward
        // word-kill, bound to `M-<` / `C-Home`, `M->` / `C-End`, and `M-d`.
        // ------------------------------------------------------------------
        item("cmd.repl.move_buffer_start", "Move the cursor to the beginning of the buffer", "REPL command description", None),
        item("cmd.repl.move_buffer_end", "Move the cursor to the end of the buffer", "REPL command description", None),
        item("cmd.repl.kill_word_forward", "Kill the word after the cursor", "REPL command description", None),

        // ------------------------------------------------------------------
        // Onboarding (Setup) step bodies, prompts, and option labels. These are
        // the visible copy of the setup workflow, declared here so a switched
        // locale reaches every step without editing the renderer (c-1, c-2).
        // ------------------------------------------------------------------
        item("onboarding.git.body_not_repo", "The current working directory is not a Git repository.", "Onboarding Git step body: directory is not a repository", None),
        item("onboarding.git.body_requires", "Tauqe strictly requires Git: isolated pre-edit checkpoints, atomic turn commits,", "Onboarding Git step body: first rationale line", None),
        item("onboarding.git.body_rollback", "instant rollback (undo), and feature commit squashing.", "Onboarding Git step body: rationale completion line", None),
        item("onboarding.git.select_prompt", "Select an action (arrows ↑/↓, then Enter):", "Onboarding action-selection prompt", None),
        item("onboarding.git.option_init", "Initialize Git repository (git init)", "Onboarding Git option: init a repository", None),
        item("onboarding.git.option_init_commit", "Initialize and create initial commit (git init && git commit)", "Onboarding Git option: init and first commit", None),
        item("onboarding.git.option_exit", "Exit Tauqe", "Onboarding option: exit the application", None),
        item("onboarding.config.body_not_found", "Configuration file not found in project root.", "Onboarding config step body: no config file", None),
        item("onboarding.config.body_will_create", "It will be created at: {}", "Onboarding config step body: creation path", None),
        item("onboarding.config.select_prompt", "Select configuration option (arrows ↑/↓, then Enter):", "Onboarding config selection prompt", None),
        item("onboarding.config.option_create", "Create default tauqe.toml (recommended configuration)", "Onboarding config option: create default file", None),
        item("onboarding.config.option_skip", "Skip (use built-in defaults without creating tauqe.toml)", "Onboarding config option: skip", None),
        item("onboarding.credentials.body_not_detected", "OpenRouter API key was not detected in environment (OPENROUTER_API_KEY) or credentials files.", "Onboarding credentials body: key not detected", None),
        item("onboarding.credentials.body_required", "An API key is required to query models (obtain at https://openrouter.ai/keys).", "Onboarding credentials body: key required", None),
        item("onboarding.credentials.body_location", "Recommended secure file location: {}", "Onboarding credentials body: recommended path", None),
        item("onboarding.credentials.select_prompt", "Select key setup method (arrows ↑/↓, then Enter):", "Onboarding credentials selection prompt", None),
        item("onboarding.credentials.option_enter", "Enter API key now (save with 0600 permissions to {})", "Onboarding credentials option: enter key now", None),
        item("onboarding.credentials.option_stub", "Create stub credentials.toml file (fill manually in editor)", "Onboarding credentials option: create stub", None),
        item("onboarding.credentials.option_env", "Configured via OPENROUTER_API_KEY environment variable (Check again)", "Onboarding credentials option: environment variable", None),
        item("onboarding.credentials.option_skip", "Skip", "Onboarding option: skip the current step", None),
        item("onboarding.workstation.body_configure", "Configure shortcut key translation and navigation keys for your system layout.", "Onboarding workstation body: configure mapping", None),
        item("onboarding.workstation.body_file", "Configuration file: {}", "Onboarding workstation body: tui config path", None),
        item("onboarding.workstation.select_prompt", "Select keyboard layout option (arrows ↑/↓, then Enter):", "Onboarding workstation selection prompt", None),
        item("onboarding.workstation.option_standard", "Standard (No translation, default QWERTY / layout-neutral)", "Onboarding workstation option: standard layout", None),
        item("onboarding.workstation.option_jcuken", "Russian JCUKEN (Translate Cyrillic ЙЦУКЕН to QWERTY for shortcuts & navigation)", "Onboarding workstation option: Russian JCUKEN mapping", None),
        item("onboarding.workstation.option_custom", "Custom Langmap (Input custom character mapping: e.g. Greek, Hebrew, etc.)", "Onboarding workstation option: custom langmap", None),
        item("onboarding.workstation.option_skip", "Skip (Use defaults without creating tui.toml)", "Onboarding workstation option: skip", None),
        item("onboarding.modifier.body_input", "In text input mode (prompt buffer, commit message editor), every regular key types text.", "Onboarding modifier body: text input mode", None),
        item("onboarding.modifier.body_commands", "Functional commands (send prompt, undo, squash, model picker, tabs) use the primary modifier.", "Onboarding modifier body: command modifier", None),
        item("onboarding.modifier.select_prompt", "Select primary command modifier (arrows ↑/↓, then Enter):", "Onboarding modifier selection prompt", None),
        item("onboarding.modifier.option_ctrl", "Ctrl (Default — standard Emacs C- shortcuts: Ctrl+Enter, Ctrl+Z, Ctrl+S, Ctrl+M)", "Onboarding modifier option: Ctrl", None),
        item("onboarding.modifier.option_alt", "Alt (Meta modifier — Alt+Enter, Alt+Z, Alt+S, Alt+M; robust on macOS and legacy terminals)", "Onboarding modifier option: Alt", None),
        item("onboarding.gatekeeper.body_no_key", "Language model calls cannot proceed without an OpenRouter API key.", "Onboarding gatekeeper body: key required", None),
        item("onboarding.gatekeeper.body_options", "You can input the key now, create credentials.toml, or export:", "Onboarding gatekeeper body: options intro", None),
        item("onboarding.gatekeeper.body_export", "export OPENROUTER_API_KEY=\"sk-or-v1-...\"", "Onboarding gatekeeper body: export example", None),
        item("onboarding.gatekeeper.option_go_back", "Go back and configure OpenRouter API key", "Onboarding gatekeeper option: go back", None),
        item("onboarding.gatekeeper.option_recheck", "Check again (reload environment and credentials files)", "Onboarding gatekeeper option: recheck", None),
        item("onboarding.gatekeeper.option_exit", "Exit Tauqe", "Onboarding option: exit the application", None),
        item("onboarding.ready.model_label", "Model:", "Onboarding ready summary: model label", None),
        item("onboarding.ready.config_label", "Configuration:", "Onboarding ready summary: configuration label", None),
        item("onboarding.ready.key_label", "OpenRouter Key:", "Onboarding ready summary: key label", None),
        item("onboarding.ready.modifier_label", "Command Modifier:", "Onboarding ready summary: modifier label", None),
        item("onboarding.ready.key_configured", "[OK] Configured", "Onboarding ready summary: key configured badge", None),
        item("onboarding.ready.press_enter", "Press Enter to open the development workspace.", "Onboarding ready: enter hint", None),
        item("onboarding.ready.help_hint", "Press '?' at any time to view the hotkey reference.", "Onboarding ready: help hint", None),
        item("onboarding.ready.start_session", "Start Session (Open Terminal)", "Onboarding ready: start session button", None),
        item("onboarding.input.langmap_prompt", "Enter Custom Langmap (e.g. 'йцукен;qwerty' or 'йq,цw'):", "Onboarding input prompt for a custom langmap", None),
        item("onboarding.input.api_key_prompt", "Enter OpenRouter API Key:", "Onboarding input prompt for the API key", None),
        item("onboarding.input.key_hint_show", "({}R: Hide, {}V: Paste, Enter: Save, Esc: Cancel)", "Onboarding API key input hint while the key is shown", None),
        item("onboarding.input.key_hint_hide", "({}R: Show, {}V: Paste, Enter: Save, Esc: Cancel)", "Onboarding API key input hint while the key is masked", None),

        // ------------------------------------------------------------------
        // View labels surfaced by the Develop, Plans, and Review render paths.
        // ------------------------------------------------------------------
        item("repl.proposed_edits", "Proposed Edits", "Develop header label for the staged file edits list", None),
        item("repl.validation_error", "Validation Error:", "Develop label prefixing a failed file validation message", None),
        item("repl.usage.tokens", "Tokens", "Develop usage line label preceding the token breakdown", None),
        item("repl.usage.prompt", "prompt", "Develop usage line unit for prompt tokens", None),
        item("repl.usage.completion", "completion", "Develop usage line unit for completion tokens", None),
        item("repl.usage.reasoning", "reasoning", "Develop usage line unit for reasoning tokens", None),
        item("repl.usage.cached", "cached", "Develop usage line unit for cached tokens", None),
        item("repl.thinking", "Thinking...", "Develop reasoning block title while reasoning is shown", None),
        item("repl.thinking_streaming", "Thinking (streaming)...", "Develop reasoning block title while reasoning streams", None),
        item("repl.thinking_hidden", "Thinking (hidden)", "Develop reasoning block title while reasoning is folded", None),
        item("repl.thinking_lines", "({} lines)", "Develop reasoning block line counter appended while reasoning streams", None),
        item("repl.thinking_truncated", "({} earlier lines dropped)", "Develop reasoning block marker stating how many earliest reasoning lines fell out of the bounded transcript window", None),
        item("plans.concept_title", "Concept", "Plans section title for architectural concept items", None),
        item("plans.impl_title", "Implementation", "Plans section title for implementation items", None),
        item("review.skip_reason_label", "Skip reason:", "Review detail label for a skipped finding rationale", None),
        item("review.note_label", "Note:", "Review detail label for a finding note", None),

        // ------------------------------------------------------------------
        // Additional notifications declared alongside the localized view copy.
        // ------------------------------------------------------------------
        item("notify.comment_empty", "Comment cannot be empty", "Notification rejecting an empty discussion comment", None),
        item("notify.squash_busy", "Cannot squash while the model is generating", "Notification rejecting a squash while the model is busy", None),
        item("notify.onboarding_required", "Complete the initial setup before using this feature", "Notification requiring onboarding to finish first", None),
        item("notify.config_reloading", "Reloading configuration...", "Notification while configuration reloads", None),
        item("notify.mouse_shift_tip", "Hold Shift and drag to select text with the mouse", "Notification tip for mouse text selection", None),
        item("notify.locale_generate_failed", "Failed to synthesize the interface language: {}", "Notification when locale synthesis fails", None),
        item("notify.locale_generated", "Interface language '{}' synthesized", "Notification after a locale is synthesized", None),
        item("notify.locale_empty_target", "Enter a target language (for example, Russian)", "Locale notification when target language is empty", None),
        item("notify.locale_synthesizing", "Synthesizing the {} locale with Scribe...", "Locale notification when synthesis starts", None),
        item("notify.locale_cancelled", "Locale generation cancelled", "Locale notification when synthesis is cancelled", None),
        item("notify.locale_reset_english", "Interface language reset to English", "Locale notification when reset to English succeeds", None),
        item("notify.locale_reset_unsaved", "Interface language reset to English, but the saved locale could not be removed: {}", "Locale notification when reset to English fails to delete file", None),
        item("notify.config_reload_error", "{}; keeping previous TUI settings", "Notification when config reload encounters error", None),
        item("notify.model_select_busy", "Cannot change model while model is generating", "Notification when model selection attempted while busy", None),
        item("notify.quit_hint", "Press Ctrl+Q to quit TAUQE, or ? for help", "Notification hint for quitting tauqe or opening help", None),
        item("dialog.squash.status_checking_base", "Checking base '{}'...", "Squash dialog status message when checking base commit", None),
        item("dialog.squash.status_no_session_base", "Session base commit not available (no AI commits in session).", "Squash dialog status message when session base unavailable", None),
        item("dialog.squash.status_no_upstream_base", "No upstream branch configured or detected.", "Squash dialog status message when upstream base unavailable", None),
        item("dialog.squash.status_switching_base", "Switching base to '{}'...", "Squash dialog status message when switching base", None),
        item("dialog.squash.status_generating_message", "Generating Conventional Commit message with AI...", "Squash dialog status message when generating commit message", None),
        item("dialog.squash.status_applying", "Applying squash...", "Squash dialog status message when applying squash commit", None),
        item("dialog.squash.status_loading_diff", "Diff is still loading, please wait...", "Squash dialog status message when diff is loading", None),
        item("dialog.squash.status_message_generating", "Commit message is generating, please wait...", "Squash dialog status message when commit message is generating", None),
        item("dialog.squash.status_invalid_base", "Invalid or empty base ref.", "Squash dialog status message when base ref is invalid", None),
        item("dialog.squash.status_no_commits", "No commits ahead of base to squash.", "Squash dialog status message when no commits to squash", None),
        item("dialog.squash.status_empty_message", "Commit message cannot be empty (Ctrl+G to generate, or type one).", "Squash dialog status message when commit message is empty", None),
    ]
}

fn item(key: &str, english: &str, context: &str, max_width: Option<u16>) -> LocaleCatalogItem {
    LocaleCatalogItem {
        key: key.to_string(),
        english: english.to_string(),
        context: context.to_string(),
        max_width,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn test_canonical_catalog_contains_essential_entries() {
        let catalog = canonical_catalog();
        assert!(!catalog.is_empty());

        let keys: HashSet<&str> = catalog.iter().map(|item| item.key.as_str()).collect();
        assert!(keys.contains("tab.repl"));
        assert!(keys.contains("tab.context"));
        assert!(keys.contains("tab.review"));
        assert!(keys.contains("tab.plans"));
        assert!(keys.contains("tab.history"));
        assert!(keys.contains("modal.confirm"));
        assert!(keys.contains("modal.cancel"));
        assert!(keys.contains("status.idle"));
        assert!(keys.contains("status.done"));
    }

    #[test]
    fn test_canonical_catalog_keys_are_unique() {
        let catalog = canonical_catalog();
        let mut seen = HashSet::new();
        for item in &catalog {
            assert!(
                seen.insert(&item.key),
                "Duplicate key detected in canonical catalog: {}",
                item.key
            );
        }
    }

    #[test]
    fn test_canonical_catalog_english_text_fits_max_width() {
        for item in canonical_catalog() {
            if let Some(limit) = item.max_width {
                let actual_width = UnicodeWidthStr::width(item.english.as_str());
                assert!(
                    actual_width <= limit as usize,
                    "English text '{}' for key '{}' exceeds max_width ({actual_width} > {limit})",
                    item.english,
                    item.key
                );
            }
        }
    }

    #[test]
    fn test_canonical_catalog_covers_every_subsystem() {
        let catalog = canonical_catalog();
        let keys: HashSet<&str> = catalog.iter().map(|item| item.key.as_str()).collect();

        // Footer telemetry and bounded-turn phases.
        assert!(keys.contains("footer.ready"));
        assert!(keys.contains("footer.stopping"));
        assert!(keys.contains("footer.retrying"));
        assert!(keys.contains("footer.retrying_cancel"));
        assert!(keys.contains("phase.discovery"));
        assert!(keys.contains("phase.healing"));

        // Modal headings and buttons.
        assert!(keys.contains("dialog.confirm_cancel.title"));
        assert!(keys.contains("dialog.server_disconnected.title"));
        assert!(keys.contains("dialog.button.interrupt"));

        // View section headers and empty states.
        assert!(keys.contains("context.section.pinned"));
        assert!(keys.contains("plans.empty_title"));
        assert!(keys.contains("review.executive_summary"));

        // Command descriptions across the subsystem command maps.
        assert!(keys.contains("cmd.global.cancel_generation"));
        assert!(keys.contains("cmd.editor.yank"));
        assert!(keys.contains("cmd.context.add_editable"));
        assert!(keys.contains("cmd.history.clear"));
        assert!(keys.contains("cmd.plans.execute"));
        assert!(keys.contains("cmd.review.refine"));
        assert!(keys.contains("cmd.repl.send"));

        // Usage telemetry headings, including the project-wide lifetime block.
        assert!(keys.contains("dialog.usage.session"));
        assert!(keys.contains("dialog.usage.lifetime"));

        // System notifications.
        assert!(keys.contains("notify.turn_completed"));
        assert!(keys.contains("notify.plan_scope_blocked"));
        assert!(keys.contains("notify.onboarding_key_detected"));
    }

    #[test]
    fn test_canonical_catalog_entries_are_well_formed() {
        for item in canonical_catalog() {
            assert!(!item.key.trim().is_empty(), "catalog entry has a blank key");
            assert!(
                !item.english.trim().is_empty(),
                "catalog key '{}' has blank English text",
                item.key
            );
            assert!(
                !item.context.trim().is_empty(),
                "catalog key '{}' is missing its usage context",
                item.key
            );
        }
    }

    #[test]
    fn test_canonical_catalog_keys_use_stable_dot_notation() {
        for item in canonical_catalog() {
            assert!(
                item.key.contains('.'),
                "catalog key '{}' must use dot notation",
                item.key
            );
            assert!(
                !item.key.starts_with('.') && !item.key.ends_with('.'),
                "catalog key '{}' must not start or end with a dot",
                item.key
            );
            assert!(
                !item.key.contains(".."),
                "catalog key '{}' must not contain consecutive dots",
                item.key
            );
            assert!(
                item.key
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_'),
                "catalog key '{}' must be lowercase dot notation",
                item.key
            );
        }
    }
}
