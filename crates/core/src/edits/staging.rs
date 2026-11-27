//! One virtual staging attempt. The caller owns retries and transcript updates.

use std::collections::{HashMap, HashSet};

use crate::edits::EditOperation;
use crate::llm::LlmCommand;
use crate::workflow::PatchStrategy;

use crate::context::ContextManager;
use crate::edits::{stage_and_validate_edits, EditError, StagedEditsState};
use crate::execution::{EventSink, ResponseEnvelope};
use crate::workflow::retry::{format_patch_retry_reason, retain_retried_failed_files};
pub use tauqe_tools::edits::staging::{apply_anchor_edits, AnchorEdit};

/// Only the inputs needed to stage and preview one proposal.
pub struct StagingEnvironment<'a> {
    pub repo_root: &'a std::path::Path,
    pub context_manager: &'a ContextManager,
    pub patch_strategy: PatchStrategy,
    pub events: &'a EventSink,
}
/// Cumulative virtual staging state of one turn.
///
/// A turn stages its change set in as many attempts as the repair rounds it
/// spends, so the virtual state it converges on belongs to the turn rather than to
/// a single attempt. The loop that drives the turn owns the value and hands every
/// attempt a mutable reference to it.
#[derive(Debug, Default)]
pub struct StagingState {
    /// Virtual state the attempts converged on.
    staged_state: Option<StagedEditsState>,
    /// Operations that converged, in change-set order.
    edit_batches: Vec<Vec<EditOperation>>,
    /// Paths whose edit reached the virtual state.
    succeeded: HashSet<String>,
    /// Paths the substrate refused in the latest attempt that addressed them.
    failed: HashMap<String, EditError>,
    /// Paths the model stopped retrying, paired with the error that refused them.
    abandoned: HashMap<String, EditError>,
    /// Exact document whose line numbers the last atomic patch refusal addresses.
    refused_patch: Option<String>,
    /// Number of staging attempts the turn spent.
    attempts: usize,
    last_step: StagingStep,
}

impl StagingState {
    pub fn last_step(&self) -> &StagingStep {
        &self.last_step
    }

    pub fn reset_step(&mut self) {
        self.last_step = StagingStep::default();
    }

    /// Drop the converged set but retain the result of this request.
    pub fn clear_accumulated(&mut self) {
        let step = std::mem::take(&mut self.last_step);
        *self = Self::default();
        self.last_step = step;
    }

    pub fn staged_state(&self) -> Option<&StagedEditsState> {
        self.staged_state.as_ref()
    }

    pub fn failures(&self) -> &HashMap<String, EditError> {
        &self.failed
    }

    pub fn abandoned(&self) -> &HashMap<String, EditError> {
        &self.abandoned
    }

    pub fn attempts(&self) -> usize {
        self.attempts
    }

    pub fn take_refused_patch(&mut self) -> Option<String> {
        self.refused_patch.take()
    }

    pub fn retry_prompt(&self, strategy: PatchStrategy) -> String {
        crate::workflow::retry::build_patch_retry_prompt(
            &self.succeeded,
            &self.failed,
            &[],
            strategy,
        )
    }

    pub(super) fn edit_batches(&self) -> &[Vec<EditOperation>] {
        &self.edit_batches
    }
}

/// What one executed staging attempt produced.
#[derive(Debug, Default)]
pub struct StagingStep {
    /// Whether the envelope carried a change set the attempt could stage.
    pub addressed: bool,
    /// Refusals of the attempt, rendered by the patch strategy of the turn and
    /// sorted by path.
    pub refusals: Vec<(String, String)>,
    /// Destinations absent on disk before this attempt; the caller records them.
    pub new_paths: Vec<String>,
}

