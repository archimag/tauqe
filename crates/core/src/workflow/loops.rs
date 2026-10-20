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
                        let _ = env.history.rollback_entry(id);
                    }
                } else {
                    record_session_response(env.history, &output);
                }
                Ok(output)
            }
            Err(err) => {
                if let Some(id) = turn_id {
                    let _ = env.history.rollback_entry(id);
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

fn merge_output(target: &mut FsmTurnOutcome, next: FsmTurnOutcome, banner: &str) {
    target.assistant_text = format!(
        "{}{}{}",
        target.assistant_text.trim_end(),
        banner,
        next.assistant_text.trim_start()
    );
    target.result = next.result;
    target.is_cancelled = next.is_cancelled;
    target.blocked = next.blocked;
    target.has_terminal_success = next.has_terminal_success;
    target.plan_saved = next.plan_saved;
    target.skip_reason = next.skip_reason;
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
    use crate::workflow::policy::DirectApplyPolicy;
    use crate::workflow::WorkflowOptions;

    use super::{AnswerEditLoop, EditLoop, Task, TaskEnv};

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
}
