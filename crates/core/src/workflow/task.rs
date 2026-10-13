//! Typed FSM tasks and the modular steps from which their turn cycles are built.
//!
//! A [`Task`] is the minimal unit of model interaction inside a bounded turn:
//! it drives a small finite-state machine through discovery, staging,
//! verification, and healing phases. Each phase is implemented by a reusable
//! [`TaskStep`], and a [`TurnCycle`] assembles those steps into an ordered
//! execution plan.
//!
//! Keeping every phase as an independent step lets the same cycle machinery
//! serve answer/edit loops and structured exchanges without duplicating
//! control flow, and keeps the transaction and transport layers out of the
//! task definitions.

use async_trait::async_trait;
use tokio::sync::{mpsc, watch};
use tauqe_protocol::{ModelRef, TurnPhase};

use crate::context::ContextManager;
use crate::prompt::protocol::PromptProtocol;
use crate::history::HistoryManager;
use crate::providers::{LlmProvider, StreamEvent};
use crate::workflow::fsm::FsmTurnOutcome;
use crate::workflow::policy::ApplyPolicy;
use crate::workflow::WorkflowOptions;

/// A single modular phase of a bounded turn.
///
/// Steps are the composable building blocks of a [`TurnCycle`]. Each step owns
/// exactly one FSM phase and reports a stable identifier for diagnostics and
/// streaming events, so the same phase can be reused across different tasks.
pub trait TaskStep: Send + Sync {
    /// FSM phase implemented by this step.
    fn phase(&self) -> TurnPhase;

    /// Stable step identifier used in diagnostics and streaming events.
    fn name(&self) -> &'static str;

    /// Whether the step may be entered more than once within a single turn.
    ///
    /// Discovery, staging, and healing are retriable phases: the model may be
    /// asked to request more context, correct failed patches, or repair
    /// compiler errors. Verification itself runs once per attempt.
    fn repeats(&self) -> bool {
        false
    }
}

/// Discovery phase: reconciles model-requested context against the repository.
#[derive(Debug, Default, Clone, Copy)]
pub struct DiscoveryStep;

impl TaskStep for DiscoveryStep {
    fn phase(&self) -> TurnPhase {
        TurnPhase::Discovery
    }

    fn name(&self) -> &'static str {
        "discovery"
    }

    fn repeats(&self) -> bool {
        true
    }
}

/// Staging phase: validates proposed edits in memory before they reach disk.
#[derive(Debug, Default, Clone, Copy)]
pub struct StagingStep;

impl TaskStep for StagingStep {
    fn phase(&self) -> TurnPhase {
        TurnPhase::Staging
    }

    fn name(&self) -> &'static str {
        "staging"
    }

    fn repeats(&self) -> bool {
        true
    }
}

/// Verification phase: runs the project toolchain against staged changes.
#[derive(Debug, Default, Clone, Copy)]
pub struct VerificationStep;

impl TaskStep for VerificationStep {
    fn phase(&self) -> TurnPhase {
        TurnPhase::Verification
    }

    fn name(&self) -> &'static str {
        "verification"
    }
}

/// Healing phase: feeds toolchain failures back to the model for repair.
#[derive(Debug, Default, Clone, Copy)]
pub struct HealingStep;

impl TaskStep for HealingStep {
    fn phase(&self) -> TurnPhase {
        TurnPhase::Healing
    }

    fn name(&self) -> &'static str {
        "healing"
    }

    fn repeats(&self) -> bool {
        true
    }
}

/// Ordered composition of [`TaskStep`]s describing a task's FSM cycle.
#[derive(Default)]
pub struct TurnCycle {
    steps: Vec<Box<dyn TaskStep>>,
}

impl TurnCycle {
    /// Creates an empty cycle.
    pub fn new() -> Self {
        Self { steps: Vec::new() }
    }

