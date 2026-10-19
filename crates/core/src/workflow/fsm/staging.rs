//! Discovery and staging phase executors for the FSM bounded turn.
//!
//! Discovery resolves the model's context-file and documentation requests
//! against the repository before any patch is proposed, enforcing the
//! `files_token_budget` guard so a single request cannot exhaust the context
//! window. Staging validates the proposed edits purely in memory and drives the
//! patch-retry loop that repairs pattern-matching discrepancies, tracking the
//! residual failures for the caller to interpret. Both phases operate on the
//! shared [`PhaseContext`] and never touch Git or the working tree directly.

use std::collections::{HashMap, HashSet};

use tauqe_protocol::{EditOperation, ModelResult, TurnPhase};

use crate::edits::{EditError, StagedEditsState};
use crate::providers::StreamEvent;
use crate::workflow::retry::{
    build_patch_retry_prompt, format_patch_retry_reason, prepare_staged_for_retry,
    retain_retried_failed_files,
};

use super::guard::finalize_blocked_turn;
use super::proposal::{log_fsm_phase, merge_text, ProposalStep};
use super::{propose, FsmTurnOutcome, PhaseContext};

/// Outcome of the FSM discovery phase.
pub(super) enum DiscoveryPhase {
    /// Discovery finished; carries the final proposal round to stage.
    ///
    /// The proposal is boxed so this variant stays small relative to
    /// `Terminate`, keeping the enum compact.
    Continue(Box<ProposalStep>),
    /// The turn must terminate immediately with the carried outcome.
    Terminate(Box<FsmTurnOutcome>),
}

/// Runs the discovery phase: resolves context and documentation requests.
///
/// Each round folds the model's requested files into the auto context layer and
/// injects requested documentation before re-asking the provider for a proposal,
/// feeding back the list of missing paths. The phase returns the last proposal
/// round so the caller can stage its edits. A breached file token budget, a
/// cancellation, or a semantic block terminates the turn instead.
pub(super) async fn run_discovery_phase(
    ctx: &mut PhaseContext<'_>,
    initial: ProposalStep,
) -> anyhow::Result<DiscoveryPhase> {
    let mut step = initial;
    let mut discovery_round = 0;
    let mut round_prompt = ctx.prompt.to_string();

    while discovery_round < ctx.options.max_discovery_rounds
        && step.edits().is_empty()
        && step.protocol_errors().is_empty()
    {
        let has_discovery = !step.context_requests().is_empty()
            || !step.doc_requests().is_empty()
            || !step.repo_searches().is_empty()
            || !step.repo_maps().is_empty()
            || !step.dir_list_requests().is_empty()
            || step.repo_dirs();
        if !has_discovery {
            break;
        }

        let outcome = crate::workflow::discovery::execute_discovery_commands(
            ctx.context_manager,
            ctx.repo_root,
            step.commands(),
            ctx.options.files_token_budget,
            ctx.options.repomap_token_budget,
            false,
        )
        .await;

        if let Some(limit) = &outcome.limit_exceeded {
            let warn = format!(
                "Context files token budget exceeded (files_token_budget = {}). The model requested {} tokens in new file(s) [{}], but the context already contains {} tokens. Scope the task down or decompose it into a plan.",
                limit.token_budget,
                limit.requested_tokens,
                limit.requested_files.join(", "),
                limit.current_tokens
            );
            tracing::warn!("{}", warn);
            let _ = ctx
                .stream_tx
                .send(StreamEvent::TextDelta(format!("\n\n⚠️ {}", warn)))
                .await;
            let _ = ctx.stream_tx.send(StreamEvent::Done).await;
            return Ok(DiscoveryPhase::Terminate(Box::new(FsmTurnOutcome {
                assistant_text: warn.clone(),
                result: ModelResult::Answer { text: warn },
                is_cancelled: false,
                blocked: None,
                has_terminal_success: false,
                plan_saved: false,
                skip_reason: None,
            })));
        }

        if !outcome.has_progress(false) {
            break;
        }

        discovery_round += 1;
        log_fsm_phase(
            ctx.workflow_name,
            ctx.model,
            ctx.intent.is_isolated_step(),
            TurnPhase::Discovery,
            Some(discovery_round),
            Some(ctx.options.max_discovery_rounds),
        );
        tracing::info!(
            workflow = ctx.workflow_name,
            model = %ctx.model,
            round = discovery_round,
            added = outcome.added_files.len(),
            missing = outcome.missing_files.len(),
            searches = outcome.searches_executed.len(),
            doc_topics = outcome.doc_topics_loaded.len(),
            "context discovery round completed"
        );
        let _ = ctx
            .stream_tx
            .send(StreamEvent::TurnPhase {
                phase: TurnPhase::Discovery,
                round: Some(discovery_round),
                max_rounds: Some(ctx.options.max_discovery_rounds),
                detail: None,
            })
            .await;

        let feedback = outcome.combined_feedback();
        if !feedback.is_empty() {
            round_prompt.push_str(&format!("\n\n{}", feedback));
        }

        let ctx_state = ctx.context_manager.get_state();
        let _ = ctx
            .stream_tx
            .send(StreamEvent::ContextChanged(ctx_state))
            .await;

        let banner_text = outcome.fsm_banner_summary();

        let banner = format!(
            "\n\n---\n**[Round {}]** {}\n\n",
            discovery_round + 1,
            banner_text
        );
        let _ = ctx
            .stream_tx
            .send(StreamEvent::TextDelta(banner.clone()))
            .await;
        log_fsm_phase(
            ctx.workflow_name,
            ctx.model,
            ctx.intent.is_isolated_step(),
            TurnPhase::Proposal,
            Some(discovery_round + 1),
            None,
        );
        let _ = ctx
            .stream_tx
            .send(StreamEvent::TurnPhase {
                phase: TurnPhase::Proposal,
                round: Some(discovery_round + 1),
                max_rounds: None,
                detail: None,
            })
            .await;

        step = propose(
            ctx.provider,
            ctx.model,
            ctx.protocol,
            ctx.history_manager,
            &*ctx.context_manager,
            ctx.workflow_name,
            ctx.stream_tx,
            ctx.cancel_rx,
            ctx.options,
            &mut *ctx.input_cache,
            ctx.state,
            ctx.intent,
            &round_prompt,
            None,
        )
        .await?;

        if step.is_cancelled {
            let _ = ctx.stream_tx.send(StreamEvent::Done).await;
            return Ok(DiscoveryPhase::Terminate(Box::new(FsmTurnOutcome {
                assistant_text: std::mem::take(&mut ctx.accumulated_text),
                result: ModelResult::Answer {
                    text: String::new(),
                },
                is_cancelled: true,
                blocked: None,
                has_terminal_success: false,
                plan_saved: false,
                skip_reason: None,
            })));
        }

        ctx.state.absorb(&step);
        let step_text = step.text();
        ctx.accumulated_text = merge_text(
            std::mem::take(&mut ctx.accumulated_text),
            &banner,
            step_text,
        );

        if let Some(reason) = ctx.state.blocked.clone() {
            let accumulated = std::mem::take(&mut ctx.accumulated_text);
            let outcome = finalize_blocked_turn(
                ctx.policy,
                ctx.context_manager,
                ctx.stream_tx,
                ctx.repo_root,
                reason,
                accumulated,
                &ctx.state.plans,
                ctx.plan_mode,
            )
            .await;
            return Ok(DiscoveryPhase::Terminate(Box::new(outcome)));
        }
    }

    Ok(DiscoveryPhase::Continue(Box::new(step)))
}

