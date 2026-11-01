//! Review batch supervisor and execution entry point.
//!
//! `handle_review_execute` resolves the members to run (an architectural
//! group, a single session's findings, a single finding, or every actionable
//! finding across all sessions), performs a deterministic pre-flight
//! verification, and hands control to the core `BatchExecution` supervisor
//! through `ReviewBatchExecutor`. The supervisor enforces ordered traversal;
//! findings repaired before a member stops the queue stay `Fixed`. Once it
//! terminates, a typed `review/batch_finished` event carries the aggregate
//! status, finding counters, and elapsed time, so clients can raise a single
//! notice for the whole batch.
//!
//! A batch member addresses either a finding or a group, and the member
//! identifier is the addressing contract: a group member carries
//! `{session}:g:{group}` and runs the group's unified horizon through
//! `ExecutionScope::ReviewGroup`, while a finding member carries
//! `{session}:{item}` and runs its degenerate one-step horizon through
//! `ExecutionScope::ReviewFinding`. A finding of an architectural group is a
//! symptom of one shared root cause, so it is never executed on its own: an
//! explicit selection that names one is refused, and the implicit queues
//! subsume the findings a group collects into a single group member.
//!
//! The member runner implements nothing of the execution itself. Every step
//! turn, every Architect wave, every ladder rung, and every durable status
//! transition belongs to the engine's unified implementation pipeline
//! (`tauqe_core::workflow::implementation`), which this module drives through
//! `ServerImplementationHost`. The server contributes the batch framing only:
//! it decodes the addressing contract, resolves the scope, and maps the
//! terminal report onto the supervisor's step outcome, so a review run and a
//! plan run cannot drift apart in how they reach a provider, how they escalate
//! a ladder, or when they settle an entity.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use tauqe_core::config::AppConfig;
use tauqe_core::providers::StreamEvent;
use tauqe_core::review::prompt::refine::has_recorded_decision;
use tauqe_core::review::ReviewStorage;
use tauqe_core::workflow::batch::{
    BatchExecution, BatchReport, BatchStep, BatchStepExecutor, BatchStepOutcome, BatchStepStatus,
    BatchStop,
};
use tauqe_core::workflow::implementation::{
    ExecutionScope, PlanImplementationOptions, PlanImplementationPipeline,
    PlanImplementationReport, PlanImplementationStop,
};
use tauqe_protocol::{
    errors, events, BatchFinishStatus, BatchFinishSummary, IssueStatus, IssueType, ModelSelection,
    Request, Response, ResponseError, ReviewBatchFinishedEvent, ReviewExecuteParams, ReviewItem,
    ReviewSession,
};
use tokio::sync::{mpsc, watch};

use crate::handlers::host::ServerImplementationHost;
use crate::handlers::PreflightVerdict;
use crate::state::{AppState, OperationGuard};

use super::common::{clear_auto_context, emit, offload_review};

/// Separator binding the session identifier to the group identifier of a
/// group-member batch target.
const GROUP_STEP_SEPARATOR: &str = ":g:";

/// Encodes the batch-member identifier of one architectural group horizon.
pub(super) fn group_member_id(session_id: &str, group_id: &str) -> String {
    format!("{session_id}{GROUP_STEP_SEPARATOR}{group_id}")
}

/// Decodes a group-member identifier into the session and the group it names.
///
/// A finding member carries no separator and resolves to `None`, so a
/// supervisor routes a queue member without knowing how it was minted. Neither
/// identifier may contain the separator: both are minted by the harness.
fn parse_group_member(member_id: &str) -> Option<(&str, &str)> {
    let (session_id, group_id) = member_id.split_once(GROUP_STEP_SEPARATOR)?;
    if session_id.is_empty() || group_id.is_empty() {
        return None;
    }
    Some((session_id, group_id))
}

/// Decodes the execution scope a batch member addresses.
fn member_scope(member_id: &str) -> Option<ExecutionScope> {
    if let Some((review_id, group_id)) = parse_group_member(member_id) {
        return Some(ExecutionScope::ReviewGroup {
            review_id: review_id.to_string(),
            group_id: group_id.to_string(),
        });
    }
    let (review_id, item_part) = member_id.split_once(':')?;
    let item_id = item_part.parse::<u32>().ok()?;
    Some(ExecutionScope::ReviewFinding {
        review_id: review_id.to_string(),
        item_id,
    })
}

/// Statistics of one review batch run, reported with its terminal outcome.
///
/// The counters separate what a queue achieved from what it merely survived: a
/// repaired finding is counted from the members that completed their horizon, a
/// group postponed by the readiness gate is counted from the resolved queue, and
/// a member whose failure was confined to it is counted from the local skips the
/// supervisor recorded.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct BatchTelemetry {
    /// Findings the batch repaired.
    pub(super) fixed: usize,
    /// Findings postponed until the questions of their group are decided.
    pub(super) deferred: usize,
    /// Members that failed locally while the queue continued.
    pub(super) failed: usize,
}

/// Builds the terminal telemetry of a finished batch run.
fn batch_telemetry(report: &BatchReport, fixed: usize, deferred: usize) -> BatchTelemetry {
    BatchTelemetry {
        fixed,
        deferred,
        failed: report
            .executed
            .iter()
            .filter(|step| step.outcome.status == BatchStepStatus::Skipped)
            .count(),
    }
}

/// Outcome of a server-driven batch of review findings.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum BatchOutcome {
    Completed {
        items: usize,
        telemetry: BatchTelemetry,
    },
    Failed {
        completed: usize,
        member: FailedMember,
        reason: String,
        telemetry: BatchTelemetry,
    },
    Cancelled {
        completed: usize,
        telemetry: BatchTelemetry,
    },
    Stopped {
        completed: usize,
        telemetry: BatchTelemetry,
    },
}

/// The batch member a terminal stop is attributed to.
///
/// A group member and a finding member are addressed by different identifiers,
/// and only a finding identifier carries an audit ordinal. The address is
/// therefore decoded from the identifier the supervisor reports, so a group is
/// named as a group instead of being reported under a finding number that never
/// existed.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum FailedMember {
    /// A single finding of one review session.
    Finding { session_id: String, item_id: u32 },
    /// An architectural group, executed through its unified horizon.
    Group {
        session_id: String,
        group_id: String,
    },
    /// A member identifier that matches neither minted form.
    Unresolved { member_id: String },
}

