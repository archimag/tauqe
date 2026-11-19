//! Interactive REPL control loop.
//!
//! [`run_repl_loop`] orchestrates interactive developer dialogue, research and
//! discovery context enrichment via [`run_research_loop`], and code modification
//! with toolchain verification via [`run_edit_loop`].

mod execution;
pub use execution::{run_repl_execution, ReplExecutionConfig};

use crate::llm::CapabilityTree;
use crate::llm::LlmCapability;
use crate::llm::LlmCommand;

use super::context::LoopContext;
use super::outcome::{LoopResult, LoopSuccess, ResponseEnvelope, SystemError};
use super::turn::{completed_without_change_set, run_turn_loop, TurnCompletion, TurnLoopConfig};
use crate::prompt::TurnBrief;
use crate::providers::StreamEvent;

/// Configuration for the interactive REPL loop.
#[derive(Debug, Clone, Default)]
pub struct ReplLoopConfig {
    /// Configuration for the underlying bounded turn loop.
    turn: TurnLoopConfig,
}

impl ReplLoopConfig {
    /// Creates REPL loop configuration from authoritative project configuration.
    pub(crate) fn from_app_config(config: &crate::config::AppConfig) -> Self {
        Self {
            turn: TurnLoopConfig::from_app_config(config),
        }
    }
}

/// Builds the algebraic capability expression for the interactive REPL loop.
pub(crate) fn repl_capability_expr(
    focus: Option<&crate::discussion::DiscussionTarget>,
) -> CapabilityTree {
    let (plan_update, review_decision) = match focus {
        Some(crate::discussion::DiscussionTarget::Plan { .. }) => (true, false),
        Some(crate::discussion::DiscussionTarget::Review { .. })
        | Some(crate::discussion::DiscussionTarget::ReviewGroup { .. }) => (false, true),
        None => (false, false),
    };
    dialogue_capability_expr(plan_update, review_decision)
}

/// Composes the capability surface every dialogue contour advertises.
///
/// The scratchpad and the conversational message are ambient: they belong to
/// no branch, so the response root carries exactly one `message` field at
/// every stage of the turn, whatever branch the `action` union selects. The
/// operational branches own the commands of their kind alone, and the answer
/// branch is a bare marker: a round with nothing to act on names the branch
/// and writes the ambient message.
pub(crate) fn dialogue_capability_expr(plan_update: bool, review_decision: bool) -> CapabilityTree {
    let ambient = CapabilityTree::root([
        CapabilityTree::leaf(LlmCapability::Scratchpad),
        CapabilityTree::leaf(LlmCapability::TextMessage),
    ]);
    let research = research_branch(true);
    let edit = edit_branch();
    let plan_capabilities = if plan_update {
        CapabilityTree::any_of(
            "plan_action",
            [
                CapabilityTree::named_set(
                    "create",
                    [CapabilityTree::leaf(LlmCapability::PlanCreation)],
                ),
                CapabilityTree::named_set(
                    "modify",
                    [CapabilityTree::leaf(LlmCapability::PlanUpdate)],
                ),
            ],
        )
    } else {
        CapabilityTree::leaf(LlmCapability::PlanCreation)
    };
    let plan = CapabilityTree::named_set(
        "plan",
        [
            plan_capabilities,
            CapabilityTree::leaf(LlmCapability::CommitMessage),
            CapabilityTree::leaf(LlmCapability::TurnError),
            CapabilityTree::leaf(LlmCapability::TurnSkip),
        ],
    );
    // The answer branch is a bare marker: it owns no field of its own, because
    // the conversational message is ambient and every operational command
    // belongs to a branch that acts.
    let answer = CapabilityTree::named_set("answer", std::iter::empty::<CapabilityTree>());
    let mut branches = vec![
        research,
        edit,
        plan,
        answer,
        blocked_branch(),
        abort_branch(),
    ];
    if review_decision {
        branches.push(CapabilityTree::named_set(
            "review",
            [
                CapabilityTree::leaf(LlmCapability::ReviewUpdate),
                CapabilityTree::leaf(LlmCapability::CommitMessage),
                CapabilityTree::leaf(LlmCapability::TurnError),
                CapabilityTree::leaf(LlmCapability::TurnSkip),
            ],
        ));
    }
    CapabilityTree::root([ambient, CapabilityTree::any_of("action", branches)])
}

