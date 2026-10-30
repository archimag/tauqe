//! Concrete typed FSM tasks.
//!
//! This module declares the concrete [`Task`] implementations used by the
//! workflow execution modes:
//!
//! - [`AnswerEditLoop`] runs the full discovery/edit/healing cycle while
//!   allowing the model to answer without proposing edits or to emit new plan
//!   definitions.
//! - [`EditLoop`] runs the same cycle but requires at least one valid edit,
//!   issuing a corrective retry whenever the model answers without edits.
//!
//! Every task delegates the low-level cycling to the shared edit pipeline, so
//! the tasks differ only in their acceptance contract, never in control flow.
//! Keeping the phases inside one pipeline preserves a single source of truth
//! for discovery, staging, and healing while the tasks stay declarative.

use async_trait::async_trait;

use crate::providers::StreamEvent;

use super::fsm::{
    begin_session_turn, record_session_response, run_fsm_turn, run_fsm_turn_body, FsmTurnOutcome,
};
use super::task::{Task, TaskEnv};

/// FSM task that runs a discovery/edit/healing cycle while allowing the model
/// to answer without edits and to define new plans.
pub struct AnswerEditLoop {
    prompt: String,
}

impl AnswerEditLoop {
    /// Creates an answer/edit task bound to the turn prompt.
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
        }
    }
}

#[async_trait]
impl Task for AnswerEditLoop {
    fn name(&self) -> &'static str {
        "answer_edit_loop"
    }

    async fn execute(&self, env: &mut TaskEnv<'_>) -> anyhow::Result<FsmTurnOutcome> {
        run_task_turn(&self.prompt, env).await
    }
}

/// FSM task that requires at least one valid edit and retries otherwise.
pub struct EditLoop {
    prompt: String,
}

impl EditLoop {
    /// Creates an edit task bound to the turn prompt.
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
        }
    }
}

#[async_trait]
impl Task for EditLoop {
    fn name(&self) -> &'static str {
        "edit_loop"
    }

    async fn execute(&self, env: &mut TaskEnv<'_>) -> anyhow::Result<FsmTurnOutcome> {
        // Session bookkeeping runs once for the whole task so retried attempts
        // never duplicate the user prompt in history or re-run compaction.
        let turn_id = begin_session_turn(
            &self.prompt,
            env.intent,
            env.context,
            env.history,
            env.provider,
            &env.cancel_rx,
            env.options,
        )
        .await;

        let res = async {
            let mut output = run_task_turn_body(&self.prompt, env).await?;

            let mut attempt = 0;
            while !output.is_cancelled
                && output.blocked.is_none()
                && output.skip_reason.is_none()
                && !has_valid_edits(&output.result)
                && !is_terminal_failure(&output.result)
                && attempt < env.options.max_retries
            {
                attempt += 1;
                let banner = format!(
                    "\n\n---\n**[Edit Retry {}/{}]** The task requires valid edits; requesting a corrected proposal...\n\n",
                    attempt, env.options.max_retries
                );
                let _ = env
                    .stream_tx
                    .send(StreamEvent::TextDelta(banner.clone()))
                    .await;

                let retry_prompt = build_edit_retry_prompt(None);
                let next = run_task_turn_body(&retry_prompt, env).await?;
                let cancelled = next.is_cancelled;
                merge_output(&mut output, next, &banner);
                if cancelled {
                    break;
                }
            }
            Ok(output)
        }
        .await;

        match res {
            Ok(output) => {
                if output.is_cancelled {
                    if let Some(id) = turn_id {
                        let _ = env
                            .history
                            .rollback_entry(id, "Turn cancelled by the developer");
                    }
                } else {
                    record_session_response(env.history, &output);
                    // A plan mutation the storage engine refused never fails the
                    // turn, but it is a protocol-level diagnostic the developer
                    // must see. The corrective round applies its own plan
                    // payload, so the merged outcome carries every attempt's
                    // rejection and the banner reports them once the turn
                    // settles.
                    if let Some(notice) = output.plan_rejection_notice() {
                        tracing::warn!("{}", notice);
                        let _ = env
                            .stream_tx
                            .send(StreamEvent::TextDelta(format!(
                                "\n\n---\n\u{26a0}\u{fe0f} {}\n\n",
                                notice
                            )))
                            .await;
                    }
                }
                Ok(output)
            }
            Err(err) => {
                if let Some(id) = turn_id {
                    let _ = env
                        .history
                        .rollback_entry(id, format!("Turn failed: {}", err));
                }
                Err(err)
            }
        }
    }
}