impl FailedMember {
    /// Decodes the member a stopped batch step belongs to.
    ///
    /// The harness mints exactly two identifier forms — `{session}:g:{group}`
    /// for a group member and `{session}:{item}` for a finding — so the group
    /// form is decoded first and the finding form follows it. An identifier of
    /// neither form keeps its text instead of borrowing an ordinal: a member
    /// the harness never minted has no finding number to report.
    fn decode(step_id: &str) -> Self {
        if let Some((session_id, group_id)) = parse_group_member(step_id) {
            return Self::Group {
                session_id: session_id.to_string(),
                group_id: group_id.to_string(),
            };
        }
        match step_id.split_once(':') {
            Some((session_id, item_part)) => match item_part.parse::<u32>() {
                Ok(item_id) => Self::Finding {
                    session_id: session_id.to_string(),
                    item_id,
                },
                Err(_) => Self::Unresolved {
                    member_id: step_id.to_string(),
                },
            },
            None => Self::Unresolved {
                member_id: step_id.to_string(),
            },
        }
    }
}

impl std::fmt::Display for FailedMember {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Finding {
                session_id,
                item_id,
            } => write!(f, "finding #{} in session '{}'", item_id, session_id),
            Self::Group {
                session_id,
                group_id,
            } => write!(f, "review group '{}' in session '{}'", group_id, session_id),
            Self::Unresolved { member_id } => write!(f, "review member '{}'", member_id),
        }
    }
}

/// Builds a typed protocol error, mirroring `Response::err`.
fn response_error(code: &str, message: impl Into<String>) -> ResponseError {
    ResponseError {
        code: code.to_string(),
        message: message.into(),
        data: None,
    }
}

/// Whether a finding can still produce work in a resolving turn.
fn is_actionable(item: &ReviewItem) -> bool {
    matches!(item.status, IssueStatus::Todo | IssueStatus::InProgress)
}

/// Resolves the architectural group a finding belongs to, if any.
///
/// A `Simple` group collects independent defects, so its findings stay
/// individually executable. Only an `Architectural` group subsumes its
/// findings: they are symptoms of the one root cause its unified horizon
/// resolves, so running one on its own is refused and the group runs instead.
fn architectural_group_of<'a>(session: &ReviewSession, item: &'a ReviewItem) -> Option<&'a str> {
    let group_id = item.group.as_deref()?;
    let architectural = session.groups.iter().any(|group| {
        group.id == group_id && group.kind == tauqe_protocol::review::ReviewGroupKind::Architectural
    });
    architectural.then_some(group_id)
}

/// Collects the open questions an architectural group still poses.
///
/// A finding that is still in `DISCUSSION` without a recorded engineering
/// decision is a question the engineer has not answered yet: running the group
/// would force the harness to answer it, so the group is not ready for an
/// autonomous run.
fn group_open_questions<'a>(session: &'a ReviewSession, group_id: &str) -> Vec<&'a ReviewItem> {
    session
        .items
        .iter()
        .filter(|item| item.group.as_deref() == Some(group_id))
        .filter(|item| item.status == IssueStatus::Discussion)
        .filter(|item| !has_recorded_decision(item))
        .collect()
}

/// Whether an architectural group can be executed without the engineer.
fn group_ready_for_autonomous_run(session: &ReviewSession, group_id: &str) -> bool {
    group_open_questions(session, group_id).is_empty()
}

/// Builds the typed refusal of a group that still poses open questions.
fn group_unresolved_questions_error(group_id: &str, open: &[&ReviewItem]) -> ResponseError {
    let ids = open
        .iter()
        .map(|item| format!("#{}", item.id))
        .collect::<Vec<_>>()
        .join(", ");
    response_error(
        errors::GROUP_UNRESOLVED_QUESTIONS,
        format!(
            "Review group '{}' still poses open question(s) on finding(s) {}: record an engineering decision on them or move them to TODO before executing the group",
            group_id, ids
        ),
    )
}

/// Whether an actionable architecture finding has no architectural group to be
/// planned through.
///
/// `IssueType::Architecture` is a deterministic requirement: such a finding is
/// remediated by a plan, and the only sanctioned carrier of a plan is an
/// architectural group. A finding whose effective group is `Simple`, or which
/// the stored group list does not declare at all, has no plan carrier, so it is
/// never executed as one isolated turn: the post-analysis stage must regroup it
/// first.
fn is_unplanned_architecture_finding(session: &ReviewSession, item: &ReviewItem) -> bool {
    is_actionable(item)
        && item.issue_type == IssueType::Architecture
        && architectural_group_of(session, item).is_none()
}

/// Builds the typed refusal of a finding that must be planned before it is
/// executed.
fn unplanned_architecture_error(item: &ReviewItem) -> ResponseError {
    response_error(
        errors::ARCHITECTURAL_GROUP_EXECUTION_REQUIRED,
        format!(
            "Review item #{} is an ARCHITECTURE finding with no architectural group to plan it; run the review post-analysis so it is planned before execution",
            item.id
        ),
    )
}

/// Renders the refusal of an implicit queue that has nothing to schedule.
fn empty_queue_message(
    scope: &str,
    pending_architecture: usize,
    deferred_findings: usize,
) -> String {
    if deferred_findings > 0 {
        return format!(
            "No executable findings {}: {} finding(s) are deferred because an architectural group still poses an open question; record an engineering decision on those findings or move them to TODO",
            scope, deferred_findings
        );
    }
    if pending_architecture == 0 {
        format!("No TODO findings to execute {}", scope)
    } else {
        format!(
            "No executable findings {}: {} ARCHITECTURE finding(s) have no architectural group to plan them; run the review post-analysis so they are planned before execution",
            scope, pending_architecture
        )
    }
}

/// Resolves a client-ordered selection of finding ids against one session.
fn resolve_session_queue(session: &ReviewSession, item_ids: &[u32]) -> Vec<u32> {
    let mut resolved: Vec<u32> = Vec::new();
    for id in item_ids {
        if resolved.contains(id) {
            continue;
        }
        let actionable = session
            .items
            .iter()
            .any(|item| item.id == *id && is_actionable(item));
        if actionable {
            resolved.push(*id);
        }
    }
    resolved
}

/// One session's contribution to an implicit review batch queue.
struct SessionContribution {
    /// Batch members the session contributes, in client order.
    members: Vec<String>,
    /// Findings the scheduled members address.
    addressed: Vec<u32>,
    /// Findings postponed until the questions of their group are decided.
    deferred_findings: usize,
    /// Architecture findings no architectural group can plan yet.
    pending_architecture: usize,
}