/// The discovery branch every contour shares.
///
/// A step withholds the internal documentation of the harness: the request
/// belongs to a dialogue that answers for the environment, never to a turn
/// that executes an approved design inside it.
pub(crate) fn research_branch(with_docs: bool) -> CapabilityTree {
    let mut capabilities = vec![
        LlmCapability::ContextRequest,
        LlmCapability::RepoSearch,
        LlmCapability::RepoDirs,
        LlmCapability::RepoMap,
        LlmCapability::DirList,
    ];
    if with_docs {
        capabilities.push(LlmCapability::DocRequest);
    }
    capabilities.push(LlmCapability::OuterDocRequest);
    capabilities.push(LlmCapability::CustomTools);
    CapabilityTree::named_set(
        "research",
        capabilities.into_iter().map(CapabilityTree::leaf),
    )
}

/// The edit branch every contour shares: the change set and the terminal
/// outcome fields that act beside it.
pub(crate) fn edit_branch() -> CapabilityTree {
    CapabilityTree::named_set(
        "edit",
        [
            LlmCapability::EditReplace,
            LlmCapability::EditCreate,
            LlmCapability::EditDelete,
            LlmCapability::EditMove,
            LlmCapability::EditMoveDir,
            LlmCapability::EditOverwrite,
            LlmCapability::CommitMessage,
            LlmCapability::TurnError,
            LlmCapability::TurnSkip,
        ]
        .map(CapabilityTree::leaf),
    )
}

/// The semantic-blocker branch every contour shares.
pub(crate) fn blocked_branch() -> CapabilityTree {
    CapabilityTree::named_set("blocked", [CapabilityTree::leaf(LlmCapability::Blocked)])
}

/// The unconditional-abort branch every contour shares.
pub(crate) fn abort_branch() -> CapabilityTree {
    CapabilityTree::named_set("abort", [CapabilityTree::leaf(LlmCapability::Abort)])
}

/// Returns the capability tree supported by the interactive REPL loop.
pub fn repl_capabilities(focus: Option<&crate::discussion::DiscussionTarget>) -> CapabilityTree {
    repl_capability_expr(focus)
}

pub struct ReplArtifact {
    /// Conversational response text addressed to the developer.
    pub message: Option<String>,
    /// Repository-relative paths touched by code edits during this turn.
    pub modified_paths: Vec<String>,
    /// Whether plan mutations were applied and saved.
    pub plans_applied: bool,
    /// Rejections or errors encountered when applying plan mutations.
    pub plan_rejections: Vec<crate::plan::PlanRejection>,
    /// Status adjustments or warnings emitted during plan mutations.
    pub plan_warnings: Vec<String>,
    /// IDs of review findings updated by review decisions during this turn.
    pub review_decisions_applied: Vec<u32>,
    pub envelope: ResponseEnvelope,
    /// Git commit hash if changes were committed and squashed.
    pub commit_hash: Option<String>,
}

/// Runs the interactive REPL turn lifecycle.
pub(crate) async fn run_repl_loop(
    ctx: &mut LoopContext<'_>,
    config: &ReplLoopConfig,
    focus: Option<&crate::discussion::DiscussionTarget>,
) -> Result<LoopResult<ReplArtifact>, SystemError> {
    ctx.check_cancelled()?;

    let repl_caps = repl_capabilities(focus);
    let mut ctx = ctx.reborrow_with_capabilities(repl_caps);
    ctx.set_turn_brief(Some(TurnBrief::dialogue()));
    let turn_result = match run_turn_loop(
        &mut ctx,
        None,
        &config.turn,
        TurnCompletion::AnswerAllowed,
    )
    .await
    {
        Err(SystemError::RoundBudgetExhausted { spent, last_answer }) => {
            let answer = last_answer.unwrap_or_else(|| {
                "The turn reached its round budget before producing an answer.".to_owned()
            });
            let text = format!(
                "{answer}\n\n---\n**[Round Budget Reached]** The turn spent its whole budget of {spent} provider call(s). \
                 The answer above is the last one the model produced; ask again to continue from the material the turn already gathered."
            );
            let envelope =
                ResponseEnvelope::from_commands(&[LlmCommand::Text(crate::llm::TextCommand {
                    content: text,
                })]);
            completed_without_change_set(spent, envelope)
        }
        result => result?,
    };

    let (mut envelope, turn_artifact) = match turn_result {
        LoopResult::Success(success) => (success.response, success.value),
        LoopResult::Diverged(env) => return Ok(LoopResult::Diverged(env)),
        LoopResult::Exhausted(report) => return Ok(LoopResult::Exhausted(report)),
    };
    let message = extract_text_message(&mut envelope);

    let (plans_applied, plan_rejections, plan_warnings) =
        apply_plan_commands(&mut ctx, &mut envelope, focus).await?;
    let review_decisions_applied = apply_review_commands(&ctx, &mut envelope, focus).await;
    let _ = envelope.take_any(&[
        LlmCapability::CommitMessage,
        LlmCapability::TurnError,
        LlmCapability::TurnSkip,
    ]);
    let remaining = envelope.remaining_capabilities();
    if !remaining.is_empty() {
        tracing::warn!(
            repo_root = %ctx.repo_root.display(),
            ?remaining,
            "Unhandled capabilities remaining in REPL response envelope"
        );
    }

    Ok(LoopResult::Success(LoopSuccess::new(
        ReplArtifact {
            message,
            modified_paths: turn_artifact.modified_paths,
            plans_applied,
            plan_rejections,
            plan_warnings,
            review_decisions_applied,
            envelope: envelope.clone(),
            commit_hash: turn_artifact.commit_hash,
        },
        envelope,
    )))
}