/// Runs one bounded FSM turn through the shared edit pipeline.
///
/// Every task delegates its low-level cycling here so discovery, staging, and
/// healing stay defined in a single place and each task remains declarative.
async fn run_task_turn(prompt: &str, env: &mut TaskEnv<'_>) -> anyhow::Result<FsmTurnOutcome> {
    run_fsm_turn(
        prompt,
        env.intent,
        env.provider,
        env.model,
        env.context,
        env.history,
        env.protocol,
        env.policy,
        env.workflow_name,
        env.stream_tx.clone(),
        env.cancel_rx.clone(),
        env.options,
    )
    .await
}

/// Runs one bounded FSM turn body without session bookkeeping.
///
/// Used by tasks that issue several attempts within a single bounded turn; the
/// caller performs the session bookkeeping once so retries never duplicate the
/// user prompt or the response in history.
async fn run_task_turn_body(prompt: &str, env: &mut TaskEnv<'_>) -> anyhow::Result<FsmTurnOutcome> {
    run_fsm_turn_body(
        prompt,
        env.intent,
        env.provider,
        env.model,
        env.context,
        env.history,
        env.protocol,
        env.policy,
        env.workflow_name,
        env.stream_tx.clone(),
        env.cancel_rx.clone(),
        env.options,
    )
    .await
}

/// Whether the pipeline produced at least one validated edit operation.
fn has_valid_edits(result: &tauqe_protocol::ModelResult) -> bool {
    matches!(result, tauqe_protocol::ModelResult::Edit { edits, .. } if !edits.is_empty())
}

/// Whether the turn ended in an explicit terminal failure rather than a
/// conclusion.
///
/// The FSM reports a declared `<error>` outcome and an exhausted staging or
/// healing cycle as an unapplied edit carrying the diagnostic. Such a turn has
/// already rejected its response, so re-asking for edit blocks would restart
/// failed work instead of correcting a missing proposal.
fn is_terminal_failure(result: &tauqe_protocol::ModelResult) -> bool {
    matches!(
        result,
        tauqe_protocol::ModelResult::Edit { outcome, .. }
            if !outcome.applied && outcome.error.is_some()
    )
}

/// Builds the corrective prompt issued when a required edit was not produced.
fn build_edit_retry_prompt(target_language: Option<&str>) -> String {
    let mut prompt = String::from(
        "The previous response did not contain any valid edit blocks, but this task requires \
         at least one concrete change. Inspect the current <context>, then emit the required \
         edits using the edit protocol.",
    );
    if let Some(lang) = target_language {
        prompt.push_str(&format!(
            "\n\nNote: Formulate your explanations in {} (the language of the user's request), while conducting all reasoning strictly in English.",
            lang
        ));
    }
    prompt
}

