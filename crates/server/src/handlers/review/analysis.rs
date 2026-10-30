//! Cross-group post-analysis of a review session.
//!
//! A grouped audit splits the repository into isolated slices and every group
//! reports its own findings, so a defect that repeats across slices stays
//! invisible inside any single group. Post-analysis is the Architect role's
//! whole-session view: it reads every finding of the session together and names
//! the systemic root causes behind them.
//!
//! Two invariants shape the orchestration:
//!
//! * The classification is the session itself, not a projection beside it.
//!   Findings keep their identity, their content, and their status, while
//!   `ReviewStorage::regroup_session` substitutes the session groups and
//!   reassigns every finding in place, so re-running the analysis never rescans
//!   the audited code and never rewrites a finding.
//!   the audited code and never rewrites a finding.
//! * A finding whose type requires planning before execution never lands in the
//!   group of independent findings: the layer mints a single-element
//!   architectural group for it, so the requirement holds regardless of how the
//!   classification described the finding.
//!   holds. A hallucinated identifier is a typed refusal instead of a group that
//!   would render a finding nothing can name.
//!
//! The decoded payload, its strict schema, and the identity validation live in
//! the core engine (`tauqe_core::review::analysis`), so this module owns only the
//! orchestration: provider resolution, streaming, persistence, publication, and
//! the transcript record.
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use tauqe_core::edits::stream::json::JsonFieldStreamFilter;
use tauqe_core::providers::{format_session_id, ChatMessage, StreamEvent, SESSION_PREFIX_REVIEW};
use tauqe_core::review::analysis::{
    parse_post_analysis, post_analysis_response_format, validate_post_analysis, PostAnalysisOutput,
};
use tauqe_core::review::prompt::analysis::{
    build_post_analysis_system_prompt, build_post_analysis_user_prompt,
};
use tauqe_core::review::{uuid::generate_uuid_v7, ReviewStorage};
use tauqe_protocol::model::{ReviewWorkPhase, TurnWorkContext, TurnWorkQueue};
use tauqe_protocol::review::{
    ReviewGroup, ReviewGroupKind, LOCAL_REVIEW_GROUP_ID, LOCAL_REVIEW_GROUP_TITLE,
};
use tauqe_protocol::{
    errors, events, IssueType, ModelRole, ModelStartedEvent, ModelUsageEvent, ModelUsageInfo,
    Request, Response, ResponseError, ReviewAnalyzeFinishedEvent, ReviewAnalyzeParams,
    ReviewAnalyzeResult, ReviewAnalyzeStartedEvent, ReviewContentDeltaEvent,
    ReviewReasoningDeltaEvent, ReviewSession,
};
use tokio::sync::mpsc;

use crate::handlers::architect::resolve_architect_provider;
use crate::handlers::common::{record_synthesis_history, withdraw_streamed_attempt};
use crate::state::{AppState, OperationGuard};

use super::common::{emit, offload_review, publish_review};

/// Handles `review/analyze`: classifies the findings of one stored session into
/// independent issues and architectural root-cause groups.
///
/// The session identifier is mandatory and never defaulted, so a run can only
/// classify the session the caller named. The findings themselves are immutable
/// input: the request replaces the classification layer and answers with the
/// session carrying it, so the client renders the new layer without a follow-up
/// fetch.
///
/// A session with nothing to classify is refused by the shared procedure before
/// any provider call, exactly as a session that cannot be resolved is refused
/// here.
pub async fn handle_review_analyze(req: Request, state: &Arc<AppState>) -> Response {
    let params: ReviewAnalyzeParams = match req.params {
        Some(params) => match serde_json::from_value(params) {
            Ok(params) => params,
            Err(err) => {
                return Response::err(
                    req.id,
                    errors::INVALID_PARAMS,
                    format!("Invalid review analyze parameters: {}", err),
                )
            }
        },
        None => {
            return Response::err(
                req.id,
                errors::INVALID_PARAMS,
                "Missing or invalid review analyze params",
            )
        }
    };

    // The identifier is never defaulted to the active session: an implicit target
    // would rewrite the classification of whichever session happened to be
    // focused, so the caller always names its subject.
    let review_id = params.review_id.trim().to_string();
    if review_id.is_empty() {
        return Response::err(
            req.id,
            errors::INVALID_PARAMS,
            "review_id is required: post-analysis is scoped to an explicit session",
        );
    }

    let repo_root = state.context.read().await.repo_root().to_path_buf();
    let load_root = repo_root.clone();
    let load_id = review_id.clone();
    let session =
        match offload_review(move || ReviewStorage::load_review(&load_root, &load_id)).await {
            Ok(Some(session)) => session,
            Ok(None) => {
                return Response::err(
                    req.id,
                    errors::NOT_FOUND,
                    format!("Review session '{}' not found", review_id),
                )
            }
            Err(err) => return Response::err(req.id, errors::REVIEW_LOAD_FAILED, err),
        };

    // Post-analysis mutates the stored session, so it takes the single operation
    // slot: a concurrent turn or batch must never rewrite the same layer.
    let operation = match state.try_begin_operation() {
        Ok(guard) => guard,
        Err(()) => {
            return Response::err(
                req.id,
                errors::OPERATION_IN_PROGRESS,
                "Another operation is already running",
            )
        }
    };

    match run_review_post_analysis(state, &repo_root, &session, &operation, false).await {
        Ok(session) => Response::ok_typed(req.id, &ReviewAnalyzeResult { session }),
        Err(err) => Response {
            id: req.id,
            result: None,
            error: Some(err),
        },
    }
}

