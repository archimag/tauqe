//! Review findings batch supervisor and execution entry point.
//!
//! `handle_review_execute` resolves the members to run (an architectural
//! group, a single session's findings, a single finding, or every actionable
//! finding across all sessions), performs a deterministic pre-flight
//! verification, and hands control to the core `BatchExecution` supervisor
//! through `ReviewBatchExecutor`. The supervisor enforces ordered, fail-fast
//! traversal; findings fixed before the first failure stay `Fixed`. Once it
//! terminates, a typed `review/batch_finished` event carries the aggregate
//! status, finding counters, and elapsed time, so clients can raise a single
//! notice for the whole batch.
//!
//! A batch member addresses either a finding or a group, and the member
//! identifier is the addressing contract: a group member carries
//! `{session}:g:{group}` and runs the group's unified subplan through
//! `group_exec`, while a finding member carries `{session}:{item}` and runs
//! its isolated turn. A finding of an architectural group is a symptom of
//! one shared root cause, so it is never executed on its own: an explicit
//! selection that names one is refused, and the implicit queues subsume the
//! findings a group collects into a single group member.
//!
//! A caller may target a single leaf sub-step by `step_id` and supply a Coder
//! model override plus developer directives.
//! A finding that reports an unconditional abort (`<abort>`) is skipped with
//! its host recommendation recorded, because the remediation lies outside
//! the repository; the Architect is never engaged for it and the queue
//! continues.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use tauqe_core::providers::StreamEvent;
use tauqe_core::review::ReviewStorage;
use tauqe_core::workflow::batch::{
    BatchExecution, BatchReport, BatchStep, BatchStepExecutor, BatchStepOutcome, BatchStop,
    BatchUsage,
};
use tauqe_protocol::{
    errors, events, BatchFinishStatus, BatchFinishSummary, IssueStatus, IssueType, ModelSelection,
    Plan, PlanScopeEvaluation, Request, Response, ResponseError, ReviewBatchFinishedEvent,
    ReviewExecuteParams, ReviewGroup, ReviewItem, ReviewSession,
};
use tokio::sync::{mpsc, watch};

use crate::handlers::common::wait_for_cancellation;
use crate::state::{AppState, OperationGuard};

use super::common::{clear_auto_context, emit, offload_review, persist_finding_update};
use super::group_exec::{
    group_step_id, parse_group_step_id, reload_group, run_group_plan_with_replans,
};
use super::step::run_review_step;

/// Outcome of a server-driven batch of review findings.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum BatchOutcome {
    Completed {
        items: usize,
    },
    Failed {
        completed: usize,
        member: FailedMember,
        reason: String,
    },
    Cancelled {
        completed: usize,
    },
    Stopped {
        completed: usize,
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
    /// An architectural group, executed through its unified subplan.
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
        if let Some((session_id, group_id)) = parse_group_step_id(step_id) {
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
            } => {
                write!(f, "finding #{} in session '{}'", item_id, session_id)
            }
            Self::Group {
                session_id,
                group_id,
            } => {
                write!(f, "review group '{}' in session '{}'", group_id, session_id)
            }
            Self::Unresolved { member_id } => write!(f, "review member '{}'", member_id),
        }
    }
}

/// Prefix of the resolution note recorded on a finding whose batch stopped.
const BATCH_STOP_NOTE_PREFIX: &str = "Batch stopped after execution failure";

/// Renders the resolution note recorded when a batch stops on a finding.
///
/// A stop is a durable fact of the session, so the diagnostic is written into
/// the finding's resolution notes. A note describing the finding itself — a
/// discussion resolution or a skip diagnostic — is preserved above the new
/// diagnostic, while a note left by an earlier stop is superseded instead of
/// accumulating.
fn failure_note(existing: Option<&str>, reason: &str) -> String {
    let diagnostic = match reason.trim() {
        "" => format!("{BATCH_STOP_NOTE_PREFIX} without diagnostics"),
        trimmed => format!("{BATCH_STOP_NOTE_PREFIX}: {trimmed}"),
    };
    match existing.map(str::trim).filter(|note| !note.is_empty()) {
        Some(note) if !note.starts_with(BATCH_STOP_NOTE_PREFIX) => {
            format!("{note}\n\n{diagnostic}")
        }
        _ => diagnostic,
    }
}

