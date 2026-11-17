//! Virtual staging of one change set: in-memory patch application with a localized retry.
//!
//! [`execute_staging_step`] owns exactly one responsibility of the turn: it takes the
//! change set a model round proposed and applies it to the virtual staging state. The
//! retry rounds a refusal opens belong to
//! [`crate::control::turn::run_turn_loop`], so a refused member can never terminate the
//! turn on a local counter: it is repaired from the same round budget the whole turn
//! draws on. The step never touches Git, the toolchain, or the working tree, and it
//! interprets no semantic outcome of the model: a round that carries no change set is
//! reported as unaddressed and its envelope stays intact for the contour that owns the
//! answer.
//!
//! A structural protocol defect is not a staging failure (`crates/core/Conventions.md`
//! §13.6): repairing malformed output belongs to the protocol sanitizer, so the step
//! only retries members the substrate refused for a semantic reason and keeps the
//! protocol surface out of the turn's budget.

use std::collections::{HashMap, HashSet};

use tauqe_protocol::{EditOperation, LlmCommand, PatchStrategy};

use crate::control::context::LoopContext;
use crate::control::outcome::ResponseEnvelope;
use crate::edits::{stage_and_validate_edits, EditError, StagedEditsState};
use crate::workflow::retry::{
    format_patch_retry_reason, prepare_staged_for_retry, retain_retried_failed_files,
};
/// Cumulative virtual staging state of one turn.
///
/// A turn stages its change set in as many attempts as the repair rounds it
/// spends, so the virtual state it converges on belongs to the turn rather than to
/// a single attempt. The loop that drives the turn owns the value and hands every
/// attempt a mutable reference to it.
#[derive(Debug, Default)]
pub struct StagingState {
    /// Virtual state the attempts converged on.
    pub(crate) staged_state: Option<StagedEditsState>,
    /// Operations that converged, in change-set order.
    pub(crate) cumulative_edits: Vec<EditOperation>,
    /// Paths whose edit reached the virtual state.
    pub(crate) succeeded: HashSet<String>,
    /// Paths the substrate refused in the latest attempt that addressed them.
    pub(crate) failed: HashMap<String, EditError>,
    /// Paths the model stopped retrying, paired with the error that refused them.
    pub(crate) abandoned: HashMap<String, EditError>,
    /// Number of staging attempts the turn spent.
    pub(crate) attempts: usize,
}

/// What one executed staging attempt produced.
#[derive(Debug, Default)]
pub struct StagingStep {
    /// Whether the envelope carried a change set the attempt could stage.
    pub(crate) addressed: bool,
    /// Refusals of the attempt, rendered by the patch strategy of the turn and
    /// sorted by path.
    pub(crate) refusals: Vec<(String, String)>,
}

/// Executes exactly one staging attempt against the virtual state of the turn.
///
/// The step drains the change set of the envelope, prepares a corrected set
/// against the refusals of the previous attempt, applies it to the virtual state,
/// and records the churn the attempt produced. It never requests a round of its
/// own: the repair rounds and their budget belong to the loop that drives the turn.
pub(crate) fn execute_staging_step(
    ctx: &mut LoopContext<'_>,
    envelope: &mut ResponseEnvelope,
    state: &mut StagingState,
) -> StagingStep {
    let edits = extract_file_edits(envelope);
    if edits.is_empty() {
        return StagingStep::default();
    }

    // A corrected change set is prepared before it is staged: the paths it
    // re-edits leave the virtual state so they are staged again against their
    // clean base, and a refusal the model left out of its corrected set is
    // carried into the abandoned record instead of vanishing with the entry.
    let unretried = retain_retried_failed_files(&mut state.failed, ctx.context_manager, &edits);
    for (path, error) in unretried {
        state.abandoned.entry(path).or_insert(error);
    }
    if let Some(staged) = state.staged_state.as_mut() {
        prepare_staged_for_retry(
            staged,
            &mut state.cumulative_edits,
            &mut state.succeeded,
            ctx.context_manager,
            &edits,
        );
    }

    state.attempts += 1;
    let result = stage_and_validate_edits(
        ctx.repo_root,
        ctx.context_manager,
        &edits,
        state.staged_state.as_ref(),
    );
    let converged = result.successful_paths.len();
    state.staged_state = Some(result.staged_state);
    // Only a path that converged in this attempt answers its earlier refusal.
    // Clearing by the cumulative set would also erase the failure of a path
    // that succeeded earlier and then failed again outside this attempt.
    for path in &result.successful_paths {
        state.failed.remove(path);
        state.abandoned.remove(path);
    }
    let converged_paths = result.successful_paths.clone();
    state.succeeded.extend(result.successful_paths);
    state.cumulative_edits.extend(result.successful_edits);
    for failure in result.failed_files {
        state.abandoned.remove(&failure.path);
        state.failed.insert(failure.path, failure.error);
    }

    // The change set this attempt produced is the churn of the working set the
    // model must read next: the members that converged relocate first, the
    // members the substrate refused follow them, and the mark stales the rendered
    // prefix so the repair or healing round re-reads the bytes it is about to
    // address instead of the bytes the turn opened with.
    let mut refused_paths: Vec<String> = state.failed.keys().cloned().collect();
    refused_paths.sort();
    ctx.note_context_churn(&converged_paths, &refused_paths);

    tracing::info!(
        repo_root = %ctx.repo_root.display(),
        attempt = state.attempts,
        converged,
        refused = state.failed.len(),
        "virtual staging attempt completed"
    );

    StagingStep {
        addressed: true,
        refusals: render_failures(&state.failed, ctx.prompt_builder.patch_strategy),
    }
}

