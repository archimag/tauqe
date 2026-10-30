//! Architect-driven decomposition of one architectural review group into an
//! embedded unified subplan.
//!
//! Group decomposition and plan horizon synthesis are the same Architect
//! operation, so both share the multi-round discovery engine in
//! `handlers::architect` (`run_architect_discovery_loop`). The synthesized
//! horizon is persisted through the core domain primitive
//! `ReviewStorage::replace_uncompleted_group_subplan`, so no plan-tree
//! manipulation happens in the server crate.
//!
//! The synthesis itself lives in [`refine_group_with_architect`]. The group plan
//! walk calls it just in time when an architectural group owns no horizon yet,
//! and the replanning wave calls it again to reproject the remaining horizon of
//! a group whose sub-step failed (see `group_exec.rs`). Planning is therefore an
//! internal phase of executing a group, not a request of its own.

use std::path::Path;
use std::sync::Arc;

use tauqe_core::context::ContextFileContent;
use tauqe_core::plan::storage::merge_subplan;
use tauqe_core::providers::{
    format_session_id, JsonSchemaDefinition, ResponseFormat, SESSION_PREFIX_REVIEW,
};
use tauqe_core::review::prompt::refine::{
    build_review_group_refine_system_prompt, build_review_group_refine_user_prompt,
    ReviewGroupRefineDiagnostics,
};
use tauqe_core::review::ReviewStorage;
use tauqe_protocol::model::{ReviewWorkPhase, TurnWorkContext, TurnWorkQueue};
use tauqe_protocol::{
    errors, events, IssueStatus, ModelDeltaEvent, ModelRole, ModelSelection, ModelStartedEvent,
    PlanItem, ResponseError, ReviewGroup, ReviewGroupKind, ReviewItem, ReviewSession,
    DEFAULT_REVIEW_GROUP_ID,
};

use crate::handlers::architect::{
    format_architect_escalation_note, resolve_architect_provider, run_architect_discovery_loop,
    ArchitectDiscoveryError, ArchitectDiscoveryOutcome, ArchitectDiscoveryRequest,
};
use crate::handlers::common::{preload_readonly_files, record_synthesis_history};
use crate::handlers::plan::find_duplicate_item_ids;
use crate::state::{next_operation_id, AppState, OperationGuard};

use super::common::{clear_auto_context, emit, offload_review, publish_review};

/// Builds a typed error for the decomposition helper, mirroring `Response::err`.
fn refine_error(code: &'static str, message: String) -> ResponseError {
    ResponseError::new(code, message)
}

/// Operation title used for group decomposition telemetry and history.
///
/// Group decomposition and finding decomposition are two different Architect
/// operations, so each one names itself in the journal instead of sharing a
/// title that would leave a log reader guessing which horizon was rewritten.
#[allow(dead_code)]
const GROUP_OPERATION_TITLE: &str = "Review Group Decomposition";

/// Typed outcome of an Architect-driven group decomposition.
///
/// The synthesized horizon itself is not returned: it is persisted on the group
/// by the core storage primitive, so a caller only learns whether the group now
/// owns an executable horizon or why the Architect refused to produce one.
#[allow(dead_code)]
#[derive(Debug)]
pub(super) enum GroupRefineOutcome {
    /// The group was decomposed and its unified subplan was persisted.
    Success { explanation: String },
    /// The Architect reported that the root cause cannot be removed as stated.
    Blocked { reason: String },
}

/// Resolves every finding that belongs to `group_id`, in stored order.
///
/// Membership follows the effective group of a finding: a finding that stores no
/// group belongs to the implicit [`DEFAULT_REVIEW_GROUP_ID`] group, exactly as
/// the grouped session view and the post-analysis layer resolve it.
#[allow(dead_code)]
pub(super) fn group_findings(session: &ReviewSession, group_id: &str) -> Vec<ReviewItem> {
    session
        .items
        .iter()
        .filter(|item| match item.group.as_deref() {
            Some(group) => group == group_id,
            None => group_id == DEFAULT_REVIEW_GROUP_ID,
        })
        .cloned()
        .collect()
}