/// Runs the Architect post-analysis over one review session and persists the
/// resulting classification layer.
///
/// Shared by the `review/analyze` handler and the automatic classification that
/// follows a completed grouped audit, so both entry points resolve the model,
/// stream, validate, persist, and publish through exactly one implementation.
///
/// Post-analysis regroups the session in place. It reads the session as stored,
/// substitutes its groups, and rewrites the group of every finding, and therefore
/// never rescans the audited code and never rewrites a finding's content. The one
/// exception is the identity: a finding that carries none is healed with a UUIDv7
/// before the prompt is built, because a classification can only reference what
/// the stored session can name.
///
/// `is_batch` declares the execution context of the round: `false` for a
/// standalone request, `true` for a member of a running batch. It is copied
/// verbatim into the emitted `ModelStartedEvent`, so the interface freezes the
/// shared turn and lets the batch supervisor emit the single terminal signal.
/// Deriving it from the caller would conflate the storage contract with the
/// execution context.
pub(super) async fn run_review_post_analysis(
    state: &Arc<AppState>,
    repo_root: &Path,
    session: &ReviewSession,
    operation: &OperationGuard,
    is_batch: bool,
) -> Result<ReviewSession, ResponseError> {
    // Nothing to classify: the classification layer references findings, so an
    // empty session has no subject. The refusal is typed and precedes both the
    // identity healing below and the provider call, so an audit that produced
    // nothing never spends a request.
    if session.items.is_empty() {
        return Err(post_analysis_error(
            errors::EMPTY_ITEMS,
            format!(
                "Review session '{}' has no findings to classify; run the audit first",
                session.id
            ),
        ));
    }
    let findings_count = session.items.len();

    // Every finding must be addressable before the model reads it. A session
    // stored before identities existed would otherwise reach the prompt as an
    // unreferenceable element, and no lawful classification could reference it.
    // Healing therefore runs here, and the healed session is persisted before
    // the provider call, so the identities the classification will name are
    // durable even when the round afterwards fails or is cancelled.
    let mut healed = session.clone();
    let minted = ensure_finding_identities(&mut healed);
    if minted > 0 {
        let save_root = repo_root.to_path_buf();
        let to_save = healed.clone();
        offload_review(move || ReviewStorage::save_session(&save_root, &to_save))
            .await
            .map_err(|err| post_analysis_error(errors::REVIEW_UPDATE_FAILED, err))?;
        tracing::info!(
            review_id = healed.id.as_str(),
            minted,
            "review findings healed with UUIDv7 identities before post-analysis"
        );
    }

    // The rest of the procedure reads the healed session, so the prompt, the
    // validator, and the persisted layer agree on the same identities.
    let session = &healed;
    let known_uuids = finding_identities(session);

    let (provider, architect_tier) = {
        let config = state.config.lock().await.config.clone();
        // Tier resolution is core-owned domain policy: the architect ladder
        // inherits the whole `primary` list when a project declares no architect
        // entries, so the base rung is always resolvable and its reasoning depth
        // travels with it instead of being re-derived here.
        let architect_tier = config.models.architect_tier(0);
        let provider =
            match resolve_architect_provider(state, &config, architect_tier.model.provider).await {
                Ok(provider) => provider,
                Err(err) => return Err(post_analysis_error(errors::NO_API_KEY, err)),
            };
        (provider, architect_tier)
    };
    let model = architect_tier.model.clone();
    let reasoning_effort = architect_tier.effort;
    let op_id = format!("op-{}", operation.id());

    emit(
        &state.out,
        events::MODEL_STARTED,
        serde_json::to_value(ModelStartedEvent {
            operation_id: op_id.clone(),
            model: model.clone(),
            role: Some(ModelRole::Architect),
            alias: None,
            effort: reasoning_effort,
            plan_context: None,
            // The round classifies the findings of one stored session, so the
            // running work names the session phase instead of leaving it
            // anonymous; a batch member never settles the shared turn itself.
            work_context: Some(TurnWorkContext::ReviewPhase {
                review_id: session.id.clone(),
                review_title: session.title.clone(),
                phase: ReviewWorkPhase::PostAnalysis,
                queue: TurnWorkQueue::default(),
            }),
            is_batch,
        })
        .ok(),
    );
    emit(
        &state.out,
        events::REVIEW_ANALYZE_STARTED,
        serde_json::to_value(ReviewAnalyzeStartedEvent {
            operation_id: op_id.clone(),
            model: model.clone(),
            findings_count,
        })
        .ok(),
    );

    let messages = vec![
        ChatMessage::system(build_post_analysis_system_prompt()),
        ChatMessage::user(build_post_analysis_user_prompt(session)),
    ];
    // Post-analysis is a non-code operation: the enforced JSON schema turns the
    // classification into a machine-readable payload instead of parsed prose.
    let response_format = post_analysis_response_format();
    // The classification extends the same provider prefix cache as the audit that
    // produced the findings, so the whole session keeps one warm node.
    let session_id = format_session_id(SESSION_PREFIX_REVIEW, &session.id);

    // The guard opens the accounting window before the provider call, so the
    // analysis is charged even when it fails or is cancelled.
    let usage_guard = state.begin_usage().await;

    let cancel_rx = operation.cancel_receiver();
    let (tx, mut rx) = mpsc::channel::<StreamEvent>(100);
    let err_tx = tx.clone();
    let model_name = model.name.clone();
    let provider_for_stream = Arc::clone(&provider);
    let stream_cancel = cancel_rx.clone();
    let stream_task = tokio::spawn(async move {
        if let Err(err) = provider_for_stream
            .stream_chat_with_session(
                &model_name,
                messages,
                Some(response_format),
                reasoning_effort,
                Some(session_id.as_str()),
                tx,
                stream_cancel,
            )
            .await
        {
            let _ = err_tx.send(StreamEvent::Error(err.to_string())).await;
        }
    });
    operation.set_abort_handle(stream_task.abort_handle());

    // The provider streams raw Structured Output JSON; this filter decodes only the
    // `summary` string, so the verdict reaches the client while JSON syntax stays
    // server-side and the whole payload stays available for decoding.
    let mut filter = JsonFieldStreamFilter::<PostAnalysisOutput>::new("summary");
    let mut usage: Option<ModelUsageInfo> = None;
    let mut failure: Option<String> = None;
    let mut stream_cancelled = false;

    while let Some(event) = rx.recv().await {
        match event {
            StreamEvent::ReasoningDelta(delta) => emit(
                &state.out,
                events::REVIEW_REASONING_DELTA,
                serde_json::to_value(ReviewReasoningDeltaEvent {
                    operation_id: op_id.clone(),
                    delta,
                })
                .ok(),
            ),
            StreamEvent::TextDelta(chunk) => {
                if let Some(delta) = filter.push_chunk(&chunk) {
                    emit(
                        &state.out,
                        events::REVIEW_CONTENT_DELTA,
                        serde_json::to_value(ReviewContentDeltaEvent {
                            operation_id: op_id.clone(),
                            delta,
                        })
                        .ok(),
                    );
                }
            }
            StreamEvent::Usage(reported) => usage = Some(reported),
            StreamEvent::Done => break,
            // The provider may report cancellation as an explicit stream event even
            // when the turn's cancel signal has not been observed yet, so the event
            // is recorded instead of inferred from `cancel_rx` alone.
            StreamEvent::Cancelled => {
                stream_cancelled = true;
                break;
            }
            StreamEvent::Error(err) => {
                failure = Some(err);
                break;
            }
            // The retry decorator withdrew the previous attempt because it broke
            // after partial output: the filter holds a prefix of a document the
            // retry regenerates from its beginning, so it is restarted instead of
            // accumulating both payloads into one unparsable body, and the client
            // drops the partial verdict it already rendered.
            StreamEvent::ResetStream => {
                withdraw_streamed_attempt(&state.out, &op_id, || {
                    filter = JsonFieldStreamFilter::<PostAnalysisOutput>::new("summary");
                });
            }
            _ => {}
        }
    }

    // The streamed call has resolved, so the slot must stop pointing at it: a later
    // hard-cancel would otherwise abort an already finished task while the
    // operation is still finalizing and persisting.
    operation.clear_abort_handle();

    // Account for the tokens, cost, and duration the analysis consumed before any
    // early return, so a failed or cancelled round is still charged.
    let reported_usage = usage.unwrap_or_default();
    usage_guard.observe(&reported_usage).await;
    usage_guard.flush().await;
    let session_total_cost = state.usage.lock().await.session.cost;
    let current_cost = reported_usage.cost.unwrap_or(0.0);
    emit(
        &state.out,
        events::MODEL_USAGE,
        serde_json::to_value(ModelUsageEvent {
            operation_id: op_id.clone(),
            usage: reported_usage,
            session_total_cost,
            current_cost: Some(current_cost),
        })
        .ok(),
    );

    if *cancel_rx.borrow() || stream_cancelled {
        stream_task.abort();
        return Err(post_analysis_error(
            errors::CANCELLED,
            format!(
                "Review post-analysis of session '{}' was cancelled",
                session.id
            ),
        ));
    }
    if let Some(message) = failure {
        stream_task.abort();
        return Err(post_analysis_error(errors::MODEL_ERROR, message));
    }

    // Capture the raw payload before finalizing: `finish` consumes the filter,
    // while the tolerant parser still needs the full body.
    let raw_buffer = filter.buffer().to_string();
    if raw_buffer.trim().is_empty() {
        return Err(post_analysis_error(
            errors::PARSE_ERROR,
            format!(
                "Model returned an empty post-analysis for review session '{}'",
                session.id
            ),
        ));
    }

    // A clean deserialization yields the typed classification; a malformed tail
    // degrades to the tolerant parser instead of dropping an otherwise usable
    // analysis.
    let output = match filter.finish() {
        Ok(output) => output,
        Err(_) => parse_post_analysis(&raw_buffer).map_err(|err| {
            post_analysis_error(
                errors::PARSE_ERROR,
                format!("Failed to parse the review post-analysis payload: {err:#}"),
            )
        })?,
    };

    // The global verdict is a required field of the classification schema and the
    // whole point of the layer, so a payload without it is a contract failure
    // rather than an empty classification.
    if output.summary.trim().is_empty() {
        return Err(post_analysis_error(
            errors::SCHEMA_ERROR,
            "Review post-analysis returned no summary; the global verdict is required by the classification schema".to_string(),
        ));
    }

    // Every reference must name a finding of this session: a classification that
    // addresses an identity the session does not hold is rejected by name, because
    // persisting it would produce a layer nothing can render.
    validate_post_analysis(&output, &known_uuids)
        .map_err(|message| post_analysis_error(errors::SCHEMA_ERROR, message))?;

    // A cancel delivered after the stream resolved — during parsing, or across a
    // storage round-trip — must not persist a regrouping the developer already
    // abandoned, so the signal is re-checked right before the session touches disk.
    if *cancel_rx.borrow() {
        return Err(post_analysis_error(
            errors::CANCELLED,
            format!(
                "Review post-analysis of session '{}' was cancelled before it was persisted",
                session.id
            ),
        ));
    }

    // The classification is the session itself, not a projection beside it: the
    // verdict becomes the session summary, the payload clusters become the session
    // groups, and every finding moves to the group that now owns it. A finding
    // whose type requires planning before execution receives a single-element
    // architectural group instead of the group of independent findings.
    let verdict = output.summary.trim().to_string();
    let redirects = arch_redirect_groups(&output, session, &known_uuids);
    let groups = regrouped_groups(&output, &redirects);
    let assignments = group_assignments(&output, &known_uuids, &redirects);
    let independent_count = output.independent_issues.len();
    let group_count = output.architectural_groups.len();
    let history_response = format!(
        "Review session [{}] post-analysis regrouped {} finding(s) into {} independent issue(s) and {} architectural group(s).\n\n{}",
        session.id, findings_count, independent_count, group_count, verdict,
    );

    // One layer is rewritten in place: the findings are never rescanned and never
    // rewritten, so a repeated analysis substitutes only the groups and the
    // assignments of the previous one.
    let save_root = repo_root.to_path_buf();
    let save_review_id = session.id.clone();
    let stored = offload_review(move || {
        ReviewStorage::regroup_session(&save_root, &save_review_id, verdict, groups, assignments)
    })
    .await
    .map_err(|err| post_analysis_error(errors::REVIEW_UPDATE_FAILED, err))?;
    let stored = match stored {
        Some(stored) => stored,
        None => {
            return Err(post_analysis_error(
                errors::NOT_FOUND,
                format!(
                    "Review session '{}' disappeared before its regrouping was persisted",
                    session.id
                ),
            ))
        }
    };

    tracing::info!(
        review_id = session.id.as_str(),
        findings = findings_count,
        independent_issues = independent_count,
        architectural_groups = group_count,
        model = %model,
        "review post-analysis persisted"
    );

    // The user turn and its response are recorded together once the classification
    // is durable, so a cancelled, failed, or rejected round leaves no orphaned user
    // turn in the transcript. The audited files describe what the Architect saw.
    let history_turn_prompt = format!(
        "# Review Post-Analysis: [{}] {}\n\nClassify the {} finding(s) of the session into independent issues and architectural root-cause groups.",
        session.id, session.title, findings_count,
    );
    record_synthesis_history(
        state,
        history_turn_prompt,
        session.target_files.clone(),
        history_response,
        Some(format!("Review Post-Analysis: {}", session.title)),
    )
    .await;

    publish_review(state, repo_root, &session.id).await;

    emit(
        &state.out,
        events::REVIEW_ANALYZE_FINISHED,
        serde_json::to_value(ReviewAnalyzeFinishedEvent {
            operation_id: op_id,
            session: stored.clone(),
        })
        .ok(),
    );

    Ok(stored)
}