/// Resolves what one session contributes to an implicit queue.
fn session_contribution(session: &ReviewSession) -> SessionContribution {
    let mut contribution = SessionContribution {
        members: Vec::new(),
        addressed: Vec::new(),
        deferred_findings: 0,
        pending_architecture: 0,
    };
    let mut scheduled_groups: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut deferred_groups: std::collections::HashSet<String> = std::collections::HashSet::new();
    for item in session.items.iter().filter(|item| is_actionable(item)) {
        if let Some(group_id) = architectural_group_of(session, item) {
            if deferred_groups.contains(group_id) {
                continue;
            }
            if !group_ready_for_autonomous_run(session, group_id) {
                if deferred_groups.insert(group_id.to_string()) {
                    contribution.deferred_findings += session
                        .items
                        .iter()
                        .filter(|member| member.group.as_deref() == Some(group_id))
                        .count();
                }
                continue;
            }
            let member = group_member_id(&session.id, group_id);
            if scheduled_groups.insert(member.clone()) {
                contribution.members.push(member);
            }
            contribution.addressed.push(item.id);
            continue;
        }
        if is_unplanned_architecture_finding(session, item) {
            contribution.pending_architecture += 1;
            continue;
        }
        contribution
            .members
            .push(format!("{}:{}", session.id, item.id));
        contribution.addressed.push(item.id);
    }
    contribution
}

/// The implicit queue every stored session contributes.
struct ImplicitQueues {
    /// Batch members to run, in stored order.
    members: Vec<String>,
    /// Findings the scheduled members address.
    addressed: Vec<u32>,
    /// Findings postponed until the questions of their group are decided.
    deferred: usize,
    /// Architecture findings no architectural group can plan yet.
    pending_architecture: usize,
}

/// Accumulates the implicit queue of every stored session.
fn accumulate_implicit_queues(sessions: &[ReviewSession]) -> ImplicitQueues {
    let mut queues = ImplicitQueues {
        members: Vec::new(),
        addressed: Vec::new(),
        deferred: 0,
        pending_architecture: 0,
    };
    for session in sessions {
        let contribution = session_contribution(session);
        queues.members.extend(contribution.members);
        queues.addressed.extend(contribution.addressed);
        queues.deferred += contribution.deferred_findings;
        queues.pending_architecture += contribution.pending_architecture;
    }
    queues
}

/// Renders the supervisor diagnostic label of one batch member.
fn member_label(member_id: &str) -> String {
    if let Some((session_id, group_id)) = parse_group_member(member_id) {
        return format!(
            "Fix the root cause of review group '{}' in session '{}'",
            group_id, session_id
        );
    }
    let Some((session_id, item_id)) = member_id.split_once(':') else {
        return format!("Execute review member '{}'", member_id);
    };
    format!(
        "Fix review finding #{} in session '{}'",
        item_id, session_id
    )
}

/// A resolved `review/execute` selection.
#[derive(Debug)]
struct ResolvedQueue {
    /// Batch members to run, in the order the client addressed them.
    members: Vec<String>,
    /// Findings the members address.
    addressed: Vec<u32>,
    /// Findings postponed until the questions of their group are decided.
    deferred: usize,
}

type ResolvedSelection = Result<ResolvedQueue, ResponseError>;

/// Resolves the members a group-scoped request runs.
///
/// A group run addresses the group itself: its unified horizon is the unit of
/// work, and no finding is scheduled beside it. The findings the group collects
/// are the work the run addresses, so they travel back as the addressed ids.
fn resolve_group_selection(
    session: &ReviewSession,
    review_id: &str,
    group_id: &str,
) -> ResolvedSelection {
    if !session.groups.iter().any(|group| group.id == group_id) {
        return Err(response_error(
            errors::NOT_FOUND,
            format!(
                "Review group '{}' not found in session '{}'",
                group_id, review_id
            ),
        ));
    }
    // The readiness gate runs before anything is scheduled: a group that still
    // poses an open question is not executed at all.
    let open_questions = group_open_questions(session, group_id);
    if !open_questions.is_empty() {
        return Err(group_unresolved_questions_error(group_id, &open_questions));
    }
    let item_ids: Vec<u32> = session
        .items
        .iter()
        .filter(|item| item.group.as_deref() == Some(group_id))
        .map(|item| item.id)
        .collect();
    Ok(ResolvedQueue {
        members: vec![group_member_id(review_id, group_id)],
        addressed: item_ids,
        deferred: 0,
    })
}

/// Resolves the members a finding-scoped request runs.
fn resolve_finding_selection(
    session: &ReviewSession,
    review_id: &str,
    params: &ReviewExecuteParams,
) -> ResolvedSelection {
    let member_of = |item_id: u32| format!("{}:{}", review_id, item_id);
    match params.item_id {
        Some(item_id) => {
            if params.item_ids.is_some() {
                return Err(response_error(
                    errors::INVALID_PARAMS,
                    "item_ids cannot be combined with item_id",
                ));
            }
            let item = match session.find_item(item_id) {
                Some(item) => item,
                None => {
                    return Err(response_error(
                        errors::NOT_FOUND,
                        format!(
                            "Review item #{} not found in session '{}'",
                            item_id, review_id
                        ),
                    ));
                }
            };
            if let Some(group_id) = architectural_group_of(session, item) {
                return Err(response_error(
                    errors::ARCHITECTURAL_GROUP_EXECUTION_REQUIRED,
                    format!(
                        "Review item #{} is a symptom of architectural group '{}' and cannot be executed on its own; execute the group instead",
                        item.id, group_id
                    ),
                ));
            }
            if is_unplanned_architecture_finding(session, item) {
                return Err(unplanned_architecture_error(item));
            }
            match item.status {
                IssueStatus::Discussion => Err(response_error(
                    errors::ITEM_IN_DISCUSSION,
                    format!(
                        "Review item #{} is in DISCUSSION status and cannot be executed autonomously until reviewed (change status to TODO first)",
                        item.id
                    ),
                )),
                IssueStatus::Rejected => Err(response_error(
                    errors::ITEM_REJECTED,
                    format!("Review item #{} is REJECTED and cannot be executed", item.id),
                )),
                IssueStatus::Fixed => Err(response_error(
                    errors::NOTHING_TO_EXECUTE,
                    format!("Review item #{} is already FIXED", item.id),
                )),
                IssueStatus::Skipped => Err(response_error(
                    errors::NOTHING_TO_EXECUTE,
                    format!("Review item #{} is already SKIPPED", item.id),
                )),
                IssueStatus::Todo | IssueStatus::InProgress => Ok(ResolvedQueue {
                    members: vec![member_of(item_id)],
                    addressed: vec![item_id],
                    deferred: 0,
                }),
            }
        }
        None => match params.item_ids.as_deref() {
            Some(selected) => {
                for id in selected {
                    if let Some(item) = session.find_item(*id) {
                        if let Some(group_id) = architectural_group_of(session, item) {
                            return Err(response_error(
                                errors::ARCHITECTURAL_GROUP_EXECUTION_REQUIRED,
                                format!(
                                    "Review item #{} is a symptom of architectural group '{}' and cannot be executed on its own; execute the group instead",
                                    item.id, group_id
                                ),
                            ));
                        }
                        if is_unplanned_architecture_finding(session, item) {
                            return Err(unplanned_architecture_error(item));
                        }
                    }
                }
                let ids = resolve_session_queue(session, selected);
                if ids.is_empty() {
                    return Err(response_error(
                        errors::NOTHING_TO_EXECUTE,
                        "No TODO findings to execute in this review session",
                    ));
                }
                let members = ids.iter().map(|id| member_of(*id)).collect();
                Ok(ResolvedQueue {
                    members,
                    addressed: ids,
                    deferred: 0,
                })
            }
            None => {
                let queues = accumulate_implicit_queues(std::slice::from_ref(session));
                if queues.members.is_empty() {
                    return Err(response_error(
                        errors::NOTHING_TO_EXECUTE,
                        empty_queue_message(
                            "in this review session",
                            queues.pending_architecture,
                            queues.deferred,
                        ),
                    ));
                }
                Ok(ResolvedQueue {
                    members: queues.members,
                    addressed: queues.addressed,
                    deferred: queues.deferred,
                })
            }
        },
    }
}

