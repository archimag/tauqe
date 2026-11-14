//! Virtual staging loop: in-memory patch application with a localized retry.
//!
//! [`run_staging_loop`] owns exactly one responsibility of the turn: it takes the
//! change set a model round proposed, applies it to the virtual staging state, and
//! repairs the members the substrate refused through a bounded patch-retry loop.
//! It never touches Git, the toolchain, or the working tree, and it interprets no
//! semantic outcome of the model: a round that carries no change set leaves the
//! loop as [`LoopResult::Diverged`] with its envelope intact, so the contour that
//! owns the answer reads it.
//!
//! A structural protocol defect is not a staging failure (`crates/core/Conventions.md`
//! §13.6): repairing malformed output belongs to the protocol sanitizer, so this
//! loop only retries members the substrate refused for a semantic reason and keeps
//! the protocol surface out of its budget.

use std::collections::{HashMap, HashSet};

use tauqe_protocol::{EditOperation, LlmCommand, PatchStrategy, ReasoningEffort, TurnPhase};

use crate::control::context::LoopContext;
use crate::control::outcome::{
    FailureReport, LoopResult, LoopSuccess, ResponseEnvelope, SystemError,
};
use crate::edits::{stage_and_validate_edits, EditError, StagedEditsState};
use crate::providers::StreamEvent;
use crate::workflow::retry::{
    build_patch_retry_prompt, format_patch_retry_reason, prepare_staged_for_retry,
    retain_retried_failed_files,
};

/// Flat configuration of one virtual-staging contour.
///
/// The contour reads nothing but these values, so whatever narrows them — a
/// workflow default today, a ladder entry per model tomorrow — stays with the
/// caller that owns that decision.
#[derive(Debug, Clone)]
pub struct StagingLoopConfig {
    /// Patch-retry rounds granted to a change set the substrate refused.
    pub max_retries: usize,
    /// Strategy that decides how a change set addresses the lines it replaces.
    pub patch_strategy: PatchStrategy,
    /// Reasoning effort requested for the repair rounds.
    pub reasoning_effort: Option<ReasoningEffort>,
}

impl StagingLoopConfig {
    /// Creates staging configuration from project REPL configuration.
    pub fn from_repl_config(config: &crate::config::ReplConfig) -> Self {
        Self {
            max_retries: config.max_retries,
            patch_strategy: config.patch_strategy,
            reasoning_effort: None,
        }
    }
}

impl Default for StagingLoopConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            patch_strategy: PatchStrategy::SearchReplace,
            reasoning_effort: None,
        }
    }
}
/// In-memory change set that converged in virtual staging.
#[derive(Debug, Clone)]
pub struct StagingArtifact {
    /// Cumulative virtual state of the attempt, including earlier rounds.
    pub staged_state: StagedEditsState,
    /// Operations that converged, in change-set order.
    pub cumulative_edits: Vec<EditOperation>,
    /// Files the model stopped retrying, paired with their rendered reason.
    pub abandoned_failures: Vec<(String, String)>,
}