/// Applies every plan mutation the round carries against local plan storage.
async fn apply_plan_mutations(
    ctx: &LoopContext<'_>,
    envelope: &mut ResponseEnvelope,
    focus: Option<&crate::discussion::DiscussionTarget>,
) -> (bool, Vec<crate::plan::PlanRejection>, Vec<String>) {
    let mut plan_cmds = envelope.take(LlmCapability::PlanCreation);
    plan_cmds.extend(envelope.take(LlmCapability::PlanUpdate));

    if plan_cmds.is_empty() {
        return (false, Vec::new(), Vec::new());
    }

    let mut updates = Vec::new();
    for cmd in plan_cmds {
        if let LlmCommand::Plan { update } = cmd {
            updates.push(update);
        }
    }

    if updates.is_empty() {
        return (false, Vec::new(), Vec::new());
    }

    // The mode is a property of the turn, decided by the focus the dialogue was
    // opened with and never derived from the payload.
    let mode = match focus {
        Some(crate::discussion::DiscussionTarget::Plan { .. }) => {
            crate::plan::storage::PlanMutationMode::Discussion
        }
        _ => crate::plan::storage::PlanMutationMode::Repl,
    };

    let repo_root = ctx.repo_root.to_path_buf();
    let outcomes = crate::blocking::run_blocking_async(move || {
        crate::plan::storage::PlanStorage::apply_discussion_plan_updates_with_mode(
            &repo_root, &updates, mode,
        )
    })
    .await
    .unwrap_or_default();

    let mut plans_applied = false;
    let mut plan_rejections = Vec::new();
    let mut plan_warnings = Vec::new();

    for outcome in outcomes {
        if !outcome.applied.is_empty() {
            plans_applied = true;
        }
        plan_rejections.extend(outcome.errors);
        plan_warnings.extend(outcome.warnings);
    }

    (plans_applied, plan_rejections, plan_warnings)
}

/// Applies the plan mutations a dialogue round declared, granting a defective
/// payload a bounded number of corrective rounds.
///
/// A refusal typed as a payload defect never reached plan storage, so the
/// model reads the refusal and the round answers again. Once the budget is
/// spent the refusal travels in the turn artifact and the developer is told
/// that the correction was exhausted.
async fn apply_plan_commands(
    ctx: &mut LoopContext<'_>,
    envelope: &mut ResponseEnvelope,
    focus: Option<&crate::discussion::DiscussionTarget>,
) -> Result<(bool, Vec<crate::plan::PlanRejection>, Vec<String>), SystemError> {
    let mut plans_applied = false;
    let mut plan_rejections = Vec::new();
    let mut plan_warnings = Vec::new();
    let mut corrections = 0usize;

    loop {
        let (applied, rejections, warnings) = apply_plan_mutations(ctx, envelope, focus).await;
        plans_applied |= applied;
        plan_warnings.extend(warnings);
        if applied && corrections > 0 {
            tracing::info!(
                repo_root = %ctx.repo_root.display(),
                corrections,
                "a corrective round repaired the plan payload"
            );
        }
        let mut defects = Vec::new();
        for rejection in rejections {
            match rejection.kind {
                crate::plan::PlanRejectionKind::PayloadDefect => {
                    defects.push(rejection.message);
                }
                _ => plan_rejections.push(rejection),
            }
        }

        if defects.is_empty() {
            return Ok((plans_applied, plan_rejections, plan_warnings));
        }

        corrections += 1;
        tracing::warn!(
            repo_root = %ctx.repo_root.display(),
            correction = corrections,
            defects = defects.len(),
            "plan mutation payload refused; asking for a corrected payload"
        );
        ctx.events.emit(StreamEvent::TextDelta(format!(
            "\n\n---\n**[Plan Correction {}]** The plan payload was refused before it reached storage; asking for a corrected payload...\n\n",
            corrections
        )));
        let repair = plan_correction_prompt(&defects, corrections);
        match ctx.request_repair(&repair, None).await {
            Ok(env) => *envelope = env,
            Err(SystemError::RoundBudgetExhausted { .. }) => {
                tracing::warn!(
                    repo_root = %ctx.repo_root.display(),
                    corrections,
                    defects = defects.len(),
                    "the plan mutation stayed defective when turn round budget was exhausted"
                );
                ctx.events.emit(StreamEvent::TextDelta(
                    "\n\n---\n**[Plan Correction Limit Reached]** The turn round budget was exhausted during plan correction; the refusal is reported below.\n\n".to_string(),
                ));
                plan_rejections.extend(
                    defects
                        .into_iter()
                        .map(crate::plan::PlanRejection::payload_defect),
                );
                return Ok((plans_applied, plan_rejections, plan_warnings));
            }
            Err(e) => return Err(e),
        }
    }
}