/// Adapter that drives one review batch member through the engine's unified
/// implementation pipeline as a [`BatchStepExecutor`] step.
///
/// The member identifier is the addressing contract: it is minted by
/// `handle_review_execute` and decoded here through the same sanctioned
/// parsers, so a group member never reaches the finding path. The pipeline owns
/// the horizon resolution, the just-in-time Architect synthesis, the ordered
/// leaf walk, the coder ladder, the replanning waves, and the durable status
/// transitions, so the supervisor stays agnostic of review storage.
///
/// The executor also counts the findings the queue repaired, because the
/// terminal telemetry cannot be reconstructed by the supervisor: the pipeline
/// reports the leaves it executed, while a queue is measured in findings.
struct ReviewBatchExecutor<'a> {
    state: &'a Arc<AppState>,
    repo_root: &'a Path,
    operation: &'a Arc<OperationGuard>,
    config: AppConfig,
    model: Option<ModelSelection>,
    instructions: Option<String>,
    /// Findings the completed members repaired.
    fixed_items: usize,
}

impl ReviewBatchExecutor<'_> {
    /// Counts the findings a completed member repaired.
    ///
    /// A group horizon repairs every finding the group collects, so the count is
    /// read from the durable membership instead of being guessed from the number
    /// of leaves; a degenerate finding run repairs exactly its own finding.
    async fn repaired_findings(&self, member_id: &str) -> usize {
        let Some((review_id, group_id)) = parse_group_member(member_id) else {
            return 1;
        };
        let load_root = self.repo_root.to_path_buf();
        let load_id = review_id.to_string();
        let group_id = group_id.to_string();
        offload_review(move || ReviewStorage::load_review(&load_root, &load_id))
            .await
            .ok()
            .flatten()
            .map(|session| {
                session
                    .items
                    .iter()
                    .filter(|item| item.group.as_deref() == Some(group_id.as_str()))
                    .count()
            })
            .unwrap_or(0)
    }

    /// Maps the terminal report of one member onto the supervisor's step outcome.
    ///
    /// The engine settles the entity of every member it ends, so a member that
    /// could not be implemented as stated is a local skip of the queue instead
    /// of a fatal stop: the remaining findings are independent of it. A
    /// cancellation and a failure the engine could not repair stop the queue,
    /// because the next member must never build on a state nobody verified.
    async fn map_report(
        &mut self,
        member_id: &str,
        report: &PlanImplementationReport,
    ) -> BatchStepOutcome {
        match &report.stop {
            PlanImplementationStop::Completed => {
                self.fixed_items += self.repaired_findings(member_id).await;
                BatchStepOutcome::succeeded()
            }
            PlanImplementationStop::Cancelled | PlanImplementationStop::Stopped => {
                BatchStepOutcome::cancelled()
            }
            PlanImplementationStop::Blocked { reason, .. } => {
                BatchStepOutcome::skipped(reason.clone())
            }
            PlanImplementationStop::Failed { reason, .. } => {
                BatchStepOutcome::failed(reason.clone())
            }
        }
    }
}

#[async_trait]
impl BatchStepExecutor for ReviewBatchExecutor<'_> {
    async fn execute_step(
        &mut self,
        step: &BatchStep,
        stream_tx: mpsc::Sender<StreamEvent>,
        cancel_rx: watch::Receiver<bool>,
    ) -> anyhow::Result<BatchStepOutcome> {
        let Some(scope) = member_scope(&step.id) else {
            return Ok(BatchStepOutcome::failed(format!(
                "Invalid review step id: {}",
                step.id
            )));
        };

        // Every member is a whole bounded turn of its own, so it mints its own
        // marker before the pipeline resolves the horizon it executes.
        let marker = self.state.next_turn_marker().await;
        let host = ServerImplementationHost::new(
            Arc::clone(self.state),
            Arc::clone(self.operation),
            self.config.clone(),
            self.repo_root.to_path_buf(),
        );
        let options = PlanImplementationOptions {
            selection: self.model.clone(),
            instructions: self.instructions.clone(),
            marker,
        };
        // The queue itself owns the graceful drain between members, so the
        // member pipeline runs without a soft-stop of its own while the sender
        // stays alive for the whole call.
        let (_soft_stop_tx, soft_stop_rx) = watch::channel(false);
        let pipeline = PlanImplementationPipeline::new(
            self.repo_root,
            scope,
            &host,
            self.config.clone(),
            options,
        );
        let report = pipeline.run(&stream_tx, &cancel_rx, &soft_stop_rx).await;
        Ok(self.map_report(&step.id, &report).await)
    }
}