/// Applies a change set to the virtual staging state and repairs its refusals.
///
/// The opening envelope is drained of [`tauqe_protocol::LlmCapability::FileEdits`]
/// and travels upward with the artifact, so companion commands of the same round
/// stay available to the contours above. Every attempt contributes its converged
/// members to the cumulative state, and the residual refusals drive one repair
/// round each until the budget is spent.
pub async fn run_staging_loop(
    ctx: &mut LoopContext<'_>,
    mut first_envelope: ResponseEnvelope,
    config: &StagingLoopConfig,
) -> Result<LoopResult<StagingArtifact>, SystemError> {
    ctx.check_cancelled()?;

    let mut current_edits = extract_file_edits(&mut first_envelope);
    if current_edits.is_empty() {
        return Ok(LoopResult::Diverged(first_envelope));
    }

    let mut staged_state: Option<StagedEditsState> = None;
    let mut cumulative_edits: Vec<EditOperation> = Vec::new();
    let mut succeeded: HashSet<String> = HashSet::new();
    let mut failed: HashMap<String, EditError> = HashMap::new();
    let mut abandoned: HashMap<String, EditError> = HashMap::new();
    let mut attempt = 0usize;

    loop {
        ctx.check_cancelled()?;

        let result = stage_and_validate_edits(
            ctx.repo_root,
            ctx.context_manager,
            &current_edits,
            staged_state.as_ref(),
        );
        let converged = result.successful_paths.len();
        staged_state = Some(result.staged_state);
        // Only a path that converged in this attempt answers its earlier refusal.
        // Clearing by the cumulative set would also erase the failure of a path
        // that succeeded earlier and then failed again outside this attempt.
        for path in &result.successful_paths {
            failed.remove(path);
            abandoned.remove(path);
        }
        succeeded.extend(result.successful_paths);
        cumulative_edits.extend(result.successful_edits);
        for failure in result.failed_files {
            abandoned.remove(&failure.path);
            failed.insert(failure.path, failure.error);
        }
        tracing::info!(
            repo_root = %ctx.repo_root.display(),
            attempt,
            converged,
            refused = failed.len(),
            "virtual staging attempt completed"
        );

        if failed.is_empty() {
            break;
        }

        if attempt >= config.max_retries {
            let diagnostics: Vec<String> = render_failures(&failed, config.patch_strategy)
                .into_iter()
                .map(|(path, reason)| format!("- `{path}`: {reason}"))
                .collect();
            tracing::warn!(
                repo_root = %ctx.repo_root.display(),
                attempt,
                refused = diagnostics.len(),
                "virtual staging exhausted its patch-retry budget"
            );
            return Ok(LoopResult::Exhausted(FailureReport::new(
                "the change set did not converge in virtual staging",
                attempt,
                diagnostics,
            )));
        }
        attempt += 1;

        ctx.events.emit(StreamEvent::TurnPhase {
            phase: TurnPhase::Staging,
            round: Some(attempt),
            max_rounds: Some(config.max_retries),
            detail: Some(format!("Patch retry {attempt}/{}", config.max_retries)),
        });
        let rendered = render_failures(&failed, config.patch_strategy);
        for (path, reason) in &rendered {
            tracing::warn!(
                repo_root = %ctx.repo_root.display(),
                attempt,
                path = path.as_str(),
                reason = reason.as_str(),
                "patch retry requested"
            );
            ctx.events.emit(StreamEvent::EditFileRetrying {
                path: path.clone(),
                attempt,
                max_retries: config.max_retries,
                reason: reason.clone(),
            });
        }
        let repair_prompt =
            build_patch_retry_prompt(&succeeded, &failed, &[], config.patch_strategy);
        let mut reply = ctx
            .request_repair(&repair_prompt, staged_state.as_ref())
            .await?;
        let corrected = extract_file_edits(&mut reply);
        if corrected.is_empty() {
            // The repair round answered with something other than a change set.
            // Its answer is not staging's to interpret, so the envelope travels
            // upward untouched, exactly as the opening round's would.
            return Ok(LoopResult::Diverged(reply));
        }

        // A path the corrected set does not touch was not retried: the loop stops
        // asking for it, but its edit never reached the change set, so the record
        // is carried into the artifact instead of vanishing with the entry.
        let unretried = retain_retried_failed_files(&mut failed, ctx.context_manager, &corrected);
        for (path, error) in unretried {
            abandoned.entry(path).or_insert(error);
        }
        if let Some(staged) = staged_state.as_mut() {
            prepare_staged_for_retry(
                staged,
                &mut cumulative_edits,
                &mut succeeded,
                ctx.context_manager,
                &corrected,
            );
        }
        current_edits = corrected;
    }

    let abandoned_failures = render_failures(&abandoned, config.patch_strategy);
    for (path, reason) in &abandoned_failures {
        tracing::warn!(
            repo_root = %ctx.repo_root.display(),
            path = path.as_str(),
            reason = reason.as_str(),
            "a refused file was not retried and stays out of the change set"
        );
    }

    Ok(LoopResult::Success(LoopSuccess::new(
        StagingArtifact {
            staged_state: staged_state.unwrap_or_default(),
            cumulative_edits,
            abandoned_failures,
        },
        first_envelope,
    )))
}

