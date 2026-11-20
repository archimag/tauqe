//! Review queue policy, member settlement, and batch lifecycle.
mod persistence;

use std::path::Path;
use std::sync::Arc;

use crate::api::{
    Application, ApplicationEvent, ApplicationObserver, OperationGuard, PreflightVerdict,
};
use crate::application::infrastructure::execution::PlanExecutionEnvironment;
use crate::application::infrastructure::step_run::StepRunPublication;
use crate::application::review_execution::StepPersistence;
use crate::application::step_dispatch::StepLoopReport;
use crate::application::turn_setup::ResolvedTurnModel;
use async_trait::async_trait;
pub(crate) use persistence::ReviewMemberPersistence;
use tauqe_core::control::outcome::LoopResult;
use tauqe_core::models::ModelSelection;
use tauqe_core::plan::{PlanItemStatus, PlanScopeEvaluation};
use tauqe_core::providers::{format_session_id, StreamEvent, SESSION_PREFIX_REVIEW};
use tauqe_core::review::prompt::refine::has_recorded_decision;
use tauqe_core::review::ReviewStorage;
use tauqe_core::review::{IssueStatus, IssueType, ReviewExecuteTarget, ReviewItem, ReviewSession};
use tauqe_core::workflow::batch::{
    BatchExecution, BatchReport, BatchStep, BatchStepExecutor, BatchStepOutcome, BatchStepStatus,
    BatchStop,
};
use tauqe_core::workflow::implementation::{classify_scope_stall, ExecutionScope};
use tauqe_core::workflow::turn::TurnWorkContext;
use tauqe_core::workflow::turn::TurnWorkItem;
use tauqe_core::workflow::turn::TurnWorkQueue;
use tokio::sync::{mpsc, watch};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewQueueFailure {
    ArchitecturalGroupExecutionRequired,
    GroupUnresolvedQuestions,
    InvalidParams,
    ItemInDiscussion,
    ItemRejected,
    NothingToExecute,
    NotFound,
    ReviewLoadFailed,
}
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct ReviewQueueError {
    pub kind: ReviewQueueFailure,
    message: String,
}
pub struct PreparedReviewBatch {
    pub owner_id: String,
    pub members: Vec<String>,
    pub addressed: Vec<u32>,
    pub deferred: usize,
}
#[derive(Debug, thiserror::Error)]
pub enum ReviewLaunchError {
    #[error("Another operation is already running")]
    Busy,
    #[error(transparent)]
    Queue(#[from] ReviewQueueError),
    #[error("Pre-flight verification was cancelled; batch execution did not start")]
    Cancelled,
    #[error("Pre-flight verification failed: the repository does not build cleanly. Batch execution was blocked before starting any finding.\n\n{0}")]
    Verification(String),
}

pub struct ReviewBatchStarted {
    pub review_id: Option<String>,
    pub item_ids: Vec<u32>,
    pub operation_id: String,
}
pub struct ExecutedReviewBatch {
    pub owner_id: String,
    pub stop: BatchStop,
    pub executed_count: usize,
    pub total_count: usize,
    pub elapsed: std::time::Duration,
    pub error: Option<String>,
    pub fixed_count: usize,
    pub deferred: usize,
    pub failed_count: usize,
}
async fn offload<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| format!("blocking task failed: {e}"))
}
async fn offload_review<T: Send + 'static>(
    f: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> Result<T, String> {
    offload(f).await?.map_err(|e| format!("{e:#}"))
}
async fn cleanup_transient_context(
    app: &Application,
    observer: &dyn ApplicationObserver,
    operation_id: &str,
) {
    if let Ok(state) = app
        .clear_context(crate::api::ContextClearScope::Transient)
        .await
    {
        observer.publish(operation_id, ApplicationEvent::Context(state));
    }
}
/// Separator binding the session identifier to the group identifier of a
/// group-member batch target.
const GROUP_STEP_SEPARATOR: &str = ":g:";