/// Collects the finding identities the session holds, in stored order.
///
/// Identity is what the classification references. Healing runs before this
/// collection, so every finding of a classified session contributes one; the
/// filter stays as a defensive guard against an identity that is present but
/// blank.
fn finding_identities(session: &ReviewSession) -> Vec<String> {
    session
        .items
        .iter()
        .filter_map(|item| item.uuid.as_deref())
        .map(str::trim)
        .filter(|uuid| !uuid.is_empty())
        .map(str::to_string)
        .collect()
}

/// Assigns a UUIDv7 identity to every finding that carries none.
///
/// Returns the number of identities minted. A finding stored before identities
/// existed cannot be referenced by the classification, so the session is healed
/// before the model reads it instead of being rendered as an unclassifiable
/// element. An identity a finding already carries is preserved, so healing never
/// renames a finding an earlier round already referenced.
fn ensure_finding_identities(session: &mut ReviewSession) -> usize {
    let mut minted = 0;
    for item in &mut session.items {
        let present = item
            .uuid
            .as_deref()
            .map(str::trim)
            .is_some_and(|uuid| !uuid.is_empty());
        if !present {
            item.uuid = Some(generate_uuid_v7());
            minted += 1;
        }
    }
    minted
}

/// Resolves one emitted identifier to the exact spelling the session stores.
///
/// Comparison ignores ASCII case, so a model that re-spells an identity still
/// addresses the finding it means, while the persisted layer keeps the
/// authoritative spelling every later projection matches exactly.
fn resolve_identity(identifier: &str, known_uuids: &[String]) -> Option<String> {
    let trimmed = identifier.trim();
    known_uuids
        .iter()
        .find(|known| known.eq_ignore_ascii_case(trimmed))
        .cloned()
}