/// Records the failure that stopped a batch in the finding it addressed.
///
/// The diagnostic travels with the durable session and is published through
/// `publish_review`, so the Review tree explains the stop after the daemon
/// restarts. The write is best-effort: the failure itself is already reported
/// to the caller, so a storage problem must never replace it.
async fn record_finding_stop(
    state: &Arc<AppState>,
    repo_root: &Path,
    review_id: &str,
    item_id: u32,
    reason: &str,
) {
    let reason = reason.to_string();
    let result = persist_finding_update(
        state,
        repo_root,
        review_id,
        item_id,
        "batch stop diagnostic",
        move |it| {
            it.note = Some(failure_note(it.note.as_deref(), &reason));
        },
    )
    .await;
    if let Err(err) = result {
        tracing::warn!(
            review_id = review_id,
            item_id = item_id,
            error = %err,
            "the review batch stop diagnostic was not persisted"
        );
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
/// findings: they are symptoms of the one root cause its unified subplan
/// resolves, so running one on its own is refused and the group runs instead.
fn architectural_group_of<'a>(session: &ReviewSession, item: &'a ReviewItem) -> Option<&'a str> {
    let group_id = item.group.as_deref()?;
    let architectural = session.groups.iter().any(|group| {
        group.id == group_id && group.kind == tauqe_protocol::review::ReviewGroupKind::Architectural
    });
    architectural.then_some(group_id)
}

/// Whether an actionable architecture finding has no architectural group to be
/// planned through.
///
/// `IssueType::Architecture` is a deterministic requirement: such a finding is
/// remediated by a plan, and the only sanctioned carrier of a plan is an
/// architectural group. A finding whose effective group is `Simple`, or which
/// the stored group list does not declare at all, has no plan carrier, so it is
/// never executed as one isolated Coder turn: the post-analysis stage must
/// regroup it first.
fn is_unplanned_architecture_finding(session: &ReviewSession, item: &ReviewItem) -> bool {
    is_actionable(item)
        && item.issue_type == IssueType::Architecture
        && architectural_group_of(session, item).is_none()
}

