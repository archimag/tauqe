//! Staging phase executor for the FSM bounded turn.
//!
//! Staging validates the proposed edits purely in memory and drives the
//! patch-retry loop that repairs pattern-matching discrepancies, tracking the
//! residual failures for the caller to interpret. The phase operates on the
//! shared [`TurnKernel`] and never touches Git or the working tree directly;
//! discovery and the corrective budget of an empty answer belong to the kernel.

use std::collections::{HashMap, HashSet};

use tauqe_protocol::{EditOperation, TurnPhase};

use crate::edits::{EditError, StagedEditsState};
use crate::providers::StreamEvent;
use crate::workflow::retry::{
    build_patch_retry_prompt, format_patch_retry_reason, prepare_staged_for_retry,
    retain_retried_failed_files,
};

use super::guard::finalize_blocked_turn;
use super::kernel::TurnKernel;
use super::proposal::{log_fsm_phase, merge_text, ProposalStep};
use super::FsmTurnOutcome;

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
    ctx: &mut TurnKernel<'_>,
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

    // The turn froze its effective project configuration on the first proposal
    // round, so every staging attempt reads that snapshot instead of re-reading
    // `tauqe.toml` and re-running a declared task runner dump.
    let project = ctx.project().await;
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
                    &project,
                    ctx.context_manager,
                    &current_edits,
                    staged_state.as_ref(),
                    Some(ctx.cancel_rx),
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
            let reason = format_patch_retry_reason(error, ctx.options.patch_strategy());
            // A rejected project configuration is a self-preservation failure
            // rather than a stale search block: the harness refuses to commit a
            // configuration it could not load on its next start, so the
            // violation is recorded as its own observable event.
            if let EditError::InvalidConfig { message, .. } = error {
                tracing::warn!(
                    workflow = ctx.workflow_name,
                    model = %ctx.model,
                    attempt = attempt,
                    path = path.as_str(),
                    violation = %message,
                    "staged project configuration rejected"
                );
            }
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
            ctx.options.patch_strategy(),
        );
        if !retry_log.is_empty() {
            retry_log.push_str("\n\n---\n\n");
        }
        retry_log.push_str(&repair_prompt);
        // A retry round keeps the knowledge Discovery already paid for: the
        // task comes first, the accumulated journal next, and this round's
        // repair diagnostics last.
        let retry_prompt = ctx
            .journal
            .compose_user_prompt(ctx.prompt, Some(&retry_log));
        let step_out = ctx.propose(&retry_prompt, staged_state.as_ref()).await?;

        if step_out.is_cancelled {
            let _ = ctx.stream_tx.send(StreamEvent::Done).await;
            return Ok(StagingPhase::Terminate(Box::new(
                FsmTurnOutcome::cancelled(std::mem::take(&mut ctx.accumulated_text)),
            )));
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

    #[tokio::test]
    async fn discovery_findings_accumulate_in_an_append_only_journal_tail() {
        let (_dir, root) = setup_repo(&[("a.rs", TARGET)]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        let provider = MockLlmProvider::scripted([
            MockResponse::text("Consulting the docs.\n<doc_request topic=\"git\" />"),
            MockResponse::text("The documentation answers the question."),
        ]);
        let model = ModelRef::openrouter("mock/model");
        let protocol = XmlPromptProtocol;
        let mut policy = DirectApplyPolicy::new();
        let options = WorkflowOptions::default();
        let (stream_tx, rx) = mpsc::channel(256);
        drop(rx);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let outcome = run_fsm_turn(
            "Explain the git workflow",
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

        assert!(matches!(outcome.result, ModelResult::Answer { .. }));
        assert_eq!(provider.requests().len(), 2);

        // The findings ride in a delimited tail of the user message, after the
        // original task, so the stable part of the request keeps its bytes.
        let user = &provider.requests()[1].messages[1];
        assert_eq!(user.role, "user");
        let task_at = user.content.find("Explain the git workflow").expect("task");
        let tail_at = user
            .content
            .find("<discovery_journal>")
            .expect("journal tail");
        assert!(task_at < tail_at);
        assert!(user.content.contains("<system_documentation>"));
        assert!(user.content.trim_end().ends_with("</discovery_journal>"));
    }

    #[tokio::test]
    async fn patch_retry_reports_an_ignored_path_and_applies_the_alternative() {
        let (_dir, root) = setup_repo(&[("a.rs", TARGET), (".gitignore", "target/\n")]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        // The first round tries to generate a file inside an ignored directory,
        // which virtual staging rejects before any byte reaches disk.
        let ignored_create = concat!(
            "<tauqe_edits summary=\"Generate helper\">\n",
            "<create path=\"target/generated.rs\">\n",
            "pub fn generated() {}\n",
            "</create>\n",
            "</tauqe_edits>",
        );
        let corrected = edit_block(
            "pub fn compute() -> i32 { 0 }",
            "pub fn compute() -> i32 { 42 }",
        );
        let provider = MockLlmProvider::scripted([
            MockResponse::text(ignored_create),
            MockResponse::text(corrected),
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

        // The retry prompt names the rejected path and explains why it cannot
        // be written, so the model can pick a tracked destination.
        let retry_request = &provider.requests()[1];
        assert!(retry_request
            .messages
            .iter()
            .any(|message| message.content.contains("target/generated.rs")));
        assert!(retry_request.messages.iter().any(|message| message
            .content
            .contains("excluded by the repository ignore rules")));

        assert!(
            !root.join("target/generated.rs").exists(),
            "an ignored path must never reach the working tree"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("a.rs")).unwrap(),
            "pub fn compute() -> i32 { 42 }\n"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn post_hook_artifact_in_a_gitignored_directory_still_finalizes_the_commit() {
        let (_dir, root) = setup_repo(&[
            ("a.rs", TARGET),
            (".gitignore", "target/\n"),
            (
                "tauqe.toml",
                r#"[models]
primary = ["openrouter:mock/model"]

[project.verify]
command = "true"

[project.post_hook]
command = "sh"
args = ['-c', 'mkdir -p target && printf "artifact\n" > target/artifact.txt']
"#,
            ),
        ]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        let provider = MockLlmProvider::scripted([MockResponse::text(edit_block(
            "pub fn compute() -> i32 { 0 }",
            "pub fn compute() -> i32 { 42 }",
        ))]);
        let model = ModelRef::openrouter("mock/model");
        let protocol = XmlPromptProtocol;
        let mut policy = crate::workflow::git_policy::GitTransactionPolicy::new();
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
            ModelResult::Edit { outcome, .. } => {
                assert!(
                    outcome.error.is_none(),
                    "unexpected error: {:?}",
                    outcome.error
                );
                assert!(outcome.applied, "the verified change set must be committed");
                assert!(
                    outcome.commit_hash.is_some(),
                    "an ignored post-hook artifact must not abort the transaction"
                );
                assert_eq!(
                    outcome.changed_files,
                    vec!["a.rs".to_string()],
                    "an ignored artifact never joins the change set"
                );
            }
            other => panic!("expected applied edits, got {other:?}"),
        }

        // The hook really ran, and its ignored output stays out of Git.
        assert_eq!(
            std::fs::read_to_string(root.join("target/artifact.txt")).unwrap(),
            "artifact\n"
        );
        let status = crate::git::cmd::run_git(Some(&root), &["status", "--porcelain"]).unwrap();
        assert!(
            status.trim().is_empty(),
            "the ignored artifact must never dirty the working copy: {status}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn formats_staged_content_before_the_change_set_reaches_disk() {
        let (_dir, root) = setup_repo(&[
            ("a.rs", TARGET),
            (
                "tauqe.toml",
                r#"[models]
primary = ["openrouter:mock/model"]

[project.verify]
command = "true"

[project.format]
command = "sh"
args = ["-c", "tr a-z A-Z"]
"#,
            ),
        ]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        let provider = MockLlmProvider::scripted([MockResponse::text(edit_block(
            "pub fn compute() -> i32 { 0 }",
            "pub fn compute() -> i32 { 42 }",
        ))]);
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
        // The filter rewrote the staged content in memory, so the formatted text
        // is what reached the working tree; the authored text never did.
        assert_eq!(
            std::fs::read_to_string(root.join("a.rs")).unwrap(),
            "PUB FN COMPUTE() -> I32 { 42 }\n"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn keeps_the_authored_content_when_the_in_memory_formatter_fails() {
        let (_dir, root) = setup_repo(&[
            ("a.rs", TARGET),
            (
                "tauqe.toml",
                r#"[models]
primary = ["openrouter:mock/model"]

[project.verify]
command = "true"

[project.format]
command = "sh"
args = ["-c", "printf 'formatter failed' >&2; exit 3"]
"#,
            ),
        ]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        let provider = MockLlmProvider::scripted([MockResponse::text(edit_block(
            "pub fn compute() -> i32 { 0 }",
            "pub fn compute() -> i32 { 42 }",
        ))]);
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
        // A failing filter is fail-safe: the authored content is committed and
        // the turn still succeeds.
        assert_eq!(
            std::fs::read_to_string(root.join("a.rs")).unwrap(),
            "pub fn compute() -> i32 { 42 }\n"
        );
    }

    /// A model that keeps requesting repository material until the discovery
    /// round limit is exhausted must be warned, handed the limit directive, and
    /// asked for a concluding answer built from the material already gathered.
    ///
    /// The scenario pins the three invariants of the limit: the developer sees
    /// an explicit banner instead of a silently truncated investigation, the
    /// concluding request carries the directive that no further material will
    /// arrive, and the turn still closes as a written `ModelResult::Answer`
    /// with the accumulated auto context intact.
    #[tokio::test]
    async fn discovery_limit_warns_and_requests_a_concluding_answer() {
        let (_dir, root) = setup_repo(&[
            ("a.rs", TARGET),
            ("helper1.rs", "pub const ONE: i32 = 1;\n"),
            ("helper2.rs", "pub const TWO: i32 = 2;\n"),
            ("helper3.rs", "pub const THREE: i32 = 3;\n"),
        ]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        // Every round requests one more file and never writes a conclusion, so
        // the investigation only stops because the round budget is spent.
        let provider = MockLlmProvider::scripted([
            MockResponse::text("<context_request path=\"helper1.rs\" access=\"read_only\" />"),
            MockResponse::text("<context_request path=\"helper2.rs\" access=\"read_only\" />"),
            MockResponse::text("<context_request path=\"helper3.rs\" access=\"read_only\" />"),
            MockResponse::text("No further files are needed: ONE is 1 and TWO is 2."),
        ]);
        let model = ModelRef::openrouter("mock/model");
        let protocol = XmlPromptProtocol;
        let mut policy = DirectApplyPolicy::new();
        let options = WorkflowOptions {
            max_discovery_rounds: 2,
            ..WorkflowOptions::default()
        };
        // The receiver stays open for the whole turn, so the warning banner is
        // delivered instead of being dropped with a closed channel.
        let (stream_tx, mut stream_rx) = mpsc::channel(1024);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let outcome = run_fsm_turn(
            "Summarize the constants",
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

        // The turn concludes with a written answer instead of dying at the
        // limit, and the accumulated text carries the reason it stopped.
        match &outcome.result {
            ModelResult::Answer { text } => {
                assert!(
                    text.contains("No further files are needed"),
                    "the turn must record the model's conclusion: {text:?}"
                );
            }
            other => panic!("expected a written answer, got {other:?}"),
        }
        assert!(outcome
            .assistant_text
            .contains("Discovery round limit reached"));

        // Two discovery rounds and one concluding answer request.
        assert_eq!(provider.requests().len(), 4);

        // The developer-facing stream explains why the investigation stopped.
        let mut streamed = String::new();
        while let Ok(event) = stream_rx.try_recv() {
            if let StreamEvent::TextDelta(delta) = event {
                streamed.push_str(&delta);
            }
        }
        assert!(
            streamed.contains("Discovery round limit reached"),
            "the stream must explain the exhausted round budget: {streamed:?}"
        );
        assert!(streamed.contains("max_discovery_rounds = 2"));

        // The concluding request carries the limit directive and the answer
        // reminder, and the gathered material is still visible in the context.
        let concluding = &provider.requests()[3].messages[1].content;
        assert!(concluding.contains("<response_directive>"));
        assert!(concluding.contains("The discovery round limit is exhausted"));
        assert!(concluding.contains("Your investigation for this turn is complete"));
        assert!(provider.requests()[3].messages[0]
            .content
            .contains("pub const ONE: i32 = 1;"));

        // The accumulated auto context survives the truncated investigation.
        let mut auto_files = context.auto_files();
        auto_files.sort();
        assert_eq!(
            auto_files,
            vec!["helper1.rs".to_string(), "helper2.rs".to_string()],
            "the files discovery already gathered must stay available"
        );

        // The turn neither edited nor committed anything.
        assert_eq!(
            std::fs::read_to_string(root.join("a.rs")).unwrap(),
            TARGET,
            "a turn without edits must leave the working tree untouched"
        );
    }

    /// Counts the commits reachable from HEAD.
    fn commit_count(root: &std::path::Path) -> usize {
        crate::git::cmd::run_git(Some(root), &["rev-list", "--count", "HEAD"])
            .expect("count the commits")
            .trim()
            .parse()
            .expect("parse the commit count")
    }

    /// Reads the working copy status of the test repository.
    fn worktree_status(root: &std::path::Path) -> String {
        crate::git::cmd::run_git(Some(root), &["status", "--porcelain"])
            .expect("read the working copy status")
    }

    /// Returns the repository-relative paths recorded by the commit at HEAD.
    #[cfg(unix)]
    fn committed_paths(root: &std::path::Path) -> Vec<String> {
        let mut paths: Vec<String> = crate::git::cmd::run_git(
            Some(root),
            &["show", "--name-only", "--pretty=format:", "HEAD"],
        )
        .expect("read the committed paths")
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();
        paths.sort();
        paths
    }

    /// A turn whose whole change set lives in the mockups sandbox applies and
    /// verifies without producing a commit.
    ///
    /// The sandbox is deliberately outside Git, so the transaction has nothing
    /// to record. Finalization must close the open checkpoint instead of failing
    /// the verified turn, and the draft must still reach the working tree.
    #[tokio::test]
    async fn mockups_only_turn_applies_without_a_commit() {
        let (_dir, root) = setup_repo(&[("a.rs", TARGET)]);
        // The sandbox is admitted only while Git ignores it, which is also the
        // reason a draft can never join the change set of a turn.
        crate::git::checkpoint::ensure_tauqe_ignored(&root).unwrap();
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        let provider = MockLlmProvider::scripted([MockResponse::text(concat!(
            "<tauqe_edits summary=\"Draft a prototype\">\n",
            "<create path=\".tauqe/mockups/prototype.rs\">\n",
            "pub fn prototype() -> i32 { 42 }\n",
            "</create>\n",
            "</tauqe_edits>",
        ))]);
        let model = ModelRef::openrouter("mock/model");
        let protocol = XmlPromptProtocol;
        let mut policy = crate::workflow::git_policy::GitTransactionPolicy::new();
        let options = WorkflowOptions::default();
        let (stream_tx, rx) = mpsc::channel(256);
        drop(rx);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let baseline = commit_count(&root);
        let outcome = run_fsm_turn(
            "Sketch a prototype",
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
            ModelResult::Edit { edits, outcome, .. } => {
                assert!(
                    outcome.error.is_none(),
                    "unexpected error: {:?}",
                    outcome.error
                );
                assert!(outcome.applied, "a verified draft must still apply");
                assert_eq!(edits.len(), 1, "the model proposed exactly one edit");
                assert!(
                    outcome.changed_files.is_empty(),
                    "the sandbox never joins the change set: {:?}",
                    outcome.changed_files
                );
                assert!(
                    outcome.commit_hash.is_none(),
                    "a draft outside Git produces no commit"
                );
            }
            other => panic!("expected applied edits, got {other:?}"),
        }

        assert_eq!(
            std::fs::read_to_string(root.join(".tauqe/mockups/prototype.rs")).unwrap(),
            "pub fn prototype() -> i32 { 42 }\n",
            "the draft must reach its sandbox file"
        );
        assert!(
            worktree_status(&root).trim().is_empty(),
            "the turn must not leave the working copy dirty"
        );
        assert_eq!(
            commit_count(&root),
            baseline,
            "an untracked draft must not produce a commit"
        );
    }

    /// A turn that drafts a mockup and edits a tracked file commits the tracked
    /// file alone.
    ///
    /// The sandbox path never reaches `git add`, the commit, or the change set
    /// the project verification command receives on its standard input, so the
    /// draft can neither break the build nor abort the Git transaction.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_mixed_turn_commits_only_the_tracked_files() {
        let capture = tempfile::tempdir().unwrap();
        let capture_path = capture.path().join("change-set.txt");
        let config = format!(
            concat!(
                "[models]\n",
                "primary = [\"openrouter:mock/model\"]\n",
                "\n",
                "[project.verify]\n",
                "command = \"sh\"\n",
                "args = ['-c', 'tee {} > /dev/null']\n",
            ),
            capture_path.display()
        );
        let (_dir, root) = setup_repo(&[("a.rs", TARGET), ("tauqe.toml", config.as_str())]);
        crate::git::checkpoint::ensure_tauqe_ignored(&root).unwrap();
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        let provider = MockLlmProvider::scripted([MockResponse::text(concat!(
            "<tauqe_edits summary=\"Prototype and edit\">\n",
            "<create path=\".tauqe/mockups/prototype.rs\">\n",
            "pub fn prototype() -> i32 { 42 }\n",
            "</create>\n",
            "<edit path=\"a.rs\">\n",
            "<search>\npub fn compute() -> i32 { 0 }\n</search>\n",
            "<replace>\npub fn compute() -> i32 { 42 }\n</replace>\n",
            "</edit>\n",
            "</tauqe_edits>",
        ))]);
        let model = ModelRef::openrouter("mock/model");
        let protocol = XmlPromptProtocol;
        let mut policy = crate::workflow::git_policy::GitTransactionPolicy::new();
        let options = WorkflowOptions::default();
        let (stream_tx, rx) = mpsc::channel(256);
        drop(rx);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let outcome = run_fsm_turn(
            "Prototype the change and apply it",
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
                assert!(
                    outcome.error.is_none(),
                    "unexpected error: {:?}",
                    outcome.error
                );
                assert!(outcome.applied, "the verified change set must be committed");
                assert_eq!(
                    outcome.changed_files,
                    vec!["a.rs".to_string()],
                    "the sandbox never joins the change set of a turn"
                );
                assert!(outcome.commit_hash.is_some());
            }
            other => panic!("expected applied edits, got {other:?}"),
        }
        assert_eq!(
            committed_paths(&root),
            vec!["a.rs".to_string()],
            "the commit records the tracked file alone"
        );
        assert!(
            root.join(".tauqe/mockups/prototype.rs").exists(),
            "the draft is written even though it stays out of the commit"
        );
        assert!(
            worktree_status(&root).trim().is_empty(),
            "the turn must not leave unaccounted files behind"
        );

        let captured = std::fs::read_to_string(&capture_path)
            .expect("the verification command captured its standard input");
        let mut delivered: Vec<&str> = captured
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect();
        delivered.sort();
        assert_eq!(
            delivered,
            vec!["a.rs"],
            "verification must read the change set without the sandbox: {captured:?}"
        );
    }
    /// A later discovery round that only repeats requests the context already
    /// satisfies still delivers the aggregate hint.
    ///
    /// The guard that decides whether such a round deserves another provider
    /// round trip asks whether the model itself wrote prose. Reading the
    /// accumulated turn text instead answers "yes" from the second round
    /// onward, because that text also carries the harness round banners, so the
    /// directive would be unreachable for every turn that performs more than
    /// one discovery round.
    #[tokio::test]
    async fn a_repeated_satisfied_request_delivers_the_directive_in_a_later_round() {
        let (_dir, root) = setup_repo(&[
            ("a.rs", TARGET),
            ("helper.rs", "pub const ANSWER: i32 = 42;\n"),
        ]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        let provider = MockLlmProvider::scripted([
            // Round one widens the context and writes no prose.
            MockResponse::text("<context_request path=\"helper.rs\" access=\"read_only\" />"),
            // Round two only repeats a request the context already satisfies.
            MockResponse::text("<context_request path=\"helper.rs\" access=\"read_only\" />"),
            // Round three answers from the material already gathered.
            MockResponse::text("The helper holds the answer: 42."),
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

        match &outcome.result {
            ModelResult::Answer { text } => {
                assert!(
                    text.contains("The helper holds the answer: 42."),
                    "the turn must record the model's conclusion: {text:?}"
                );
            }
            other => panic!("expected a written answer, got {other:?}"),
        }
        assert_eq!(
            provider.requests().len(),
            3,
            "the directive round must conclude without an extra reminder round"
        );

        // The repeated request is answered with the specific directive, so the
        // model learns why the round widened nothing.
        let directive_round = &provider.requests()[2].messages[1];
        assert_eq!(directive_round.role, "user");
        assert!(
            directive_round
                .content
                .contains("Every file you requested is already loaded in <context>"),
            "the directive must reach the next proposal round: {}",
            directive_round.content
        );
        assert!(directive_round.content.contains("<response_directive>"));
        assert!(
            !directive_round
                .content
                .contains("Your investigation for this turn is complete"),
            "the generic answer reminder belongs to the withheld-hint path only"
        );
    }

    /// The satisfied-only directive is withheld once the model wrote prose.
    ///
    /// A model that already stated its analysis has concluded, so a later round
    /// that only repeats a loaded path must not buy another provider round trip
    /// on the strength of the aggregate hint: the turn falls through to the
    /// generic written-answer request instead.
    #[tokio::test]
    async fn the_satisfied_directive_is_withheld_after_the_model_wrote_prose() {
        let (_dir, root) = setup_repo(&[
            ("a.rs", TARGET),
            ("helper.rs", "pub const ANSWER: i32 = 42;\n"),
        ]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        let provider = MockLlmProvider::scripted([
            // Round one states an intention and widens the context.
            MockResponse::text(
                "I need the helper before I answer.\n<context_request path=\"helper.rs\" access=\"read_only\" />",
            ),
            // Round two only repeats a request the context already satisfies.
            MockResponse::text("<context_request path=\"helper.rs\" access=\"read_only\" />"),
            // The answer request is answered from the material already held.
            MockResponse::text("The helper holds the answer: 42."),
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

        match &outcome.result {
            ModelResult::Answer { text } => {
                assert!(
                    text.contains("The helper holds the answer: 42."),
                    "the turn must record the model's conclusion: {text:?}"
                );
            }
            other => panic!("expected a written answer, got {other:?}"),
        }
        assert_eq!(
            provider.requests().len(),
            3,
            "the withheld hint costs one written-answer request, not a satisfied-hint round"
        );

        let answer_round = &provider.requests()[2].messages[1];
        assert!(
            answer_round
                .content
                .contains("Your investigation for this turn is complete"),
            "a prose-writing model receives the generic answer request: {}",
            answer_round.content
        );
        assert!(
            !answer_round
                .content
                .contains("Every file you requested is already loaded in <context>"),
            "the satisfied-hint directive must stay withheld after the model wrote prose"
        );
    }
}