/// Executes exactly one staging attempt against the virtual state of the turn.
///
/// The step drains the change set of the envelope, prepares a corrected set
/// against the refusals of the previous attempt, applies it to the virtual state,
/// and records the churn the attempt produced. It never requests a round of its
/// own: the repair rounds and their budget belong to the loop that drives the turn.
pub fn execute_staging_step(
    ctx: &mut StagingEnvironment<'_>,
    envelope: &mut ResponseEnvelope,
    state: &mut StagingState,
) {
    let commands = envelope.take_any(super::execution::EDIT_CAPABILITIES);
    if commands.is_empty() {
        return;
    }

    state.attempts += 1;
    let (result, patch_batches) = if let [LlmCommand::ApplyPatch { patch }] = commands.as_slice() {
        state.failed.clear();
        state.refused_patch = None;
        match crate::edits::stage_apply_patch(
            ctx.repo_root,
            ctx.context_manager,
            patch,
            state.staged_state.as_ref(),
        ) {
            Ok((result, batches)) => {
                super::execution::emit_staged_patch_diff(
                    ctx.repo_root,
                    ctx.events,
                    state.staged_state.as_ref(),
                    &result,
                );
                (result, Some(batches))
            }
            Err(error) => {
                state.refused_patch = Some(patch.clone());
                let path = error.patch_path().map_or_else(String::new, |path| {
                    ctx.context_manager
                        .normalize_path(path)
                        .unwrap_or_else(|_| path.to_owned())
                });
                state.failed.insert(path, error);
                state.last_step = StagingStep {
                    addressed: true,
                    refusals: render_failures(&state.failed, ctx.patch_strategy),
                    new_paths: Vec::new(),
                };
                return;
            }
        }
    } else {
        let edits: Vec<_> = commands
            .into_iter()
            .filter_map(LlmCommand::into_edit_operation)
            .collect();
        if edits.is_empty() {
            return;
        }
        let unretried = retain_retried_failed_files(&mut state.failed, ctx.context_manager, &edits);
        for (path, error) in unretried {
            state.abandoned.entry(path).or_insert(error);
        }
        (
            stage_and_validate_edits(
                ctx.repo_root,
                ctx.context_manager,
                &edits,
                state.staged_state.as_ref(),
            ),
            None,
        )
    };
    let converged = result.successful_paths.len();
    let mut new_paths = crate::blocking::run_blocking(|| {
        result
            .staged_state
            .staged_files
            .keys()
            .filter(|path| !ctx.repo_root.join(path).exists())
            .cloned()
            .collect::<Vec<_>>()
    });
    new_paths.sort();
    state.staged_state = Some(result.staged_state);
    // Only a path that converged in this attempt answers its earlier refusal.
    // Clearing by the cumulative set would also erase the failure of a path
    // that succeeded earlier and then failed again outside this attempt.
    for path in &result.successful_paths {
        state.failed.remove(path);
        state.abandoned.remove(path);
    }
    state.succeeded.extend(result.successful_paths);
    if let Some(batches) = patch_batches {
        state.edit_batches.extend(batches);
    } else if !result.successful_edits.is_empty() {
        state.edit_batches.push(result.successful_edits);
    }
    for failure in result.failed_files {
        state.abandoned.remove(&failure.path);
        state.failed.insert(failure.path, failure.error);
    }

    tracing::info!(
        repo_root = %ctx.repo_root.display(),
        attempt = state.attempts,
        converged,
        refused = state.failed.len(),
        "virtual staging attempt completed"
    );

    state.last_step = StagingStep {
        addressed: true,
        refusals: render_failures(&state.failed, ctx.patch_strategy),
        new_paths,
    };
}

/// Renders the refusals of one attempt as a path-sorted diagnostic list.
///
/// The wording comes from the active patch strategy, so a retry round is never
/// taught the vocabulary of a contour the turn does not speak.
pub fn render_failures(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ContextAccess;
    use crate::context::ContextManager;
    use crate::edits::EditReplace;
    use crate::llm::{LlmCommand, TextCommand};

    const TARGET: &str = "pub fn compute() -> i32 { 0 }\n";
    const CORRECTED: &str = "pub fn compute() -> i32 { 42 }\n";

    struct Fixture {
        dir: tempfile::TempDir,
        context: ContextManager,
        events: EventSink,
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
                events: EventSink::noop(),
            }
        }

        fn context(&mut self) -> StagingEnvironment<'_> {
            StagingEnvironment {
                repo_root: self.dir.path(),
                context_manager: &self.context,
                patch_strategy: PatchStrategy::default(),
                events: &self.events,
            }
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

        execute_staging_step(&mut ctx, &mut envelope, &mut state);
        assert!(!state.last_step().addressed);
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

        execute_staging_step(&mut ctx, &mut envelope, &mut state);
        assert!(state.last_step().addressed);
        assert!(state.last_step().refusals.is_empty());
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
        execute_staging_step(&mut ctx, &mut repaired, &mut state);
        assert!(state.last_step().refusals.is_empty());
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
    fn a_second_patch_can_restore_the_initial_bytes() {
        let mut fixture = Fixture::new(&[("a.rs", TARGET)], &["a.rs"]);
        let mut ctx = fixture.context();
        let mut state = StagingState::default();
        let mut first = replace("{ 0 }", "{ 42 }");
        let mut restore = replace("{ 42 }", "{ 0 }");
        execute_staging_step(&mut ctx, &mut first, &mut state);
        assert!(state.last_step().refusals.is_empty());
        execute_staging_step(&mut ctx, &mut restore, &mut state);
        assert!(state.last_step().refusals.is_empty());
        assert_eq!(
            state.staged_state.unwrap().staged_files["a.rs"],
            TARGET.as_bytes()
        );
        assert_eq!(state.edit_batches.len(), 2);
    }
}