/// Identifier of the architectural cluster emitted at one payload position.
///
/// A cluster has no identity outside the classification that produced it, so its
/// identifier is a deterministic position within that classification. A random
/// identifier would make two identical analyses produce two different sessions.
fn architectural_group_id(index: usize) -> String {
    format!("arch-{}", index + 1)
}

/// Materializes the groups that replace the session groups.
///
/// The classification is the session, so the payload is the group list: one group
/// per architectural cluster of the findings, one single-element group per finding
/// whose type requires planning before execution, plus the group collecting the
/// findings that need no architectural remediation. That group is materialized
/// even when it turns out empty, so a classified session keeps one uniform shape:
/// every finding belongs to exactly one rendered group.
///
/// The class of each group is declared here rather than derived from its
/// identifier: an architectural cluster and a single-element architectural group
/// become [`ReviewGroupKind::Architectural`], while the group collecting the
/// findings that share no root cause becomes [`ReviewGroupKind::Simple`], so
/// execution never guesses a class from a name.
fn regrouped_groups(
    output: &PostAnalysisOutput,
    redirects: &[(String, ReviewGroup)],
) -> Vec<ReviewGroup> {
    let mut groups: Vec<ReviewGroup> = output
        .architectural_groups
        .iter()
        .enumerate()
        .map(|(index, group)| ReviewGroup {
            id: architectural_group_id(index),
            title: group.title.trim().to_string(),
            description: Some(group.description.trim().to_string()),
            // A cluster is remediated by one shared horizon that removes the root
            // cause, never finding by finding.
            kind: ReviewGroupKind::Architectural,
            ..Default::default()
        })
        .collect();
    // A finding whose type requires planning before execution owns a
    // single-element group of its own, rendered after the clusters so the layer
    // keeps one deterministic numbering.
    for (_, group) in redirects {
        groups.push(group.clone());
    }
    groups.push(ReviewGroup {
        id: LOCAL_REVIEW_GROUP_ID.to_string(),
        title: LOCAL_REVIEW_GROUP_TITLE.to_string(),
        description: Some(
            "Findings that stand on their own: each one is resolved by its own finding subplan, with no shared group horizon."
                .to_string(),
        ),
        // The findings that share no root cause stay independent, so the group
        // collecting them is executed finding by finding.
        kind: ReviewGroupKind::Simple,
        ..Default::default()
    });
    groups
}