/// Composes the directive that asks the model to repair a refused plan payload.
fn plan_correction_prompt(defects: &[String], correction: usize) -> String {
    let mut prompt = format!(
        "<response_directive>\n[Plan Correction {correction}] The plan payload you declared was refused before it reached plan storage, so the stored plan is unchanged. Repair the payload and emit it again through the plan branch of the action union.\n"
    );
    for defect in defects {
        prompt.push_str("\n<plan_rejection>\n");
        prompt.push_str(&crate::escape::escape_xml_text(defect));
        prompt.push_str("\n</plan_rejection>");
    }
    prompt.push_str("\n</response_directive>");
    prompt
}

async fn apply_review_commands(
    ctx: &LoopContext<'_>,
    envelope: &mut ResponseEnvelope,
    focus: Option<&crate::discussion::DiscussionTarget>,
) -> Vec<u32> {
    let review_cmds = envelope.take(LlmCapability::ReviewUpdate);
    if review_cmds.is_empty() {
        return Vec::new();
    }

    let Some(review_id) = focus.and_then(crate::discussion::DiscussionTarget::review_session_id)
    else {
        tracing::warn!(
            "Review update commands were emitted without an active review focus; ignoring"
        );
        return Vec::new();
    };

    let repo_root = ctx.repo_root.to_path_buf();
    let mut updated_items = Vec::new();

    for cmd in review_cmds {
        if let LlmCommand::ReviewUpdate { update } = cmd {
            let item_id = update.item_id;
            let root_clone = repo_root.clone();
            let rev_id_clone = review_id.to_owned();
            let apply_res = crate::blocking::run_blocking_async(move || {
                crate::review::storage::ReviewStorage::apply_decision(
                    &root_clone,
                    &rev_id_clone,
                    &update,
                )
            })
            .await;

            match apply_res {
                Ok(Ok(Some(_))) => {
                    updated_items.push(item_id);
                }
                Ok(Ok(None)) => {
                    tracing::warn!(item_id, "Review item not found in session for decision");
                }
                Ok(Err(err)) => {
                    tracing::error!(item_id, error = %err, "Failed to apply review decision");
                }
                Err(err) => {
                    tracing::error!(item_id, error = %err, "Failed to apply review decision");
                }
            }
        }
    }

    updated_items
}