/// Builds the typed refusal of a finding that must be planned before it is
/// executed.
///
/// The refusal names the missing stage instead of reporting an execution
/// failure: grouping is the duty of the post-analysis, not a manual
/// decomposition ritual performed by the developer.
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
///
/// A queue emptied by architecture findings is not an empty queue: the findings
/// exist, they simply have no plan carrier yet. The diagnostic then names the
/// missing post-analysis instead of reporting a session with no work.
fn empty_queue_message(scope: &str, pending_architecture: usize) -> String {
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
///
/// The ids are kept verbatim in the given order; ids that are unknown,
/// duplicated, or no longer actionable are dropped instead of guessed at, so
/// the queue runs exactly the findings the client saw in the section it
/// launched.
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

/// Evaluates a targeted sub-step of a plan-backed remediation horizon and
/// maps a stall onto the typed protocol error the request is refused with.
///
/// Both horizons a batch may target — a finding's embedded subplan and a
/// group's unified plan — are walks over a [`Plan`], so the readiness verdict
/// is derived once here through the shared scope evaluator instead of being
/// re-implemented per subject.
fn validate_plan_step_target(
    plan: &Plan,
    step_id: &str,
    subject: &str,
) -> Result<(), ResponseError> {
    let target = plan.find_item(step_id).ok_or_else(|| {
        response_error(
            errors::NOT_FOUND,
            format!("Sub-step '{}' not found in {}", step_id, subject),
        )
    })?;
    if !target.is_leaf() {
        return Err(response_error(
            errors::INVALID_PARAMS,
            format!(
                "Sub-step '{}' is not a leaf item and cannot be executed directly",
                target.id
            ),
        ));
    }
    let step_id = target.id.clone();

    match plan.evaluate_scope(Some(step_id.as_str())) {
        PlanScopeEvaluation::Ready(_) => Ok(()),
        PlanScopeEvaluation::AllCompleted => Err(response_error(
            errors::NOTHING_TO_EXECUTE,
            format!("Sub-step '{}' is already DONE", step_id),
        )),
        PlanScopeEvaluation::Cancelled => Err(response_error(
            errors::NOTHING_TO_EXECUTE,
            format!("Sub-step '{}' is CANCELLED and cannot be executed", step_id),
        )),
        PlanScopeEvaluation::StepBlocked => Err(response_error(
            errors::STEP_BLOCKED,
            format!("Sub-step '{}' is BLOCKED and cannot be executed", step_id),
        )),
        PlanScopeEvaluation::BlockedByDependencies(blocked) => Err(response_error(
            errors::STEP_BLOCKED_BY_DEPENDENCIES,
            format!(
                "Sub-step '{}' depends on unfinished sub-step(s): {}",
                step_id,
                blocked.join(", ")
            ),
        )),
        PlanScopeEvaluation::BlockedByBlockers(blocked) => Err(response_error(
            errors::STEP_BLOCKED,
            format!(
                "Sub-step '{}' cannot run while blocked sub-step(s) remain: {}",
                step_id,
                blocked.join(", ")
            ),
        )),
        PlanScopeEvaluation::BlockedByDiscussion(discussion) => Err(response_error(
            errors::STEP_IN_DISCUSSION,
            format!(
                "Sub-step '{}' is in DISCUSSION status and cannot be executed until reviewed: {}",
                step_id,
                discussion.join(", ")
            ),
        )),
        PlanScopeEvaluation::NotFound => Err(response_error(
            errors::NOT_FOUND,
            format!("Sub-step '{}' not found in {}", step_id, subject),
        )),
    }
}

/// Validates that a targeted sub-step of a group's unified subplan exists,
/// is an executable leaf, and is ready to run before a group batch starts.
///
/// A group without a subplan is synthesized just in time by the Architect,
/// so a step target cannot be validated against a horizon that does not
/// exist yet: the request is refused instead of deferring the verdict to
/// the walk.
fn validate_group_step_target(group: &ReviewGroup, step_id: &str) -> Result<(), ResponseError> {
    if group.subplan.is_empty() {
        return Err(response_error(
            errors::INVALID_PARAMS,
            format!(
                "Review group '{}' has no subplan yet; cannot target sub-step '{}'",
                group.id, step_id
            ),
        ));
    }
    let plan = Plan {
        items: group.subplan.clone(),
        ..Default::default()
    };
    validate_plan_step_target(&plan, step_id, &format!("review group '{}'", group.id))
}

/// Enumerates the batch members one session contributes to an implicit queue.
///
/// An architectural group is scheduled once through its unified subplan,
/// subsuming the actionable findings it collects; every other actionable
/// finding is scheduled on its own. The group member takes the position of
/// the group's first actionable finding, so the traversal order a client saw
/// in its section is preserved.
///
/// An architecture finding with no architectural group is scheduled by neither
/// form: it must be planned before it is executed, so the post-analysis owns it
/// until it is regrouped.
fn session_members(session: &ReviewSession) -> Vec<String> {
    let mut members: Vec<String> = Vec::new();
    let mut scheduled_groups: HashSet<String> = HashSet::new();
    for item in session.items.iter().filter(|item| is_actionable(item)) {
        if let Some(group_id) = architectural_group_of(session, item) {
            let member = group_step_id(&session.id, group_id);
            if scheduled_groups.insert(member.clone()) {
                members.push(member);
            }
            continue;
        }
        if is_unplanned_architecture_finding(session, item) {
            continue;
        }
        members.push(format!("{}:{}", session.id, item.id));
    }
    members
}

/// Accumulates the implicit queue every stored session contributes.
///
/// The queue is the concatenation of the per-session queues in stored order, so
/// a cross-session run preserves the order a client rendered. The addressed
/// findings travel back beside it, and architecture findings that no
/// architectural group can plan are counted instead of scheduled: they are the
/// reason a session with actionable findings can contribute an empty queue.
fn accumulate_implicit_queues(sessions: &[ReviewSession]) -> (Vec<String>, Vec<u32>, usize) {
    let mut tasks: Vec<String> = Vec::new();
    let mut item_ids: Vec<u32> = Vec::new();
    let mut pending_architecture = 0;
    for session in sessions {
        for item in &session.items {
            if is_unplanned_architecture_finding(session, item) {
                pending_architecture += 1;
            } else if is_actionable(item) {
                item_ids.push(item.id);
            }
        }
        tasks.extend(session_members(session));
    }
    (tasks, item_ids, pending_architecture)
}

/// Renders the supervisor diagnostic label of one batch member.
fn member_label(member_id: &str, step_id: Option<&str>) -> String {
    if let Some((session_id, group_id)) = parse_group_step_id(member_id) {
        return match step_id {
            Some(step) => format!(
                "Fix sub-step '{}' of review group '{}' in session '{}'",
                step, group_id, session_id
            ),
            None => format!(
                "Fix review group '{}' in session '{}'",
                group_id, session_id
            ),
        };
    }
    let Some((session_id, item_id)) = member_id.split_once(':') else {
        return format!("Execute review member '{}'", member_id);
    };
    match step_id {
        Some(step) => format!(
            "Fix sub-step '{}' of review finding #{} in session '{}",
            step, item_id, session_id
        ),
        None => format!(
            "Fix review finding #{} in session '{}'",
            item_id, session_id
        ),
    }
}
/// A resolved `review/execute` selection: the batch members to run, the
/// finding ids the run addresses, and the optional targeted sub-step.
type ResolvedSelection = Result<(Vec<String>, Vec<u32>, Option<String>), ResponseError>;
/// Resolves the members a group-scoped request runs.
///
/// A group run addresses the group itself: its unified subplan is the unit of
/// work, `step_id` names a step of that subplan, and no finding of the
/// session is scheduled beside it. The findings the group collects are the
/// work the run addresses, so they travel back as the addressed finding ids.
fn resolve_group_selection(
    session: &ReviewSession,
    review_id: &str,
    group_id: &str,
    params: &ReviewExecuteParams,
) -> ResolvedSelection {
    let group = match session.groups.iter().find(|group| group.id == group_id) {
        Some(group) => group,
        None => {
            return Err(response_error(
                errors::NOT_FOUND,
                format!(
                    "Review group '{}' not found in session '{}'",
                    group_id, review_id
                ),
            ));
        }
    };
    let step_id = match params
        .step_id
        .as_deref()
        .map(str::trim)
        .filter(|step| !step.is_empty())
    {
        Some(step) => {
            validate_group_step_target(group, step)?;
            Some(step.to_string())
        }
        None => None,
    };
    let item_ids: Vec<u32> = session
        .items
        .iter()
        .filter(|item| item.group.as_deref() == Some(group_id))
        .map(|item| item.id)
        .collect();
    Ok((vec![group_step_id(review_id, group_id)], item_ids, step_id))
}

/// Resolves the members a finding-scoped request runs.
///
/// Returns the batch members, the addressed finding ids, and the optional
/// targeted sub-step of a finding's embedded subplan. The architectural
/// dichotomy is enforced here: a finding of an architectural group is a
/// symptom of one root cause, so an explicit selection that names one is
/// refused, while the implicit queues subsume such findings into a single
/// group member.
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
            // A symptom of an architectural group is never executed on its
            // own: the group's unified subplan is the unit of remediation.
            if let Some(group_id) = architectural_group_of(session, item) {
                return Err(response_error(
                    errors::ARCHITECTURAL_GROUP_EXECUTION_REQUIRED,
                    format!(
                        "Review item #{} is a symptom of architectural group '{}' and cannot be executed on its own; execute the group instead",
                        item.id, group_id
                    ),
                ));
            }

            // An architecture finding is planned before it is executed, and
            // only an architectural group carries a plan: without one the
            // finding is refused here instead of being run as a bare turn.
            if is_unplanned_architecture_finding(session, item) {
                return Err(unplanned_architecture_error(item));
            }
            // #c-1/#c-2: a review finding executes as one isolated turn and owns
            // no subplan, so a targeted sub-step can only name a step of an
            // architectural group horizon.
            if let Some(step) = params
                .step_id
                .as_deref()
                .map(str::trim)
                .filter(|step| !step.is_empty())
            {
                return Err(response_error(
                    errors::INVALID_PARAMS,
                    format!(
                        "Sub-step '{}' cannot be targeted on review item #{}: a review finding executes as one isolated turn and owns no subplan",
                        step, item.id
                    ),
                ));
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
                    format!(
                        "Review item #{} is REJECTED and cannot be executed",
                        item.id
                    ),
                )),
                IssueStatus::Fixed => Err(response_error(
                    errors::NOTHING_TO_EXECUTE,
                    format!("Review item #{} is already FIXED", item.id),
                )),
                IssueStatus::Skipped => Err(response_error(
                    errors::NOTHING_TO_EXECUTE,
                    format!("Review item #{} is already SKIPPED", item.id),
                )),
                IssueStatus::Todo | IssueStatus::InProgress => {
                    Ok((vec![member_of(item_id)], vec![item_id], None))
                }
            }
        }
        None => {
            if params.step_id.is_some() {
                return Err(response_error(
                    errors::INVALID_PARAMS,
                    "step_id requires both review_id and item_id",
                ));
            }
            match params.item_ids.as_deref() {
                Some(selected) => {
                    for id in selected {
                        // An explicit selection names findings, and a symptom
                        // of an architectural group is not one of them: the
                        // whole selection is refused instead of silently
                        // dropping work the client believes it scheduled.
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
                            // An architecture finding without an architectural
                            // group has no plan carrier, so scheduling it as a
                            // bare turn is refused for the whole selection.
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
                    let tasks = ids.iter().map(|id| member_of(*id)).collect();
                    Ok((tasks, ids, None))
                }
                None => {
                    // The implicit queue subsumes the findings an
                    // architectural group collects into one group member,
                    // while every other actionable finding runs on its own.
                    // An architecture finding that no architectural group can
                    // plan is scheduled by neither form, so a session made only
                    // of such findings yields an empty queue and is refused
                    // with a diagnostic naming the missing post-analysis.
                    let (tasks, item_ids, pending_architecture) =
                        accumulate_implicit_queues(std::slice::from_ref(session));
                    if tasks.is_empty() {
                        return Err(response_error(
                            errors::NOTHING_TO_EXECUTE,
                            empty_queue_message("in this review session", pending_architecture),
                        ));
                    }
                    Ok((tasks, item_ids, None))
                }
            }
        }
    }
}