/// Encodes the batch-member identifier of one architectural group horizon.
pub fn group_member_id(session_id: &str, group_id: &str) -> String {
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
pub struct BatchTelemetry {
    /// Findings the batch repaired.
    pub fixed: usize,
    /// Findings postponed until the questions of their group are decided.
    pub deferred: usize,
    /// Members that failed locally while the queue continued.
    pub failed: usize,
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

/// Outcome of a headless batch of review findings.
#[derive(Debug, PartialEq, Eq)]
pub enum BatchOutcome {
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
pub enum FailedMember {
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

/// Builds a typed refusal of review queue preparation.
fn response_error(kind: ReviewQueueFailure, message: impl Into<String>) -> ReviewQueueError {
    ReviewQueueError {
        kind,
        message: message.into(),
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
        group.id == group_id && group.kind == tauqe_core::review::ReviewGroupKind::Architectural
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
fn group_unresolved_questions_error(group_id: &str, open: &[&ReviewItem]) -> ReviewQueueError {
    let ids = open
        .iter()
        .map(|item| format!("#{}", item.id))
        .collect::<Vec<_>>()
        .join(", ");
    response_error(
        ReviewQueueFailure::GroupUnresolvedQuestions,
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
fn unplanned_architecture_error(item: &ReviewItem) -> ReviewQueueError {
    response_error(
        ReviewQueueFailure::ArchitecturalGroupExecutionRequired,
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

type ResolvedSelection = Result<ResolvedQueue, ReviewQueueError>;

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
            ReviewQueueFailure::NotFound,
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

/// Resolves the members one session-scoped target runs.
///
/// The target states which queue the request addresses, so the resolver only
/// decides the members: a group runs its unified horizon, a finding runs the
/// single-finding validation, an explicit selection keeps the client order, and
/// a session target runs the implicit queue of the whole session. Only a
/// cross-session run names no session, and it is accumulated from the stored
/// sessions instead of being resolved against one of them.
fn resolve_finding_selection(
    session: &ReviewSession,
    review_id: &str,
    target: &ReviewExecuteTarget,
) -> ResolvedSelection {
    let member_of = |item_id: u32| format!("{}:{}", review_id, item_id);
    match target {
        ReviewExecuteTarget::AllSessions => Err(response_error(
            ReviewQueueFailure::InvalidParams,
            "A cross-session target is resolved from the stored sessions, not from one session",
        )),
        ReviewExecuteTarget::Group(group) => {
            resolve_group_selection(session, review_id, &group.group_id)
        }
        ReviewExecuteTarget::Finding(finding) => {
            let item_id = finding.item_id;
            let item = match session.find_item(item_id) {
                Some(item) => item,
                None => {
                    return Err(response_error(
                        ReviewQueueFailure::NotFound,
                        format!(
                            "Review item #{} not found in session '{}'",
                            item_id, review_id
                        ),
                    ));
                }
            };
            if let Some(group_id) = architectural_group_of(session, item) {
                return Err(response_error(
                    ReviewQueueFailure::ArchitecturalGroupExecutionRequired,
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
                    ReviewQueueFailure::ItemInDiscussion,
                    format!(
                        "Review item #{} is in DISCUSSION status and cannot be executed autonomously until reviewed (change status to TODO first)",
                        item.id
                    ),
                )),
                IssueStatus::Rejected => Err(response_error(
                    ReviewQueueFailure::ItemRejected,
                    format!("Review item #{} is REJECTED and cannot be executed", item.id),
                )),
                IssueStatus::Fixed => Err(response_error(
                    ReviewQueueFailure::NothingToExecute,
                    format!("Review item #{} is already FIXED", item.id),
                )),
                IssueStatus::Skipped => Err(response_error(
                    ReviewQueueFailure::NothingToExecute,
                    format!("Review item #{} is already SKIPPED", item.id),
                )),
                IssueStatus::Todo | IssueStatus::InProgress => Ok(ResolvedQueue {
                    members: vec![member_of(item_id)],
                    addressed: vec![item_id],
                    deferred: 0,
                }),
            }
        }
        ReviewExecuteTarget::Findings(findings) => {
            for id in &findings.item_ids {
                if let Some(item) = session.find_item(*id) {
                    if let Some(group_id) = architectural_group_of(session, item) {
                        return Err(response_error(
                            ReviewQueueFailure::ArchitecturalGroupExecutionRequired,
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
            let ids = resolve_session_queue(session, &findings.item_ids);
            if ids.is_empty() {
                return Err(response_error(
                    ReviewQueueFailure::NothingToExecute,
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
        ReviewExecuteTarget::Session(_) => {
            let queues = accumulate_implicit_queues(std::slice::from_ref(session));
            if queues.members.is_empty() {
                return Err(response_error(
                    ReviewQueueFailure::NothingToExecute,
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
    application: &'a Application,
    operation: Arc<OperationGuard>,
    observer: &'a Arc<dyn ApplicationObserver>,
    repo_root: &'a Path,
    execution: PlanExecutionEnvironment,
    publication_model: ResolvedTurnModel,
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

    /// Takes the step the member is about to execute over, before its turn starts.
    ///
    /// The lifecycle of the entity belongs to the control loops, which write the
    /// very same transition when they start the step. Writing it ahead of the run
    /// lets the client observe the work that is actually executing: the caller
    /// publishes the session right after this call and before `model/started`, so a
    /// session that still reports `TODO` never covers the turn that already owns it.
    async fn mark_taken(&self, persistence: &ReviewMemberPersistence, step_id: Option<String>) {
        let Some(step_id) = step_id else {
            return;
        };
        let tracked = step_id.clone();
        let persistence = persistence.clone();
        let root = self.repo_root.to_path_buf();
        let outcome = offload(move || {
            let mut plan = persistence.load(&root).map_err(|err| err.to_string())?;
            if !plan
                .implementation
                .find_item(&step_id)
                .is_some_and(|item| item.status == PlanItemStatus::Todo)
                || !plan
                    .implementation
                    .update_item_status(&step_id, PlanItemStatus::InProgress)
            {
                return Ok(());
            }
            persistence
                .persist(&root, &plan)
                .map_err(|err| err.to_string())
        })
        .await;
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(err)) | Err(err) => tracing::warn!(
                step_id = tracked.as_str(),
                error = %err,
                "the taken review step was not persisted"
            ),
        }
    }

    /// States the verdict of a settled member on the entity it addressed.
    ///
    /// The verdict is written exactly once, when the run of the member reached its
    /// terminal outcome, and it describes the review entity rather than the step
    /// graph: a completed horizon closes the findings a group collects as `Fixed`,
    /// and a member the run could not implement states `Skipped` with the
    /// diagnostic. A plan scope owns no review entity, so it settles nothing, and
    /// a finding keeps the note the developer already stated.
    async fn settle(&self, scope: &ExecutionScope, verdict: MemberVerdict<'_>) {
        let status_code = match &verdict {
            MemberVerdict::Fixed => "fixed",
            MemberVerdict::Skipped(_) => "skipped",
        };
        match scope {
            ExecutionScope::Plan { .. } => {}
            ExecutionScope::ReviewGroup {
                review_id,
                group_id,
            } => match verdict {
                MemberVerdict::Fixed => {
                    let root = self.repo_root.to_path_buf();
                    let review = review_id.clone();
                    let group = group_id.clone();
                    let settled = offload(move || {
                        ReviewStorage::close_group_implementation(
                            &root,
                            &review,
                            &group,
                            GROUP_COMPLETION_NOTE,
                        )
                    })
                    .await;
                    match settled {
                        Ok(Ok(persisted)) => tracing::info!(
                            review_id = review_id.as_str(),
                            group_id = group_id.as_str(),
                            persisted = ?persisted,
                            "review group implementation closed"
                        ),
                        Ok(Err(err)) => tracing::warn!(
                            review_id = review_id.as_str(),
                            group_id = group_id.as_str(),
                            error = %err,
                            "the review group completion was not persisted"
                        ),
                        Err(err) => tracing::warn!(
                            review_id = review_id.as_str(),
                            group_id = group_id.as_str(),
                            error = %err,
                            "the review group completion was not persisted"
                        ),
                    }
                }
                MemberVerdict::Skipped(reason) => {
                    let root = self.repo_root.to_path_buf();
                    let review = review_id.clone();
                    let group = group_id.clone();
                    let reason = reason.to_string();
                    let settled = offload(move || {
                        ReviewStorage::set_group_blocked_reason(
                            &root,
                            &review,
                            &group,
                            Some(reason.as_str()),
                        )
                    })
                    .await;
                    match settled {
                        Ok(Ok(persisted)) => tracing::warn!(
                            review_id = review_id.as_str(),
                            group_id = group_id.as_str(),
                            persisted = ?persisted,
                            status = status_code,
                            "the review group horizon was not completed"
                        ),
                        Ok(Err(err)) => tracing::warn!(
                            review_id = review_id.as_str(),
                            group_id = group_id.as_str(),
                            status = status_code,
                            error = %err,
                            "the review group blocker was not persisted"
                        ),
                        Err(err) => tracing::warn!(
                            review_id = review_id.as_str(),
                            group_id = group_id.as_str(),
                            status = status_code,
                            error = %err,
                            "the review group blocker was not persisted"
                        ),
                    }
                }
            },
            ExecutionScope::ReviewFinding { review_id, item_id } => {
                let (status, note) = match verdict {
                    MemberVerdict::Fixed => {
                        (IssueStatus::Fixed, FINDING_COMPLETION_NOTE.to_string())
                    }
                    MemberVerdict::Skipped(reason) => (
                        IssueStatus::Skipped,
                        format!("The implementation run could not address this finding: {reason}"),
                    ),
                };
                let root = self.repo_root.to_path_buf();
                let review = review_id.clone();
                let id = *item_id;
                let settled = offload(move || {
                    ReviewStorage::update_session_item(&root, &review, id, |issue| {
                        issue.status = status;
                        // A note the developer already stated is never rewritten by
                        // a settlement: the note of the run is recorded only on a
                        // finding that carries none.
                        if !carries_note(issue.note.as_deref()) {
                            issue.note = Some(note);
                        }
                    })
                })
                .await;
                match settled {
                    Ok(Ok(Some(_))) => tracing::info!(
                        review_id = review_id.as_str(),
                        item_id = *item_id,
                        status = status_code,
                        "review finding settled"
                    ),
                    Ok(Ok(None)) => tracing::warn!(
                        review_id = review_id.as_str(),
                        item_id = *item_id,
                        status = status_code,
                        "the review finding verdict was not persisted: the finding was not found"
                    ),
                    Ok(Err(err)) => tracing::warn!(
                        review_id = review_id.as_str(),
                        item_id = *item_id,
                        status = status_code,
                        error = %err,
                        "the review finding verdict was not persisted"
                    ),
                    Err(err) => tracing::warn!(
                        review_id = review_id.as_str(),
                        item_id = *item_id,
                        status = status_code,
                        error = %err,
                        "the review finding verdict was not persisted"
                    ),
                }
            }
        }
    }

    /// Settles the terminal report of one member run on the entity it addressed.
    ///
    /// The engine ends every run it starts, so a member that could not be
    /// implemented as stated is a local skip of the queue instead of a fatal
    /// stop: the remaining findings are independent of it. A cancellation and an
    /// infrastructure failure stop the queue, because the next member must never
    /// build on a state nobody verified.
    async fn settle_run(&self, scope: &ExecutionScope, report: StepLoopReport) -> BatchStepOutcome {
        match report {
            StepLoopReport::Cancelled => BatchStepOutcome::cancelled(),
            StepLoopReport::Failed(reason) => {
                self.settle(scope, MemberVerdict::Skipped(reason.as_str()))
                    .await;
                BatchStepOutcome::failed(reason)
            }
            StepLoopReport::Turn(_) => BatchStepOutcome::failed(
                "A review member reported a single-turn outcome instead of a step sequence"
                    .to_string(),
            ),
            StepLoopReport::Sequence(result) => match result {
                LoopResult::Success(_) => {
                    if self.horizon_complete(scope).await {
                        self.settle(scope, MemberVerdict::Fixed).await;
                        BatchStepOutcome::succeeded()
                    } else {
                        let reason = "The run stopped before the horizon became complete; execute the run again to continue from the remaining steps".to_string();
                        self.settle(scope, MemberVerdict::Skipped(reason.as_str()))
                            .await;
                        BatchStepOutcome::skipped()
                    }
                }
                LoopResult::Diverged(_) => {
                    let reason = "The run diverged without a completed horizon".to_string();
                    self.settle(scope, MemberVerdict::Skipped(reason.as_str()))
                        .await;
                    BatchStepOutcome::skipped()
                }
                LoopResult::Exhausted(report) => {
                    self.settle(scope, MemberVerdict::Skipped(report.reason.as_str()))
                        .await;
                    BatchStepOutcome::skipped()
                }
            },
        }
    }

    /// Whether the horizon the member executes reached its aggregate `Done`.
    ///
    /// A degenerate single-finding horizon owns no persisted step graph, so a run
    /// that completed it is the whole verdict of that member.
    async fn horizon_complete(&self, scope: &ExecutionScope) -> bool {
        if matches!(scope, ExecutionScope::ReviewFinding { .. }) {
            return true;
        }
        let resolve_scope = scope.clone();
        let root = self.repo_root.to_path_buf();
        matches!(
            self.application.implementation_frame(&resolve_scope, &root).await,
            Ok(frame) if frame.plan.implementation.status() == PlanItemStatus::Done
        )
    }

    /// Resolves the work one review member executes.
    ///
    /// The horizon is read from the storage that owns it, so the sequence loop
    /// drives exactly the steps the entity declares. A scope that cannot be
    /// resolved, or whose horizon is not synthesized yet, yields the diagnostic
    /// the member reports instead of a model turn.
    async fn member_run(&self, scope: &ExecutionScope) -> Result<MemberRun, String> {
        let resolve_scope = scope.clone();
        let root = self.repo_root.to_path_buf();
        let frame = match self
            .application
            .implementation_frame(&resolve_scope, &root)
            .await
        {
            Ok(frame) => frame,
            Err(failure) => return Err(failure.into_reason()),
        };
        let title = frame.plan.title.clone();
        match scope {
            ExecutionScope::Plan { .. } => {
                Err("A review batch member addresses no plan horizon".to_string())
            }
            ExecutionScope::ReviewFinding { review_id, item_id } => Ok(MemberRun {
                work: TurnWorkContext::ReviewStep {
                    review_id: review_id.clone(),
                    review_title: review_id.clone(),
                    queue: TurnWorkQueue {
                        items: vec![TurnWorkItem {
                            id: item_id.to_string(),
                            title,
                        }],
                        active_index: 0,
                    },
                },
                steps: vec![item_id.to_string()],
            }),
            ExecutionScope::ReviewGroup {
                review_id,
                group_id,
            } => {
                if frame.plan.implementation.items.is_empty() {
                    return Err(format!(
                        "Review group '{}' of session '{}' owns no implementation horizon yet; synthesize its horizon before the group is executed",
                        group_id, review_id
                    ));
                }
                let items = match frame.plan.implementation.evaluate_scope(None) {
                    PlanScopeEvaluation::Ready(leaves) => leaves
                        .into_iter()
                        .map(|item| TurnWorkItem {
                            id: item.id.clone(),
                            title: item.title.clone(),
                        })
                        .collect::<Vec<_>>(),
                    PlanScopeEvaluation::AllCompleted => Vec::new(),
                    stall => {
                        let failure = classify_scope_stall(&frame.plan, None, &stall);
                        return Err(failure.into_reason());
                    }
                };
                let steps = items.iter().map(|item| item.id.clone()).collect();
                Ok(MemberRun {
                    work: TurnWorkContext::ReviewGroupStep {
                        review_id: review_id.clone(),
                        review_title: review_id.clone(),
                        group_id: group_id.clone(),
                        group_title: title,
                        queue: TurnWorkQueue {
                            items,
                            active_index: 0,
                        },
                    },
                    steps,
                })
            }
        }
    }
}

/// Work one review batch member executes through the control loops.
struct MemberRun {
    /// Work the turns of the run execute, published with `model/started`.
    work: TurnWorkContext,
    /// Ordered steps the sequence loop drives.
    steps: Vec<String>,
}

/// Verdict a settled batch member states on the review entity it addressed.
enum MemberVerdict<'a> {
    /// The run completed the work the member addressed.
    Fixed,
    /// The run could not address the work of the member, with its diagnostic.
    Skipped(&'a str),
}

/// Note recorded on the findings a completed group horizon closes.
///
/// It states why the findings became `Fixed`: the horizon the group owns
/// finished. A finding that already carries a note keeps it.
const GROUP_COMPLETION_NOTE: &str =
    "Resolved by the completed implementation horizon of the review group";

/// Note recorded on a finding the run addressed through its own step.
const FINDING_COMPLETION_NOTE: &str = "Addressed by the implementation run";

/// Whether a finding already carries a resolution note.
fn carries_note(note: Option<&str>) -> bool {
    note.map(str::trim).is_some_and(|text| !text.is_empty())
}

/// Resolves the review session a batch member addresses.
fn scope_review_id(scope: &ExecutionScope) -> Option<String> {
    match scope {
        ExecutionScope::ReviewGroup { review_id, .. }
        | ExecutionScope::ReviewFinding { review_id, .. } => Some(review_id.clone()),
        ExecutionScope::Plan { .. } => None,
    }
}

#[async_trait]
impl BatchStepExecutor for ReviewBatchExecutor<'_> {
    async fn execute_step(
        &mut self,
        step: &BatchStep,
        _stream_tx: mpsc::Sender<StreamEvent>,
        cancel_rx: watch::Receiver<bool>,
    ) -> anyhow::Result<BatchStepOutcome> {
        let Some(scope) = member_scope(&step.id) else {
            return Ok(BatchStepOutcome::failed(format!(
                "Invalid review step id: {}",
                step.id
            )));
        };
        let Some(review_id) = scope_review_id(&scope) else {
            return Ok(BatchStepOutcome::failed(format!(
                "Review step '{}' addresses no review session",
                step.id
            )));
        };
        let Some(persistence) = ReviewMemberPersistence::of(&scope) else {
            return Ok(BatchStepOutcome::failed(format!(
                "Review step '{}' addresses no executable review entity",
                step.id
            )));
        };

        // A cancel observed before the member is launched ends it before any model
        // turn is requested and before the addressed entity is touched.
        if *cancel_rx.borrow() {
            return Ok(BatchStepOutcome::cancelled());
        }

        // The horizon is resolved before the run starts, because the sequence loop
        // drives exactly the steps the entity declares: a scope that cannot be
        // resolved, or whose horizon is not synthesized yet, reports its diagnostic
        // instead of spending a model turn.
        let run = match self.member_run(&scope).await {
            Ok(run) => run,
            Err(reason) => {
                self.settle(&scope, MemberVerdict::Skipped(reason.as_str()))
                    .await;
                self.application
                    .publish_review_snapshot(self.repo_root, &review_id, self.observer.as_ref())
                    .await;
                return Ok(BatchStepOutcome::skipped());
            }
        };

        let outcome = if run.steps.is_empty() {
            // Nothing is left to schedule: the horizon already reached its
            // aggregate completion, so the member settles the findings it collects.
            self.settle(&scope, MemberVerdict::Fixed).await;
            BatchStepOutcome::succeeded()
        } else {
            // The member is taken over and published before its first turn starts,
            // so the client observes the entity the run executes instead of the
            // `TODO` it held when the batch was launched.
            self.mark_taken(&persistence, run.steps.first().cloned())
                .await;
            self.application
                .publish_review_snapshot(self.repo_root, &review_id, self.observer.as_ref())
                .await;
            let model = &self.publication_model;
            let publication = StepRunPublication {
                model: model.model.clone(),
                role: model.role,
                effort: model.effort,
                alias: model.alias.clone(),
                plan_context: None,
                work_context: Some(run.work),
                session_label: Some(format_session_id(SESSION_PREFIX_REVIEW, &review_id)),
            };
            let app = self.application.clone();
            let operation = self.operation.clone();
            let environment = self.execution.clone();
            let root = self.repo_root.to_path_buf();
            let context = self.application.context_for_turn().await;
            let selection = model.selection.clone();
            let session_id = Some(format_session_id(SESSION_PREFIX_REVIEW, &review_id));
            let report = self
                .application
                .run_prepared_step(
                    publication,
                    self.observer.clone(),
                    move |op_id, sink| async move {
                        let task_app = app.clone();
                        let task_operation = operation.clone();
                        app.run_step_task(&operation, &op_id, async move {
                            let mut context = context;
                            let result = crate::application::review_sequence::run(
                                &task_app,
                                &task_operation,
                                &environment,
                                &root,
                                &mut context,
                                Box::new(persistence),
                                &run.steps,
                                Some(selection),
                                session_id,
                                None,
                                &tauqe_core::control::context::CancellationToken::from_receiver(
                                    task_operation.cancel_receiver(),
                                ),
                                &sink,
                            )
                            .await;
                            let report = match result {
                                Ok(result) => StepLoopReport::Sequence(result),
                                Err(tauqe_core::control::outcome::SystemError::Cancelled) => {
                                    StepLoopReport::Cancelled
                                }
                                Err(error) => StepLoopReport::Failed(error.to_string()),
                            };
                            (report, context)
                        })
                        .await
                    },
                )
                .await;
            self.settle_run(&scope, report).await
        };

        if outcome.status == BatchStepStatus::Succeeded {
            self.fixed_items += self.repaired_findings(&step.id).await;
        }
        // The member is settled before it is published, so the client observes the
        // verdict already on disk instead of the state it held when the batch was
        // launched.
        self.application
            .publish_review_snapshot(self.repo_root, &review_id, self.observer.as_ref())
            .await;
        Ok(outcome)
    }
}

impl Application {
    async fn prepare_review_batch(
        &self,
        target: &ReviewExecuteTarget,
    ) -> Result<PreparedReviewBatch, ReviewQueueError> {
        let repo_root = self.repository_root().await;
        let (owner_id, queue) = match target {
            ReviewExecuteTarget::AllSessions => {
                let list_root = repo_root.clone();
                let sessions =
                    match offload_review(move || ReviewStorage::load_all(&list_root)).await {
                        Ok(s) => s,
                        Err(err) => {
                            return Err(response_error(ReviewQueueFailure::ReviewLoadFailed, err))
                        }
                    };
                let queues = accumulate_implicit_queues(&sessions);
                if queues.members.is_empty() {
                    return Err(response_error(
                        ReviewQueueFailure::NothingToExecute,
                        empty_queue_message(
                            "across all review sessions",
                            queues.pending_architecture,
                            queues.deferred,
                        ),
                    ));
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
            target => {
                let Some(review_id) = target.review_id() else {
                    return Err(response_error(
                        ReviewQueueFailure::InvalidParams,
                        "Review execute target names no review session",
                    ));
                };
                let load_root = repo_root.clone();
                let load_id = review_id.to_string();
                let session =
                    match offload_review(move || ReviewStorage::load_review(&load_root, &load_id))
                        .await
                    {
                        Ok(Some(s)) => s,
                        Ok(None) => {
                            return Err(response_error(
                                ReviewQueueFailure::NotFound,
                                format!("Review session '{}' not found", review_id),
                            ))
                        }
                        Err(err) => {
                            return Err(response_error(ReviewQueueFailure::ReviewLoadFailed, err))
                        }
                    };
                let queue = match resolve_finding_selection(&session, review_id, target) {
                    Ok(queue) => queue,
                    Err(err) => return Err(err),
                };
                (review_id.to_string(), queue)
            }
        };
        let ResolvedQueue {
            members,
            addressed,
            deferred,
        } = queue;

        Ok(PreparedReviewBatch {
            owner_id,
            members,
            addressed,
            deferred,
        })
    }
    pub async fn execute_review(
        &self,
        target: ReviewExecuteTarget,
        model: Option<ModelSelection>,
        observer: Arc<dyn ApplicationObserver>,
    ) -> Result<ReviewBatchStarted, ReviewLaunchError> {
        let operation = Arc::new(
            self.try_begin_operation()
                .map_err(|_| ReviewLaunchError::Busy)?,
        );
        let queue = self.prepare_review_batch(&target).await?;
        let root = self.repository_root().await;
        match crate::api::run_preflight_verification(&root, operation.cancel_receiver()).await {
            PreflightVerdict::Passed => {}
            PreflightVerdict::Cancelled => return Err(ReviewLaunchError::Cancelled),
            PreflightVerdict::Failed(report) => {
                return Err(ReviewLaunchError::Verification(report))
            }
        }
        let operation_id = operation.publication_id();
        self.clear_discussion_focus()
            .await
            .publish(&operation_id, observer.as_ref());
        let started = ReviewBatchStarted {
            review_id: target.review_id().map(str::to_owned),
            item_ids: queue.addressed,
            operation_id,
        };
        let app = self.clone();
        tokio::spawn(async move {
            app.run_review_batch(
                &queue.owner_id,
                queue.members,
                queue.deferred,
                model,
                operation,
                observer,
            )
            .await;
        });
        Ok(started)
    }

    #[cfg(feature = "test-support")]
    pub async fn test_run_review_batch(
        &self,
        owner_id: &str,
        tasks: Vec<String>,
        deferred: usize,
        model: Option<ModelSelection>,
        operation: Arc<OperationGuard>,
        observer: Arc<dyn ApplicationObserver>,
    ) -> BatchOutcome {
        self.run_review_batch(owner_id, tasks, deferred, model, operation, observer)
            .await
    }

    async fn run_review_batch(
        &self,
        owner_id: &str,
        tasks: Vec<String>,
        deferred: usize,
        model: Option<ModelSelection>,
        operation: Arc<OperationGuard>,
        observer: Arc<dyn ApplicationObserver>,
    ) -> BatchOutcome {
        assert!(
            self.owns_operation(&operation),
            "operation belongs to another application"
        );
        let repo_root = self.repository_root().await;
        if owner_id != "all" {
            let save_root = repo_root.clone();
            let owner_owned = owner_id.to_string();
            let _ = offload_review(move || {
                ReviewStorage::save_active_id(&save_root, Some(&owner_owned))
            })
            .await;
        }
        // A batch starts from a released transient context: nothing a previous turn
        // discovered or mounted survives into the findings it is about to run.
        let operation_id = operation.publication_id();
        cleanup_transient_context(self, observer.as_ref(), &operation_id).await;

        let started_at = std::time::Instant::now();

        let steps: Vec<BatchStep> = tasks
            .iter()
            .map(|member_id| BatchStep::new(member_id.clone(), member_label(member_id)))
            .collect();

        let (stream_tx, mut stream_rx) = mpsc::channel::<StreamEvent>(1);
        // The relay drains the batch channel for the whole run, so the executor
        // never parks on a full queue and every promotion reaches the client under
        // the operation of the batch.
        let relay_observer = observer.clone();
        let relay_id = operation_id.clone();
        let relay = tokio::spawn(async move {
            while let Some(event) = stream_rx.recv().await {
                if matches!(event, StreamEvent::TierEscalated { .. }) {
                    relay_observer.publish(&relay_id, ApplicationEvent::Stream(event));
                }
            }
        });
        // The whole batch shares one operation and one cancel channel: a cancel
        // delivered between members is honored instead of being lost, and the slot
        // stays registered for the batch's entire lifetime.
        let cancel_rx = operation.cancel_receiver();
        // Soft stop ends the queue gracefully after the current member: the
        // operation owns the signal, and a `model/cancel` request with
        // `mode: "soft"` raises it so the current member finishes its horizon,
        // verification, and commit before the queue stops.
        let soft_stop_rx = operation.soft_stop_receiver();

        let snapshot = self.configuration().await;
        let execution = PlanExecutionEnvironment::new(snapshot, &operation);
        let publication_model = execution.resolve_step_model(model);
        let mut executor = ReviewBatchExecutor {
            application: self,
            operation: operation.clone(),
            observer: &observer,
            repo_root: &repo_root,
            execution,
            publication_model,
            fixed_items: 0,
        };

        let report = BatchExecution::new()
            .run(&steps, &mut executor, &stream_tx, &cancel_rx, &soft_stop_rx)
            .await;

        // The last sender closes the batch channel, so the relay publishes the
        // promotions of the run before the terminal report leaves.
        drop(stream_tx);
        if let Err(err) = relay.await {
            tracing::warn!(
                operation_id = operation_id.as_str(),
                error = %err,
                "review batch escalation relay terminated unexpectedly"
            );
        }

        let executed_count = report.executed.len();
        let total_count = report.scheduled;
        let telemetry = batch_telemetry(&report, executor.fixed_items, deferred);
        let fixed_count = telemetry.fixed;
        let failed_count = telemetry.failed;
        let error = match &report.stop {
            BatchStop::Completed => None,
            BatchStop::Failed { .. } => {
                report.executed.last().and_then(|r| r.outcome.error.clone())
            }
            BatchStop::Blocked { reason, .. } => Some(reason.clone()),
            BatchStop::Cancelled => None,
            BatchStop::Stopped => None,
        };
        let stop = report.stop.clone();
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

        // The transient context is released again once the queue settles, so the
        // next run starts from the same clean basis.
        cleanup_transient_context(self, observer.as_ref(), &operation_id).await;

        tracing::info!(
            owner_id,
            executed_count,
            fixed_count,
            deferred,
            failed_count,
            ?stop,
            "Review batch finished"
        );
        observer.publish(
            &operation_id,
            ApplicationEvent::ReviewBatchFinished(ExecutedReviewBatch {
                owner_id: owner_id.into(),
                stop,
                executed_count,
                total_count,
                elapsed: started_at.elapsed(),
                error,
                fixed_count,
                deferred,
                failed_count,
            }),
        );
        outcome
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_core::models::ModelRef;
    use tauqe_core::review::{ReviewFindingTarget, ReviewFindingsTarget, ReviewIssue};
    use tauqe_core::workflow::batch::{BatchStepReport, BatchUsage};

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
            commit: None,
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
        kind: tauqe_core::review::ReviewGroupKind,
    ) -> tauqe_core::review::ReviewGroup {
        tauqe_core::review::ReviewGroup {
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
                group_of("arch-1", tauqe_core::review::ReviewGroupKind::Architectural),
                group_of("grp-simple", tauqe_core::review::ReviewGroupKind::Simple),
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
                group_of("arch-1", tauqe_core::review::ReviewGroupKind::Architectural),
                group_of("grp-simple", tauqe_core::review::ReviewGroupKind::Simple),
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
                group_of("arch-1", tauqe_core::review::ReviewGroupKind::Architectural),
                group_of("grp-simple", tauqe_core::review::ReviewGroupKind::Simple),
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
                tauqe_core::review::ReviewGroupKind::Architectural,
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

        let targeted = ReviewExecuteTarget::Finding(ReviewFindingTarget {
            review_id: "rev-queue".to_string(),
            item_id: 1,
        });
        let err = resolve_finding_selection(&session, "rev-queue", &targeted)
            .expect_err("an unplanned architecture finding is refused");
        assert_eq!(
            err.kind,
            ReviewQueueFailure::ArchitecturalGroupExecutionRequired
        );
        assert!(err.message.contains("post-analysis"));

        let selected = ReviewExecuteTarget::Findings(ReviewFindingsTarget {
            review_id: "rev-queue".to_string(),
            item_ids: vec![2, 1],
        });
        let err = resolve_finding_selection(&session, "rev-queue", &selected)
            .expect_err("the whole selection is refused");
        assert_eq!(
            err.kind,
            ReviewQueueFailure::ArchitecturalGroupExecutionRequired
        );

        let finished = ReviewExecuteTarget::Finding(ReviewFindingTarget {
            review_id: "rev-queue".to_string(),
            item_id: 3,
        });
        let err = resolve_finding_selection(&session, "rev-queue", &finished)
            .expect_err("a finished finding is not re-executed");
        assert_eq!(err.kind, ReviewQueueFailure::NothingToExecute);
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
                tauqe_core::review::ReviewGroupKind::Architectural,
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
                tauqe_core::review::ReviewGroupKind::Architectural,
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
                    outcome: BatchStepOutcome::skipped(),
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