/// Folds a retried attempt into the accumulated turn outcome.
///
/// A corrective retry is a complete bounded turn of its own: it stages and
/// verifies its own change set and applies its own plan payload. Its plan
/// rejections are therefore appended to the ones already collected instead of
/// replacing them, and an abort report travels with the attempt that raised
/// it, so a supervisor can still tell a host-environment stop from an ordinary
/// failure.
///
/// Every field is destructured explicitly, so a field added to
/// [`FsmTurnOutcome`] later cannot be silently dropped here: the compiler
/// rejects the incomplete pattern instead.
fn merge_output(target: &mut FsmTurnOutcome, next: FsmTurnOutcome, banner: &str) {
    let FsmTurnOutcome {
        assistant_text,
        result,
        is_cancelled,
        blocked,
        has_terminal_success,
        plan_saved,
        plan_errors,
        skip_reason,
        abort,
    } = next;

    target.assistant_text = format!(
        "{}{}{}",
        target.assistant_text.trim_end(),
        banner,
        assistant_text.trim_start()
    );
    target.result = result;
    target.is_cancelled = is_cancelled;
    target.blocked = blocked;
    target.has_terminal_success = has_terminal_success;
    target.plan_saved = plan_saved;
    target.skip_reason = skip_reason;
    // Each attempt applies its plan payload independently, so a rejection the
    // corrective round reported is appended rather than replacing the
    // first-attempt list.
    target.plan_errors.extend(plan_errors);
    target.abort = abort;
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use tauqe_protocol::{ContextAccess, ModelRef, ModelResult, ProviderKind};
    use tokio::sync::{mpsc, watch};

    use crate::context::ContextManager;
    use crate::history::HistoryManager;
    use crate::prompt::protocol::PromptProtocol;
    use crate::providers::{ChatMessage, LlmProvider, ResponseFormat, StreamEvent};
    use crate::workflow::fsm::{AbortReport, FsmTurnOutcome};
    use crate::workflow::policy::DirectApplyPolicy;
    use crate::workflow::WorkflowOptions;

    use super::{merge_output, AnswerEditLoop, EditLoop, Task, TaskEnv};

    struct MockScriptProvider {
        responses: Mutex<VecDeque<String>>,
    }

    impl MockScriptProvider {
        fn new(responses: Vec<String>) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().collect()),
            }
        }

        fn remaining(&self) -> usize {
            self.responses.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl LlmProvider for MockScriptProvider {
        fn kind(&self) -> ProviderKind {
            ProviderKind::OpenRouter
        }

        async fn stream_chat(
            &self,
            _model: &str,
            _messages: Vec<ChatMessage>,
            _response_format: Option<ResponseFormat>,
            tx: mpsc::Sender<StreamEvent>,
            _cancel_rx: watch::Receiver<bool>,
        ) -> anyhow::Result<()> {
            let chunk = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_default();
            let _ = tx.send(StreamEvent::TextDelta(chunk)).await;
            let _ = tx.send(StreamEvent::Done).await;
            Ok(())
        }

        async fn stream_chat_with_effort(
            &self,
            model: &str,
            messages: Vec<ChatMessage>,
            response_format: Option<ResponseFormat>,
            _reasoning_effort: Option<tauqe_protocol::ReasoningEffort>,
            tx: mpsc::Sender<StreamEvent>,
            cancel_rx: watch::Receiver<bool>,
        ) -> anyhow::Result<()> {
            self.stream_chat(model, messages, response_format, tx, cancel_rx)
                .await
        }

        async fn stream_text(
            &self,
            _model: &str,
            _messages: Vec<ChatMessage>,
            tx: mpsc::Sender<StreamEvent>,
            _cancel_rx: watch::Receiver<bool>,
        ) -> anyhow::Result<()> {
            let _ = tx.send(StreamEvent::Done).await;
            Ok(())
        }
    }

    fn init_repo_with_target() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path().to_path_buf();
        crate::git::init_repository(Some(&repo_root), true).unwrap();
        std::fs::write(
            repo_root.join("target.rs"),
            "pub fn compute() -> i32 { 0 }\n",
        )
        .unwrap();
        crate::git::cmd::stage_files(&repo_root, &["target.rs".to_string()]).unwrap();
        crate::git::cmd::commit_with_developer_author(&repo_root, "Initial commit", false).unwrap();
        (dir, repo_root)
    }

    #[test]
    fn test_loop_names() {
        let answer_loop = AnswerEditLoop::new("prompt");
        assert_eq!(answer_loop.name(), "answer_edit_loop");

        let edit_loop = EditLoop::new("prompt");
        assert_eq!(edit_loop.name(), "edit_loop");
    }

    #[tokio::test]
    async fn test_answer_edit_loop_accepts_answer_without_edits() {
        let (_dir, repo_root) = init_repo_with_target();
        let mut context = ContextManager::new(repo_root.clone());
        context
            .add_file("target.rs", ContextAccess::Editable)
            .unwrap();
        let mut history = HistoryManager::new(repo_root.clone());
        let protocol = crate::prompt::xml::XmlPromptProtocol;
        let protocol = protocol.with_turn_marker("MOCK").unwrap();

        let provider = MockScriptProvider::new(vec![
            "The function compute currently returns 0; no changes are required.".to_string(),
        ]);
        let model = ModelRef::openrouter("test/model");
        let (tx, _rx) = mpsc::channel(100);
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let options = WorkflowOptions::default();
        let mut policy = DirectApplyPolicy::new();

        let mut env = TaskEnv {
            intent: tauqe_protocol::TurnIntent::InteractiveRepl,
            provider: &provider,
            model: &model,
            context: &mut context,
            history: &mut history,
            protocol: protocol.as_ref(),
            policy: &mut policy,
            workflow_name: "git",
            stream_tx: tx,
            cancel_rx,
            options: &options,
        };

        let task = AnswerEditLoop::new("Explain target.rs");
        let out = task.execute(&mut env).await.unwrap();

        assert!(!out.is_cancelled);
        assert!(matches!(out.result, ModelResult::Answer { .. }));
    }

    #[tokio::test]
    async fn test_edit_loop_retries_until_edits_present() {
        let (_dir, repo_root) = init_repo_with_target();
        let mut context = ContextManager::new(repo_root.clone());
        context
            .add_file("target.rs", ContextAccess::Editable)
            .unwrap();
        let mut history = HistoryManager::new(repo_root.clone());
        let protocol = crate::prompt::xml::XmlPromptProtocol;
        let protocol = protocol.with_turn_marker("MOCK").unwrap();

        let no_edit = "Here is an explanation with no edits at all.".to_string();
        let with_edit = "<tauqe_edits_MOCK summary=\"Fix compute\">\n<edit_MOCK path=\"target.rs\">\n<search_MOCK>\npub fn compute() -> i32 { 0 }\n</search_MOCK>\n<replace_MOCK>\npub fn compute() -> i32 { 42 }\n</replace_MOCK>\n</edit_MOCK>\n</tauqe_edits_MOCK>".to_string();

        let provider = MockScriptProvider::new(vec![no_edit, with_edit]);
        let model = ModelRef::openrouter("test/model");
        let (tx, _rx) = mpsc::channel(100);
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let options = WorkflowOptions::default();
        let mut policy = DirectApplyPolicy::new();

        let mut env = TaskEnv {
            intent: tauqe_protocol::TurnIntent::InteractiveRepl,
            provider: &provider,
            model: &model,
            context: &mut context,
            history: &mut history,
            protocol: protocol.as_ref(),
            policy: &mut policy,
            workflow_name: "git",
            stream_tx: tx,
            cancel_rx,
            options: &options,
        };

        let task = EditLoop::new("Change compute to return 42");
        let out = task.execute(&mut env).await.unwrap();

        match &out.result {
            ModelResult::Edit { edits, .. } => assert!(!edits.is_empty()),
            other => panic!("expected edits after retry, got {:?}", other),
        }
        assert!(out.assistant_text.contains("Edit Retry 1/3"));
    }

    #[tokio::test]
    async fn test_edit_loop_retry_records_single_turn_and_response() {
        let (_dir, repo_root) = init_repo_with_target();
        let mut context = ContextManager::new(repo_root.clone());
        context
            .add_file("target.rs", ContextAccess::Editable)
            .unwrap();
        let mut history = HistoryManager::new(repo_root.clone());
        let protocol = crate::prompt::xml::XmlPromptProtocol;
        let protocol = protocol.with_turn_marker("MOCK").unwrap();

        let no_edit = "Here is an explanation with no edits at all.".to_string();
        let with_edit = "<tauqe_edits_MOCK summary=\"Fix compute\">\n<edit_MOCK path=\"target.rs\">\n<search_MOCK>\npub fn compute() -> i32 { 0 }\n</search_MOCK>\n<replace_MOCK>\npub fn compute() -> i32 { 42 }\n</replace_MOCK>\n</edit_MOCK>\n</tauqe_edits_MOCK>".to_string();

        let provider = MockScriptProvider::new(vec![no_edit, with_edit]);
        let model = ModelRef::openrouter("test/model");
        let (tx, _rx) = mpsc::channel(100);
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let options = WorkflowOptions::default();
        let mut policy = DirectApplyPolicy::new();

        let mut env = TaskEnv {
            intent: tauqe_protocol::TurnIntent::InteractiveRepl,
            provider: &provider,
            model: &model,
            context: &mut context,
            history: &mut history,
            protocol: protocol.as_ref(),
            policy: &mut policy,
            workflow_name: "git",
            stream_tx: tx,
            cancel_rx,
            options: &options,
        };

        let task = EditLoop::new("Change compute to return 42");
        let out = task.execute(&mut env).await.unwrap();

        assert!(matches!(out.result, ModelResult::Edit { ref edits, .. } if !edits.is_empty()));
        assert!(out.assistant_text.contains("Edit Retry 1/3"));

        let entries = history.get_entries().unwrap();
        let turns = entries
            .iter()
            .filter(|entry| matches!(entry, crate::history::entry::HistoryEntry::Turn { .. }))
            .count();
        let responses = entries
            .iter()
            .filter(|entry| matches!(entry, crate::history::entry::HistoryEntry::Response { .. }))
            .count();
        assert_eq!(turns, 1, "retries must not record extra user turns");
        assert_eq!(responses, 1, "retries must not record extra responses");
    }

    #[tokio::test]
    async fn test_edit_loop_accepts_immediate_edits_without_retry() {
        let (_dir, repo_root) = init_repo_with_target();
        let mut context = ContextManager::new(repo_root.clone());
        context
            .add_file("target.rs", ContextAccess::Editable)
            .unwrap();
        let mut history = HistoryManager::new(repo_root.clone());
        let protocol = crate::prompt::xml::XmlPromptProtocol;
        let protocol = protocol.with_turn_marker("MOCK").unwrap();

        let with_edit = "<tauqe_edits_MOCK summary=\"Fix compute\">\n<edit_MOCK path=\"target.rs\">\n<search_MOCK>\npub fn compute() -> i32 { 0 }\n</search_MOCK>\n<replace_MOCK>\npub fn compute() -> i32 { 42 }\n</replace_MOCK>\n</edit_MOCK>\n</tauqe_edits_MOCK>".to_string();

        let provider = MockScriptProvider::new(vec![with_edit]);
        let model = ModelRef::openrouter("test/model");
        let (tx, _rx) = mpsc::channel(100);
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let options = WorkflowOptions::default();
        let mut policy = DirectApplyPolicy::new();

        let mut env = TaskEnv {
            intent: tauqe_protocol::TurnIntent::InteractiveRepl,
            provider: &provider,
            model: &model,
            context: &mut context,
            history: &mut history,
            protocol: protocol.as_ref(),
            policy: &mut policy,
            workflow_name: "git",
            stream_tx: tx,
            cancel_rx,
            options: &options,
        };

        let task = EditLoop::new("Change compute to return 42");
        let out = task.execute(&mut env).await.unwrap();

        assert!(matches!(out.result, ModelResult::Edit { ref edits, .. } if !edits.is_empty()));
        assert!(!out.assistant_text.contains("Edit Retry"));
    }

    #[tokio::test]
    async fn test_edit_loop_does_not_retry_on_semantic_blocker() {
        let (_dir, repo_root) = init_repo_with_target();
        let mut context = ContextManager::new(repo_root.clone());
        context
            .add_file("target.rs", ContextAccess::Editable)
            .unwrap();
        let mut history = HistoryManager::new(repo_root.clone());
        let protocol = crate::prompt::xml::XmlPromptProtocol;
        let protocol = protocol.with_turn_marker("MOCK").unwrap();

        let blocked = "<blocked_MOCK reason=\"Precondition violated: table missing\" />";
        let with_edit = "<tauqe_edits_MOCK summary=\"Fix compute\">\n<edit_MOCK path=\"target.rs\">\n<search_MOCK>\npub fn compute() -> i32 { 0 }\n</search_MOCK>\n<replace_MOCK>\npub fn compute() -> i32 { 42 }\n</replace_MOCK>\n</edit_MOCK>\n</tauqe_edits_MOCK>";
        let provider = MockScriptProvider::new(vec![blocked.to_string(), with_edit.to_string()]);
        let model = ModelRef::openrouter("test/model");
        let (tx, _rx) = mpsc::channel(100);
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let options = WorkflowOptions::default();
        let mut policy = DirectApplyPolicy::new();

        let mut env = TaskEnv {
            intent: tauqe_protocol::TurnIntent::InteractiveRepl,
            provider: &provider,
            model: &model,
            context: &mut context,
            history: &mut history,
            protocol: protocol.as_ref(),
            policy: &mut policy,
            workflow_name: "git",
            stream_tx: tx,
            cancel_rx,
            options: &options,
        };

        let task = EditLoop::new("Refactor compute");
        let out = task.execute(&mut env).await.unwrap();

        assert_eq!(
            out.blocked.as_deref(),
            Some("Precondition violated: table missing")
        );
        assert!(
            !out.assistant_text.contains("Edit Retry"),
            "a semantic block must never trigger a corrective edit retry"
        );
        assert_eq!(
            provider.remaining(),
            1,
            "a semantic block must not consume a retry attempt"
        );
        assert_eq!(
            std::fs::read_to_string(repo_root.join("target.rs")).unwrap(),
            "pub fn compute() -> i32 { 0 }\n"
        );
    }

    #[tokio::test]
    async fn test_edit_loop_does_not_retry_on_terminal_error() {
        let (_dir, repo_root) = init_repo_with_target();
        let mut context = ContextManager::new(repo_root.clone());
        context
            .add_file("target.rs", ContextAccess::Editable)
            .unwrap();
        let mut history = HistoryManager::new(repo_root.clone());
        let protocol = crate::prompt::xml::XmlPromptProtocol;
        let protocol = protocol.with_turn_marker("MOCK").unwrap();

        let error = "<error_MOCK message=\"Missing dependency\" />";
        let with_edit = "<tauqe_edits_MOCK summary=\"Fix compute\">\n<edit_MOCK path=\"target.rs\">\n<search_MOCK>\npub fn compute() -> i32 { 0 }\n</search_MOCK>\n<replace_MOCK>\npub fn compute() -> i32 { 42 }\n</replace_MOCK>\n</edit_MOCK>\n</tauqe_edits_MOCK>";
        let provider = MockScriptProvider::new(vec![error.to_string(), with_edit.to_string()]);
        let model = ModelRef::openrouter("test/model");
        let (tx, _rx) = mpsc::channel(100);
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let options = WorkflowOptions::default();
        let mut policy = DirectApplyPolicy::new();

        let mut env = TaskEnv {
            intent: tauqe_protocol::TurnIntent::InteractiveRepl,
            provider: &provider,
            model: &model,
            context: &mut context,
            history: &mut history,
            protocol: protocol.as_ref(),
            policy: &mut policy,
            workflow_name: "git",
            stream_tx: tx,
            cancel_rx,
            options: &options,
        };

        let task = EditLoop::new("Refactor compute");
        let out = task.execute(&mut env).await.unwrap();

        match &out.result {
            ModelResult::Edit { edits, outcome, .. } => {
                assert!(edits.is_empty());
                assert!(!outcome.applied);
                assert_eq!(outcome.error.as_deref(), Some("Missing dependency"));
            }
            other => panic!("expected a terminal error outcome, got {:?}", other),
        }
        assert!(
            !out.assistant_text.contains("Edit Retry"),
            "a terminal error must never trigger a corrective edit retry"
        );
        assert_eq!(
            provider.remaining(),
            1,
            "a terminal error must not consume a retry attempt"
        );
    }

    /// A plan mutation the storage engine refuses during a corrective retry is
    /// a diagnostic the merged outcome must carry: it reaches the recorded
    /// history, the turn result, and the developer-facing stream.
    #[tokio::test]
    async fn test_edit_loop_retry_keeps_the_retried_plan_rejection() {
        let (_dir, repo_root) = init_repo_with_target();
        // An established plan makes the corrective round's `save` an explicit
        // mutation of stored work, which Develop mode refuses. The turn regime
        // authorizes plan creation rather than plan update, so `save` is the
        // payload a corrective round can actually emit and have rejected.
        let existing = tauqe_protocol::Plan {
            id: "develop-plan".to_string(),
            title: "Develop Plan".to_string(),
            items: vec![tauqe_protocol::PlanItem {
                id: "1".to_string(),
                title: "Step 1".to_string(),
                status: tauqe_protocol::PlanItemStatus::Todo,
                ..Default::default()
            }],
            ..Default::default()
        };
        crate::plan::storage::PlanStorage::save_plan(&repo_root, &existing)
            .expect("seed the established plan");

        let mut context = ContextManager::new(repo_root.clone());
        context
            .add_file("target.rs", ContextAccess::Editable)
            .unwrap();
        let mut history = HistoryManager::new(repo_root.clone());
        let protocol = crate::prompt::xml::XmlPromptProtocol;
        let protocol = protocol.with_turn_marker("MOCK").unwrap();

        let no_edit = "The helper already looks correct; no changes are required.".to_string();
        let with_edit_and_rejected_plan = concat!(
            "<tauqe_edits_MOCK summary=\"Fix compute\">\n",
            "<edit_MOCK path=\"target.rs\">\n",
            "<search_MOCK>\npub fn compute() -> i32 { 0 }\n</search_MOCK>\n",
            "<replace_MOCK>\npub fn compute() -> i32 { 42 }\n</replace_MOCK>\n",
            "</edit_MOCK>\n",
            "</tauqe_edits_MOCK>\n",
            "<plan_MOCK action=\"save\" id=\"develop-plan\" title=\"Develop Plan\">\n",
            "<concept_MOCK>\n",
            "<concept_item_MOCK id=\"c-1\" title=\"Retry invariant\" />\n",
            "</concept_MOCK>\n",
            "<item_MOCK id=\"1\" status=\"done\" />\n",
            "</plan_MOCK>"
        )
        .to_string();

        let provider = MockScriptProvider::new(vec![no_edit, with_edit_and_rejected_plan]);
        let model = ModelRef::openrouter("test/model");
        let (tx, mut rx) = mpsc::channel(256);
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let options = WorkflowOptions::default();
        let mut policy = DirectApplyPolicy::new();

        // Drain the stream concurrently: the sender lives on the environment,
        // which is released at the end of the block, so collecting afterwards
        // would race the channel shutdown.
        let collector = tokio::spawn(async move {
            let mut streamed = String::new();
            while let Some(event) = rx.recv().await {
                if let StreamEvent::TextDelta(delta) = event {
                    streamed.push_str(&delta);
                }
            }
            streamed
        });

        let out = {
            let mut env = TaskEnv {
                intent: tauqe_protocol::TurnIntent::InteractiveRepl,
                provider: &provider,
                model: &model,
                context: &mut context,
                history: &mut history,
                protocol: protocol.as_ref(),
                policy: &mut policy,
                workflow_name: "git",
                stream_tx: tx,
                cancel_rx,
                options: &options,
            };
            let task = EditLoop::new("Change compute to return 42");
            task.execute(&mut env).await.unwrap()
        };

        assert!(
            out.assistant_text.contains("Edit Retry 1/3"),
            "the corrective round must run: {}",
            out.assistant_text
        );
        assert!(
            matches!(&out.result, ModelResult::Edit { outcome, .. } if outcome.applied),
            "the corrective round must apply its edits: {:?}",
            out.result
        );
        assert_eq!(
            provider.remaining(),
            0,
            "both scripted rounds must be spent"
        );

        // The merged outcome carries the rejection the corrective round produced.
        assert_eq!(out.plan_errors.len(), 1, "{:?}", out.plan_errors);
        assert!(
            out.plan_errors[0].contains("Develop mode"),
            "{:?}",
            out.plan_errors
        );
        let notice = out
            .plan_rejection_notice()
            .expect("a rejected mutation must produce a notice");
        assert!(notice.contains("Develop mode"), "{notice}");

        // The diagnostic reaches the recorded history, so the next round's
        // prompt carries the exact cause the model must repair.
        let messages: Vec<String> = history
            .get_entries()
            .unwrap()
            .iter()
            .filter_map(|entry| match entry {
                crate::history::entry::HistoryEntry::Response { message, .. } => {
                    Some(message.clone())
                }
                _ => None,
            })
            .collect();
        assert!(
            messages
                .iter()
                .any(|message| message.contains("Plan mutation rejected")
                    && message.contains("Develop mode")),
            "the rejection must reach the recorded history: {messages:?}"
        );

        // And the developer sees it on the turn stream.
        let streamed = collector.await.expect("the collector joins");
        assert!(
            streamed.contains("Plan mutation rejected"),
            "the rejection must surface in the turn stream: {streamed:?}"
        );
    }

    /// `merge_output` folds every field of the retried outcome. The helper
    /// destructures [`FsmTurnOutcome`] exhaustively, so a field added later
    /// cannot be dropped silently, and this checklist names each field the
    /// retry must carry into the merged outcome.
    #[test]
    fn merge_output_folds_every_field_of_the_retried_outcome() {
        // The first attempt and the retry carry deliberately distinct values,
        // so every assertion below proves the corresponding field was folded.
        let mut target = FsmTurnOutcome {
            assistant_text: "first attempt text".to_string(),
            result: ModelResult::Answer {
                text: "first attempt text".to_string(),
            },
            is_cancelled: false,
            blocked: None,
            has_terminal_success: false,
            plan_saved: false,
            plan_errors: vec!["first attempt rejection".to_string()],
            skip_reason: None,
            abort: None,
        };
        let retried = FsmTurnOutcome {
            assistant_text: "retried attempt text".to_string(),
            result: ModelResult::Answer {
                text: "retried attempt text".to_string(),
            },
            is_cancelled: true,
            blocked: Some("blocked on the retried attempt".to_string()),
            has_terminal_success: true,
            plan_saved: true,
            plan_errors: vec!["retried attempt rejection".to_string()],
            skip_reason: Some("skipped on the retried attempt".to_string()),
            abort: Some(AbortReport {
                reason: "host capability missing".to_string(),
                recommendation: "install the missing package".to_string(),
            }),
        };

        merge_output(&mut target, retried, "\n\n--- retry ---\n\n");

        assert_eq!(
            target.assistant_text,
            "first attempt text\n\n--- retry ---\n\nretried attempt text"
        );
        match &target.result {
            ModelResult::Answer { text } => assert_eq!(text, "retried attempt text"),
            other => panic!("the retried result must be folded, got {other:?}"),
        }
        assert!(target.is_cancelled);
        assert_eq!(
            target.blocked.as_deref(),
            Some("blocked on the retried attempt")
        );
        assert!(target.has_terminal_success);
        assert!(target.plan_saved);
        assert_eq!(
            target.plan_errors,
            vec![
                "first attempt rejection".to_string(),
                "retried attempt rejection".to_string()
            ],
            "each attempt's rejections are independent and both must survive"
        );
        assert_eq!(
            target.skip_reason.as_deref(),
            Some("skipped on the retried attempt")
        );
        let abort = target
            .abort
            .expect("the retried abort report must be folded");
        assert_eq!(abort.reason, "host capability missing");
        assert_eq!(abort.recommendation, "install the missing package");
    }
}