/// Adapter that executes one review batch member as a [`BatchStepExecutor`]
/// step: a finding through its isolated turn, or an architectural group
/// through its unified subplan.
///
/// The member identifier is the addressing contract: it is minted by
/// `handle_review_execute` and decoded here through the same sanctioned
/// parsers, so a group member never reaches the finding path. Finding status
/// synchronization (InProgress/Fixed/Todo) and file preloading stay inside
/// the executor, keeping the batch supervisor agnostic of review storage.
/// Each finding member carries `{session}:{item}` to allow seamless traversal
/// across multiple review sessions; a group member carries
/// `{session}:g:{group}`.
struct ReviewBatchExecutor<'a> {
    state: &'a Arc<AppState>,
    repo_root: &'a Path,
    operation: &'a Arc<OperationGuard>,
    step_id: Option<&'a str>,
    model: Option<ModelSelection>,
    instructions: Option<String>,
}

impl ReviewBatchExecutor<'_> {
    /// Executes one architectural group through its unified subplan.
    ///
    /// The walk itself is owned by `group_exec`: just-in-time Architect
    /// synthesis, ordered leaf traversal, and reprojecting replans around a
    /// failed sub-step. The executor contributes the batch framing only: it
    /// switches the active session, decodes the durable group state, and
    /// maps the walk's verdict onto the supervisor's step outcome.
    ///
    /// A walk that exhausted its replanning budget leaves the group's
    /// findings in `Todo` with the interruption note, so the step reports the
    /// failure instead of claiming a completion the durable record does not
    /// hold.
    async fn execute_group(
        &self,
        review_id: &str,
        group_id: &str,
        cancel_rx: &watch::Receiver<bool>,
    ) -> BatchStepOutcome {
        // Switch the active review session to match the group being executed.
        let save_root = self.repo_root.to_path_buf();
        let review_id_owned = review_id.to_string();
        let _ = offload_review(move || {
            ReviewStorage::save_active_id(&save_root, Some(&review_id_owned))
        })
        .await;

        let (session, group) = match reload_group(self.repo_root, review_id, group_id).await {
            Ok(loaded) => loaded,
            Err(failure) => return BatchStepOutcome::failed(failure.into_reason()),
        };

        let walked = run_group_plan_with_replans(
            self.state,
            self.repo_root,
            review_id,
            &session,
            &group,
            self.step_id,
            self.model.clone(),
            self.instructions.clone(),
            Arc::clone(self.operation),
        )
        .await;

        match walked {
            Ok(()) => BatchStepOutcome::succeeded(),
            Err(failure) => {
                // A failure that coincides with the batch's cancel signal is
                // a cancelled step, not a failed one: the supervisor then
                // stops the queue as `Cancelled` instead of reporting a group
                // that never had the chance to run.
                if *cancel_rx.borrow() {
                    return BatchStepOutcome::cancelled();
                }
                BatchStepOutcome::failed(failure.into_reason())
            }
        }
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
        // A group member addresses the group's unified subplan: it is decoded
        // and executed as one, never as a finding.
        if let Some((review_id, group_id)) = parse_group_step_id(&step.id) {
            return Ok(self.execute_group(review_id, group_id, &cancel_rx).await);
        }

        let (session_id, item_id) = if let Some((sess, item_part)) = step.id.split_once(':') {
            match item_part.parse::<u32>() {
                Ok(id) => (sess.to_string(), id),
                Err(_) => {
                    return Ok(BatchStepOutcome::failed(format!(
                        "Invalid review item id: {}",
                        step.id
                    )));
                }
            }
        } else {
            return Ok(BatchStepOutcome::failed(format!(
                "Invalid review step id (missing session prefix): {}",
                step.id
            )));
        };

        // Switch the active review session to match the finding being executed.
        let save_root = self.repo_root.to_path_buf();
        let session_id_owned = session_id.clone();
        let _ = offload_review(move || {
            ReviewStorage::save_active_id(&save_root, Some(&session_id_owned))
        })
        .await;

        match run_review_step(
            self.state,
            self.repo_root,
            &session_id,
            item_id,
            self.model.clone(),
            self.instructions.clone(),
            &cancel_rx,
            Arc::clone(self.operation),
        )
        .await
        {
            Ok(()) => Ok(BatchStepOutcome::succeeded()),
            Err(failure) => {
                if failure.is_domain() {
                    // The finding is already marked `Skipped` with diagnostics.
                    // Report the step as successful so the supervisor advances
                    // to the next finding instead of aborting the queue. An
                    // unconditional abort travels this same path: its
                    // recommendation is stored on the finding and no Architect
                    // replanning is engaged for it.
                    Ok(BatchStepOutcome::succeeded())
                } else {
                    // A failure that coincides with the batch's cancel signal is
                    // a cancelled step, not a failed one: the supervisor then
                    // stops the queue as `Cancelled` instead of reporting a
                    // finding that never had a chance to run.
                    let cancelled = *cancel_rx.borrow();
                    if cancelled {
                        return Ok(BatchStepOutcome::cancelled());
                    }
                    // The stop is a durable fact of the session: the diagnostic
                    // is recorded on the finding it stopped before the queue
                    // reports the failure, so the Review tree explains the stop
                    // after the daemon restarts.
                    let reason = failure.into_reason();
                    record_finding_stop(self.state, self.repo_root, &session_id, item_id, &reason)
                        .await;
                    Ok(BatchStepOutcome::failed(reason))
                }
            }
        }
    }
}