/// Runs the review members through the core [`BatchExecution`] supervisor with
/// explicit execution options (model override, directives, deferred findings).
///
/// Ordered traversal is preserved; findings repaired before a member stops the
/// queue stay `Fixed`. Once the supervisor terminates, a typed
/// `review/batch_finished` event carries the aggregate status, finding
/// counters, and elapsed time, so clients can raise a single notice for the
/// whole batch.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_review_batch_with_options(
    state: &Arc<AppState>,
    owner_id: &str,
    tasks: Vec<String>,
    _step_id: Option<String>,
    deferred: usize,
    model: Option<ModelSelection>,
    instructions: Option<String>,
    operation: Arc<OperationGuard>,
) -> BatchOutcome {
    let repo_root = state.context.read().await.repo_root().to_path_buf();
    let config = state.config.lock().await.config.clone();
    if owner_id != "all" {
        let save_root = repo_root.clone();
        let owner_owned = owner_id.to_string();
        let _ =
            offload_review(move || ReviewStorage::save_active_id(&save_root, Some(&owner_owned)))
                .await;
    }
    clear_auto_context(state).await;

    let started_at = std::time::Instant::now();

    let steps: Vec<BatchStep> = tasks
        .iter()
        .map(|member_id| BatchStep::new(member_id.clone(), member_label(member_id)))
        .collect();

    let (stream_tx, _stream_rx) = mpsc::channel::<StreamEvent>(1);
    // The whole batch shares one operation and one cancel channel: a cancel
    // delivered between members is honored instead of being lost, and the slot
    // stays registered for the batch's entire lifetime.
    let cancel_rx = operation.cancel_receiver();
    // Soft stop ends the queue gracefully after the current member: the
    // operation owns the signal, and a `model/cancel` request with
    // `mode: "soft"` raises it so the current member finishes its horizon,
    // verification, and commit before the queue stops.
    let soft_stop_rx = operation.soft_stop_receiver();

    let mut executor = ReviewBatchExecutor {
        state,
        repo_root: &repo_root,
        operation: &operation,
        config,
        model,
        instructions,
        fixed_items: 0,
    };

    let report = BatchExecution::new()
        .run(&steps, &mut executor, &stream_tx, &cancel_rx, &soft_stop_rx)
        .await;

    let executed_count = report.executed.len();
    let total_count = report.scheduled;
    let telemetry = batch_telemetry(&report, executor.fixed_items, deferred);
    let fixed_count = telemetry.fixed;
    let failed_count = telemetry.failed;
    let (status, error) = match &report.stop {
        BatchStop::Completed => (BatchFinishStatus::Completed, None),
        BatchStop::Failed { .. } => (
            BatchFinishStatus::Failed,
            report.executed.last().and_then(|r| r.outcome.error.clone()),
        ),
        BatchStop::Blocked { reason, .. } => (BatchFinishStatus::Blocked, Some(reason.clone())),
        BatchStop::Cancelled => (BatchFinishStatus::Cancelled, None),
        BatchStop::Stopped => (BatchFinishStatus::Stopped, None),
    };
    let outcome = match report.stop {
        BatchStop::Completed => BatchOutcome::Completed {
            items: executed_count,
            telemetry,
        },
        BatchStop::Cancelled => BatchOutcome::Cancelled {
            completed: executed_count,
            telemetry,
        },
        BatchStop::Stopped => BatchOutcome::Stopped {
            completed: executed_count,
            telemetry,
        },
        BatchStop::Failed { step_id } => {
            let completed = executed_count.saturating_sub(1);
            let member = FailedMember::decode(&step_id);
            let reason = error
                .clone()
                .unwrap_or_else(|| "batch member failed without diagnostics".to_string());
            BatchOutcome::Failed {
                completed,
                member,
                reason,
                telemetry,
            }
        }
        BatchStop::Blocked { step_id, reason } => {
            let completed = executed_count.saturating_sub(1);
            let member = FailedMember::decode(&step_id);
            BatchOutcome::Failed {
                completed,
                member,
                reason,
                telemetry,
            }
        }
    };

    clear_auto_context(state).await;

    emit(
        &state.out,
        events::REVIEW_BATCH_FINISHED,
        serde_json::to_value(ReviewBatchFinishedEvent {
            operation_id: format!("op-{}", operation.id()),
            summary: BatchFinishSummary {
                owner_id: owner_id.to_string(),
                status,
                executed_count,
                total_count,
                elapsed_seconds: started_at.elapsed().as_secs(),
                error,
                fixed_count: Some(fixed_count),
                deferred_count: Some(deferred),
                failed_count: Some(failed_count),
            },
        })
        .ok(),
    );

    outcome
}

/// Convenience entry point used by tests: runs a batch of findings without a
/// model override or developer directives.
#[cfg(test)]
pub(super) async fn run_review_batch(
    state: &Arc<AppState>,
    owner_id: &str,
    tasks: Vec<(String, u32)>,
    operation: Arc<OperationGuard>,
) -> BatchOutcome {
    let members = tasks
        .into_iter()
        .map(|(session_id, item_id)| format!("{}:{}", session_id, item_id))
        .collect();
    run_review_batch_with_options(state, owner_id, members, None, 0, None, None, operation).await
}