/// Resolves the findings whose type requires planning before execution, paired
/// with the architectural group that owns each of them.
///
/// `IssueType::Architecture` is a deterministic requirement, not a label: such a
/// finding is remediated by a plan, and the only sanctioned carrier of a plan is
/// an architectural group. The group of independent findings is a simple group,
/// so it may hold no architecture finding at all. A classification that names one
/// independent, or that does not name it at all, therefore does not decide where
/// the finding lands; the group is derived here instead, titled after the finding
/// because the finding is the whole content of its root cause.
///
/// A finding the classification placed in a cluster is already owned by an
/// architectural group and keeps it, so no group of the layer is ever empty. The
/// numbering continues after the clusters and follows the stored order of the
/// findings, so a repeated analysis of the same payload mints the same
/// identifiers.
fn arch_redirect_groups(
    output: &PostAnalysisOutput,
    session: &ReviewSession,
    known_uuids: &[String],
) -> Vec<(String, ReviewGroup)> {
    let clustered: HashSet<String> = output
        .architectural_groups
        .iter()
        .flat_map(|group| group.issue_ids.iter())
        .filter_map(|identifier| resolve_identity(identifier, known_uuids))
        .collect();
    let mut redirects = Vec::new();
    let mut index = output.architectural_groups.len();
    for item in &session.items {
        if item.issue_type != IssueType::Architecture {
            continue;
        }
        let uuid = item.uuid.as_deref().map(str::trim).unwrap_or_default();
        if uuid.is_empty() || clustered.contains(uuid) {
            continue;
        }
        redirects.push((
            uuid.to_string(),
            ReviewGroup {
                id: architectural_group_id(index),
                title: item.title.trim().to_string(),
                description: Some(
                    "The finding requires a plan before it is executed: the group synthesizes one coherent remediation of its root cause."
                        .to_string(),
                ),
                kind: ReviewGroupKind::Architectural,
                ..Default::default()
            },
        ));
        index += 1;
    }
    redirects
}

/// Resolves the group that owns every finding the session holds.
///
/// A finding the payload names belongs to the group it named. A finding the payload
/// does not name would otherwise keep pointing at a group the new layer no longer
/// declares, so it lands in the group of independent findings. An architecture
/// finding never lands there: the single-element architectural group minted for it
/// replaces the independent assignment, because the group of independent findings
/// holds no finding that must be planned before it is executed. Every assignment
/// therefore names a group of [`regrouped_groups`], and no finding is left outside
/// the rendered tree.
fn group_assignments(
    output: &PostAnalysisOutput,
    known_uuids: &[String],
    redirects: &[(String, ReviewGroup)],
) -> Vec<(String, String)> {
    let mut assigned: HashMap<String, String> = HashMap::new();
    for identifier in &output.independent_issues {
        if let Some(known) = resolve_identity(identifier, known_uuids) {
            assigned.insert(known, LOCAL_REVIEW_GROUP_ID.to_string());
        }
    }
    for (index, group) in output.architectural_groups.iter().enumerate() {
        let group_id = architectural_group_id(index);
        for identifier in &group.issue_ids {
            if let Some(known) = resolve_identity(identifier, known_uuids) {
                assigned.insert(known, group_id.clone());
            }
        }
    }
    // The redirects are applied last, so an architecture finding keeps its own
    // architectural group even when the payload called it independent.
    for (uuid, group) in redirects {
        assigned.insert(uuid.clone(), group.id.clone());
    }
    known_uuids
        .iter()
        .map(|uuid| {
            let group_id = assigned
                .get(uuid)
                .cloned()
                .unwrap_or_else(|| LOCAL_REVIEW_GROUP_ID.to_string());
            (uuid.clone(), group_id)
        })
        .collect()
}

