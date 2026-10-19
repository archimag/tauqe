//! Typed FSM tasks and the shared turn environment.
//!
//! A [`Task`] is the minimal unit of model interaction inside a bounded turn:
//! it runs the shared turn pipeline against a borrowed [`TaskEnv`]. The
//! individual FSM phases (discovery, staging, verification, healing) are
//! orchestrated by that pipeline, so a task declares only its identity and its
//! acceptance contract. Keeping the phases inside one pipeline preserves a
//! single source of truth for discovery, staging, and healing while the tasks
//! stay declarative.

use async_trait::async_trait;
use tauqe_protocol::{ModelRef, TurnIntent};
use tokio::sync::{mpsc, watch};

use crate::context::ContextManager;
use crate::history::HistoryManager;
use crate::prompt::protocol::PromptProtocol;
use crate::providers::{LlmProvider, StreamEvent};
use crate::workflow::fsm::FsmTurnOutcome;
use crate::workflow::policy::ApplyPolicy;
use crate::workflow::WorkflowOptions;

/// Borrowed bundle of collaborators shared by a task and the turn pipeline.
///
/// Passing a single environment keeps task signatures stable as the number of
/// collaborators grows and lets the pipeline stay decoupled from the task
/// definitions.
pub struct TaskEnv<'a> {
    /// Semantic mode of the bounded turn, declared by the caller that starts it.
    ///
    /// The task never re-derives the mode from the prompt text: prompt
    /// assembly, plan mutation, and verification gating all read this value.
    pub intent: TurnIntent,
    /// Model transport used for every interaction of the task.
    pub provider: &'a dyn LlmProvider,
    /// Model reference resolved for the current turn.
    pub model: &'a ModelRef,
    /// Editable context layer driving prompt assembly and patch validation.
    pub context: &'a mut ContextManager,
    /// Session history used for compaction and turn recording.
    pub history: &'a mut HistoryManager,
    /// Protocol decoding the model output into normalized commands.
    pub protocol: &'a dyn PromptProtocol,
    /// Policy defining how edits are applied and committed to the workspace.
    pub policy: &'a mut dyn ApplyPolicy,
    /// Workflow name reported in diagnostics and prompt assembly.
    pub workflow_name: &'a str,
    /// Stream channel forwarding UI events to the client.
    pub stream_tx: mpsc::Sender<StreamEvent>,
    /// Cancellation signal observed by long-running phases.
    pub cancel_rx: watch::Receiver<bool>,
    /// Token and retry budgets applied to the turn.
    pub options: &'a WorkflowOptions,
}

/// A typed FSM task: the minimal unit of model interaction inside a turn.
///
/// A task runs the shared turn pipeline against a [`TaskEnv`] and reports a
/// stable identifier for diagnostics and streaming events. Concrete tasks
/// (answer/edit loops) are built on top of this contract.
#[async_trait]
pub trait Task: Send + Sync {
    /// Stable task identifier used in diagnostics and streaming events.
    fn name(&self) -> &'static str;

    /// Runs the task against the shared turn environment.
    async fn execute(&self, env: &mut TaskEnv<'_>) -> anyhow::Result<FsmTurnOutcome>;
}