/// Renders the refusals of one attempt as a path-sorted diagnostic list.
///
/// The wording comes from the active patch strategy, so a retry round is never
/// taught the vocabulary of a contour the turn does not speak.
pub(super) fn render_failures(
    failed: &HashMap<String, EditError>,
    patch_strategy: PatchStrategy,
) -> Vec<(String, String)> {
    let mut rendered: Vec<(String, String)> = failed
        .iter()
        .map(|(path, error)| {
            (
                path.clone(),
                format_patch_retry_reason(error, patch_strategy),
            )
        })
        .collect();
    rendered.sort();
    rendered
}

/// Extracts all file edit operations from the envelope by draining its [`LlmCapability::FileEdits`].
fn extract_file_edits(envelope: &mut ResponseEnvelope) -> Vec<EditOperation> {
    envelope
        .take_any(super::edit::EDIT_CAPABILITIES)
        .into_iter()
        .filter_map(LlmCommand::into_edit_operation)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ContextManager;
    use crate::control::context::{CancellationToken, EventSink};
    use crate::prompt::json::JsonPromptProtocol;
    use crate::providers::mock::MockLlmProvider;
    use tauqe_protocol::{
        CapabilityTree, ContextAccess, EditReplace, LlmCapability, LlmCommand, ModelRef,
        TextCommand,
    };

    const TARGET: &str = "pub fn compute() -> i32 { 0 }\n";
    const CORRECTED: &str = "pub fn compute() -> i32 { 42 }\n";

    struct Fixture {
        dir: tempfile::TempDir,
        context: ContextManager,
        journal: crate::workflow::research::journal::TurnResearchJournal,
        provider: MockLlmProvider,
        model: ModelRef,
        protocol: JsonPromptProtocol,
        cache: crate::control::cache::TurnInputCache,
        cancel: CancellationToken,
        events: EventSink,
        policy: crate::workflow::policy::DirectApplyPolicy,
    }

    impl Fixture {
        fn new(files: &[(&str, &str)], context_files: &[&str]) -> Self {
            let dir = tempfile::tempdir().expect("scratch directory");
            let root = dir.path().to_path_buf();
            crate::git::init_repository(Some(&root), true).expect("repository");
            for (path, content) in files {
                let full = root.join(path);
                if let Some(parent) = full.parent() {
                    std::fs::create_dir_all(parent).expect("fixture directory");
                }
                std::fs::write(full, content).expect("fixture file");
            }
            let mut context = ContextManager::new(root);
            for path in context_files {
                context
                    .add_file(path, ContextAccess::Editable)
                    .expect("context file");
            }
            Self {
                dir,
                context,
                journal: crate::workflow::research::journal::TurnResearchJournal::default(),
                provider: MockLlmProvider::scripted(Vec::new()),
                model: ModelRef::openrouter("mock/model"),
                protocol: JsonPromptProtocol::edits(),
                cache: crate::control::cache::TurnInputCache::default(),
                cancel: CancellationToken::new(),
                events: EventSink::noop(),
                policy: crate::workflow::policy::DirectApplyPolicy::new(),
            }
        }

        fn context(&mut self) -> LoopContext<'_> {
            let caps = CapabilityTree::set(
                crate::control::turn::EDIT_CAPABILITIES
                    .iter()
                    .copied()
                    .chain([LlmCapability::TextMessage])
                    .map(CapabilityTree::leaf),
            );
            let prompt_builder = crate::control::prompt::PromptBuilder::new(
                &self.provider,
                &self.model,
                &self.protocol,
                &mut self.cache,
            );
            LoopContext::new(
                &self.cancel,
                &self.events,
                self.dir.path(),
                &mut self.context,
                &mut self.journal,
                &mut self.policy,
                caps,
                prompt_builder,
            )
        }
    }

    /// Builds the change set of a replacement of `a.rs`.
    fn replace(search: &str, replacement: &str) -> ResponseEnvelope {
        ResponseEnvelope::from_commands(&[LlmCommand::EditReplace(EditReplace {
            path: "a.rs".to_string(),
            old_text: search.to_string(),
            new_text: replacement.to_string(),
        })])
    }

    #[test]
    fn a_round_without_a_change_set_is_unaddressed() {
        let mut fixture = Fixture::new(&[("a.rs", TARGET)], &["a.rs"]);
        let mut envelope = ResponseEnvelope::from_commands(&[LlmCommand::Text(TextCommand {
            content: "no change set".to_string(),
        })]);
        let mut ctx = fixture.context();
        let mut state = StagingState::default();

        let step = execute_staging_step(&mut ctx, &mut envelope, &mut state);
        assert!(!step.addressed);
        assert_eq!(state.attempts, 0, "an unaddressed round spends no attempt");
        assert_eq!(
            envelope.len(),
            1,
            "the envelope of an unaddressed round stays intact"
        );
    }

    #[test]
    fn a_converging_change_set_reaches_the_virtual_state() {
        let mut fixture = Fixture::new(&[("a.rs", TARGET)], &["a.rs"]);
        let mut envelope = replace(
            "pub fn compute() -> i32 { 0 }",
            "pub fn compute() -> i32 { 42 }",
        );
        let mut ctx = fixture.context();
        let mut state = StagingState::default();

        let step = execute_staging_step(&mut ctx, &mut envelope, &mut state);
        assert!(step.addressed);
        assert!(step.refusals.is_empty());
        assert!(
            envelope.is_empty(),
            "the change set is drained from the envelope"
        );
        assert_eq!(
            state
                .staged_state
                .as_ref()
                .expect("the change set converged")
                .staged_text_files()
                .get("a.rs")
                .map(String::as_str),
            Some(CORRECTED)
        );
    }

    #[test]
    fn a_refused_member_is_repaired_by_a_later_attempt() {
        let mut fixture = Fixture::new(&[("a.rs", TARGET)], &["a.rs"]);
        let mut ctx = fixture.context();
        let mut state = StagingState::default();

        let mut refused = replace("nonexistent text", "irrelevant");
        execute_staging_step(&mut ctx, &mut refused, &mut state);
        assert_eq!(state.failed.len(), 1, "the member is refused");

        let mut repaired = replace(
            "pub fn compute() -> i32 { 0 }",
            "pub fn compute() -> i32 { 42 }",
        );
        let step = execute_staging_step(&mut ctx, &mut repaired, &mut state);
        assert!(step.refusals.is_empty());
        assert!(state.failed.is_empty(), "the repair answers the refusal");
        assert_eq!(state.attempts, 2);
    }

    #[test]
    fn an_unretried_member_is_recorded_as_abandoned() {
        let mut fixture = Fixture::new(
            &[("a.rs", TARGET), ("b.rs", "pub fn b() {}\n")],
            &["a.rs", "b.rs"],
        );
        let mut ctx = fixture.context();
        let mut state = StagingState::default();

        let mut both = ResponseEnvelope::from_commands(&[
            LlmCommand::EditReplace(EditReplace {
                path: "a.rs".to_string(),
                old_text: "nonexistent in a".to_string(),
                new_text: "irrelevant".to_string(),
            }),
            LlmCommand::EditReplace(EditReplace {
                path: "b.rs".to_string(),
                old_text: "nonexistent in b".to_string(),
                new_text: "irrelevant".to_string(),
            }),
        ]);
        execute_staging_step(&mut ctx, &mut both, &mut state);

        // The corrected set names `a.rs` only, so `b.rs` is abandoned instead of
        // vanishing with the entry it left behind.
        let mut corrected = replace(
            "pub fn compute() -> i32 { 0 }",
            "pub fn compute() -> i32 { 42 }",
        );
        execute_staging_step(&mut ctx, &mut corrected, &mut state);
        assert_eq!(state.abandoned.len(), 1);
        assert!(state.abandoned.contains_key("b.rs"));
    }

    #[test]
    fn an_attempt_marks_the_context_churn_of_its_change_set() {
        let mut fixture = Fixture::new(&[("a.rs", TARGET)], &["a.rs"]);
        let mut envelope = replace(
            "pub fn compute() -> i32 { 0 }",
            "pub fn compute() -> i32 { 42 }",
        );
        let mut ctx = fixture.context();
        assert!(!ctx.transcript().prefix_is_stale());
        let mut state = StagingState::default();

        execute_staging_step(&mut ctx, &mut envelope, &mut state);
        assert!(
            ctx.transcript().prefix_is_stale(),
            "the change set stales the rendered prefix"
        );
    }
}