/// Runs the review members through the core [`BatchExecution`] supervisor
/// with explicit execution options (targeted sub-step, model override,
/// directives).
///
/// Ordered, fail-fast traversal is preserved; findings fixed before the
/// first failure stay FIXED. Once the supervisor terminates, a typed
/// `review/batch_finished` event carries the aggregate status, finding
/// counters, and elapsed time, so clients can raise a single notice for the
/// whole batch.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_review_batch_with_options(
    state: &Arc<AppState>,
    owner_id: &str,
    tasks: Vec<String>,
    step_id: Option<String>,
    model: Option<ModelSelection>,
    instructions: Option<String>,
    operation: Arc<OperationGuard>,
) -> BatchOutcome {
    let repo_root = state.context.read().await.repo_root().to_path_buf();
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
        .map(|member_id| {
            BatchStep::new(
                member_id.clone(),
                member_label(member_id, step_id.as_deref()),
            )
        })
        .collect();

    let (stream_tx, _stream_rx) = mpsc::channel::<StreamEvent>(1);
    // The whole batch shares one operation and one cancel channel: a cancel
    // delivered between members is honored instead of being lost, and the
    // slot stays registered for the batch's entire lifetime.
    let cancel_rx = operation.cancel_receiver();
    // Soft stop ends the queue gracefully after the current member: the
    // operation owns the signal, and a `model/cancel` request with
    // `mode: "soft"` raises it so the current member finishes its turn,
    // verification, and commit before the queue stops.
    let soft_stop_rx = operation.soft_stop_receiver();

    let mut executor = ReviewBatchExecutor {
        state,
        repo_root: &repo_root,
        operation: &operation,
        step_id: step_id.as_deref(),
        model,
        instructions,
    };

    // A cancel delivered while a member runs must unwind the queue at once.
    // The member itself observes the signal at its own phase boundaries and
    // kills the verification subprocess; this race is the guarantee that the
    // batch never waits for the remainder of a long verification. An
    // abandoned finding is never marked FIXED and never committed, and its
    // checkpoint is unwound by the existing rollback contour.
    let report = {
        let batch = BatchExecution::new();
        let run = batch.run(&steps, &mut executor, &stream_tx, &cancel_rx, &soft_stop_rx);
        tokio::pin!(run);
        tokio::select! {
            biased;
            report = &mut run => Some(report),
            _ = wait_for_cancellation(cancel_rx.clone()) => None,
        }
    };
    // An unwound run carries no step result, so it publishes the same
    // terminal report as a batch cancelled between members.
    let report = report.unwrap_or_else(|| BatchReport {
        executed: Vec::new(),
        scheduled: steps.len(),
        stop: BatchStop::Cancelled,
        usage: BatchUsage::default(),
    });

    let executed_count = report.executed.len();
    let total_count = report.scheduled;
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
        },
        BatchStop::Cancelled => BatchOutcome::Cancelled {
            completed: executed_count,
        },
        BatchStop::Stopped => BatchOutcome::Stopped {
            completed: executed_count,
        },
        BatchStop::Failed { step_id } => {
            let completed = executed_count.saturating_sub(1);
            // The stop is attributed to the member the supervisor reports,
            // through the same identifier the batch was minted with: a group
            // is named as a group and never as a finding number.
            let member = FailedMember::decode(&step_id);
            let reason = error
                .clone()
                .unwrap_or_else(|| "batch member failed without diagnostics".to_string());
            BatchOutcome::Failed {
                completed,
                member,
                reason,
            }
        }
        BatchStop::Blocked { step_id, reason } => {
            // Review findings have no blocked status; a semantic block stops
            // the batch and is reported as a failed member.
            let completed = executed_count.saturating_sub(1);
            let member = FailedMember::decode(&step_id);
            BatchOutcome::Failed {
                completed,
                member,
                reason,
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
            },
        })
        .ok(),
    );

    outcome
}