/// Builds a typed error for the post-analysis procedure.
fn post_analysis_error(code: &'static str, message: String) -> ResponseError {
    ResponseError::new(code, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_core::config::AppConfig;
    use tauqe_core::providers::mock::MockResponse;
    use tauqe_core::providers::MockLlmProvider;
    use tauqe_protocol::{IssueStatus, IssueType, ModelRef, ReviewItem, ReviewSeverity};

    /// Identity of the finding the core engine group produced.
    const CORE_UUID: &str = "018f9c1e-6b3a-7c1e-8b3a-000000000001";
    /// Identity of the finding the terminal frontend group produced.
    const TUI_UUID: &str = "018f9c1e-6b3a-7c1e-8b3a-000000000002";

    /// Scratch repository and its daemon state.
    struct AnalysisFixture {
        root: std::path::PathBuf,
        state: Arc<AppState>,
    }

    impl Drop for AnalysisFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn fixture(label: &str) -> AnalysisFixture {
        let root = std::env::temp_dir().join(format!(
            "tauqe-review-analysis-{}-{}-{}",
            std::process::id(),
            label,
            crate::state::next_operation_id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let (out, _events) = crate::transport::OutChannel::new();
        let (history_sender, _history_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(AppConfig::default(), root.clone(), history_sender, out);
        AnalysisFixture { root, state }
    }

    fn finding(id: u32, uuid: &str, group: &str, status: IssueStatus) -> ReviewItem {
        ReviewItem {
            id,
            title: format!("Finding {id}"),
            uuid: Some(uuid.to_string()),
            group: Some(group.to_string()),
            model: None,
            issue_type: IssueType::Architecture,
            severity: ReviewSeverity::from(IssueType::Architecture),
            status,
            recommendations: vec!["Introduce one shared transaction boundary".to_string()],
            files: vec!["crates/core/src/git.rs".to_string()],
            readonly_files: vec!["Conventions.md".to_string()],
            resolution_notes: None,
            body: "The change set reaches the worktree without one shared boundary.".to_string(),
        }
    }

    fn session() -> ReviewSession {
        ReviewSession {
            id: "rev-analyze".to_string(),
            title: "Multi-Level Audit".to_string(),
            created_at: 100,
            model: ModelRef::openrouter("test/architect"),
            target_files: vec!["crates/core/src/git.rs".to_string()],
            items: vec![
                finding(1, CORE_UUID, "core-engine", IssueStatus::Todo),
                finding(2, TUI_UUID, "terminal-ui", IssueStatus::Discussion),
            ],
            ..ReviewSession::default()
        }
    }

    /// Renders one well-formed classification payload.
    fn classification(summary: &str, independent: &[&str], groups: &[(&str, &[&str])]) -> String {
        let groups: Vec<serde_json::Value> = groups
            .iter()
            .map(|(title, ids)| {
                serde_json::json!({
                    "title": title,
                    "description": format!("The systemic root cause behind {title}."),
                    "issue_ids": ids,
                })
            })
            .collect();
        serde_json::json!({
            "summary": summary,
            "independent_issues": independent,
            "architectural_groups": groups,
        })
        .to_string()
    }

    /// Re-running the analysis regroups the session in place while the findings
    /// it references keep their exact identities, content, and statuses.
    #[tokio::test]
    async fn post_analysis_replaces_the_layer_without_touching_findings() {
        let fixture = fixture("replace");
        let session = session();
        ReviewStorage::save_session(&fixture.root, &session).unwrap();

        let provider = Arc::new(MockLlmProvider::scripted([
            MockResponse::text(classification(
                "One shared transaction boundary explains both findings.",
                &[CORE_UUID],
                &[("Shared commit boundary", &[TUI_UUID])],
            )),
            MockResponse::text(classification(
                "The frontend repeats the same boundary defect.",
                &[TUI_UUID],
                &[("Shared commit boundary", &[CORE_UUID])],
            )),
        ]));
        *fixture.state.provider_override.lock().await = Some(provider);

        let operation = fixture.state.try_begin_operation().unwrap();
        let first =
            run_review_post_analysis(&fixture.state, &fixture.root, &session, &operation, false)
                .await
                .expect("the classification is persisted");
        // The classification is the session itself: the verdict becomes the
        // session description, the payload clusters become the session groups,
        // and every finding moves to the group that now owns it. An architecture
        // finding the classification left on its own receives a single-element
        // architectural group instead of the group of independent findings.
        assert_eq!(
            first.description.as_deref(),
            Some("One shared transaction boundary explains both findings.")
        );
        assert_eq!(first.groups.len(), 3);
        assert_eq!(first.groups[0].id, "arch-1");
        assert_eq!(first.groups[0].kind, ReviewGroupKind::Architectural);
        assert_eq!(first.groups[0].title, "Shared commit boundary");
        assert_eq!(
            first.groups[0].description.as_deref(),
            Some("The systemic root cause behind Shared commit boundary.")
        );
        // The architecture finding the classification called independent is
        // planned before it is executed, so it owns a single-element
        // architectural group titled after the finding itself.
        assert_eq!(first.groups[1].id, "arch-2");
        assert_eq!(first.groups[1].kind, ReviewGroupKind::Architectural);
        assert_eq!(first.groups[1].title, "Finding 1");
        assert_eq!(first.groups[2].id, LOCAL_REVIEW_GROUP_ID);
        assert_eq!(first.groups[2].title, LOCAL_REVIEW_GROUP_TITLE);
        assert_eq!(first.groups[2].kind, ReviewGroupKind::Simple);
        assert_eq!(first.items[0].group.as_deref(), Some("arch-2"));
        assert_eq!(first.items[1].group.as_deref(), Some("arch-1"));
        // Findings are immutable input: regrouping reassigns the group of each
        // finding and never rewrites what it reports.
        assert_eq!(first.items[0].title, session.items[0].title);
        assert_eq!(first.items[1].title, session.items[1].title);
        assert_eq!(first.items[0].status, IssueStatus::Todo);
        assert_eq!(first.items[1].status, IssueStatus::Discussion);

        let second =
            run_review_post_analysis(&fixture.state, &fixture.root, &session, &operation, false)
                .await
                .expect("the second classification is persisted");
        // The second classification substitutes the grouping wholesale: no part
        // of the previous one survives, while every finding keeps its identity,
        // its content, and its lifecycle status.
        assert_eq!(
            second.description.as_deref(),
            Some("The frontend repeats the same boundary defect.")
        );
        assert_eq!(second.groups.len(), 3);
        assert_eq!(second.groups[0].id, "arch-1");
        assert_eq!(second.groups[1].id, "arch-2");
        assert_eq!(second.groups[1].title, "Finding 2");
        assert_eq!(second.groups[2].id, LOCAL_REVIEW_GROUP_ID);
        assert_eq!(second.items[0].group.as_deref(), Some("arch-1"));
        assert_eq!(second.items[1].group.as_deref(), Some("arch-2"));

        let stored = ReviewStorage::load_review(&fixture.root, "rev-analyze")
            .unwrap()
            .expect("the session is stored");
        assert_eq!(
            stored.description.as_deref(),
            Some("The frontend repeats the same boundary defect.")
        );
        assert_eq!(stored.groups.len(), 3);
        assert_eq!(stored.groups[0].title, "Shared commit boundary");
        assert_eq!(stored.groups[1].id, "arch-2");
        assert_eq!(stored.groups[1].title, "Finding 2");
        assert_eq!(stored.groups[1].kind, ReviewGroupKind::Architectural);
        assert_eq!(stored.groups[2].id, LOCAL_REVIEW_GROUP_ID);
        assert_eq!(stored.groups[2].title, LOCAL_REVIEW_GROUP_TITLE);
        assert_eq!(stored.groups[2].kind, ReviewGroupKind::Simple);
        assert_eq!(stored.items[0].group.as_deref(), Some("arch-1"));
        assert_eq!(stored.items[1].group.as_deref(), Some("arch-2"));
        assert_eq!(stored.items[0].status, IssueStatus::Todo);
        assert_eq!(stored.items[1].status, IssueStatus::Discussion);
        assert_eq!(stored.items[0].uuid.as_deref(), Some(CORE_UUID));
        assert_eq!(stored.items[1].uuid.as_deref(), Some(TUI_UUID));
    }

    /// An architecture finding never lands in the group of independent findings,
    /// while an independent finding of another type stays exactly where the
    /// classification placed it.
    #[tokio::test]
    async fn post_analysis_keeps_an_independent_architecture_finding_out_of_the_local_group() {
        let fixture = fixture("arch-redirect");
        let mut session = session();
        // The second finding is a quality finding: nothing requires it to be
        // planned before it is executed.
        session.items[1].issue_type = IssueType::Quality;
        session.items[1].severity = ReviewSeverity::from(IssueType::Quality);
        ReviewStorage::save_session(&fixture.root, &session).unwrap();

        let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
            classification(
                "Both findings stand on their own.",
                &[CORE_UUID, TUI_UUID],
                &[],
            ),
        )]));
        *fixture.state.provider_override.lock().await = Some(provider);

        let operation = fixture.state.try_begin_operation().unwrap();
        let stored =
            run_review_post_analysis(&fixture.state, &fixture.root, &session, &operation, false)
                .await
                .expect("the classification is persisted");

        // The architecture finding is planned before it is executed, so it owns a
        // single-element architectural group titled after the finding itself,
        // while the group of independent findings stays non-architectural.
        assert_eq!(stored.groups.len(), 2);
        assert_eq!(stored.groups[0].id, "arch-1");
        assert_eq!(stored.groups[0].title, "Finding 1");
        assert_eq!(stored.groups[0].kind, ReviewGroupKind::Architectural);
        assert_eq!(stored.groups[1].id, LOCAL_REVIEW_GROUP_ID);
        assert_eq!(stored.groups[1].kind, ReviewGroupKind::Simple);
        assert_eq!(stored.items[0].group.as_deref(), Some("arch-1"));
        assert_eq!(
            stored.items[1].group.as_deref(),
            Some(LOCAL_REVIEW_GROUP_ID)
        );
        // Regrouping reassigns the owning group and never rewrites the finding.
        assert_eq!(stored.items[0].uuid.as_deref(), Some(CORE_UUID));
        assert_eq!(stored.items[1].uuid.as_deref(), Some(TUI_UUID));
        assert_eq!(stored.items[0].title, session.items[0].title);
        assert_eq!(stored.items[1].title, session.items[1].title);
        assert_eq!(stored.items[0].status, IssueStatus::Todo);
        assert_eq!(stored.items[1].status, IssueStatus::Discussion);
    }
    /// A classification naming an identity the session does not hold is rejected
    /// by name and never reaches disk.
    #[tokio::test]
    async fn post_analysis_refuses_a_foreign_identifier() {
        let fixture = fixture("foreign");
        let session = session();
        ReviewStorage::save_session(&fixture.root, &session).unwrap();

        let foreign = "018f9c1e-6b3a-7c1e-8b3a-0000000000ff";
        let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
            classification("Root cause", &[foreign], &[]),
        )]));
        *fixture.state.provider_override.lock().await = Some(provider);

        let operation = fixture.state.try_begin_operation().unwrap();
        let err =
            run_review_post_analysis(&fixture.state, &fixture.root, &session, &operation, false)
                .await
                .expect_err("a foreign identifier must be refused");
        assert_eq!(err.code, errors::SCHEMA_ERROR);
        assert!(
            err.message.contains(foreign),
            "the diagnostic must name the foreign uuid: {}",
            err.message
        );

        let stored = ReviewStorage::load_review(&fixture.root, "rev-analyze")
            .unwrap()
            .expect("the session is stored");
        assert!(
            stored.post_analysis.is_none(),
            "a refused classification never reaches disk"
        );
        assert_eq!(stored.items, session.items);
    }

    /// A session with nothing to classify is refused before any provider call.
    #[tokio::test]
    async fn post_analysis_refuses_a_session_without_findings() {
        let fixture = fixture("empty");
        let session = ReviewSession {
            id: "rev-empty".to_string(),
            title: "Empty Audit".to_string(),
            created_at: 1,
            model: ModelRef::openrouter("test/architect"),
            ..ReviewSession::default()
        };
        ReviewStorage::save_session(&fixture.root, &session).unwrap();

        let operation = fixture.state.try_begin_operation().unwrap();
        let err =
            run_review_post_analysis(&fixture.state, &fixture.root, &session, &operation, false)
                .await
                .expect_err("an empty session has nothing to classify");
        assert_eq!(err.code, errors::EMPTY_ITEMS);

        let stored = ReviewStorage::load_review(&fixture.root, "rev-empty")
            .unwrap()
            .expect("the session is stored");
        assert!(stored.post_analysis.is_none());
    }

    /// A re-spelled identity still addresses the finding it names, so the
    /// regrouping moves exactly that finding and leaves its identity intact.
    #[tokio::test]
    async fn post_analysis_stores_the_authoritative_identifier_spelling() {
        let fixture = fixture("respell");
        let session = session();
        ReviewStorage::save_session(&fixture.root, &session).unwrap();

        let re_spelled = CORE_UUID.to_ascii_uppercase();
        let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
            classification(
                "One shared boundary.",
                &[],
                &[("Shared boundary", &[re_spelled.as_str()])],
            ),
        )]));
        *fixture.state.provider_override.lock().await = Some(provider);

        let operation = fixture.state.try_begin_operation().unwrap();
        let stored =
            run_review_post_analysis(&fixture.state, &fixture.root, &session, &operation, false)
                .await
                .expect("a re-spelled identity resolves");
        assert_eq!(stored.description.as_deref(), Some("One shared boundary."));
        assert_eq!(stored.groups.len(), 3);
        assert_eq!(stored.groups[0].id, "arch-1");
        assert_eq!(stored.groups[0].title, "Shared boundary");
        assert_eq!(stored.items[0].group.as_deref(), Some("arch-1"));
        assert_eq!(stored.items[0].uuid.as_deref(), Some(CORE_UUID));
        // The finding the classification did not name keeps no stale
        // provenance, and its architecture type still keeps it out of the group
        // of independent findings: it receives a single-element group of its own.
        assert_eq!(stored.groups[1].id, "arch-2");
        assert_eq!(stored.groups[1].title, "Finding 2");
        assert_eq!(stored.groups[1].kind, ReviewGroupKind::Architectural);
        assert_eq!(stored.groups[2].id, LOCAL_REVIEW_GROUP_ID);
        assert_eq!(stored.groups[2].kind, ReviewGroupKind::Simple);
        assert_eq!(stored.items[1].group.as_deref(), Some("arch-2"));
        assert_eq!(stored.items[1].uuid.as_deref(), Some(TUI_UUID));
    }

    /// The identifier is mandatory: an implicit target would rewrite whichever
    /// session happened to be focused.
    #[tokio::test]
    async fn analyze_requires_an_explicit_session_identifier() {
        let fixture = fixture("strict-id");
        let req = Request {
            id: tauqe_protocol::RequestId::Number(3),
            method: tauqe_protocol::methods::REVIEW_ANALYZE.to_string(),
            params: Some(serde_json::json!({})),
        };

        let response = handle_review_analyze(req, &fixture.state).await;
        assert_eq!(
            response.error.expect("the request is refused").code,
            errors::INVALID_PARAMS
        );
    }
}
