//! Retained supervision for a direct technical TURN, without a file command worker.

use super::super::state::TurnState;
use super::{TurnInput, TurnOutcome, TurnWorkflow};
use crate::api::OperationGuard;
use crate::application::context::ContextSession;
use crate::application::operation::PublicationHold;
use crate::workflows::turn::RoundError;
use std::path::PathBuf;
use tauqe_core::context::ContextManager;
use tauqe_core::edits::transaction::RollbackOutcome;
use tauqe_core::execution::task::{self, TaskOutcome};
use tauqe_core::execution::CancellationToken;
use tauqe_core::execution::{EventSink, ReadyPromptExecutor};
use tauqe_core::prompt::TurnBrief;
use tauqe_core::workflow::policy::owned::{OwnedApplyPolicy, Settlement};
use tauqe_core::workflow::policy::ApplyPolicy;

/// One terminal technical report; consumers do not join a separate disk receipt.
pub(crate) struct TurnContinuation {
    pub state: TurnState,
    pub executor: ReadyPromptExecutor,
}

/// The settled file transaction and the computation's optional continuation.
pub(crate) struct TechnicalTurnReport {
    pub outcome: Result<TurnOutcome, RoundError>,
    pub continuation: Option<TurnContinuation>,
    pub context: ContextManager,
    pub settlement: Settlement,
    pub cancelled: bool,
    pub diagnostic: Option<String>,
    pub observed: bool,
}

pub(crate) struct TechnicalObservation {
    pub session: ContextSession,
    pub operation_id: u64,
}

/// Both scenarios supply data; the shared entry executes the same bounded turn.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_technical_turn(
    operation: &OperationGuard,
    root: PathBuf,
    mut state: TurnState,
    mut executor: ReadyPromptExecutor,
    policy: Box<dyn ApplyPolicy>,
    task: String,
    brief: Option<TurnBrief>,
    events: EventSink,
    observation: Option<TechnicalObservation>,
) -> Result<TechnicalTurnReport, String> {
    let hold = operation
        .retain_supervision()
        .ok_or_else(|| "Technical turn requires an active operation".to_owned())?;
    let cancel = operation.hard_cancellation();
    let policy = OwnedApplyPolicy::bind(policy, &root, cancel.clone());
    let fallback = state.context.clone();
    let work_policy = policy.clone();
    let work_cancel = cancel.clone();
    let computation = tokio::spawn(async move {
        let project = executor.project().clone();
        let outcome = TurnWorkflow::run_direct(
            &mut executor,
            &events,
            &work_cancel,
            &project,
            &mut state,
            TurnInput {
                task: &task,
                repo_root: &root,
                brief: brief.as_ref(),
                feedback: None,
                reason: None,
            },
            &work_policy,
        )
        .await;
        (outcome, state, executor)
    });
    supervise_technical_task(computation, policy, cancel, observation, fallback, hold)
        .await
        .map_err(|error| format!("Technical turn supervisor failed: {error}"))
}

/// Install only the computation's abort handle. Spawning the retained supervisor
/// is synchronous, so dropping its waiter cannot abandon transaction settlement.
pub(super) fn supervise_technical_task(
    computation: tokio::task::JoinHandle<(
        Result<TurnOutcome, RoundError>,
        TurnState,
        ReadyPromptExecutor,
    )>,
    policy: OwnedApplyPolicy,
    cancel: CancellationToken,
    observation: Option<TechnicalObservation>,
    fallback: ContextManager,
    hold: PublicationHold,
) -> tokio::task::JoinHandle<TechnicalTurnReport> {
    let task_id = computation.id();
    hold.set_abort_handle(computation.abort_handle());
    tokio::spawn(async move {
        let joined = task::join(computation).await;
        hold.clear_abort_handle(task_id);
        let report = settle_technical_turn(policy, cancel, observation, joined, fallback).await;
        drop(hold);
        report
    })
}

async fn settle_technical_turn(
    policy: OwnedApplyPolicy,
    cancel: CancellationToken,
    observation: Option<TechnicalObservation>,
    joined: TaskOutcome<(
        Result<TurnOutcome, RoundError>,
        TurnState,
        ReadyPromptExecutor,
    )>,
    fallback: ContextManager,
) -> TechnicalTurnReport {
    let (outcome, mut continuation, mut context) = match joined {
        TaskOutcome::Completed((result, state, executor)) => {
            let context = state.context.clone();
            (result, Some(TurnContinuation { state, executor }), context)
        }
        TaskOutcome::Failed(error) => (
            Err(tauqe_core::execution::ExecutionError::Other(format!(
                "The technical turn task failed: {error}"
            ))
            .into()),
            None,
            fallback,
        ),
        TaskOutcome::Cancelled | TaskOutcome::Orphaned => (
            Err(tauqe_core::execution::ExecutionError::Cancelled.into()),
            None,
            fallback,
        ),
    };
    if let Some(observation) = &observation {
        observation
            .session
            .rebind_research_publication(observation.operation_id, &mut context);
    }
    let settlement = policy.finish_interrupted(&mut context).await;
    let (settlement, diagnostic) = match settlement {
        Ok(settlement) => {
            let diagnostic = match &settlement {
                Settlement::RolledBack(rollback) => rollback.diagnostic(),
                _ => None,
            };
            (settlement, diagnostic)
        }
        Err(error) => {
            let diagnostic = format!("File transaction settlement was not confirmed: {error:#}");
            (
                Settlement::RolledBack(RollbackOutcome::Incomplete {
                    policy_error: None,
                    backup_error: diagnostic.clone(),
                }),
                Some(diagnostic),
            )
        }
    };
    let late_cancel = match &settlement {
        Settlement::Finalized { cancelled, .. } | Settlement::NoChanges { cancelled } => *cancelled,
        Settlement::RolledBack(_) => false,
    };
    if let Some(observation) = &observation {
        let applied = matches!(&settlement, Settlement::Finalized { edit, .. } if edit.applied && !edit.changed_files.is_empty());
        if applied {
            tauqe_core::blocking::run_blocking(|| context.clear_transient());
        }
        if let Err(error) = observation
            .session
            .merge_turn_context(context.clone())
            .await
        {
            tracing::warn!(%error, "Failed to merge the step context");
        }
        observation
            .session
            .close_research_publication(observation.operation_id);
        context.set_listener(None);
    }
    if let Some(continuation) = &mut continuation {
        continuation.state.context = context.clone();
    }
    TechnicalTurnReport {
        outcome,
        continuation,
        context,
        settlement,
        cancelled: cancel.is_cancelled() || late_cancel,
        diagnostic,
        observed: observation.is_some(),
    }
}