/// Convenience entry point used by tests: runs a batch of findings without
/// targeted sub-step selection, model override, or developer directives.
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
    run_review_batch_with_options(state, owner_id, members, None, None, None, operation).await
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

    let (owner_id, tasks, item_ids, step_id) = match params.review_id.as_deref() {
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

            // The dichotomy inside one session: a request resolves either
            // the group it names or a finding selection, and every refusal
            // happens here, before the operation slot is claimed.
            let selection = match params.group_id.as_deref() {
                Some(group_id) => resolve_group_selection(&session, review_id, group_id, &params),
                None => resolve_finding_selection(&session, review_id, &params),
            };
            match selection {
                Ok((members, addressed, target_step)) => {
                    (review_id.to_string(), members, addressed, target_step)
                }
                Err(err) => {
                    return Response {
                        id: req.id,
                        result: None,
                        error: Some(err),
                    }
                }
            }
        }
        None => {
            if params.item_id.is_some()
                || params.step_id.is_some()
                || params.item_ids.is_some()
                || params.group_id.is_some()
            {
                return Response::err(
                    req.id,
                    errors::INVALID_PARAMS,
                    "item_id, step_id, item_ids, and group_id cannot be specified when review_id is omitted",
                );
            }
            let list_root = repo_root.clone();
            let sessions = match offload_review(move || ReviewStorage::load_all(&list_root)).await {
                Ok(s) => s,
                Err(err) => return Response::err(req.id, errors::REVIEW_LOAD_FAILED, err),
            };
            // Every session contributes its implicit queue: architectural
            // groups are scheduled once through their unified subplans,
            // subsuming the findings they collect, while every other
            // actionable finding runs on its own.
            // An architecture finding that no architectural group can plan is
            // scheduled by neither form, so a queue made only of such findings
            // is refused with a diagnostic naming the missing post-analysis.
            let (tasks, item_ids, pending_architecture) = accumulate_implicit_queues(&sessions);
            if tasks.is_empty() {
                return Response::err(
                    req.id,
                    errors::NOTHING_TO_EXECUTE,
                    empty_queue_message("across all review sessions", pending_architecture),
                );
            }
            ("all".to_string(), tasks, item_ids, None)
        }
    };

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
    if let Err(report) = crate::handlers::run_preflight_verification(&repo_root).await {
        let message = format!(
            "Pre-flight verification failed: the repository does not build cleanly. Batch execution was blocked before starting any finding.\n\n{}",
            report
        );
        return Response::err(req.id, errors::PREFLIGHT_VERIFICATION_FAILED, message);
    }

    // Execution hands control to the Coder role: the discussion focus is
    // reset unconditionally so no discussion anchor survives into code
    // generation.
    state.clear_discussion_focus().await;

    let model_selection = params.model.clone();
    let developer_instructions = params.instructions.clone();
    // The registered operation runs the whole batch, so its identity travels
    // with the acknowledgement and every event of the run, including the
    // terminal `review/batch_finished`, carries the same identifier.
    let response = Response::ok(
        req.id,
        serde_json::json!({
            "review_id": params.review_id,
            "item_ids": item_ids,
            "operation_id": format!("op-{}", operation.id()),
        }),
    );

    let state = Arc::clone(state);
    tokio::spawn(async move {
        match run_review_batch_with_options(
            &state,
            &owner_id,
            tasks,
            step_id,
            model_selection,
            developer_instructions,
            operation,
        )
        .await
        {
            BatchOutcome::Completed { items } => {
                tracing::info!(
                    "Review batch '{}' completed: {} finding(s)",
                    owner_id,
                    items
                );
            }
            BatchOutcome::Failed {
                completed,
                member,
                reason,
            } => {
                tracing::warn!(
                    "Review batch '{}' stopped at {} after {} fixed finding(s): {}",
                    owner_id,
                    member,
                    completed,
                    reason
                );
            }
            BatchOutcome::Cancelled { completed } => {
                tracing::warn!(
                    "Review batch '{}' cancelled after {} fixed finding(s)",
                    owner_id,
                    completed
                );
            }
            BatchOutcome::Stopped { completed } => {
                tracing::info!(
                    "Review batch '{}' stopped gracefully after {} fixed finding(s)",
                    owner_id,
                    completed
                );
            }
        }
    });

    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::{IssueType, ModelRef, ReviewIssue};

    /// Builds a finding of the given semantic category.
    ///
    /// The category decides whether a finding may be executed as a bare turn,
    /// so every fixture names it explicitly instead of inheriting one by
    /// accident.
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

    /// Builds an architecture finding: it must be planned before execution, so
    /// only an architectural group may carry it.
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

    fn group_of(id: &str, kind: tauqe_protocol::review::ReviewGroupKind) -> ReviewGroup {
        ReviewGroup {
            id: id.to_string(),
            title: format!("Group {id}"),
            kind,
            ..Default::default()
        }
    }

    /// The queue resolver is the contract between the section-scoped launch
    /// and the supervisor: client order wins, stale and unknown ids are
    /// dropped, and duplicates collapse to their first occurrence, so the
    /// batch runs exactly the findings the confirmation showed.
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

    /// The implicit queue subsumes the findings an architectural group
    /// collects into a single group member, while a simple group's findings
    /// and ungrouped findings stay individually scheduled.
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
            session_members(&session),
            vec![
                "rev-x:g:arch-1".to_string(),
                "rev-x:3".to_string(),
                "rev-x:4".to_string(),
                "rev-x:6".to_string(),
            ]
        );
    }

    /// Only the stored `Architectural` kind subsumes a finding: a simple or
    /// unknown group leaves it individually executable.
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

    /// An architecture finding an architectural group carries is subsumed by
    /// the group member, while an architecture finding with no such group is
    /// scheduled by neither form: the post-analysis must regroup it before it
    /// can be executed.
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
            session_members(&session),
            vec!["rev-arch:g:arch-1".to_string(), "rev-arch:7".to_string()]
        );
    }

    /// The implicit session queue schedules the findings it can execute, so an
    /// unplanned architecture finding appears in neither the members nor the
    /// addressed findings.
    #[test]
    fn implicit_session_queue_skips_unplanned_architecture_findings() {
        let session = ReviewSession {
            id: "rev-queue".to_string(),
            groups: vec![group_of(
                "arch-1",
                tauqe_protocol::review::ReviewGroupKind::Architectural,
            )],
            items: vec![
                architecture_finding(1, IssueStatus::Todo, Some("arch-1")),
                finding(2, IssueStatus::Todo),
                architecture_finding(3, IssueStatus::Todo, None),
            ],
            ..Default::default()
        };
        let params = ReviewExecuteParams {
            review_id: Some("rev-queue".to_string()),
            ..Default::default()
        };

        let (tasks, item_ids, step) = resolve_finding_selection(&session, "rev-queue", &params)
            .expect("the queued findings are executable");

        assert_eq!(
            tasks,
            vec!["rev-queue:g:arch-1".to_string(), "rev-queue:2".to_string()]
        );
        assert_eq!(item_ids, vec![1, 2]);
        assert!(step.is_none());
    }

    /// A session whose only actionable findings are architecture findings
    /// without an architectural group has nothing the queue can schedule, so
    /// it is refused instead of being dispatched with zero members.
    #[test]
    fn implicit_session_queue_refuses_a_session_of_unplanned_architecture_findings() {
        let session = ReviewSession {
            id: "rev-queue".to_string(),
            groups: vec![group_of(
                "grp-simple",
                tauqe_protocol::review::ReviewGroupKind::Simple,
            )],
            items: vec![
                architecture_finding(1, IssueStatus::Todo, None),
                architecture_finding(2, IssueStatus::InProgress, Some("grp-simple")),
                finding(3, IssueStatus::Fixed),
            ],
            ..Default::default()
        };
        let params = ReviewExecuteParams::default();

        let err = resolve_finding_selection(&session, "rev-queue", &params)
            .expect_err("the queue has nothing to execute");

        assert_eq!(err.code, errors::NOTHING_TO_EXECUTE);
        assert!(
            err.message.contains("2 ARCHITECTURE"),
            "the diagnostic must count the pending findings: {}",
            err.message
        );
        assert!(
            err.message.contains("post-analysis"),
            "the diagnostic must name the missing stage: {}",
            err.message
        );
    }

    /// The cross-session queue concatenates the per-session queues in stored
    /// order, reports the findings the run addresses, and counts the
    /// architecture findings that no architectural group can plan.
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

        let (tasks, item_ids, pending_architecture) =
            accumulate_implicit_queues(&[clustered, plain]);

        assert_eq!(
            tasks,
            vec![
                "rev-a:g:arch-1".to_string(),
                "rev-a:2".to_string(),
                "rev-b:4".to_string()
            ]
        );
        assert_eq!(item_ids, vec![1, 2, 4]);
        assert_eq!(pending_architecture, 1);
    }

    /// An architecture finding is planned before it is executed, so an explicit
    /// selection of one that has no architectural group is refused with the
    /// typed error directing the developer to the post-analysis, while a
    /// finished finding of the same type keeps reporting its terminal status.
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
        assert!(
            err.message.contains("post-analysis"),
            "the diagnostic must name the missing stage: {}",
            err.message
        );

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

    /// A group member carries no finding ordinal, so it is decoded as the group
    /// it names instead of a finding number that never existed.
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

    /// A finding member is decoded by the audit ordinal the batch was addressed
    /// with.
    #[test]
    fn failed_member_decodes_a_finding_member() {
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
    }

    /// An identifier the harness never minted keeps its text instead of
    /// borrowing the ordinal of an unrelated member.
    #[test]
    fn failed_member_keeps_an_unrecognized_identifier() {
        let member = FailedMember::decode("rev-42:g:");
        assert_eq!(
            member,
            FailedMember::Unresolved {
                member_id: "rev-42:g:".to_string(),
            }
        );
        assert_eq!(member.to_string(), "review member 'rev-42:g:'");
    }
    /// A batch stop is recorded on the finding it stopped: the diagnostic
    /// names the failure, a note describing the finding itself survives above
    /// it, and an earlier stop is superseded instead of accumulating.
    #[test]
    fn failure_note_records_the_stop_and_preserves_the_finding_note() {
        let fresh = failure_note(None, "provider error");
        assert!(fresh.starts_with(BATCH_STOP_NOTE_PREFIX));
        assert!(fresh.contains("provider error"));

        let silent = failure_note(Some("   "), "  ");
        assert_eq!(
            silent,
            format!("{BATCH_STOP_NOTE_PREFIX} without diagnostics")
        );

        let preserved = failure_note(Some("Keep the signature stable."), "provider error");
        assert!(preserved.starts_with("Keep the signature stable."));
        assert!(preserved.contains("provider error"));

        let superseded = failure_note(Some(&fresh), "second failure");
        assert!(superseded.contains("second failure"));
        assert!(!superseded.contains("provider error"));
    }
}