/// Collects the declared target files of every finding of a group.
///
/// The files are de-duplicated in declaration order, so the preload request list
/// stays deterministic and a file two findings share is requested once.
#[allow(dead_code)]
fn group_target_files(items: &[ReviewItem]) -> Vec<String> {
    let mut files: Vec<String> = Vec::new();
    for item in items {
        for file in &item.files {
            if !files.iter().any(|known| known == file) {
                files.push(file.clone());
            }
        }
    }
    files
}

/// Validates that a group may be decomposed before anything is published.
///
/// The gate protects two invariants at once. A simple group collects independent
/// defects and is executed finding by finding, so it never owns a shared horizon
/// and asking for one is a caller error. An architectural group whose findings
/// all reached a terminal status has no remaining work to reproject, which is a
/// different outcome: nothing to execute.
///
/// The decision is taken before any status is published and before any provider
/// is resolved, so a refused round leaves the persisted session exactly as it was
/// and spends no request.
#[allow(dead_code)]
fn admit_group(group: &ReviewGroup, findings: &[ReviewItem]) -> Result<(), ResponseError> {
    if group.kind != ReviewGroupKind::Architectural {
        return Err(refine_error(
            errors::INVALID_PARAMS,
            format!(
                "Review group '{}' is a simple group: its findings are executed finding by finding, so it owns no shared horizon to decompose",
                group.id
            ),
        ));
    }

    if findings.is_empty() {
        return Err(refine_error(
            errors::NOTHING_TO_EXECUTE,
            format!(
                "Review group '{}' owns no finding, so there is no root cause to decompose",
                group.id
            ),
        ));
    }

    // The horizon is synthesized for the findings that still have work left in
    // them, so a group whose findings are all terminal has nothing to reproject.
    let pending = findings
        .iter()
        .any(|item| matches!(item.status, IssueStatus::Todo | IssueStatus::InProgress));
    if !pending {
        return Err(refine_error(
            errors::NOTHING_TO_EXECUTE,
            format!(
                "Every finding of review group '{}' is resolved; no remaining horizon can be synthesized",
                group.id
            ),
        ));
    }

    Ok(())
}

/// Validates that a synthesized group horizon is executable once merged with the
/// group's immutable `Done` foundation.
///
/// The merge itself belongs to the engine: `merge_subplan` is the single
/// authoritative synthesis primitive, shared with plan horizon replacement and
/// finding decomposition, so the server only maps its verdict into the typed
/// review error instead of re-deriving the dependency rules locally. A horizon
/// that names an unknown step, closes a dependency cycle, or leaves no ready step
/// is refused before it reaches storage, leaving the stored group intact.
#[allow(dead_code)]
fn validate_synthesized_group_subplan(
    existing: &[PlanItem],
    synthesized: &[PlanItem],
) -> Result<(), ResponseError> {
    merge_subplan(existing, synthesized.to_vec())
        .map(|_| ())
        .map_err(|err| {
            refine_error(
                errors::INVALID_PLAN_TREE,
                format!("Architect returned an unexecutable group subplan: {}", err),
            )
        })
}