/// Number of additional proposal attempts granted to a model that produced an
/// empty round: no text, no edits, and no protocol command.
pub(super) const MAX_EMPTY_STEP_RETRIES: usize = 2;

/// Corrective suffix appended to the request after an empty proposal round.
///
/// The suffix extends the previous request instead of replacing it, so the
/// corrective round keeps the provider-cached prompt prefix and the model keeps
/// the original task statement in view.
pub(super) const EMPTY_STEP_REMINDER: &str = "\n\nYour previous reply was empty: it carried no text, no file edits, and no protocol command, so nothing could be acted upon. Respond again with a concrete proposal: state your findings in plain text, emit the file edits, request the missing context, or declare an explicit terminal outcome.";

/// Outcome of the corrective proposal round requested after an empty round.
pub(super) enum EmptyStepRetry {
    /// The model answered the corrective request; carries the new round.
    Proposed(Box<ProposalStep>),
    /// The turn must terminate immediately with the carried outcome.
    Terminate(Box<FsmTurnOutcome>),
}

/// Re-asks the model after it produced a round with nothing actionable.
///
/// An empty generation is a transport-level non-answer, so decoding it as a
/// validated `ModelResult::Answer` would record silence as a conclusion. The
/// helper appends a corrective reminder to `prompt` and asks once more, folding
/// the new round into the accumulated turn state and the accumulated text. A
/// cancellation or a semantic block terminates the turn instead.
pub(super) async fn reask_after_empty_round(
    ctx: &mut PhaseContext<'_>,
    prompt: &str,
    attempt: usize,
) -> anyhow::Result<EmptyStepRetry> {
    tracing::warn!(
        workflow = ctx.workflow_name,
        model = %ctx.model,
        attempt = attempt,
        max_retries = MAX_EMPTY_STEP_RETRIES,
        "empty proposal round; re-asking the model"
    );

    let detail = format!(
        "Empty response retry {}/{}",
        attempt, MAX_EMPTY_STEP_RETRIES
    );
    let banner = format!(
        "\n\n---\n**[Empty Response {}/{}]** The previous reply carried nothing actionable; asking for a concrete proposal...\n\n",
        attempt, MAX_EMPTY_STEP_RETRIES
    );
    let _ = ctx
        .stream_tx
        .send(StreamEvent::TextDelta(banner.clone()))
        .await;
    log_fsm_phase(
        ctx.workflow_name,
        ctx.model,
        ctx.intent.is_isolated_step(),
        TurnPhase::Proposal,
        Some(attempt),
        Some(MAX_EMPTY_STEP_RETRIES),
    );
    let _ = ctx
        .stream_tx
        .send(StreamEvent::TurnPhase {
            phase: TurnPhase::Proposal,
            round: Some(attempt),
            max_rounds: Some(MAX_EMPTY_STEP_RETRIES),
            detail: Some(detail),
        })
        .await;

    let step = propose(
        ctx.provider,
        ctx.model,
        ctx.protocol,
        ctx.history_manager,
        &*ctx.context_manager,
        ctx.workflow_name,
        ctx.stream_tx,
        ctx.cancel_rx,
        ctx.options,
        &mut *ctx.input_cache,
        ctx.state,
        ctx.intent,
        prompt,
        None,
    )
    .await?;

    if step.is_cancelled {
        let _ = ctx.stream_tx.send(StreamEvent::Done).await;
        return Ok(EmptyStepRetry::Terminate(Box::new(FsmTurnOutcome {
            assistant_text: std::mem::take(&mut ctx.accumulated_text),
            result: ModelResult::Answer {
                text: String::new(),
            },
            is_cancelled: true,
            blocked: None,
            has_terminal_success: false,
            plan_saved: false,
            skip_reason: None,
        })));
    }

    ctx.state.absorb(&step);
    ctx.accumulated_text = merge_text(
        std::mem::take(&mut ctx.accumulated_text),
        &banner,
        step.text(),
    );

    if let Some(reason) = ctx.state.blocked.clone() {
        let accumulated = std::mem::take(&mut ctx.accumulated_text);
        let outcome = finalize_blocked_turn(
            ctx.policy,
            ctx.context_manager,
            ctx.stream_tx,
            ctx.repo_root,
            reason,
            accumulated,
            &ctx.state.plans,
            ctx.plan_mode,
        )
        .await;
        return Ok(EmptyStepRetry::Terminate(Box::new(outcome)));
    }

    Ok(EmptyStepRetry::Proposed(Box::new(step)))
}

/// Outcome of the FSM staging phase.
pub(super) enum StagingPhase {
    /// Staging finished; carries the successfully staged edits and residual
    /// failures for the caller to interpret.
    Ready(StagingOutcome),
    /// The turn must terminate immediately with the carried outcome.
    Terminate(Box<FsmTurnOutcome>),
}