    /// Creates the standard discovery -> staging -> verification -> healing cycle.
    pub fn standard() -> Self {
        Self::new()
            .with(DiscoveryStep)
            .with(StagingStep)
            .with(VerificationStep)
            .with(HealingStep)
    }

    /// Appends a step to the cycle, returning the updated cycle.
    pub fn with(mut self, step: impl TaskStep + 'static) -> Self {
        self.steps.push(Box::new(step));
        self
    }

    /// Number of steps in the cycle.
    pub fn len(&self) -> usize {
        self.steps.len()
    }

    /// Whether the cycle contains no steps.
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// FSM phases of the cycle in execution order.
    pub fn phases(&self) -> Vec<TurnPhase> {
        self.steps.iter().map(|step| step.phase()).collect()
    }
}

/// Borrowed bundle of collaborators shared by a task and its steps.
///
/// Passing a single environment keeps task signatures stable as the number of
/// collaborators grows and lets steps stay decoupled from one another.
pub struct TaskEnv<'a> {
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
/// A task declares the [`TurnCycle`] it drives and executes that cycle against a
/// shared [`TaskEnv`]. Concrete tasks (answer/edit loops, structured exchanges)
/// are built by composing the base steps declared in this module.
#[async_trait]
pub trait Task: Send + Sync {
    /// Stable task identifier used in diagnostics and streaming events.
    fn name(&self) -> &'static str;

    /// Ordered FSM cycle this task drives.
    fn cycle(&self) -> TurnCycle;

    /// Runs the task against the shared turn environment.
    async fn execute(&self, env: &mut TaskEnv<'_>) -> anyhow::Result<FsmTurnOutcome>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_turn_cycle_empty_has_no_phases() {
        let cycle = TurnCycle::new();
        assert!(cycle.is_empty());
        assert_eq!(cycle.len(), 0);
        assert!(cycle.phases().is_empty());
    }

    #[test]
    fn test_turn_cycle_standard_phases_in_order() {
        let cycle = TurnCycle::standard();
        assert_eq!(cycle.len(), 4);
        assert!(!cycle.is_empty());

        let phases = cycle.phases();
        assert_eq!(phases.len(), 4);
        assert!(matches!(phases[0], TurnPhase::Discovery));
        assert!(matches!(phases[1], TurnPhase::Staging));
        assert!(matches!(phases[2], TurnPhase::Verification));
        assert!(matches!(phases[3], TurnPhase::Healing));
    }

    #[test]
    fn test_base_steps_report_phase_and_name() {
        assert!(matches!(DiscoveryStep.phase(), TurnPhase::Discovery));
        assert_eq!(DiscoveryStep.name(), "discovery");

        assert!(matches!(StagingStep.phase(), TurnPhase::Staging));
        assert_eq!(StagingStep.name(), "staging");

        assert!(matches!(VerificationStep.phase(), TurnPhase::Verification));
        assert_eq!(VerificationStep.name(), "verification");

        assert!(matches!(HealingStep.phase(), TurnPhase::Healing));
        assert_eq!(HealingStep.name(), "healing");
    }

    #[test]
    fn test_base_steps_repeat_semantics() {
        assert!(DiscoveryStep.repeats());
        assert!(StagingStep.repeats());
        assert!(!VerificationStep.repeats());
        assert!(HealingStep.repeats());
    }

    #[test]
    fn test_turn_cycle_accepts_custom_step() {
        struct ProposalStep;

        impl TaskStep for ProposalStep {
            fn phase(&self) -> TurnPhase {
                TurnPhase::Proposal
            }

            fn name(&self) -> &'static str {
                "proposal"
            }
        }

        let cycle = TurnCycle::new().with(ProposalStep).with(VerificationStep);
        assert_eq!(cycle.len(), 2);

        let phases = cycle.phases();
        assert!(matches!(phases[0], TurnPhase::Proposal));
        assert!(matches!(phases[1], TurnPhase::Verification));
    }
}