/// Runs an Architect-driven decomposition of one architectural review group.
///
/// Plan horizon synthesis, finding decomposition, and group decomposition are the
/// same Architect operation, so this function drives the shared
/// [`run_architect_discovery_loop`] and differs only in what it feeds the model
/// and where the synthesized horizon is persisted.
///
/// The group is the operational unit of remediation: its findings are symptoms of
/// one root cause, so the Architect answers with a single subplan that removes the
/// cause instead of one plan per symptom. The horizon reaches disk through the
/// core domain primitive `ReviewStorage::replace_uncompleted_group_subplan`, which
/// preserves the immutable `Done` foundation, records the Architect configuration
/// that produced the horizon, and leaves every finding untouched.
///
/// The function publishes no finding status. Planning is a lifecycle state of the
/// execution that runs the horizon, and a round that yields no horizon must leave
/// every finding exactly as it was stored.
///
/// `diagnostics` carries the failure of an already executed sub-step of this
/// group. Supplying it turns the same synthesis into horizon replanning: the
/// Architect reprojects the remaining work around the failed approach while the
/// immutable `Done` foundation is preserved. A replanning round also escalates the
/// Architect configuration recorded on the group, so the reprojection runs on a
/// stronger rung than the one whose horizon already failed.
///
/// `custom_model` and `instructions` are the developer's explicit override and
/// directives for this run. `is_batch` declares the execution context of the
/// round: `false` for a standalone turn, `true` for a member of a running batch.
/// The flag is copied verbatim into the emitted `ModelStartedEvent`, so the
/// interface freezes the shared turn and lets the batch supervisor emit the single
/// terminal signal.
#[allow(clippy::too_many_arguments)]
#[allow(dead_code)]
pub(super) async fn refine_group_with_architect(
    state: &Arc<AppState>,
    repo_root: &Path,
    session: &ReviewSession,
    group: &ReviewGroup,
    custom_model: Option<ModelSelection>,
    instructions: Option<String>,
    diagnostics: Option<ReviewGroupRefineDiagnostics<'_>>,
    operation: &OperationGuard,
    is_batch: bool,
) -> Result<GroupRefineOutcome, ResponseError> {
    // The gate runs before anything is published or resolved: a refused round
    // must leave the stored session exactly as it was and spend no request.
    let group_items = group_findings(session, &group.id);
    admit_group(group, &group_items)?;

    let (
        provider,
        architect_tier,
        level_up,
        escalation_excluded,
        repomap_budget,
        max_discovery_rounds,
        files_token_budget,
        max_parse_retries,
        include_repo_map,
        discovery_limits,
    ) = {
        let cfg = state.config.lock().await.config.clone();
        // Tier resolution and the lazy level-up invariant are core-owned domain
        // policy shared with plan horizon synthesis and finding decomposition, so
        // the server never re-derives the ladder locally.
        let (architect_tier, level_up) = tauqe_core::plan::prompt::resolve_architect_tier(
            &cfg,
            group.subplan_architect.as_ref(),
            custom_model.as_ref(),
            diagnostics.as_ref().map(|d| d.failed_step_id),
            group.subplan_last_replanned_step.as_deref(),
        );
        let provider =
            match resolve_architect_provider(state, &cfg, architect_tier.model.provider).await {
                Ok(provider) => provider,
                Err(err) => return Err(refine_error(errors::NO_API_KEY, err)),
            };
        // A manual run on a configuration the automatic ladder excludes cannot
        // escalate, which the published note states instead of leaving the
        // developer to assume a silent downgrade.
        let escalation_excluded = cfg
            .models
            .excluded_from_escalation(ModelRole::Architect, &architect_tier.model);
        (
            provider,
            architect_tier,
            level_up,
            escalation_excluded,
            cfg.context.repomap_token_budget,
            cfg.context.max_discovery_rounds,
            cfg.context.files_token_budget,
            cfg.develop.max_retries,
            cfg.context.include_repo_map,
            tauqe_core::workflow::discovery::DiscoveryLimits::from_context_config(&cfg.context),
        )
    };
    let model = architect_tier.model.clone();
    let reasoning_effort = architect_tier.effort;
    let group_label = if group.title.trim().is_empty() {
        group.id.as_str()
    } else {
        group.title.trim()
    };

    // A repeated failure of the same sub-step runs on the escalated rung and
    // records it with the reprojected horizon, so the next wave continues up the
    // ladder instead of restarting from the configuration that already failed.
    if let Some(level) = &level_up {
        tracing::warn!(
            review_id = %session.id,
            group_id = %group.id,
            from_tier = ?level.from,
            to_tier = ?level.to,
            "review group architect level up"
        );
    }

    let level_up_note = format_architect_escalation_note(
        &architect_tier,
        level_up.as_ref(),
        escalation_excluded && diagnostics.is_some(),
    );

    clear_auto_context(state).await;

    // Guarantee baseline visibility: load the target files every finding of the
    // group declares into the read-only auto layer before the first generation
    // round, so the architect inspects the code that manifests the root cause
    // instead of inferring it from the repo map alone.
    let group_files = group_target_files(&group_items);
    preload_readonly_files(state, repo_root, &group_files).await;

    let op_id = format!("op-{}", next_operation_id());
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
            // The round is the just-in-time decomposition of one review group, so
            // the running work is declared here instead of being reconstructed
            // from the stored review; a batch member never settles the shared
            // turn on its own.
            work_context: Some(TurnWorkContext::ReviewPhase {
                review_id: session.id.clone(),
                review_title: session.title.clone(),
                phase: ReviewWorkPhase::Decomposition,
                queue: TurnWorkQueue::default(),
            }),
            is_batch,
        })
        .ok(),
    );

    let intro_header = match &diagnostics {
        Some(diag) => format!(
            "# {}: [{}] {}\n*Replanning the group horizon around failed sub-step `{}`...*{}\n\n---\n\n",
            GROUP_OPERATION_TITLE, session.id, group_label, diag.failed_step_id, level_up_note
        ),
        None => format!(
            "# {}: [{}] {}\n*Analyzing codebase contours and decomposing the group root cause into executable sub-steps...*{}\n\n---\n\n",
            GROUP_OPERATION_TITLE, session.id, group_label, level_up_note
        ),
    };
    emit(
        &state.out,
        events::MODEL_TEXT_DELTA,
        serde_json::to_value(ModelDeltaEvent {
            operation_id: op_id.clone(),
            delta: intro_header.clone(),
        })
        .ok(),
    );

    // The user turn and its response are recorded together once the decomposition
    // reaches a terminal outcome, so a cancelled, failed, or rejected round leaves
    // no orphaned user turn in the transcript. The prompt and the context paths are
    // captured here because they describe the state the Architect actually saw.
    let (history_turn_prompt, history_context_paths) = {
        let ctx = state.context.read().await;
        let context_paths: Vec<String> = ctx
            .get_state()
            .items
            .into_iter()
            .map(|item| item.path)
            .collect();
        let mut turn_prompt = format!(
            "# {}: [{}] {}\n\nDecompose the root cause shared by {} finding(s) of this group into one executable subplan.\n",
            GROUP_OPERATION_TITLE,
            session.id,
            group_label,
            group_items.len()
        );
        if let Some(directives) = &instructions {
            let trimmed = directives.trim();
            if !trimmed.is_empty() {
                turn_prompt.push_str(&format!("\nDirectives: {}\n", trimmed));
            }
        }
        (turn_prompt, context_paths)
    };

    let response_format = {
        let schema_val = tauqe_core::plan::prompt::plan_refine_response_schema_definition();
        match serde_json::from_value::<JsonSchemaDefinition>(schema_val) {
            Ok(schema) => ResponseFormat::JsonSchema {
                json_schema: schema,
            },
            Err(err) => return Err(refine_error(errors::SCHEMA_ERROR, err.to_string())),
        }
    };
    let system_prompt = build_review_group_refine_system_prompt();
    let discovery = ArchitectDiscoveryRequest {
        repo_root,
        op_id: &op_id,
        operation_title: GROUP_OPERATION_TITLE,
        system_prompt: &system_prompt,
        intro_header: &intro_header,
        budget_hint: "The group decomposition is too broad and should be scoped down.",
        response_format,
        repomap_budget,
        include_repo_map,
        max_discovery_rounds,
        files_token_budget,
        discovery_limits,
        max_parse_retries,
        // Decomposition is an isolated design dialogue of one review session: its
        // cache is keyed by the audit, never by the interactive REPL.
        session_id: Some(format_session_id(SESSION_PREFIX_REVIEW, &session.id)),
    };

    let build_user_prompt =
        |context_files: &[ContextFileContent], repo_map: Option<&str>, repo_dirs: Option<&str>| {
            let diagnostics_view = diagnostics.map(|d| ReviewGroupRefineDiagnostics {
                failed_step_id: d.failed_step_id,
                failure_reason: d.failure_reason,
            });
            build_review_group_refine_user_prompt(
                session,
                group,
                &group_items,
                context_files,
                repo_map,
                repo_dirs,
                diagnostics_view,
                instructions.as_deref(),
            )
        };

    let output = match run_architect_discovery_loop(
        state,
        provider,
        &model.name,
        discovery,
        build_user_prompt,
        reasoning_effort,
        operation,
    )
    .await
    {
        Ok(ArchitectDiscoveryOutcome {
            output,
            budget_exceeded: None,
        }) => output,
        Ok(ArchitectDiscoveryOutcome {
            budget_exceeded: Some(warn_msg),
            ..
        }) => {
            record_synthesis_history(
                state,
                history_turn_prompt,
                history_context_paths,
                warn_msg.clone(),
                Some(format!(
                    "{}: Token Budget Exceeded ({})",
                    GROUP_OPERATION_TITLE, group_label
                )),
            )
            .await;
            return Ok(GroupRefineOutcome::Blocked { reason: warn_msg });
        }
        Err(ArchitectDiscoveryError::Cancelled) => {
            clear_auto_context(state).await;
            return Err(refine_error(
                errors::CANCELLED,
                format!("{} was cancelled", GROUP_OPERATION_TITLE),
            ));
        }
        Err(ArchitectDiscoveryError::ModelError(err)) => {
            clear_auto_context(state).await;
            return Err(refine_error(errors::MODEL_ERROR, err));
        }
        Err(ArchitectDiscoveryError::ParseError(err)) => {
            clear_auto_context(state).await;
            return Err(refine_error(errors::PARSE_ERROR, err));
        }
    };

    if output.status.eq_ignore_ascii_case("blocked") {
        let reason = output.reason.unwrap_or(output.explanation);
        let blocked_msg = format!(
            "Architect reported blockers for review group [{}]:\n\n{}",
            group.id, reason
        );
        record_synthesis_history(
            state,
            history_turn_prompt,
            history_context_paths,
            blocked_msg,
            Some(format!(
                "{}: Blocked ({})",
                GROUP_OPERATION_TITLE, group_label
            )),
        )
        .await;
        return Ok(GroupRefineOutcome::Blocked { reason });
    }

    if output.status.eq_ignore_ascii_case("needs_context") {
        return Err(refine_error(
            errors::NEEDS_CONTEXT,
            format!(
                "Architect requested additional context but no discovery rounds remain (limit: {}). Narrow the group scope or raise the discovery round limit, then decompose the group again",
                max_discovery_rounds
            ),
        ));
    }

    if !output.status.eq_ignore_ascii_case("success") {
        return Err(refine_error(
            errors::UNEXPECTED_STATUS,
            format!(
                "Architect returned unsupported status '{}'; expected 'success' or 'blocked'",
                output.status
            ),
        ));
    }

    if output.items.is_empty() {
        return Err(refine_error(
            errors::EMPTY_ITEMS,
            "Model returned success status but an empty items array".to_string(),
        ));
    }

    if let Some(duplicate) = find_duplicate_item_ids(&output.items) {
        return Err(refine_error(
            errors::INVALID_PLAN_TREE,
            format!(
                "Architect returned a group subplan with a duplicate or empty step ID: '{}'",
                duplicate
            ),
        ));
    }

    // A structurally valid step list can still be unexecutable once merged with
    // the group's immutable `Done` foundation: a `depends_on` may name a step the
    // foundation and the new horizon both lack, and the horizon may leave no ready
    // step at all. The verdict is taken from the engine's synthesis primitive here,
    // before anything reaches storage, so a refused horizon never rewrites the
    // persisted group.
    validate_synthesized_group_subplan(&group.subplan, &output.items)?;

    // Persist the merged subplan through the core domain primitive, which preserves
    // the immutable `Done` foundation and rejects dependency cycles before anything
    // reaches disk.
    let review_id = session.id.clone();
    let group_id = group.id.clone();
    let explanation = output.explanation;
    let new_subplan = output.items;
    let save_root = repo_root.to_path_buf();
    let save_review_id = review_id.clone();
    let subplan_architect = architect_tier;
    // The sub-step whose horizon this wave reprojects is persisted with the new
    // subplan, so the next wave reads from disk whether this sub-step already
    // failed a replanning round.
    let subplan_replanned_step = diagnostics.map(|d| d.failed_step_id.to_string());
    let updated = match offload_review(move || {
        ReviewStorage::replace_uncompleted_group_subplan(
            &save_root,
            &save_review_id,
            &group_id,
            new_subplan,
            subplan_architect,
            subplan_replanned_step,
        )
    })
    .await
    {
        Ok(Some(updated)) => updated,
        Ok(None) => {
            return Err(refine_error(
                errors::NOT_FOUND,
                format!(
                    "Review group '{}' not found in session '{}'",
                    group.id, review_id
                ),
            ));
        }
        Err(err) => return Err(refine_error(errors::PLAN_SAVE_FAILED, err)),
    };

    let subplan_steps = updated.subplan.len();
    tracing::info!(
        review_id = %review_id,
        group_id = %updated.id,
        subplan_steps,
        findings = group_items.len(),
        "review group decomposed"
    );

    let history_response = format!(
        "Review group [{}] decomposed into {} top-level sub-step(s).\n\n{}",
        review_id, subplan_steps, explanation
    );
    record_synthesis_history(
        state,
        history_turn_prompt,
        history_context_paths,
        history_response,
        Some(format!("{}: {}", GROUP_OPERATION_TITLE, group_label)),
    )
    .await;

    publish_review(state, repo_root, &review_id).await;

    Ok(GroupRefineOutcome::Success { explanation })
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::architect::format_architect_tier;
    use tauqe_core::config::AppConfig;
    use tauqe_protocol::{ArchitectTier, ModelRef, ReasoningEffort};

    #[test]
    fn architect_tier_banner_renders_its_reasoning_depth() {
        // Escalation is visible to the developer, so the banner must name the
        // rung that actually runs, including its reasoning depth.
        let tier = ArchitectTier::new(
            ModelRef::openrouter("planner/model"),
            Some(ReasoningEffort::High),
        );
        assert_eq!(
            format_architect_tier(&tier),
            format!("openrouter:planner/model ({})", ReasoningEffort::High)
        );
        // A rung without a reasoning depth renders as a bare model reference.
        assert_eq!(
            format_architect_tier(&ArchitectTier::from(ModelRef::openrouter("planner/model"))),
            "openrouter:planner/model"
        );
    }

    /// Builds a review group of the given class.
    fn group_of(id: &str, kind: ReviewGroupKind) -> ReviewGroup {
        ReviewGroup {
            id: id.to_string(),
            title: format!("Group {id}"),
            kind,
            ..Default::default()
        }
    }

    /// Builds a finding with the given status that belongs to no explicit group.
    fn finding_in(status: IssueStatus) -> ReviewItem {
        ReviewItem {
            id: 1,
            title: "Finding".to_string(),
            uuid: None,
            group: None,
            model: None,
            issue_type: tauqe_protocol::IssueType::Quality,
            severity: tauqe_protocol::ReviewSeverity::Suggestion,
            status,
            recommendations: Vec::new(),
            files: Vec::new(),
            readonly_files: Vec::new(),
            resolution_notes: None,
            body: String::new(),
        }
    }

    /// Builds a finding that belongs to the named group.
    fn finding_of_group(group: Option<&str>, status: IssueStatus) -> ReviewItem {
        let mut item = finding_in(status);
        item.group = group.map(str::to_string);
        item
    }

    /// Builds a session holding the given findings.
    fn grouped_session(items: Vec<ReviewItem>) -> ReviewSession {
        ReviewSession {
            id: "rev-groups".to_string(),
            title: "Grouped Audit".to_string(),
            created_at: 7,
            model: ModelRef::openrouter("test/architect"),
            items,
            ..ReviewSession::default()
        }
    }

    #[test]
    fn group_membership_follows_the_effective_group_of_a_finding() {
        let session = grouped_session(vec![
            finding_of_group(Some("arch-1"), IssueStatus::Todo),
            // A finding stored before grouping existed belongs to the implicit
            // default group, exactly as the grouped session view resolves it.
            finding_of_group(None, IssueStatus::Todo),
        ]);

        assert_eq!(group_findings(&session, "arch-1").len(), 1);
        assert_eq!(group_findings(&session, DEFAULT_REVIEW_GROUP_ID).len(), 1);
        assert!(group_findings(&session, "arch-2").is_empty());
    }

    #[test]
    fn group_decomposition_admits_an_architectural_group_with_pending_work() {
        let group = group_of("arch-1", ReviewGroupKind::Architectural);
        let session = grouped_session(vec![
            finding_of_group(Some("arch-1"), IssueStatus::Fixed),
            finding_of_group(Some("arch-1"), IssueStatus::Todo),
        ]);

        assert!(admit_group(&group, &group_findings(&session, "arch-1")).is_ok());
    }

    #[test]
    fn group_decomposition_refuses_a_simple_group() {
        // A simple group collects independent defects: its findings are executed
        // finding by finding, so it never owns a shared horizon.
        let group = group_of("local", ReviewGroupKind::Simple);
        let findings = vec![finding_of_group(Some("local"), IssueStatus::Todo)];

        let err = admit_group(&group, &findings).expect_err("a simple group has no horizon");
        assert_eq!(err.code, errors::INVALID_PARAMS);
        assert!(
            err.message.contains("simple group"),
            "the diagnostic must name the class conflict: {}",
            err.message
        );
    }

    #[test]
    fn group_decomposition_refuses_a_group_without_remaining_work() {
        let group = group_of("arch-1", ReviewGroupKind::Architectural);

        // The group owns no member finding at all.
        let err = admit_group(&group, &[]).expect_err("nothing to decompose");
        assert_eq!(err.code, errors::NOTHING_TO_EXECUTE);

        // Every finding of the group already reached a terminal status.
        let findings = vec![
            finding_of_group(Some("arch-1"), IssueStatus::Fixed),
            finding_of_group(Some("arch-1"), IssueStatus::Skipped),
        ];
        let err = admit_group(&group, &findings).expect_err("nothing is left to reproject");
        assert_eq!(err.code, errors::NOTHING_TO_EXECUTE);
    }

    /// The admission gate runs before any publication and before any provider is
    /// resolved, so a refused round spends no request and leaves the stored session
    /// exactly as it was. The fixture configures no provider at all, so a gate that
    /// ran late would surface `NO_API_KEY` instead of the class refusal.
    #[tokio::test]
    async fn group_decomposition_refuses_a_simple_group_before_any_provider_call() {
        let root = std::env::temp_dir().join(format!(
            "tauqe-review-group-refine-{}-{}",
            std::process::id(),
            crate::state::next_operation_id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let (out, _events) = crate::transport::OutChannel::new();
        let (history_sender, _history_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(AppConfig::default(), root.clone(), history_sender, out);

        let session = grouped_session(vec![finding_of_group(Some("local"), IssueStatus::Todo)]);
        let group = group_of("local", ReviewGroupKind::Simple);
        let operation = state.try_begin_operation().expect("the slot is free");

        let err = refine_group_with_architect(
            &state, &root, &session, &group, None, None, None, &operation, false,
        )
        .await
        .expect_err("a simple group is never decomposed");

        assert_eq!(err.code, errors::INVALID_PARAMS);
        let _ = std::fs::remove_dir_all(&root);
    }
}