/// Renders the refusals of one attempt as a path-sorted diagnostic list.
///
/// The wording comes from the active patch strategy, so a retry round is never
/// taught the vocabulary of a contour the turn does not speak.
fn render_failures(
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
    use crate::providers::mock::{MockLlmProvider, MockResponse};
    use tauqe_protocol::{
        CapabilityExpr, ContextAccess, EditReplace, LlmCapability, LlmCommand, ModelRef,
        TextCommand,
    };

    const TARGET: &str = "pub fn compute() -> i32 { 0 }\n";
    const CORRECTED: &str = "pub fn compute() -> i32 { 42 }\n";

    /// Builds an XML change set for `a.rs` in the dialect the tests speak.
    fn edit_block(search: &str, replace: &str) -> String {
        serde_json::json!({
            "message": "Edit",
            "changes": [{
                "op": "replace",
                "path": "a.rs",
                "to": "",
                "start": "",
                "end": "",
                "content": "",
                "old_text": search,
                "new_text": replace
            }],
            "commit_message": "Edit",
            "reason": ""
        })
        .to_string()
    }
    /// Owns the whole operating environment one staging contour observes.
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
        fn new(
            files: &[(&str, &str)],
            context_files: &[&str],
            responses: Vec<MockResponse>,
        ) -> Self {
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
                provider: MockLlmProvider::scripted(responses),
                model: ModelRef::openrouter("mock/model"),
                protocol: JsonPromptProtocol::edits(),
                cache: crate::control::cache::TurnInputCache::default(),
                cancel: CancellationToken::new(),
                events: EventSink::noop(),
                journal: crate::workflow::research::journal::TurnResearchJournal::default(),
                policy: crate::workflow::policy::DirectApplyPolicy::new(),
            }
        }

        fn context(&mut self) -> LoopContext<'_> {
            let caps = CapabilityExpr::set(
                crate::control::turn::EDIT_CAPABILITIES
                    .iter()
                    .copied()
                    .chain([LlmCapability::TextMessage])
                    .map(CapabilityExpr::leaf),
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

    #[tokio::test]
    async fn a_round_without_a_change_set_diverges_with_its_envelope() {
        let mut fixture = Fixture::new(&[("a.rs", TARGET)], &["a.rs"], Vec::new());
        let envelope = ResponseEnvelope::from_commands(&[LlmCommand::Text(TextCommand {
            content: "no change set".to_string(),
        })]);

        let mut ctx = fixture.context();
        let outcome = run_staging_loop(&mut ctx, envelope, &StagingLoopConfig::default())
            .await
            .expect("staging completes");

        assert!(outcome.is_diverged());
        assert!(
            fixture.provider.requests().is_empty(),
            "a round without a change set costs no provider round trip"
        );
    }

    #[tokio::test]
    async fn a_converging_change_set_returns_its_virtual_state() {
        let mut fixture = Fixture::new(&[("a.rs", TARGET)], &["a.rs"], Vec::new());
        let envelope = ResponseEnvelope::from_commands(&[LlmCommand::EditReplace(EditReplace {
            path: "a.rs".to_string(),
            old_text: "pub fn compute() -> i32 { 0 }".to_string(),
            new_text: "pub fn compute() -> i32 { 42 }".to_string(),
        })]);

        let mut ctx = fixture.context();
        let outcome = run_staging_loop(&mut ctx, envelope, &StagingLoopConfig::default())
            .await
            .expect("staging completes");
        let success = outcome.into_success().expect("the change set converges");

        assert_eq!(success.value.cumulative_edits.len(), 1);
        assert_eq!(
            success
                .value
                .staged_state
                .staged_text_files()
                .get("a.rs")
                .map(String::as_str),
            Some(CORRECTED)
        );
        assert!(success.value.abandoned_failures.is_empty());
        assert!(
            success.response.is_empty(),
            "the change set is drained from the envelope that travels upward"
        );
        assert!(fixture.provider.requests().is_empty());
    }

    #[tokio::test]
    async fn a_refused_member_is_repaired_by_one_patch_retry_round() {
        let responses = vec![MockResponse::text(edit_block(
            "pub fn compute() -> i32 { 0 }",
            "pub fn compute() -> i32 { 42 }",
        ))];
        let mut fixture = Fixture::new(&[("a.rs", TARGET)], &["a.rs"], responses);
        let envelope = ResponseEnvelope::from_commands(&[LlmCommand::EditReplace(EditReplace {
            path: "a.rs".to_string(),
            old_text: "nonexistent text".to_string(),
            new_text: "irrelevant".to_string(),
        })]);

        let mut ctx = fixture.context();
        let outcome = run_staging_loop(&mut ctx, envelope, &StagingLoopConfig::default())
            .await
            .expect("staging completes");
        let success = outcome
            .into_success()
            .expect("the repaired change set converges");

        assert_eq!(
            success
                .value
                .staged_state
                .staged_text_files()
                .get("a.rs")
                .map(String::as_str),
            Some(CORRECTED)
        );
        assert_eq!(fixture.provider.requests().len(), 1);
    }

    #[tokio::test]
    async fn an_unrepairable_change_set_exhausts_its_retry_budget() {
        let responses = vec![MockResponse::text(edit_block(
            "nonexistent text",
            "irrelevant",
        ))];
        let mut fixture = Fixture::new(&[("a.rs", TARGET)], &["a.rs"], responses);
        let envelope = ResponseEnvelope::from_commands(&[LlmCommand::EditReplace(EditReplace {
            path: "a.rs".to_string(),
            old_text: "nonexistent text".to_string(),
            new_text: "irrelevant".to_string(),
        })]);
        let config = StagingLoopConfig {
            max_retries: 1,
            ..StagingLoopConfig::default()
        };

        let mut ctx = fixture.context();
        let outcome = run_staging_loop(&mut ctx, envelope, &config)
            .await
            .expect("staging completes");
        let report = match outcome {
            LoopResult::Exhausted(report) => report,
            other => panic!("expected an exhausted staging budget, got {other:?}"),
        };

        assert_eq!(report.attempts, 1);
        assert!(!report.diagnostics.is_empty());
        assert!(report.diagnostics[0].contains("a.rs"));
    }

    #[tokio::test]
    async fn staging_loop_records_unretried_file_in_abandoned_failures() {
        let responses = vec![MockResponse::text(edit_block(
            "pub fn compute() -> i32 { 0 }",
            "pub fn compute() -> i32 { 42 }",
        ))];
        let mut fixture = Fixture::new(
            &[("a.rs", TARGET), ("b.rs", "pub fn b() {}")],
            &["a.rs", "b.rs"],
            responses,
        );
        let envelope = ResponseEnvelope::from_commands(&[
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

        let mut ctx = fixture.context();
        let outcome = run_staging_loop(&mut ctx, envelope, &StagingLoopConfig::default())
            .await
            .expect("staging completes");
        let success = outcome.into_success().expect("success");

        assert_eq!(success.value.abandoned_failures.len(), 1);
        assert_eq!(success.value.abandoned_failures[0].0, "b.rs");
    }

    #[tokio::test]
    async fn a_concrete_edit_command_converges_through_into_edit_operation() {
        let mut fixture = Fixture::new(&[("a.rs", TARGET)], &["a.rs"], Vec::new());
        let envelope = ResponseEnvelope::from_commands(&[LlmCommand::EditReplace(EditReplace {
            path: "a.rs".to_string(),
            old_text: "pub fn compute() -> i32 { 0 }".to_string(),
            new_text: "pub fn compute() -> i32 { 42 }".to_string(),
        })]);

        let mut ctx = fixture.context();
        let outcome = run_staging_loop(&mut ctx, envelope, &StagingLoopConfig::default())
            .await
            .expect("staging completes");
        let success = outcome.into_success().expect("concrete edit converges");

        assert_eq!(success.value.cumulative_edits.len(), 1);
        assert_eq!(
            success
                .value
                .staged_state
                .staged_text_files()
                .get("a.rs")
                .map(String::as_str),
            Some(CORRECTED)
        );
    }
}