/// Successfully staged edits and residual failures from the staging phase.
pub(super) struct StagingOutcome {
    pub(super) cumulative_edits: Vec<EditOperation>,
    pub(super) last_failed_errors: HashMap<String, EditError>,
    pub(super) current_protocol_errors: Vec<String>,
}

/// Runs the staging phase: validates edits in memory and repairs failures.
///
/// Every round applies the proposed edits into a virtual staged state; edits
/// that match cleanly accumulate while failures are repaired through the
/// patch-retry loop up to `max_retries`. A cancellation or a semantic block
/// terminates the turn; otherwise the accumulated edits and residual failures
/// are returned for the caller to finalize.
pub(super) async fn run_staging_phase(
    ctx: &mut PhaseContext<'_>,
    step: &ProposalStep,
) -> anyhow::Result<StagingPhase> {
    let mut current_edits = step.edits().to_vec();
    let mut current_protocol_errors = step.protocol_errors().to_vec();
    let mut staged_state: Option<StagedEditsState> = None;
    let mut cumulative_edits: Vec<EditOperation> = Vec::new();
    let mut succeeded_files: HashSet<String> = HashSet::new();
    let mut last_failed_errors: HashMap<String, EditError> = HashMap::new();

    log_fsm_phase(
        ctx.workflow_name,
        ctx.model,
        ctx.intent.is_isolated_step(),
        TurnPhase::Staging,
        Some(1),
        Some(ctx.options.max_retries),
    );
    let _ = ctx
        .stream_tx
        .send(StreamEvent::TurnPhase {
            phase: TurnPhase::Staging,
            round: Some(1),
            max_rounds: Some(ctx.options.max_retries),
            detail: None,
        })
        .await;

    let mut attempt = 0;
    // Append-only repair log: every retry appends its diagnostics to the
    // previous request instead of replacing it, so the round extends the
    // provider-cached prompt prefix and the model keeps the full attempt
    // history.
    let mut retry_log = String::new();
    loop {
        if !current_edits.is_empty() {
            let stage_res = ctx
                .policy
                .staging(
                    ctx.repo_root,
                    ctx.context_manager,
                    &current_edits,
                    staged_state.as_ref(),
                )
                .await;
            tracing::info!(
                workflow = ctx.workflow_name,
                model = %ctx.model,
                attempt = attempt,
                succeeded_files = stage_res.successful_paths.len(),
                failed_files = stage_res.failed_files.len(),
                "staging round completed"
            );
            staged_state = Some(stage_res.staged_state);
            succeeded_files.extend(stage_res.successful_paths);
            cumulative_edits.extend(stage_res.successful_edits);

            for path in &succeeded_files {
                last_failed_errors.remove(path);
            }
            for failed in stage_res.failed_files {
                last_failed_errors.insert(failed.path, failed.error);
            }
        }

        if (last_failed_errors.is_empty() && current_protocol_errors.is_empty())
            || attempt >= ctx.options.max_retries
        {
            break;
        }
        attempt += 1;

        let _ = ctx
            .stream_tx
            .send(StreamEvent::TurnPhase {
                phase: TurnPhase::Staging,
                round: Some(attempt),
                max_rounds: Some(ctx.options.max_retries),
                detail: Some(format!(
                    "Patch retry {}/{}",
                    attempt, ctx.options.max_retries
                )),
            })
            .await;
        for (path, error) in &last_failed_errors {
            let reason = format_patch_retry_reason(error);
            tracing::warn!(
                workflow = ctx.workflow_name,
                model = %ctx.model,
                attempt = attempt,
                max_retries = ctx.options.max_retries,
                path = path.as_str(),
                reason = %reason,
                "patch retry requested"
            );
            let _ = ctx
                .stream_tx
                .send(StreamEvent::EditFileRetrying {
                    path: path.clone(),
                    attempt,
                    max_retries: ctx.options.max_retries,
                    reason,
                })
                .await;
        }

        let retry_banner = format!(
            "\n\n---\n**[Patch Retry {}/{}]** Correcting failed edits...\n\n",
            attempt, ctx.options.max_retries
        );
        let _ = ctx
            .stream_tx
            .send(StreamEvent::TextDelta(retry_banner.clone()))
            .await;

        let repair_prompt = build_patch_retry_prompt(
            &succeeded_files,
            &last_failed_errors,
            &current_protocol_errors,
            ctx.state.language.as_deref(),
        );
        if !retry_log.is_empty() {
            retry_log.push_str("\n\n---\n\n");
        }
        retry_log.push_str(&repair_prompt);
        let step_out = propose(
            ctx.provider,
            ctx.model,
            ctx.protocol,
            ctx.history_manager,
            &*ctx.context_manager,
            ctx.workflow_name,
            ctx.stream_tx,
            ctx.cancel_rx,
            ctx.options,
            &mut *ctx.input_cache,
            ctx.state,
            ctx.intent,
            &retry_log,
            staged_state.as_ref(),
        )
        .await?;

        if step_out.is_cancelled {
            let _ = ctx.stream_tx.send(StreamEvent::Done).await;
            return Ok(StagingPhase::Terminate(Box::new(FsmTurnOutcome {
                assistant_text: std::mem::take(&mut ctx.accumulated_text),
                result: ModelResult::Answer {
                    text: String::new(),
                },
                is_cancelled: true,
                blocked: None,
                has_terminal_success: false,
                plan_saved: false,
                skip_reason: None,
            })));
        }
        ctx.state.absorb(&step_out);
        let step_text = step_out.text();
        ctx.accumulated_text = merge_text(
            std::mem::take(&mut ctx.accumulated_text),
            &retry_banner,
            step_text,
        );

        if let Some(reason) = ctx.state.blocked.clone() {
            let accumulated = std::mem::take(&mut ctx.accumulated_text);
            let outcome = finalize_blocked_turn(
                ctx.policy,
                ctx.context_manager,
                ctx.stream_tx,
                ctx.repo_root,
                reason,
                accumulated,
                &ctx.state.plans,
                ctx.plan_mode,
            )
            .await;
            return Ok(StagingPhase::Terminate(Box::new(outcome)));
        }

        current_edits = step_out.edits().to_vec();
        current_protocol_errors = step_out.protocol_errors().to_vec();
        if !current_edits.is_empty() {
            retain_retried_failed_files(
                &mut last_failed_errors,
                ctx.context_manager,
                &current_edits,
            );
            if let Some(ref mut staged) = staged_state {
                prepare_staged_for_retry(
                    staged,
                    &mut cumulative_edits,
                    &mut succeeded_files,
                    ctx.context_manager,
                    &current_edits,
                );
            }
        }
    }

    Ok(StagingPhase::Ready(StagingOutcome {
        cumulative_edits,
        last_failed_errors,
        current_protocol_errors,
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::super::{run_fsm_turn as run_turn_with_intent, FsmTurnOutcome};
    use crate::context::ContextManager;
    use crate::history::HistoryManager;
    use crate::prompt::xml::XmlPromptProtocol;
    use crate::providers::mock::{MockLlmProvider, MockResponse};
    use crate::providers::{
        ChatMessage, ReasoningEffort, ResponseFormat, RetryConfig, RetryingProvider, StreamEvent,
    };
    use crate::workflow::policy::DirectApplyPolicy;
    use crate::workflow::WorkflowOptions;
    use tauqe_protocol::{ContextAccess, ModelRef, ModelResult, TurnIntent};
    use tokio::sync::{mpsc, watch};

    const TARGET: &str = "pub fn compute() -> i32 { 0 }\n";

    /// Runs an interactive develop turn through the FSM entry point.
    ///
    /// Every scenario in this module drives a free develop dialogue, so the
    /// declared turn intent is pinned once here instead of being repeated at
    /// every call site.
    #[allow(clippy::too_many_arguments)]
    async fn run_fsm_turn(
        prompt: &str,
        provider: &dyn crate::providers::LlmProvider,
        model: &ModelRef,
        context: &mut ContextManager,
        history: &mut HistoryManager,
        protocol: &dyn crate::prompt::protocol::PromptProtocol,
        policy: &mut dyn crate::workflow::policy::ApplyPolicy,
        workflow_name: &str,
        stream_tx: mpsc::Sender<StreamEvent>,
        cancel_rx: watch::Receiver<bool>,
        options: &WorkflowOptions,
    ) -> anyhow::Result<FsmTurnOutcome> {
        run_turn_with_intent(
            prompt,
            TurnIntent::InteractiveRepl,
            provider,
            model,
            context,
            history,
            protocol,
            policy,
            workflow_name,
            stream_tx,
            cancel_rx,
            options,
        )
        .await
    }

    fn setup_repo(files: &[(&str, &str)]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        crate::git::init_repository(Some(&root), true).unwrap();
        for (path, content) in files {
            let full = root.join(path);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(full, content).unwrap();
        }
        let owned: Vec<String> = files.iter().map(|(path, _)| path.to_string()).collect();
        crate::git::cmd::stage_files(&root, &owned).unwrap();
        crate::git::cmd::commit_with_developer_author(&root, "Add fixtures", false).unwrap();
        (dir, root)
    }

    fn context_with_target(root: &std::path::Path) -> ContextManager {
        let mut context = ContextManager::new(root.to_path_buf());
        context.add_file("a.rs", ContextAccess::Editable).unwrap();
        context
    }

    fn edit_block(search: &str, replace: &str) -> String {
        format!(
            "<tauqe_edits summary=\"Edit\">\n<edit path=\"a.rs\">\n<search>\n{search}\n</search>\n<replace>\n{replace}\n</replace>\n</edit>\n</tauqe_edits>"
        )
    }

    /// Retry policy whose backoff pauses are effectively instantaneous.
    fn fast_retry_config() -> RetryConfig {
        RetryConfig {
            max_attempts: 3,
            base_delay_ms: 1,
            max_delay_ms: 1,
        }
    }

    /// Adapter that lets the retry decorator wrap a scripted mock whose
    /// recorded requests stay observable from the test.
    struct RetryMock(Arc<MockLlmProvider>);

    #[async_trait::async_trait]
    impl crate::providers::LlmProvider for RetryMock {
        fn kind(&self) -> tauqe_protocol::ProviderKind {
            self.0.kind()
        }

        async fn stream_chat(
            &self,
            model: &str,
            messages: Vec<ChatMessage>,
            response_format: Option<ResponseFormat>,
            tx: mpsc::Sender<StreamEvent>,
            cancel_rx: watch::Receiver<bool>,
        ) -> anyhow::Result<()> {
            self.0
                .stream_chat(model, messages, response_format, tx, cancel_rx)
                .await
        }

        async fn stream_chat_with_session(
            &self,
            model: &str,
            messages: Vec<ChatMessage>,
            response_format: Option<ResponseFormat>,
            reasoning_effort: Option<ReasoningEffort>,
            session_id: Option<&str>,
            tx: mpsc::Sender<StreamEvent>,
            cancel_rx: watch::Receiver<bool>,
        ) -> anyhow::Result<()> {
            self.0
                .stream_chat_with_session(
                    model,
                    messages,
                    response_format,
                    reasoning_effort,
                    session_id,
                    tx,
                    cancel_rx,
                )
                .await
        }
    }

    #[tokio::test]
    async fn discovers_context_before_proposing_edits() {
        let (_dir, root) = setup_repo(&[
            ("a.rs", TARGET),
            ("helper.rs", "pub const ANSWER: i32 = 42;\n"),
        ]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        let provider = MockLlmProvider::scripted([
            MockResponse::text(
                "I need helper.rs.\n<context_request path=\"helper.rs\" access=\"read_only\" />",
            ),
            MockResponse::text(edit_block(
                "pub fn compute() -> i32 { 0 }",
                "pub fn compute() -> i32 { 42 }",
            )),
        ]);
        let model = ModelRef::openrouter("mock/model");
        let protocol = XmlPromptProtocol;
        let mut policy = DirectApplyPolicy::new();
        let options = WorkflowOptions::default();
        let (stream_tx, rx) = mpsc::channel(256);
        drop(rx);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let outcome = run_fsm_turn(
            "Use the helper constant",
            &provider,
            &model,
            &mut context,
            &mut history,
            &protocol,
            &mut policy,
            "git",
            stream_tx,
            cancel_rx,
            &options,
        )
        .await
        .unwrap();

        assert!(matches!(
            outcome.result,
            ModelResult::Edit { outcome, .. } if outcome.applied
        ));
        assert_eq!(provider.requests().len(), 2);
        let second_request = &provider.requests()[1];
        assert!(second_request.messages[0]
            .content
            .contains("pub const ANSWER: i32 = 42;"));
    }

    #[tokio::test]
    async fn retries_failed_patch_before_applying() {
        let (_dir, root) = setup_repo(&[("a.rs", TARGET)]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        let provider = MockLlmProvider::scripted([
            MockResponse::text(edit_block("nonexistent text", "irrelevant")),
            MockResponse::text(edit_block(
                "pub fn compute() -> i32 { 0 }",
                "pub fn compute() -> i32 { 42 }",
            )),
        ]);
        let model = ModelRef::openrouter("mock/model");
        let protocol = XmlPromptProtocol;
        let mut policy = DirectApplyPolicy::new();
        let options = WorkflowOptions::default();
        let (stream_tx, rx) = mpsc::channel(256);
        drop(rx);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let outcome = run_fsm_turn(
            "Change compute to 42",
            &provider,
            &model,
            &mut context,
            &mut history,
            &protocol,
            &mut policy,
            "git",
            stream_tx,
            cancel_rx,
            &options,
        )
        .await
        .unwrap();

        assert!(matches!(
            outcome.result,
            ModelResult::Edit { outcome, .. } if outcome.applied
        ));
        assert!(outcome.assistant_text.contains("Patch Retry"));
        assert_eq!(provider.requests().len(), 2);
    }

    #[tokio::test]
    async fn scratchpad_is_preserved_and_injected_across_rounds() {
        let (_dir, root) = setup_repo(&[
            ("a.rs", TARGET),
            ("helper.rs", "pub const ANSWER: i32 = 42;\n"),
        ]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        let provider = MockLlmProvider::scripted([
            MockResponse::text(
                "Recording my working notes.\n<scratchpad>\n- Hypothesis: helper.rs holds ANSWER\n</scratchpad>\n<context_request path=\"helper.rs\" access=\"read_only\" />",
            ),
            MockResponse::text(edit_block(
                "pub fn compute() -> i32 { 0 }",
                "pub fn compute() -> i32 { 42 }",
            )),
        ]);
        let model = ModelRef::openrouter("mock/model");
        let protocol = XmlPromptProtocol;
        let mut policy = DirectApplyPolicy::new();
        let options = WorkflowOptions::default();
        let (stream_tx, rx) = mpsc::channel(256);
        drop(rx);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let outcome = run_fsm_turn(
            "Use the helper constant",
            &provider,
            &model,
            &mut context,
            &mut history,
            &protocol,
            &mut policy,
            "git",
            stream_tx,
            cancel_rx,
            &options,
        )
        .await
        .unwrap();

        assert!(matches!(
            outcome.result,
            ModelResult::Edit { outcome, .. } if outcome.applied
        ));
        assert_eq!(provider.requests().len(), 2);
        let follow_up = &provider.requests()[1];
        assert!(follow_up.messages[0].content.contains("<scratchpad>"));
        assert!(follow_up.messages[0]
            .content
            .contains("Hypothesis: helper.rs holds ANSWER"));
    }

    #[tokio::test]
    async fn scratchpad_from_discovery_reaches_patch_retry() {
        let (_dir, root) = setup_repo(&[
            ("a.rs", TARGET),
            ("helper.rs", "pub const ANSWER: i32 = 42;\n"),
        ]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        let provider = MockLlmProvider::scripted([
            MockResponse::text(
                "Recording my working notes.\n<scratchpad>\n- Hypothesis: helper.rs holds ANSWER\n</scratchpad>\n<context_request path=\"helper.rs\" access=\"read_only\" />",
            ),
            MockResponse::text(edit_block("nonexistent text", "irrelevant")),
            MockResponse::text(edit_block(
                "pub fn compute() -> i32 { 0 }",
                "pub fn compute() -> i32 { 42 }",
            )),
        ]);
        let model = ModelRef::openrouter("mock/model");
        let protocol = XmlPromptProtocol;
        let mut policy = DirectApplyPolicy::new();
        let options = WorkflowOptions::default();
        let (stream_tx, rx) = mpsc::channel(256);
        drop(rx);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let outcome = run_fsm_turn(
            "Use the helper constant",
            &provider,
            &model,
            &mut context,
            &mut history,
            &protocol,
            &mut policy,
            "git",
            stream_tx,
            cancel_rx,
            &options,
        )
        .await
        .unwrap();

        assert!(matches!(
            outcome.result,
            ModelResult::Edit { outcome, .. } if outcome.applied
        ));
        assert_eq!(provider.requests().len(), 3);
        let retry_request = &provider.requests()[2];
        assert!(retry_request.messages[0].content.contains("<scratchpad>"));
        assert!(retry_request.messages[0]
            .content
            .contains("Hypothesis: helper.rs holds ANSWER"));
    }

    #[tokio::test]
    async fn retries_malformed_xml_edit_block_and_applies() {
        let (_dir, root) = setup_repo(&[("a.rs", TARGET)]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        let malformed_xml = "<tauqe_edits summary=\"Broken\"><edit path=\"a.rs\"><search>broken";
        let valid_xml = edit_block(
            "pub fn compute() -> i32 { 0 }",
            "pub fn compute() -> i32 { 42 }",
        );

        let provider = MockLlmProvider::scripted([
            MockResponse::text(malformed_xml),
            MockResponse::text(valid_xml),
        ]);
        let model = ModelRef::openrouter("mock/model");
        let protocol = XmlPromptProtocol;
        let mut policy = DirectApplyPolicy::new();
        let options = WorkflowOptions::default();
        let (stream_tx, rx) = mpsc::channel(256);
        drop(rx);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let outcome = run_fsm_turn(
            "Change compute to 42",
            &provider,
            &model,
            &mut context,
            &mut history,
            &protocol,
            &mut policy,
            "git",
            stream_tx,
            cancel_rx,
            &options,
        )
        .await
        .unwrap();

        assert!(matches!(
            outcome.result,
            ModelResult::Edit { outcome, .. } if outcome.applied
        ));
        assert_eq!(
            std::fs::read_to_string(root.join("a.rs")).unwrap(),
            "pub fn compute() -> i32 { 42 }\n"
        );
        assert_eq!(provider.requests().len(), 2);
        assert!(provider.requests()[1]
            .messages
            .iter()
            .any(|m| m.content.contains("Protocol parsing errors")));
    }

    #[tokio::test]
    async fn discovers_context_via_repo_search_and_applies() {
        let (_dir, root) = setup_repo(&[
            ("a.rs", TARGET),
            ("src/helper.rs", "pub const MAGIC_NUMBER: i32 = 42;\n"),
        ]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        let provider = MockLlmProvider::scripted([
            MockResponse::text(
                "Searching for magic number:\n<repo_search path=\"src\" regex=\"pub const MAGIC_\\w+\" />",
            ),
            MockResponse::text(
                "Found in src/helper.rs, requesting it:\n<context_request path=\"src/helper.rs\" access=\"read_only\" />",
            ),
            MockResponse::text(edit_block(
                "pub fn compute() -> i32 { 0 }",
                "pub fn compute() -> i32 { 42 }",
            )),
        ]);
        let model = ModelRef::openrouter("mock/model");
        let protocol = XmlPromptProtocol;
        let mut policy = DirectApplyPolicy::new();
        let options = WorkflowOptions::default();
        let (stream_tx, rx) = mpsc::channel(256);
        drop(rx);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let outcome = run_fsm_turn(
            "Use magic number from helper",
            &provider,
            &model,
            &mut context,
            &mut history,
            &protocol,
            &mut policy,
            "git",
            stream_tx,
            cancel_rx,
            &options,
        )
        .await
        .unwrap();

        assert!(matches!(
            outcome.result,
            ModelResult::Edit { outcome, .. } if outcome.applied
        ));
        assert_eq!(provider.requests().len(), 3);
        let second_req = &provider.requests()[1];
        assert!(second_req.messages[1]
            .content
            .contains("<search_feedback path=\"src\" regex=\"pub const MAGIC_\\w+\">"));
        assert!(second_req.messages[1]
            .content
            .contains("src/helper.rs: 1 match"));
    }

    #[tokio::test]
    async fn discovers_via_directory_listing_before_proposing_edits() {
        let (_dir, root) = setup_repo(&[
            ("a.rs", TARGET),
            ("src/helper.rs", "pub const MAGIC_NUMBER: i32 = 42;\n"),
        ]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        let provider = MockLlmProvider::scripted([
            MockResponse::text(
                "Listing the subtree:\n<dir_list path=\"src\" recursive=\"true\" />",
            ),
            MockResponse::text(
                "Requesting the helper:\n<context_request path=\"src/helper.rs\" access=\"read_only\" />",
            ),
            MockResponse::text(edit_block(
                "pub fn compute() -> i32 { 0 }",
                "pub fn compute() -> i32 { 42 }",
            )),
        ]);
        let model = ModelRef::openrouter("mock/model");
        let protocol = XmlPromptProtocol;
        let mut policy = DirectApplyPolicy::new();
        let options = WorkflowOptions::default();
        let (stream_tx, rx) = mpsc::channel(256);
        drop(rx);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let outcome = run_fsm_turn(
            "Use magic number from helper",
            &provider,
            &model,
            &mut context,
            &mut history,
            &protocol,
            &mut policy,
            "git",
            stream_tx,
            cancel_rx,
            &options,
        )
        .await
        .unwrap();

        assert!(matches!(
            outcome.result,
            ModelResult::Edit { outcome, .. } if outcome.applied
        ));
        // A round that only listed a directory still widens what the model can
        // see, so the turn continues instead of staging an empty proposal.
        assert_eq!(provider.requests().len(), 3);
        let listing_request = &provider.requests()[1];
        assert!(listing_request
            .messages
            .iter()
            .any(|m| m.content.contains("<dir_list_feedback path=\"src\"")));
        assert!(listing_request
            .messages
            .iter()
            .any(|m| m.content.contains("helper.rs")));
    }

    #[tokio::test]
    async fn recovers_from_search_regex_syntax_error() {
        let (_dir, root) = setup_repo(&[
            ("a.rs", TARGET),
            ("src/helper.rs", "pub const MAGIC_NUMBER: i32 = 42;\n"),
        ]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        let provider = MockLlmProvider::scripted([
            // Round 1: Model emits unsupported lookaround regex
            MockResponse::text(
                "Searching:\n<repo_search regex=\"(?=MAGIC)\" />",
            ),
            // Round 2: Model sees error feedback, falls back to direct context request
            MockResponse::text(
                "Correction after regex error:\n<context_request path=\"src/helper.rs\" access=\"read_only\" />",
            ),
            // Round 3: Model emits edit
            MockResponse::text(edit_block(
                "pub fn compute() -> i32 { 0 }",
                "pub fn compute() -> i32 { 42 }",
            )),
        ]);
        let model = ModelRef::openrouter("mock/model");
        let protocol = XmlPromptProtocol;
        let mut policy = DirectApplyPolicy::new();
        let options = WorkflowOptions::default();
        let (stream_tx, rx) = mpsc::channel(256);
        drop(rx);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let outcome = run_fsm_turn(
            "Use magic number",
            &provider,
            &model,
            &mut context,
            &mut history,
            &protocol,
            &mut policy,
            "git",
            stream_tx,
            cancel_rx,
            &options,
        )
        .await
        .unwrap();

        assert!(matches!(
            outcome.result,
            ModelResult::Edit { outcome, .. } if outcome.applied
        ));
        assert_eq!(provider.requests().len(), 3);
        let second_req = &provider.requests()[1];
        assert!(second_req.messages[1]
            .content
            .contains("Lookaround assertions are not supported in Rust regex"));
    }

    #[tokio::test]
    async fn discovers_via_doc_and_repo_search_across_rounds() {
        let (_dir, root) = setup_repo(&[
            ("a.rs", TARGET),
            ("src/helper.rs", "pub const MAGIC_NUMBER: i32 = 42;\n"),
        ]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        let provider = MockLlmProvider::scripted([
            MockResponse::text("Reviewing workflow docs:\n<doc_request topic=\"workflow\" />"),
            MockResponse::text(
                "Searching for the constant:\n<repo_search path=\"src\" regex=\"pub const MAGIC_\\w+\" />",
            ),
            MockResponse::text(
                "Requesting the helper:\n<context_request path=\"src/helper.rs\" access=\"read_only\" />",
            ),
            MockResponse::text(edit_block(
                "pub fn compute() -> i32 { 0 }",
                "pub fn compute() -> i32 { 42 }",
            )),
        ]);
        let model = ModelRef::openrouter("mock/model");
        let protocol = XmlPromptProtocol;
        let mut policy = DirectApplyPolicy::new();
        let options = WorkflowOptions {
            max_discovery_rounds: 4,
            ..Default::default()
        };
        let (stream_tx, rx) = mpsc::channel(256);
        drop(rx);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let outcome = run_fsm_turn(
            "Use magic number from helper",
            &provider,
            &model,
            &mut context,
            &mut history,
            &protocol,
            &mut policy,
            "git",
            stream_tx,
            cancel_rx,
            &options,
        )
        .await
        .unwrap();

        assert!(matches!(
            outcome.result,
            ModelResult::Edit { outcome, .. } if outcome.applied
        ));
        assert_eq!(provider.requests().len(), 4);

        // Documentation feedback reaches the proposal round after the doc request.
        let doc_request = &provider.requests()[1];
        assert!(doc_request
            .messages
            .iter()
            .any(|m| m.content.contains("<system_documentation>")));

        // Search feedback reaches the proposal round after the repo search.
        let search_request = &provider.requests()[2];
        assert!(search_request
            .messages
            .iter()
            .any(|m| m.content.contains("<search_feedback")));
    }

    #[tokio::test]
    async fn malformed_xml_edit_block_fails_as_edit_when_retries_exhausted() {
        let (_dir, root) = setup_repo(&[("a.rs", TARGET)]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        let malformed_xml = "<tauqe_edits summary=\"Broken\"><edit path=\"a.rs\"><search>broken";

        let provider = MockLlmProvider::scripted([
            MockResponse::text(malformed_xml),
            MockResponse::text(malformed_xml),
        ]);
        let model = ModelRef::openrouter("mock/model");
        let protocol = XmlPromptProtocol;
        let mut policy = DirectApplyPolicy::new();
        let options = WorkflowOptions {
            max_retries: 1,
            ..Default::default()
        };
        let (stream_tx, rx) = mpsc::channel(256);
        drop(rx);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let outcome = run_fsm_turn(
            "Change compute to 42",
            &provider,
            &model,
            &mut context,
            &mut history,
            &protocol,
            &mut policy,
            "git",
            stream_tx,
            cancel_rx,
            &options,
        )
        .await
        .unwrap();

        match outcome.result {
            ModelResult::Edit { outcome, .. } => {
                assert!(!outcome.applied);
                assert!(outcome.error.unwrap().contains("Protocol error"));
            }
            other => panic!("expected failed Edit result, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rolled_back_turn_does_not_apply_plans() {
        let (_dir, root) = setup_repo(&[("a.rs", TARGET)]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        // Model emits a bad edit that fails staging and also emits a plan tag
        let text = concat!(
            "<tauqe_edits summary=\"Bad Edit\">\n",
            "<edit path=\"a.rs\">\n",
            "<search>\nNON_EXISTENT_CONTENT\n</search>\n",
            "<replace>\nREPLACEMENT\n</replace>\n",
            "</edit>\n",
            "</tauqe_edits>\n",
            "<plan action=\"save\" id=\"rollback-plan\" title=\"Should Not Save\">\n",
            "  <concept>\n",
            "    <concept_item id=\"c-1\" title=\"Concept\" />\n",
            "  </concept>\n",
            "</plan>",
        );
        let provider = MockLlmProvider::scripted([
            MockResponse::text(text),
            MockResponse::text("Still failing"),
        ]);
        let model = ModelRef::openrouter("mock/model");
        let protocol = XmlPromptProtocol;
        let mut policy = DirectApplyPolicy::new();
        let options = WorkflowOptions {
            max_retries: 1,
            ..Default::default()
        };
        let (stream_tx, rx) = mpsc::channel(256);
        drop(rx);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let outcome = run_fsm_turn(
            "Do bad edit",
            &provider,
            &model,
            &mut context,
            &mut history,
            &protocol,
            &mut policy,
            "git",
            stream_tx,
            cancel_rx,
            &options,
        )
        .await
        .unwrap();

        assert!(matches!(
            outcome.result,
            ModelResult::Edit { outcome, .. } if !outcome.applied
        ));
        let loaded = crate::plan::storage::PlanStorage::load_plan(&root, "rollback-plan").unwrap();
        assert!(loaded.is_none(), "rolled back turn must not persist plan");
    }

    #[tokio::test]
    async fn retries_empty_proposal_round_before_applying() {
        let (_dir, root) = setup_repo(&[("a.rs", TARGET)]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        let provider = MockLlmProvider::scripted([
            MockResponse::text(""),
            MockResponse::text(edit_block(
                "pub fn compute() -> i32 { 0 }",
                "pub fn compute() -> i32 { 42 }",
            )),
        ]);
        let model = ModelRef::openrouter("mock/model");
        let protocol = XmlPromptProtocol;
        let mut policy = DirectApplyPolicy::new();
        let options = WorkflowOptions::default();
        let (stream_tx, rx) = mpsc::channel(256);
        drop(rx);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let outcome = run_fsm_turn(
            "Change compute to 42",
            &provider,
            &model,
            &mut context,
            &mut history,
            &protocol,
            &mut policy,
            "git",
            stream_tx,
            cancel_rx,
            &options,
        )
        .await
        .unwrap();

        assert!(matches!(
            outcome.result,
            ModelResult::Edit { outcome, .. } if outcome.applied
        ));
        assert!(outcome.assistant_text.contains("Empty Response"));
        assert_eq!(provider.requests().len(), 2);
        let retry_request = &provider.requests()[1];
        assert!(retry_request
            .messages
            .iter()
            .any(|m| m.content.contains("Your previous reply was empty")));
    }

    #[tokio::test]
    async fn recovers_from_a_transport_break_and_an_empty_round() {
        let (_dir, root) = setup_repo(&[("a.rs", TARGET)]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        let mock = Arc::new(MockLlmProvider::scripted([
            // The first attempt breaks before streaming anything.
            MockResponse::error("OpenRouter stream ended prematurely without [DONE] marker"),
            // The transparent retry completes but yields nothing actionable.
            MockResponse::text(""),
            // The corrective re-ask finally carries the edit.
            MockResponse::text(edit_block(
                "pub fn compute() -> i32 { 0 }",
                "pub fn compute() -> i32 { 42 }",
            )),
        ]));
        let provider =
            RetryingProvider::new(Box::new(RetryMock(Arc::clone(&mock))), fast_retry_config());
        let model = ModelRef::openrouter("mock/model");
        let protocol = XmlPromptProtocol;
        let mut policy = DirectApplyPolicy::new();
        let options = WorkflowOptions::default();
        let (stream_tx, rx) = mpsc::channel(256);
        drop(rx);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let outcome = run_fsm_turn(
            "Change compute to 42",
            &provider,
            &model,
            &mut context,
            &mut history,
            &protocol,
            &mut policy,
            "git",
            stream_tx,
            cancel_rx,
            &options,
        )
        .await
        .unwrap();

        assert!(matches!(
            outcome.result,
            ModelResult::Edit { outcome, .. } if outcome.applied
        ));
        assert!(outcome.assistant_text.contains("Empty Response"));
        // One re-issued attempt for the interrupted stream and one corrective
        // re-ask for the empty round that followed it.
        assert_eq!(mock.requests().len(), 3);
        assert_eq!(
            std::fs::read_to_string(root.join("a.rs")).unwrap(),
            "pub fn compute() -> i32 { 42 }\n"
        );
    }

    #[tokio::test]
    async fn fails_the_turn_when_every_empty_retry_stays_empty() {
        let (_dir, root) = setup_repo(&[("a.rs", TARGET)]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        let provider = MockLlmProvider::scripted([
            MockResponse::text(""),
            MockResponse::text(""),
            MockResponse::text(""),
        ]);
        let model = ModelRef::openrouter("mock/model");
        let protocol = XmlPromptProtocol;
        let mut policy = DirectApplyPolicy::new();
        let options = WorkflowOptions::default();
        let (stream_tx, rx) = mpsc::channel(256);
        drop(rx);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let outcome = run_fsm_turn(
            "Change compute to 42",
            &provider,
            &model,
            &mut context,
            &mut history,
            &protocol,
            &mut policy,
            "git",
            stream_tx,
            cancel_rx,
            &options,
        )
        .await
        .unwrap();

        match outcome.result {
            ModelResult::Edit { outcome, edits, .. } => {
                assert!(!outcome.applied);
                assert!(edits.is_empty());
                assert!(outcome.error.unwrap().contains("empty response"));
            }
            other => panic!("expected an empty-step failure, got {other:?}"),
        }
        assert_eq!(provider.requests().len(), 3);
        assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), TARGET);
    }

    /// Provider that breaks its first attempt mid-stream after emitting a
    /// partial edit block, then answers the re-issued request completely.
    struct BreakAfterPartial {
        attempts: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl crate::providers::LlmProvider for BreakAfterPartial {
        fn kind(&self) -> tauqe_protocol::ProviderKind {
            tauqe_protocol::ProviderKind::OpenRouter
        }

        async fn stream_chat(
            &self,
            _model: &str,
            _messages: Vec<ChatMessage>,
            _response_format: Option<ResponseFormat>,
            tx: mpsc::Sender<StreamEvent>,
            _cancel_rx: watch::Receiver<bool>,
        ) -> anyhow::Result<()> {
            let attempt = self
                .attempts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if attempt == 0 {
                let partial = concat!(
                    "<tauqe_edits summary=\"Edit\">\n",
                    "<edit path=\"a.rs\">\n<search>\npub fn compute"
                );
                let _ = tx.send(StreamEvent::TextDelta(partial.to_string())).await;
                return Err(anyhow::anyhow!(
                    "OpenRouter stream read error: connection reset by peer"
                ));
            }
            let _ = tx
                .send(StreamEvent::TextDelta(edit_block(
                    "pub fn compute() -> i32 { 0 }",
                    "pub fn compute() -> i32 { 42 }",
                )))
                .await;
            let _ = tx.send(StreamEvent::Done).await;
            Ok(())
        }
    }

    #[tokio::test]
    async fn discards_the_fragments_of_an_interrupted_attempt() {
        let (_dir, root) = setup_repo(&[("a.rs", TARGET)]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        // The first attempt breaks mid-stream after emitting a partial edit
        // block; the retry decorator re-issues the request and the second
        // attempt answers with the complete edit.
        let provider = RetryingProvider::new(
            Box::new(BreakAfterPartial {
                attempts: std::sync::atomic::AtomicUsize::new(0),
            }),
            fast_retry_config(),
        );
        let model = ModelRef::openrouter("mock/model");
        let protocol = XmlPromptProtocol;
        let mut policy = DirectApplyPolicy::new();
        let options = WorkflowOptions::default();
        let (stream_tx, rx) = mpsc::channel(256);
        drop(rx);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let outcome = run_fsm_turn(
            "Change compute to 42",
            &provider,
            &model,
            &mut context,
            &mut history,
            &protocol,
            &mut policy,
            "git",
            stream_tx,
            cancel_rx,
            &options,
        )
        .await
        .unwrap();

        assert!(matches!(
            outcome.result,
            ModelResult::Edit { outcome, .. } if outcome.applied
        ));
        assert_eq!(
            std::fs::read_to_string(root.join("a.rs")).unwrap(),
            "pub fn compute() -> i32 { 42 }\n"
        );
    }

    #[allow(dead_code)]
    fn _unused_stream_event(_event: StreamEvent) {}
}