pub async fn handle_review_execute(req: Request, state: &Arc<AppState>) -> Response {
    let params: ReviewExecuteParams = match req.params.and_then(|p| serde_json::from_value(p).ok())
    {
        Some(p) => p,
        None => {
            return Response::err(
                req.id,
                errors::INVALID_PARAMS,
                "Missing or invalid review execute params",
            )
        }
    };

    let repo_root = state.context.read().await.repo_root().to_path_buf();

    // The addressing dichotomy: a request names either an architectural
    // group or findings, never both. The refusal happens before any session
    // is loaded, so a rejected request can never leave a batch half started.
    if params.group_id.is_some() && (params.item_id.is_some() || params.item_ids.is_some()) {
        return Response::err(
            req.id,
            errors::INVALID_PARAMS,
            "group_id cannot be combined with item_id or item_ids",
        );
    }

    let (owner_id, queue) = match params.review_id.as_deref() {
        Some(review_id) => {
            let load_root = repo_root.clone();
            let load_id = review_id.to_string();
            let session = match offload_review(move || {
                ReviewStorage::load_review(&load_root, &load_id)
            })
            .await
            {
                Ok(Some(s)) => s,
                Ok(None) => {
                    return Response::err(
                        req.id,
                        errors::NOT_FOUND,
                        format!("Review session '{}' not found", review_id),
                    )
                }
                Err(err) => return Response::err(req.id, errors::REVIEW_LOAD_FAILED, err),
            };

            let selection = match params.group_id.as_deref() {
                Some(group_id) => resolve_group_selection(&session, review_id, group_id),
                None => resolve_finding_selection(&session, review_id, &params),
            };
            let queue = match selection {
                Ok(queue) => queue,
                Err(err) => {
                    return Response {
                        id: req.id,
                        result: None,
                        error: Some(err),
                    }
                }
            };
            // A single sub-step of a horizon is never addressed on its own: the
            // implementation pipeline executes the whole remaining horizon of the
            // entity it is given, so a partial target has no meaning here. The
            // entity resolves first, so a group that still poses an open question
            // is refused by its readiness gate before that target is validated.
            if params.step_id.is_some() {
                return Response::err(
                    req.id,
                    errors::INVALID_PARAMS,
                    "step_id cannot be targeted: the implementation horizon of the addressed review entity is executed as a whole",
                );
            }
            (review_id.to_string(), queue)
        }
        None => {
            if params.item_id.is_some()
                || params.item_ids.is_some()
                || params.group_id.is_some()
                || params.step_id.is_some()
            {
                return Response::err(
                    req.id,
                    errors::INVALID_PARAMS,
                    "item_id, item_ids, group_id, and step_id cannot be specified when review_id is omitted",
                );
            }
            let list_root = repo_root.clone();
            let sessions = match offload_review(move || ReviewStorage::load_all(&list_root)).await {
                Ok(s) => s,
                Err(err) => return Response::err(req.id, errors::REVIEW_LOAD_FAILED, err),
            };
            let queues = accumulate_implicit_queues(&sessions);
            if queues.members.is_empty() {
                return Response::err(
                    req.id,
                    errors::NOTHING_TO_EXECUTE,
                    empty_queue_message(
                        "across all review sessions",
                        queues.pending_architecture,
                        queues.deferred,
                    ),
                );
            }
            (
                "all".to_string(),
                ResolvedQueue {
                    members: queues.members,
                    addressed: queues.addressed,
                    deferred: queues.deferred,
                },
            )
        }
    };
    let ResolvedQueue {
        members,
        addressed,
        deferred,
    } = queue;

    let operation = match state.try_begin_operation() {
        Ok(guard) => Arc::new(guard),
        Err(()) => {
            return Response::err(
                req.id,
                errors::OPERATION_IN_PROGRESS,
                "Another operation is already running",
            )
        }
    };

    // Pre-flight verification runs while the batch operation stays registered,
    // so the slot is never briefly free and a concurrent `config/set`,
    // `git/undo`, or a second `review/execute` cannot slip in mid-verification.
    let preflight =
        crate::handlers::run_preflight_verification(&repo_root, operation.cancel_receiver()).await;
    match preflight {
        PreflightVerdict::Passed => {}
        PreflightVerdict::Cancelled => {
            return Response::err(
                req.id,
                errors::CANCELLED,
                "Pre-flight verification was cancelled; batch execution did not start",
            );
        }
        PreflightVerdict::Failed(report) => {
            let message = format!(
                "Pre-flight verification failed: the repository does not build cleanly. Batch execution was blocked before starting any finding.\n\n{}",
                report
            );
            return Response::err(req.id, errors::PREFLIGHT_VERIFICATION_FAILED, message);
        }
    }

    // Execution hands control to the Coder role: the discussion focus is
    // reset unconditionally so no discussion anchor survives into code
    // generation.
    state.clear_discussion_focus().await;

    let model_selection = params.model.clone();
    let developer_instructions = params.instructions.clone();
    let response = Response::ok(
        req.id,
        serde_json::json!({
            "review_id": params.review_id,
            "item_ids": addressed,
            "operation_id": format!("op-{}", operation.id()),
        }),
    );

    let state = Arc::clone(state);
    tokio::spawn(async move {
        match run_review_batch_with_options(
            &state,
            &owner_id,
            members,
            None,
            deferred,
            model_selection,
            developer_instructions,
            operation,
        )
        .await
        {
            BatchOutcome::Completed { items, telemetry } => {
                tracing::info!(
                    "Review batch '{}' completed: {} member(s), fixed: {}, deferred: {}, failed: {}",
                    owner_id,
                    items,
                    telemetry.fixed,
                    telemetry.deferred,
                    telemetry.failed
                );
            }
            BatchOutcome::Failed {
                completed,
                member,
                reason,
                telemetry,
            } => {
                tracing::warn!(
                    "Review batch '{}' stopped at {} after {} completed member(s): {} (fixed: {}, deferred: {}, failed: {})",
                    owner_id,
                    member,
                    completed,
                    reason,
                    telemetry.fixed,
                    telemetry.deferred,
                    telemetry.failed
                );
            }
            BatchOutcome::Cancelled {
                completed,
                telemetry,
            } => {
                tracing::warn!(
                    "Review batch '{}' cancelled after {} completed member(s) (fixed: {}, deferred: {}, failed: {})",
                    owner_id,
                    completed,
                    telemetry.fixed,
                    telemetry.deferred,
                    telemetry.failed
                );
            }
            BatchOutcome::Stopped {
                completed,
                telemetry,
            } => {
                tracing::info!(
                    "Review batch '{}' stopped gracefully after {} completed member(s) (fixed: {}, deferred: {}, failed: {})",
                    owner_id,
                    completed,
                    telemetry.fixed,
                    telemetry.deferred,
                    telemetry.failed
                );
            }
        }
    });

    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_core::workflow::batch::{BatchStepReport, BatchUsage};
    use tauqe_protocol::{ModelRef, ReviewIssue};

    /// Builds a finding of the given semantic category.
    fn typed_finding(
        id: u32,
        status: IssueStatus,
        group: Option<&str>,
        issue_type: IssueType,
    ) -> ReviewItem {
        ReviewItem::from(ReviewIssue {
            id,
            title: format!("Finding {id}"),
            uuid: None,
            group: group.map(str::to_string),
            model: None,
            issue_type,
            status,
            description: String::new(),
            recommendations: Vec::new(),
            files: Vec::new(),
            readonly_files: Vec::new(),
            note: None,
        })
    }

    /// Builds an independent finding that needs no plan before execution.
    fn finding(id: u32, status: IssueStatus) -> ReviewItem {
        typed_finding(id, status, None, IssueType::Quality)
    }

    /// Builds a grouped finding of a category that needs no plan.
    fn grouped_finding(id: u32, status: IssueStatus, group: Option<&str>) -> ReviewItem {
        typed_finding(id, status, group, IssueType::Quality)
    }

    /// Builds an architecture finding: it must be planned before execution.
    fn architecture_finding(id: u32, status: IssueStatus, group: Option<&str>) -> ReviewItem {
        typed_finding(id, status, group, IssueType::Architecture)
    }

    fn session_with(items: Vec<ReviewItem>) -> ReviewSession {
        ReviewSession {
            id: "rev-queue".to_string(),
            title: "Queue".to_string(),
            created_at: 0,
            model: ModelRef::default(),
            items,
            ..Default::default()
        }
    }

    fn group_of(
        id: &str,
        kind: tauqe_protocol::review::ReviewGroupKind,
    ) -> tauqe_protocol::ReviewGroup {
        tauqe_protocol::ReviewGroup {
            id: id.to_string(),
            title: format!("Group {id}"),
            kind,
            ..Default::default()
        }
    }

    /// The queue resolver is the contract between the section-scoped launch and
    /// the supervisor: client order wins, stale and unknown ids are dropped, and
    /// duplicates collapse to their first occurrence.
    #[test]
    fn test_resolve_session_queue_keeps_client_order_and_drops_stale_ids() {
        let session = session_with(vec![
            finding(1, IssueStatus::Fixed),
            finding(2, IssueStatus::Todo),
            finding(3, IssueStatus::InProgress),
            finding(4, IssueStatus::Discussion),
        ]);

        assert_eq!(
            resolve_session_queue(&session, &[4, 9, 3, 2, 3]),
            vec![3, 2]
        );
        assert!(resolve_session_queue(&session, &[1, 4]).is_empty());
        assert!(resolve_session_queue(&session, &[]).is_empty());
    }

    /// The implicit queue subsumes the findings an architectural group collects
    /// into a single member, while a simple group's findings and ungrouped
    /// findings stay individually scheduled.
    #[test]
    fn session_members_subsume_architectural_groups_once() {
        let session = ReviewSession {
            id: "rev-x".to_string(),
            groups: vec![
                group_of(
                    "arch-1",
                    tauqe_protocol::review::ReviewGroupKind::Architectural,
                ),
                group_of(
                    "grp-simple",
                    tauqe_protocol::review::ReviewGroupKind::Simple,
                ),
            ],
            items: vec![
                grouped_finding(1, IssueStatus::Todo, Some("arch-1")),
                grouped_finding(2, IssueStatus::InProgress, Some("arch-1")),
                grouped_finding(3, IssueStatus::Todo, Some("grp-simple")),
                grouped_finding(4, IssueStatus::InProgress, None),
                grouped_finding(5, IssueStatus::Fixed, Some("arch-1")),
                grouped_finding(6, IssueStatus::Todo, Some("ghost")),
            ],
            ..Default::default()
        };

        assert_eq!(
            session_contribution(&session).members,
            vec![
                "rev-x:g:arch-1".to_string(),
                "rev-x:3".to_string(),
                "rev-x:4".to_string(),
                "rev-x:6".to_string(),
            ]
        );
    }

    /// Only the stored `Architectural` kind subsumes a finding.
    #[test]
    fn architectural_resolution_follows_the_stored_group_kind() {
        let session = ReviewSession {
            id: "rev-x".to_string(),
            groups: vec![
                group_of(
                    "arch-1",
                    tauqe_protocol::review::ReviewGroupKind::Architectural,
                ),
                group_of(
                    "grp-simple",
                    tauqe_protocol::review::ReviewGroupKind::Simple,
                ),
            ],
            items: vec![
                grouped_finding(1, IssueStatus::Todo, Some("arch-1")),
                grouped_finding(3, IssueStatus::Todo, Some("grp-simple")),
                grouped_finding(4, IssueStatus::Todo, None),
                grouped_finding(6, IssueStatus::Todo, Some("ghost")),
            ],
            ..Default::default()
        };

        assert_eq!(
            architectural_group_of(&session, &session.items[0]),
            Some("arch-1")
        );
        assert_eq!(architectural_group_of(&session, &session.items[1]), None);
        assert_eq!(architectural_group_of(&session, &session.items[2]), None);
        assert_eq!(architectural_group_of(&session, &session.items[3]), None);
    }

    /// An architecture finding with no architectural group is scheduled by
    /// neither form: the post-analysis must regroup it before it can run.
    #[test]
    fn session_members_never_schedule_unplanned_architecture_findings() {
        let session = ReviewSession {
            id: "rev-arch".to_string(),
            groups: vec![
                group_of(
                    "arch-1",
                    tauqe_protocol::review::ReviewGroupKind::Architectural,
                ),
                group_of(
                    "grp-simple",
                    tauqe_protocol::review::ReviewGroupKind::Simple,
                ),
            ],
            items: vec![
                architecture_finding(1, IssueStatus::Todo, Some("arch-1")),
                architecture_finding(2, IssueStatus::Todo, Some("grp-simple")),
                architecture_finding(4, IssueStatus::Todo, None),
                architecture_finding(6, IssueStatus::Todo, Some("ghost")),
                finding(7, IssueStatus::Todo),
            ],
            ..Default::default()
        };

        assert_eq!(
            session_contribution(&session).members,
            vec!["rev-arch:g:arch-1".to_string(), "rev-arch:7".to_string()]
        );
    }

    /// The cross-session queue concatenates the per-session queues in stored
    /// order and counts the architecture findings no group can plan.
    #[test]
    fn implicit_queues_accumulate_across_sessions() {
        let clustered = ReviewSession {
            id: "rev-a".to_string(),
            groups: vec![group_of(
                "arch-1",
                tauqe_protocol::review::ReviewGroupKind::Architectural,
            )],
            items: vec![
                architecture_finding(1, IssueStatus::Todo, Some("arch-1")),
                finding(2, IssueStatus::Todo),
            ],
            ..Default::default()
        };
        let plain = ReviewSession {
            id: "rev-b".to_string(),
            items: vec![
                architecture_finding(3, IssueStatus::Todo, None),
                finding(4, IssueStatus::InProgress),
                finding(5, IssueStatus::Fixed),
            ],
            ..Default::default()
        };

        let queues = accumulate_implicit_queues(&[clustered, plain]);

        assert_eq!(
            queues.members,
            vec![
                "rev-a:g:arch-1".to_string(),
                "rev-a:2".to_string(),
                "rev-b:4".to_string()
            ]
        );
        assert_eq!(queues.addressed, vec![1, 2, 4]);
        assert_eq!(queues.pending_architecture, 1);
        assert_eq!(queues.deferred, 0);
    }

    /// An explicit selection of an unplanned architecture finding is refused.
    #[test]
    fn explicit_selection_refuses_an_unplanned_architecture_finding() {
        let session = session_with(vec![
            architecture_finding(1, IssueStatus::Todo, None),
            finding(2, IssueStatus::Todo),
            architecture_finding(3, IssueStatus::Fixed, None),
        ]);

        let targeted = ReviewExecuteParams {
            review_id: Some("rev-queue".to_string()),
            item_id: Some(1),
            ..Default::default()
        };
        let err = resolve_finding_selection(&session, "rev-queue", &targeted)
            .expect_err("an unplanned architecture finding is refused");
        assert_eq!(err.code, errors::ARCHITECTURAL_GROUP_EXECUTION_REQUIRED);
        assert!(err.message.contains("post-analysis"));

        let selected = ReviewExecuteParams {
            review_id: Some("rev-queue".to_string()),
            item_ids: Some(vec![2, 1]),
            ..Default::default()
        };
        let err = resolve_finding_selection(&session, "rev-queue", &selected)
            .expect_err("the whole selection is refused");
        assert_eq!(err.code, errors::ARCHITECTURAL_GROUP_EXECUTION_REQUIRED);

        let finished = ReviewExecuteParams {
            review_id: Some("rev-queue".to_string()),
            item_id: Some(3),
            ..Default::default()
        };
        let err = resolve_finding_selection(&session, "rev-queue", &finished)
            .expect_err("a finished finding is not re-executed");
        assert_eq!(err.code, errors::NOTHING_TO_EXECUTE);
    }

    /// A group member carries no finding ordinal, so it is decoded as the group.
    #[test]
    fn failed_member_decodes_a_group_member() {
        assert_eq!(
            FailedMember::decode("rev-42:g:arch-1"),
            FailedMember::Group {
                session_id: "rev-42".to_string(),
                group_id: "arch-1".to_string(),
            }
        );
        assert_eq!(
            FailedMember::decode("rev-42:g:arch-1").to_string(),
            "review group 'arch-1' in session 'rev-42'"
        );
    }

    /// A finding member keeps its audit ordinal; an unminted identifier keeps
    /// its text instead of borrowing the ordinal of an unrelated member.
    #[test]
    fn failed_member_decodes_findings_and_keeps_unknown_identifiers() {
        assert_eq!(
            FailedMember::decode("rev-42:7"),
            FailedMember::Finding {
                session_id: "rev-42".to_string(),
                item_id: 7,
            }
        );
        assert_eq!(
            FailedMember::decode("rev-42:7").to_string(),
            "finding #7 in session 'rev-42'"
        );
        assert_eq!(
            FailedMember::decode("rev-42:g:"),
            FailedMember::Unresolved {
                member_id: "rev-42:g:".to_string(),
            }
        );
    }

    /// The readiness gate postpones an architectural group while one of its
    /// findings still poses an unanswered question.
    #[test]
    fn session_contribution_defers_a_group_with_an_open_question() {
        let open = grouped_finding(1, IssueStatus::Discussion, Some("arch-1"));
        let pending = grouped_finding(2, IssueStatus::Todo, Some("arch-1"));
        let standalone = finding(3, IssueStatus::Todo);
        let session = ReviewSession {
            id: "rev-gate".to_string(),
            groups: vec![group_of(
                "arch-1",
                tauqe_protocol::review::ReviewGroupKind::Architectural,
            )],
            items: vec![open, pending, standalone],
            ..Default::default()
        };

        let contribution = session_contribution(&session);

        assert_eq!(contribution.members, vec!["rev-gate:3".to_string()]);
        assert_eq!(contribution.addressed, vec![3]);
        assert_eq!(contribution.deferred_findings, 2);
        assert_eq!(contribution.pending_architecture, 0);
    }

    /// A recorded engineering decision answers the question, so the group leaves
    /// the gate and is scheduled through its unified horizon again.
    #[test]
    fn a_recorded_decision_lifts_the_deferral() {
        let mut decided = grouped_finding(1, IssueStatus::Discussion, Some("arch-1"));
        decided.resolution_notes = Some("Keep the public API stable".to_string());
        let pending = grouped_finding(2, IssueStatus::Todo, Some("arch-1"));
        let session = ReviewSession {
            id: "rev-gate".to_string(),
            groups: vec![group_of(
                "arch-1",
                tauqe_protocol::review::ReviewGroupKind::Architectural,
            )],
            items: vec![decided, pending],
            ..Default::default()
        };

        assert!(group_ready_for_autonomous_run(&session, "arch-1"));

        let contribution = session_contribution(&session);

        assert_eq!(contribution.members, vec!["rev-gate:g:arch-1".to_string()]);
        assert_eq!(contribution.deferred_findings, 0);
    }

    /// The empty-queue diagnostic names the postponed findings before the
    /// missing post-analysis.
    #[test]
    fn empty_queue_message_names_deferred_findings_first() {
        let deferred = empty_queue_message("in this review session", 0, 2);
        assert!(deferred.contains("2 finding(s) are deferred"));
        assert!(!deferred.contains("post-analysis"));

        let planned = empty_queue_message("in this review session", 3, 0);
        assert!(planned.contains("3 ARCHITECTURE"));
        assert!(planned.contains("post-analysis"));

        assert_eq!(
            empty_queue_message("in this review session", 0, 0),
            "No TODO findings to execute in this review session"
        );
    }

    /// An isolated member failure is counted in the terminal telemetry while the
    /// queue still completes, so a night run reports what it left behind.
    #[test]
    fn isolated_member_failure_is_counted_in_the_terminal_telemetry() {
        let report = BatchReport {
            executed: vec![
                BatchStepReport {
                    id: "rev-queue:g:arch-1".to_string(),
                    outcome: BatchStepOutcome::skipped("the root cause was not decomposed"),
                },
                BatchStepReport {
                    id: "rev-queue:2".to_string(),
                    outcome: BatchStepOutcome::succeeded(),
                },
            ],
            scheduled: 2,
            stop: BatchStop::Completed,
            usage: BatchUsage::default(),
        };

        let telemetry = batch_telemetry(&report, 1, 0);

        assert_eq!(telemetry.fixed, 1);
        assert_eq!(telemetry.deferred, 0);
        assert_eq!(telemetry.failed, 1);
        assert!(report.is_success());
    }
}