fn extract_text_message(envelope: &mut ResponseEnvelope) -> Option<String> {
    let mut combined = String::new();
    for cmd in envelope.take(LlmCapability::TextMessage) {
        if let LlmCommand::Text(command) = cmd {
            if !combined.is_empty() {
                combined.push('\n');
            }
            combined.push_str(&command.content);
        }
    }
    if combined.is_empty() {
        None
    } else {
        Some(combined)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ContextAccess;
    use crate::context::ContextManager;
    use crate::control::context::{CancellationToken, EventSink};
    use crate::control::turn::{EDIT_CAPABILITIES, RESEARCH_CAPABILITIES};
    use crate::models::ModelRef;
    use crate::prompt::json::JsonPromptProtocol;
    use crate::providers::mock::{MockLlmProvider, MockResponse};
    use crate::workflow::research::journal::TurnResearchJournal;

    struct Fixture {
        dir: tempfile::TempDir,
        context: ContextManager,
        journal: TurnResearchJournal,
        provider: MockLlmProvider,
        model: ModelRef,
        protocol: JsonPromptProtocol,
        cache: crate::control::cache::TurnInputCache,
        cancel: CancellationToken,
        events: EventSink,
        policy: crate::workflow::git_policy::GitTransactionPolicy,
    }

    impl Fixture {
        fn new(files: &[(&str, &str)], responses: Vec<MockResponse>) -> Self {
            let context_paths: Vec<&str> = files.iter().map(|(p, _)| *p).collect();
            Self::new_with_context(files, &context_paths, responses)
        }

        fn new_with_context(
            files: &[(&str, &str)],
            context_files: &[&str],
            responses: Vec<MockResponse>,
        ) -> Self {
            let dir = tempfile::tempdir().expect("scratch directory");
            let root = dir.path().to_path_buf();
            crate::git::init_repository(Some(&root), true).expect("repository");
            for (path, content) in files {
                let full = root.join(path);
                if let Some(parent) = full.parent() {
                    std::fs::create_dir_all(parent).expect("dir");
                }
                std::fs::write(full, content).expect("file");
            }
            let owned: Vec<String> = files.iter().map(|(path, _)| path.to_string()).collect();
            if !owned.is_empty() {
                crate::git::cmd::stage_files(&root, &owned).expect("stage");
                crate::git::cmd::commit_with_developer_author(&root, "Initial commit", false)
                    .expect("commit");
            }
            let mut context = ContextManager::new(root);
            for path in context_files {
                context
                    .add_file(path, ContextAccess::Editable)
                    .expect("context file");
            }
            Self {
                dir,
                context,
                journal: TurnResearchJournal::default(),
                provider: MockLlmProvider::scripted(responses),
                model: ModelRef::openrouter("mock/model"),
                protocol: JsonPromptProtocol::new(crate::control::repl_capabilities(None)),
                cache: crate::control::cache::TurnInputCache::default(),
                cancel: CancellationToken::new(),
                events: EventSink::noop(),
                policy: crate::workflow::git_policy::GitTransactionPolicy::new(),
            }
        }

        fn context(&mut self) -> LoopContext<'_> {
            let caps = CapabilityTree::root(
                EDIT_CAPABILITIES
                    .iter()
                    .copied()
                    .chain(RESEARCH_CAPABILITIES.iter().copied())
                    .chain([LlmCapability::TextMessage])
                    .map(CapabilityTree::leaf),
            );
            let prompt_builder = crate::control::prompt::PromptBuilder::new(
                &self.provider,
                &self.model,
                &self.protocol,
                &mut self.cache,
            )
            .with_patch_strategy(crate::workflow::PatchStrategy::LineAnchor);
            LoopContext::new(
                &self.cancel,
                &self.events,
                self.dir.path(),
                &mut self.context,
                &mut self.journal,
                &mut self.policy,
                caps,
                prompt_builder,
            )
        }
    }

    #[tokio::test]
    async fn repl_loop_returns_pure_text_response() {
        let mut fixture = Fixture::new(&[], vec![MockResponse::text("Hello")]);
        let mut ctx = fixture.context();

        let outcome = run_repl_loop(&mut ctx, &ReplLoopConfig::default(), None)
            .await
            .expect("runs loop");
        assert!(outcome.is_success());
        let success = outcome.into_success().expect("success");
        assert_eq!(success.value.message.as_deref(), Some("Hello"));
        assert!(success.value.modified_paths.is_empty());
    }

    #[tokio::test]
    async fn review_decision_streams_the_message_and_updates_only_the_focused_session() {
        use crate::discussion::DiscussionTarget;
        use crate::review::{IssueStatus, ReviewSession, ReviewStorage};

        let response = serde_json::json!({
            "message": "Decision recorded.",
            "action": {
                "kind": "review",
                "review_update": {"item_id": 7, "group_id": null, "status": "rejected",
                    "note": "  Already verified  ", "recommendations": ["Add coverage", "Add coverage"]},
                "commit_message": "", "reason": ""
            }
        });
        let mut fixture = Fixture::new(&[], vec![MockResponse::text(response.to_string())]);
        for id in ["focused", "other"] {
            let session = ReviewSession {
                id: id.into(),
                title: id.into(),
                items: vec![
                    serde_json::from_value(serde_json::json!({"id": 7, "title": "Race"})).unwrap(),
                ],
                ..Default::default()
            };
            ReviewStorage::save_session(fixture.dir.path(), &session).unwrap();
        }
        let focus = DiscussionTarget::Review {
            id: "focused".into(),
        };
        fixture.protocol = JsonPromptProtocol::new(repl_capabilities(Some(&focus)));
        let (events, mut rx) = EventSink::channel();
        fixture.events = events;
        let result = run_repl_loop(&mut fixture.context(), &Default::default(), Some(&focus))
            .await
            .unwrap()
            .into_success()
            .unwrap();
        assert_eq!(result.value.review_decisions_applied, [7]);
        assert_eq!(result.value.message.as_deref(), Some("Decision recorded."));
        let mut streamed = String::new();
        while let Ok(event) = rx.try_recv() {
            if let StreamEvent::TextDelta(text) = event {
                streamed.push_str(&text);
            }
        }
        assert_eq!(streamed, "Decision recorded.");
        let load = |id| {
            ReviewStorage::load_review(fixture.dir.path(), id)
                .unwrap()
                .unwrap()
        };
        let stored = load("focused");
        let item = stored.find_item(7).unwrap();
        assert_eq!(item.status, IssueStatus::Rejected);
        assert_eq!(item.resolution_notes.as_deref(), Some("Already verified"));
        assert_eq!(item.recommendations, ["Add coverage"]);
        assert_eq!(
            load("other").find_item(7).unwrap().status,
            IssueStatus::Discussion
        );
    }

    #[tokio::test]
    async fn repl_loop_executes_edits_when_changes_present() {
        let payload = concat!(
            "{\"message\":\"Update hello\",\"action\":{\"kind\":\"edit\",",
            "\"changes\":[{\"op\":\"replace\",\"path\":\"hello.rs\",",
            "\"old_text\":\"fn hello() {}\\n\",",
            "\"new_text\":\"fn hello() -> i32 { 1 }\\n\"}],",
            "\"commit_message\":\"Update hello\",\"reason\":\"\"}}"
        );
        let mut fixture = Fixture::new(
            &[("hello.rs", "fn hello() {}\n")],
            vec![MockResponse::text(payload)],
        );
        let mut ctx = fixture.context();

        let outcome = run_repl_loop(&mut ctx, &ReplLoopConfig::default(), None)
            .await
            .expect("runs loop");
        assert!(outcome.is_success());
        let success = outcome.into_success().expect("success");
        assert_eq!(success.value.modified_paths, vec!["hello.rs"]);
        assert_eq!(
            std::fs::read_to_string(fixture.dir.path().join("hello.rs")).unwrap(),
            "fn hello() -> i32 { 1 }\n"
        );
    }

    #[tokio::test]
    async fn repl_loop_includes_history_in_prompt_when_present() {
        let mut fixture = Fixture::new(&[], vec![MockResponse::text("OK")]);
        let history =
            crate::history::HistorySnapshot::new(vec![crate::history::HistoryEntry::Summary {
                id: Some(1),
                text: "Prior turn summary".to_string(),
            }]);
        let mut ctx = fixture.context();
        ctx.set_history(Some(&history));

        let outcome = run_repl_loop(&mut ctx, &ReplLoopConfig::default(), None)
            .await
            .expect("runs loop");
        assert!(outcome.is_success());

        let requests = fixture.provider.requests();
        assert_eq!(requests.len(), 1);
        let messages = &requests[0].messages;
        assert!(!messages[0].content.contains("<history>"));
        assert!(!messages[0].content.contains("Prior turn summary"));
        assert!(messages
            .iter()
            .any(|message| message.content.contains("Prior turn summary")));
    }

    #[test]
    fn repl_loop_config_from_app_config_inherits_max_retries() {
        let app_config: crate::config::AppConfig =
            toml::from_str("[repl]\nmax_turn_rounds = 25\n").expect("parse app config");
        let repl_config = ReplLoopConfig::from_app_config(&app_config);
        assert_eq!(repl_config.turn.max_turn_rounds, 25);
    }

    #[test]
    fn repl_capabilities_dynamic_with_focus() {
        let base = repl_capabilities(None);
        assert!(base.contains(LlmCapability::PlanCreation));
        assert!(!base.contains(LlmCapability::PlanUpdate));
        assert!(!base.contains(LlmCapability::ReviewUpdate));
        assert!(base.contains(LlmCapability::Blocked));
        assert!(base.contains(LlmCapability::Abort));

        let plan_focus = crate::discussion::DiscussionTarget::Plan { id: "p1".into() };
        let with_plan = repl_capabilities(Some(&plan_focus));
        assert!(with_plan.contains(LlmCapability::PlanCreation));
        assert!(with_plan.contains(LlmCapability::PlanUpdate));
        assert!(!with_plan.contains(LlmCapability::ReviewUpdate));

        let rev_focus = crate::discussion::DiscussionTarget::Review { id: "r1".into() };
        let with_rev = repl_capabilities(Some(&rev_focus));
        assert!(with_rev.contains(LlmCapability::PlanCreation));
        assert!(!with_rev.contains(LlmCapability::PlanUpdate));
        assert!(with_rev.contains(LlmCapability::ReviewUpdate));
    }

    #[test]
    fn repl_capability_expr_branches_are_mutually_exclusive() {
        let expr = repl_capability_expr(None);
        let leaves = expr.leaves();
        assert!(leaves.contains(&LlmCapability::TextMessage));
        assert!(leaves.contains(&LlmCapability::EditReplace));
        assert!(leaves.contains(&LlmCapability::PlanCreation));
        assert!(leaves.contains(&LlmCapability::Blocked));
        assert!(leaves.contains(&LlmCapability::Abort));
        assert!(!leaves.contains(&LlmCapability::PlanUpdate));

        let CapabilityTree::Set(label, items) = &expr else {
            panic!("expected Set at root");
        };
        assert!(label.is_empty(), "the root conjunction carries no label");
        assert_eq!(items.len(), 2);
        let CapabilityTree::AnyOf(action, branches) = &items[1] else {
            panic!("expected AnyOf for action branches");
        };
        assert_eq!(action, "action");
        let kinds: Vec<&str> = branches
            .iter()
            .filter_map(|branch| branch.label())
            .collect();
        assert_eq!(
            kinds,
            vec!["research", "edit", "plan", "answer", "blocked", "abort"]
        );
        assert_eq!(
            items[0].leaves(),
            vec![LlmCapability::Scratchpad, LlmCapability::TextMessage]
        );
        assert!(branches[3].leaves().is_empty());
        assert_eq!(branches[4].leaves(), vec![LlmCapability::Blocked]);
        assert_eq!(branches[5].leaves(), vec![LlmCapability::Abort]);
    }

    #[test]
    fn a_plan_focused_dialogue_wraps_the_plan_capabilities_in_one_union() {
        let focused = repl_capability_expr(Some(&crate::discussion::DiscussionTarget::Plan {
            id: "p1".into(),
        }));
        let CapabilityTree::Set(_, items) = &focused else {
            panic!("expected Set at root");
        };
        let CapabilityTree::AnyOf(label, branches) = &items[1] else {
            panic!("expected AnyOf for action branches");
        };
        assert_eq!(label, "action");
        let plan = branches
            .iter()
            .find(|branch| branch.label() == Some("plan"))
            .expect("the plan branch is declared");
        let CapabilityTree::Set(_, children) = plan else {
            panic!("expected Set for the plan branch");
        };
        let union = children
            .iter()
            .find_map(|child| match child {
                CapabilityTree::AnyOf(label, variants) if label == "plan_action" => Some(variants),
                _ => None,
            })
            .expect("the plan branch wraps its payloads in one union");
        let kinds: Vec<&str> = union.iter().filter_map(|branch| branch.label()).collect();
        assert_eq!(kinds, vec!["create", "modify"]);

        let unfocused = repl_capability_expr(None);
        assert!(unfocused.contains(LlmCapability::PlanCreation));
        assert!(!unfocused.contains(LlmCapability::PlanUpdate));
    }

    #[test]
    fn every_branch_that_carries_the_text_channel_survives_narrowing() {
        let narrowed = repl_capability_expr(None)
            .filter(&|capability| capability == LlmCapability::TextMessage)
            .expect("the narrowing keeps the ambient channel and the answer marker");
        let CapabilityTree::Set(_, items) = &narrowed else {
            panic!("expected a conjunction at the root: {narrowed:?}")
        };
        let (ambient, branches) = match items.as_slice() {
            [ambient, CapabilityTree::AnyOf(_, branches)] => (ambient, branches),
            other => {
                panic!("expected the ambient channel beside the branch envelope: {other:?}")
            }
        };
        assert_eq!(
            ambient.leaves(),
            vec![LlmCapability::TextMessage],
            "the ambient message channel survives the narrowing"
        );
        let kinds: Vec<&str> = branches
            .iter()
            .filter_map(|branch| branch.label())
            .collect();
        assert_eq!(kinds, vec!["answer"]);
        for branch in branches {
            assert!(
                branch.leaves().is_empty(),
                "the answer marker owns no field of its own"
            );
        }
    }

    #[tokio::test]
    async fn repl_loop_assembles_the_dialogue_contour() {
        let mut fixture = Fixture::new(&[], vec![MockResponse::text("OK")]);
        let mut ctx = fixture.context();

        let outcome = run_repl_loop(&mut ctx, &ReplLoopConfig::default(), None)
            .await
            .expect("runs loop");
        assert!(outcome.is_success());

        let requests = fixture.provider.requests();
        assert_eq!(requests.len(), 1);
        let system = &requests[0].messages[0].content;
        assert!(system.contains("proactive engineering assistant"));
        assert!(!system.contains("Tauqe Coder"));
        assert!(system.contains("Action Authorization"));
        assert!(!system.contains("Step Execution Scope"));
    }

    /// A plan payload the storage barrier refuses: it declares no concept tree.
    fn defective_plan_payload() -> String {
        concat!(
            "{\"message\":\"Draft the plan\",\"action\":{\"kind\":\"plan\",",
            "\"plan_create\":{\"action\":\"save\",\"id\":\"corrected-plan\",",
            "\"title\":\"Corrected Plan\",\"concept\":[],\"items\":[]},",
            "\"commit_message\":\"create corrected plan\",",
            "\"reason\":\"\"}}"
        )
        .to_string()
    }

    /// The same mutation once its payload satisfies the planning contract.
    fn corrected_plan_payload() -> String {
        concat!(
            "{\"message\":\"Corrected\",\"action\":{\"kind\":\"plan\",",
            "\"plan_create\":{\"action\":\"save\",\"id\":\"corrected-plan\",",
            "\"title\":\"Corrected Plan\",\"concept\":[{\"id\":\"c-1\",",
            "\"title\":\"Invariant\"}],\"items\":[{\"id\":\"1\",",
            "\"title\":\"Step 1\",\"files\":[\"src/step1.rs\"]}]},",
            "\"commit_message\":\"create corrected plan\",",
            "\"reason\":\"\"}}"
        )
        .to_string()
    }

    /// Collects the corrective directives the dialogue delivered to the model.
    fn plan_correction_directives(provider: &MockLlmProvider) -> Vec<String> {
        provider
            .requests()
            .iter()
            .flat_map(|request| request.messages.iter())
            .filter(|message| message.content.contains("[Plan Correction "))
            .map(|message| message.content.clone())
            .collect()
    }

    #[tokio::test]
    async fn repl_loop_asks_for_a_corrected_plan_payload_within_the_budget() {
        let mut fixture = Fixture::new(
            &[],
            vec![
                MockResponse::text(defective_plan_payload()),
                MockResponse::text(corrected_plan_payload()),
            ],
        );
        let mut ctx = fixture.context();
        let config = ReplLoopConfig::default();

        let outcome = run_repl_loop(&mut ctx, &config, None)
            .await
            .expect("runs loop");
        assert!(outcome.is_success());
        let success = outcome.into_success().expect("success");
        assert!(
            success.value.plans_applied,
            "the corrected payload reaches plan storage"
        );
        assert!(success.value.plan_rejections.is_empty());

        let directives = plan_correction_directives(&fixture.provider);
        assert_eq!(
            directives.len(),
            1,
            "one corrective round within the budget"
        );
        assert!(directives[0].contains("[Plan Correction 1]"));
        assert!(directives[0].contains("concept tree"));
    }

    #[tokio::test]
    async fn repl_loop_reports_a_plan_payload_that_stayed_defective() {
        let mut fixture = Fixture::new(
            &[],
            vec![
                MockResponse::text(defective_plan_payload()),
                MockResponse::text(defective_plan_payload()),
            ],
        );
        let mut ctx = fixture.context();
        let config = ReplLoopConfig {
            turn: TurnLoopConfig {
                max_turn_rounds: 2,
                ..TurnLoopConfig::default()
            },
        };

        let outcome = run_repl_loop(&mut ctx, &config, None)
            .await
            .expect("runs loop");
        assert!(outcome.is_success());
        let success = outcome.into_success().expect("success");
        assert!(!success.value.plans_applied);
        assert_eq!(success.value.plan_rejections.len(), 1);
        assert_eq!(
            success.value.plan_rejections[0].kind,
            crate::plan::PlanRejectionKind::PayloadDefect
        );
        assert_eq!(
            plan_correction_directives(&fixture.provider).len(),
            1,
            "the correction stops when turn round budget was exhausted"
        );
    }
}
