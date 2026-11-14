//! Interactive REPL control loop.
//!
//! [`run_repl_loop`] orchestrates interactive developer dialogue, research and
//! discovery context enrichment via [`run_research_loop`], and code modification
//! with toolchain verification via [`run_edit_loop`].

use tauqe_protocol::{CapabilityBranch, CapabilityExpr, LlmCapability, LlmCommand};

use super::context::LoopContext;
use super::outcome::{LoopResult, LoopSuccess, ResponseEnvelope, SystemError};
use super::turn::{run_turn_loop, TurnLoopConfig};
use crate::prompt::TurnBrief;
/// Configuration for the interactive REPL loop.
#[derive(Debug, Clone, Default)]
pub struct ReplLoopConfig {
    /// Configuration for the underlying bounded turn loop.
    pub turn: TurnLoopConfig,
}

impl ReplLoopConfig {
    /// Creates REPL loop configuration from authoritative project configuration.
    pub fn from_app_config(config: &crate::config::AppConfig) -> Self {
        Self {
            turn: TurnLoopConfig::from_app_config(config),
        }
    }
}

/// Capabilities managed directly by the interactive REPL dialogue loop.
pub const REPL_OWN_CAPABILITIES: &[LlmCapability] = &[
    LlmCapability::TextMessage,
    LlmCapability::PlanCreation,
    LlmCapability::CommitMessage,
    LlmCapability::TurnError,
    LlmCapability::TurnSkip,
];
/// Builds the algebraic capability expression for the interactive REPL loop.
///
/// The dialogue contours share one head: a free dialogue composes it as is, a
/// plan-anchored dialogue adds plan mutation, and a review-anchored dialogue
/// adds the recorded review decision. The tree is composed here, so the
/// capability surface of a dialogue is declared once beside the loop that
/// drives it.
pub fn repl_capability_expr(focus: Option<&tauqe_protocol::DiscussionTarget>) -> CapabilityExpr {
    let (plan_update, review_decision) = match focus {
        Some(tauqe_protocol::DiscussionTarget::Plan { .. }) => (true, false),
        Some(tauqe_protocol::DiscussionTarget::Review { .. })
        | Some(tauqe_protocol::DiscussionTarget::ReviewGroup { .. }) => (false, true),
        None => (false, false),
    };
    dialogue_capability_expr(plan_update, review_decision)
}

/// Composes the capability surface every dialogue contour advertises.
///
/// The head is shared by construction, which is what keeps the provider prefix
/// cache warm across focus switching. A focused regime declares the capability
/// its focus authorizes beside that head: plan mutation joins the plan branch,
/// and a recorded review decision opens a review branch of the same union, so a
/// decision never widens the head its sibling contours render.
pub(crate) fn dialogue_capability_expr(plan_update: bool, review_decision: bool) -> CapabilityExpr {
    // The root of a dialogue carries only the working-memory channel: the
    // conversational channel belongs to the branch a round selects, so one
    // composed wire never declares two `message` properties.
    let ambient = CapabilityExpr::set([CapabilityExpr::leaf(LlmCapability::Scratchpad)]);
    let research = CapabilityBranch::new(
        "research",
        CapabilityExpr::set([
            CapabilityExpr::leaf(LlmCapability::ShortDescription),
            CapabilityExpr::leaf(LlmCapability::ContextRequest),
            CapabilityExpr::leaf(LlmCapability::RepoSearch),
            CapabilityExpr::leaf(LlmCapability::RepoDirs),
            CapabilityExpr::leaf(LlmCapability::RepoMap),
            CapabilityExpr::leaf(LlmCapability::DirList),
            CapabilityExpr::leaf(LlmCapability::DocRequest),
            CapabilityExpr::leaf(LlmCapability::OuterDocRequest),
            CapabilityExpr::leaf(LlmCapability::CustomTools),
        ]),
    );
    // A discovery round reports through the short description of the action it
    // performs, while a round that acts on the working tree explains the change
    // it proposes.
    let edit = CapabilityBranch::new(
        "edit",
        CapabilityExpr::set([
            CapabilityExpr::leaf(LlmCapability::TextMessage),
            CapabilityExpr::leaf(LlmCapability::EditReplace),
            CapabilityExpr::leaf(LlmCapability::EditCreate),
            CapabilityExpr::leaf(LlmCapability::EditDelete),
            CapabilityExpr::leaf(LlmCapability::EditMove),
            CapabilityExpr::leaf(LlmCapability::EditMoveDir),
            CapabilityExpr::leaf(LlmCapability::EditOverwrite),
            CapabilityExpr::leaf(LlmCapability::ToolchainVerify),
            CapabilityExpr::leaf(LlmCapability::CommitMessage),
            CapabilityExpr::leaf(LlmCapability::TurnError),
            CapabilityExpr::leaf(LlmCapability::TurnSkip),
        ]),
    );
    // A dialogue anchored on a plan advertises both plan capabilities, so the
    // branch wraps them in one discriminated union: their payloads never share a
    // level and never become simultaneously required. A free dialogue
    // advertises plan authorship alone and carries the leaf directly.
    let plan_capabilities = if plan_update {
        CapabilityExpr::any_of(
            "plan_action",
            [
                CapabilityBranch::new("create", CapabilityExpr::leaf(LlmCapability::PlanCreation)),
                CapabilityBranch::new("modify", CapabilityExpr::leaf(LlmCapability::PlanUpdate)),
            ],
        )
    } else {
        CapabilityExpr::leaf(LlmCapability::PlanCreation)
    };
    let plan = CapabilityBranch::new(
        "plan",
        CapabilityExpr::set([
            CapabilityExpr::leaf(LlmCapability::ShortDescription),
            plan_capabilities,
            CapabilityExpr::leaf(LlmCapability::CommitMessage),
            CapabilityExpr::leaf(LlmCapability::TurnError),
            CapabilityExpr::leaf(LlmCapability::TurnSkip),
        ]),
    );
    // The `answer` branch carries the conversational channel and nothing else,
    // so a purely conversational round names the branch its message belongs to
    // instead of smuggling itself through `plan` with an idle payload.
    let answer = CapabilityBranch::new(
        "answer",
        CapabilityExpr::set([CapabilityExpr::leaf(LlmCapability::TextMessage)]),
    );
    // A terminal signal owns its branch instead of riding beside a change set
    // or a plan payload: one response states exactly one of them.
    let blocked = CapabilityBranch::new(
        "blocked",
        CapabilityExpr::set([CapabilityExpr::leaf(LlmCapability::Blocked)]),
    );
    let abort = CapabilityBranch::new(
        "abort",
        CapabilityExpr::set([CapabilityExpr::leaf(LlmCapability::Abort)]),
    );
    let mut branches = vec![research, edit, plan, answer, blocked, abort];
    if review_decision {
        branches.push(CapabilityBranch::new(
            "review",
            CapabilityExpr::set([
                CapabilityExpr::leaf(LlmCapability::TextMessage),
                CapabilityExpr::leaf(LlmCapability::ReviewUpdate),
                CapabilityExpr::leaf(LlmCapability::CommitMessage),
                CapabilityExpr::leaf(LlmCapability::TurnError),
                CapabilityExpr::leaf(LlmCapability::TurnSkip),
            ]),
        ));
    }
    CapabilityExpr::set([ambient, CapabilityExpr::any_of("action", branches)])
}

/// Appends one leaf to a capability tree without disturbing its branches.
///
/// A conjunction gains the leaf beside its existing children, so a contour that
/// declared a branch structure keeps it; any other tree is wrapped in a
/// conjunction with the leaf.
pub(crate) fn append_leaf(expr: CapabilityExpr, capability: LlmCapability) -> CapabilityExpr {
    match expr {
        CapabilityExpr::Set(mut items) => {
            items.push(CapabilityExpr::leaf(capability));
            CapabilityExpr::Set(items)
        }
        other => CapabilityExpr::set([other, CapabilityExpr::leaf(capability)]),
    }
}

/// Returns the capability tree supported by the interactive REPL loop.
pub fn repl_capabilities(focus: Option<&tauqe_protocol::DiscussionTarget>) -> CapabilityExpr {
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
    pub plan_rejections: Vec<tauqe_protocol::PlanRejection>,
    /// Status adjustments or warnings emitted during plan mutations.
    pub plan_warnings: Vec<String>,
    /// IDs of review findings updated by review decisions during this turn.
    pub review_decisions_applied: Vec<u32>,
    pub envelope: ResponseEnvelope,
    /// Git commit hash if changes were committed and squashed.
    pub commit_hash: Option<String>,
}

/// Runs the interactive REPL turn lifecycle.
pub async fn run_repl_loop(
    ctx: &mut LoopContext<'_>,
    config: &ReplLoopConfig,
    focus: Option<&tauqe_protocol::DiscussionTarget>,
) -> Result<LoopResult<ReplArtifact>, SystemError> {
    ctx.check_cancelled()?;

    let repl_caps = repl_capabilities(focus);
    let mut ctx = ctx.reborrow_with_capabilities(repl_caps);
    // The dialogue owns its introductory brief: the loop that drives the turn
    // declares the contour and the text it opens with.
    ctx.set_turn_brief(Some(TurnBrief::dialogue()));
    // 1. Run the turn loop (prompt -> research -> edit/verify/healing)
    let turn_result = run_turn_loop(&mut ctx, None, &config.turn).await?;

    let (mut envelope, turn_artifact) = match turn_result {
        LoopResult::Success(success) => (success.response, success.value),
        LoopResult::Diverged(env) => return Ok(LoopResult::Diverged(env)),
        LoopResult::Exhausted(report) => return Ok(LoopResult::Exhausted(report)),
    };
    // 4. Drain remaining REPL capabilities (TextMessage, PlanCreation, PlanUpdate, ReviewUpdate)
    let message = extract_text_message(&mut envelope);

    let (plans_applied, plan_rejections, plan_warnings) =
        apply_plan_commands(&ctx, &mut envelope, focus).await;
    let review_decisions_applied = apply_review_commands(&ctx, &mut envelope, focus).await;
    // The edit phase already recorded the terminal success it consumed into
    // the commit summary, so the command leaves the envelope here whether or
    // not the change set reached the disk. Draining it explicitly keeps the
    // anti-leak invariant meaningful: a capability REPL owns must never
    // survive the turn.
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

async fn apply_plan_commands(
    ctx: &LoopContext<'_>,
    envelope: &mut ResponseEnvelope,
    focus: Option<&tauqe_protocol::DiscussionTarget>,
) -> (bool, Vec<tauqe_protocol::PlanRejection>, Vec<String>) {
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

    let mode = match focus {
        Some(tauqe_protocol::DiscussionTarget::Plan { .. }) => {
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

async fn apply_review_commands(
    ctx: &LoopContext<'_>,
    envelope: &mut ResponseEnvelope,
    focus: Option<&tauqe_protocol::DiscussionTarget>,
) -> Vec<u32> {
    let review_cmds = envelope.take(LlmCapability::ReviewUpdate);
    if review_cmds.is_empty() {
        return Vec::new();
    }

    let review_id = match focus {
        Some(tauqe_protocol::DiscussionTarget::Review { id }) => id.clone(),
        Some(tauqe_protocol::DiscussionTarget::ReviewGroup { session_id, .. }) => {
            session_id.clone()
        }
        _ => {
            tracing::warn!(
                "Review update commands were emitted without an active review focus; ignoring"
            );
            return Vec::new();
        }
    };

    let repo_root = ctx.repo_root.to_path_buf();
    let mut updated_items = Vec::new();

    for cmd in review_cmds {
        if let LlmCommand::ReviewUpdate { update } = cmd {
            let item_id = update.item_id;
            let root_clone = repo_root.clone();
            let rev_id_clone = review_id.clone();
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
    let mut text_cmds = envelope.take(LlmCapability::TextMessage);
    text_cmds.extend(envelope.take(LlmCapability::ShortDescription));
    let mut combined = String::new();
    for cmd in text_cmds {
        match cmd {
            LlmCommand::Text(command) => {
                if !combined.is_empty() {
                    combined.push('\n');
                }
                combined.push_str(&command.content);
            }
            LlmCommand::ShortDescription(command) => {
                if !combined.is_empty() {
                    combined.push('\n');
                }
                combined.push_str(&command.content);
            }
            _ => {}
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
    use crate::context::ContextManager;
    use crate::control::context::{CancellationToken, EventSink};
    use crate::control::turn::{EDIT_CAPABILITIES, RESEARCH_CAPABILITIES};
    use crate::prompt::json::JsonPromptProtocol;
    use crate::providers::mock::{MockLlmProvider, MockResponse};
    use crate::workflow::research::journal::TurnResearchJournal;
    use tauqe_protocol::{ContextAccess, ModelRef};

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
                protocol: JsonPromptProtocol::edits(),
                cache: crate::control::cache::TurnInputCache::default(),
                cancel: CancellationToken::new(),
                events: EventSink::noop(),
                policy: crate::workflow::git_policy::GitTransactionPolicy::new(),
            }
        }

        fn context(&mut self) -> LoopContext<'_> {
            let caps = CapabilityExpr::set(
                EDIT_CAPABILITIES
                    .iter()
                    .copied()
                    .chain(RESEARCH_CAPABILITIES.iter().copied())
                    .chain([LlmCapability::TextMessage])
                    .map(CapabilityExpr::leaf),
            );
            let prompt_builder = crate::control::prompt::PromptBuilder::new(
                &self.provider,
                &self.model,
                &self.protocol,
                &mut self.cache,
            );
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
        let mut fixture = Fixture::new(&[], vec![MockResponse::text("Привет, чем могу помочь?")]);
        let mut ctx = fixture.context();

        let outcome = run_repl_loop(&mut ctx, &ReplLoopConfig::default(), None)
            .await
            .expect("runs loop");
        assert!(outcome.is_success());
        let success = outcome.into_success().expect("success");
        assert_eq!(
            success.value.message.as_deref(),
            Some("Привет, чем могу помочь?")
        );
        assert!(success.value.modified_paths.is_empty());
    }

    #[tokio::test]
    async fn repl_loop_executes_edits_when_changes_present() {
        let mut fixture = Fixture::new(
            &[("hello.rs", "fn hello() {}\n")],
            vec![MockResponse::text(
                serde_json::json!({
                    "message": "Update hello",
                    "action": {
                        "kind": "edit",
                        "changes": [{
                            "op": "replace",
                            "path": "hello.rs",
                            "to": "",
                            "start": "",
                            "end": "",
                            "content": "",
                            "old_text": "fn hello() {}\n",
                            "new_text": "fn hello() -> i32 { 1 }\n"
                        }],
                        "commit_message": "Update hello",
                        "message": "",
                        "reason": ""
                    }
                })
                .to_string(),
            )],
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
    async fn repl_loop_commits_with_the_message_the_round_declared() {
        let mut fixture = Fixture::new(
            &[("hello.rs", "fn hello() {}\n")],
            vec![MockResponse::text(
                serde_json::json!({
                    "message": "Update hello",
                    "action": {
                        "kind": "edit",
                        "changes": [{
                            "op": "replace",
                            "path": "hello.rs",
                            "to": "",
                            "start": "",
                            "end": "",
                            "content": "",
                            "old_text": "fn hello() {}\n",
                            "new_text": "fn hello() -> i32 { 1 }\n"
                        }],
                        "commit_message": "feat(hello): return a value",
                        "message": "",
                        "reason": ""
                    }
                })
                .to_string(),
            )],
        );
        let mut ctx = fixture.context();

        let outcome = run_repl_loop(&mut ctx, &ReplLoopConfig::default(), None)
            .await
            .expect("runs loop");
        assert!(outcome.is_success());
        let success = outcome.into_success().expect("success");
        assert!(success.value.commit_hash.is_some());

        let subject =
            crate::git::cmd::run_git(Some(fixture.dir.path()), &["log", "-1", "--format=%s"])
                .expect("read head subject")
                .trim()
                .to_string();
        assert_eq!(
            subject, "feat(hello): return a value",
            "the commit records the message the model declared"
        );
    }

    #[tokio::test]
    async fn repl_loop_falls_back_to_default_summary_without_a_declared_message() {
        let mut fixture = Fixture::new(
            &[("hello.rs", "fn hello() {}\n")],
            vec![MockResponse::text(
                serde_json::json!({
                    "message": "Update hello",
                    "action": {
                        "kind": "edit",
                        "changes": [{
                            "op": "replace",
                            "path": "hello.rs",
                            "to": "",
                            "start": "",
                            "end": "",
                            "content": "",
                            "old_text": "fn hello() {}\n",
                            "new_text": "fn hello() -> i32 { 1 }\n"
                        }],
                        "commit_message": "REPL changes",
                        "message": "",
                        "reason": ""
                    }
                })
                .to_string(),
            )],
        );
        let mut ctx = fixture.context();

        let outcome = run_repl_loop(&mut ctx, &ReplLoopConfig::default(), None)
            .await
            .expect("runs loop");
        assert!(outcome.is_success());
        let success = outcome.into_success().expect("success");
        assert!(success.value.commit_hash.is_some());

        let subject =
            crate::git::cmd::run_git(Some(fixture.dir.path()), &["log", "-1", "--format=%s"])
                .expect("read head subject")
                .trim()
                .to_string();
        assert_eq!(subject, "REPL changes");
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
        let prompt_text = &requests[0].messages[0].content;
        assert!(prompt_text.contains("## Session History"));
        assert!(prompt_text.contains("<history>"));
        assert!(prompt_text.contains("Prior turn summary"));
    }
    #[test]
    fn repl_loop_config_from_app_config_inherits_context_max_rounds() {
        let toml_str = r#"
[context]
max_research_rounds = 8
"#;
        let app_config: crate::config::AppConfig =
            toml::from_str(toml_str).expect("parse app config");
        let repl_config = ReplLoopConfig::from_app_config(&app_config);
        assert_eq!(repl_config.turn.research.max_rounds, 8);
    }

    #[test]
    fn repl_loop_config_from_app_config_inherits_max_retries() {
        let toml_str = r#"
[repl]
max_retries = 7
"#;
        let app_config: crate::config::AppConfig =
            toml::from_str(toml_str).expect("parse app config");
        let repl_config = ReplLoopConfig::from_app_config(&app_config);
        assert_eq!(repl_config.turn.edit.max_healing_rounds, 7);
        assert_eq!(repl_config.turn.edit.staging.max_retries, 7);
    }

    #[tokio::test]
    async fn repl_loop_includes_repomap_when_configured() {
        let mut fixture = Fixture::new_with_context(
            &[
                ("hello.rs", "pub fn hello() {}\n"),
                ("lib.rs", "pub fn lib_func() {}\n"),
            ],
            &["hello.rs"],
            vec![MockResponse::text("OK")],
        );
        let mut ctx = fixture.context();
        ctx.set_repo_map_config(true, 4000);

        let outcome = run_repl_loop(&mut ctx, &ReplLoopConfig::default(), None)
            .await
            .expect("runs loop");
        assert!(outcome.is_success());

        let requests = fixture.provider.requests();
        assert_eq!(requests.len(), 1);
        let prompt_text = &requests[0].messages[0].content;
        assert!(prompt_text.contains("## Repository Map"));
        assert!(prompt_text.contains("<repo_map>"));
        assert!(prompt_text.contains("lib.rs"));
    }

    #[tokio::test]
    async fn repl_loop_includes_repo_dirs_when_repomap_disabled() {
        let mut fixture = Fixture::new(
            &[("hello.rs", "pub fn hello() {}\n")],
            vec![MockResponse::text("OK")],
        );
        let mut ctx = fixture.context();
        ctx.set_repo_map_config(false, 4000);

        let outcome = run_repl_loop(&mut ctx, &ReplLoopConfig::default(), None)
            .await
            .expect("runs loop");
        assert!(outcome.is_success());

        let requests = fixture.provider.requests();
        assert_eq!(requests.len(), 1);
        let prompt_text = &requests[0].messages[0].content;
        assert!(!prompt_text.contains("## Repository Map"));
        assert!(prompt_text.contains("## Repository Directories"));
        assert!(prompt_text.contains("<repo_dirs>"));
        assert!(prompt_text.contains("hello.rs"));
    }

    #[tokio::test]
    async fn repl_loop_applies_plan_creation_in_repl_mode() {
        let plan_json = serde_json::json!({
            "message": "Test Plan",
            "action": {
                "kind": "plan",
                "plan_create": {
                    "action": "save",
                    "id": "test-repl-plan",
                    "title": "Test REPL Plan",
                    "description": "description",
                    "concept": [{
                        "id": "c-1",
                        "title": "Concept 1",
                        "details": null,
                        "children": []
                    }],
                    "items": [{
                        "id": "1",
                        "title": "Step 1",
                        "details": null,
                        "status": "todo",
                        "commit": null,
                        "files": ["src/step1.rs"],
                        "readonly_files": [],
                        "outer_docs": [],
                        "concept_refs": [],
                        "depends_on": [],
                        "children": []
                    }]
                },
                "commit_message": "create test plan",
                "message": "",
                "reason": ""
            }
        })
        .to_string();
        let mut fixture = Fixture::new(&[], vec![MockResponse::text(plan_json)]);
        let mut ctx = fixture.context();

        let outcome = run_repl_loop(&mut ctx, &ReplLoopConfig::default(), None)
            .await
            .expect("runs loop");
        assert!(outcome.is_success());
        let success = outcome.into_success().expect("success");
        assert!(success.value.plans_applied);
        assert!(success.value.plan_rejections.is_empty());

        let loaded =
            crate::plan::storage::PlanStorage::load_plan(fixture.dir.path(), "test-repl-plan")
                .expect("load plan")
                .expect("plan exists");
        assert_eq!(loaded.id, "test-repl-plan");
    }

    #[test]
    fn repl_capabilities_dynamic_with_focus() {
        let base = repl_capabilities(None);
        assert!(base.contains(LlmCapability::PlanCreation));
        assert!(!base.contains(LlmCapability::PlanUpdate));
        assert!(!base.contains(LlmCapability::ReviewUpdate));
        assert!(base.contains(LlmCapability::Blocked));
        assert!(base.contains(LlmCapability::Abort));

        let plan_focus = tauqe_protocol::DiscussionTarget::Plan { id: "p1".into() };
        let with_plan = repl_capabilities(Some(&plan_focus));
        assert!(with_plan.contains(LlmCapability::PlanCreation));
        assert!(with_plan.contains(LlmCapability::PlanUpdate));
        assert!(!with_plan.contains(LlmCapability::ReviewUpdate));

        let rev_focus = tauqe_protocol::DiscussionTarget::Review { id: "r1".into() };
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

        let CapabilityExpr::Set(items) = expr else {
            panic!("expected Set at root");
        };
        assert_eq!(items.len(), 2);
        let CapabilityExpr::AnyOf(label, branches) = &items[1] else {
            panic!("expected AnyOf for action branches");
        };
        assert_eq!(label, "action");
        let kinds: Vec<&str> = branches.iter().map(|branch| branch.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec!["research", "edit", "plan", "answer", "blocked", "abort"]
        );
        // The answer branch owns the conversational channel, so a purely
        // conversational round names the branch its message belongs to and the
        // branch stays operational on its own.
        assert_eq!(branches[3].expr.leaves(), vec![LlmCapability::TextMessage]);
        // The terminal signals own single-leaf branches.
        assert_eq!(branches[4].expr.leaves(), vec![LlmCapability::Blocked]);
        assert_eq!(branches[5].expr.leaves(), vec![LlmCapability::Abort]);
        let edit_leaves = branches[1].expr.leaves();
        let plan_leaves = branches[2].expr.leaves();
        assert!(edit_leaves.contains(&LlmCapability::EditReplace));
        assert!(!edit_leaves.contains(&LlmCapability::PlanCreation));
        assert!(!edit_leaves.contains(&LlmCapability::Blocked));
        assert!(!edit_leaves.contains(&LlmCapability::Abort));
        assert!(plan_leaves.contains(&LlmCapability::PlanCreation));
        assert!(!plan_leaves.contains(&LlmCapability::EditReplace));
        assert!(!plan_leaves.contains(&LlmCapability::Abort));
    }

    /// A dialogue anchored on a plan advertises both plan capabilities, so the
    /// plan branch wraps them in one discriminated union: their payloads never
    /// share a level and never become simultaneously required. A free dialogue
    /// advertises plan authorship alone and carries the leaf directly.
    #[test]
    fn a_plan_focused_dialogue_wraps_the_plan_capabilities_in_one_union() {
        let focused = repl_capability_expr(Some(&tauqe_protocol::DiscussionTarget::Plan {
            id: "p1".into(),
        }));
        let CapabilityExpr::Set(items) = &focused else {
            panic!("expected Set at root");
        };
        let CapabilityExpr::AnyOf(label, branches) = &items[1] else {
            panic!("expected AnyOf for action branches");
        };
        assert_eq!(label, "action");
        let plan = branches
            .iter()
            .find(|branch| branch.kind == "plan")
            .expect("the plan branch is declared");
        let CapabilityExpr::Set(children) = &plan.expr else {
            panic!("expected Set for the plan branch");
        };
        let union = children
            .iter()
            .find_map(|child| match child {
                CapabilityExpr::AnyOf(label, variants) if label == "plan_action" => Some(variants),
                _ => None,
            })
            .expect("the plan branch wraps its payloads in one union");
        let kinds: Vec<&str> = union.iter().map(|branch| branch.kind.as_str()).collect();
        assert_eq!(kinds, vec!["create", "modify"]);
        assert_eq!(union[0].expr.leaves(), vec![LlmCapability::PlanCreation]);
        assert_eq!(union[1].expr.leaves(), vec![LlmCapability::PlanUpdate]);

        // A free dialogue advertises plan authorship alone, so its branch keeps
        // the leaf instead of opening a union of a single variant.
        let unfocused = repl_capability_expr(None);
        assert!(unfocused.contains(LlmCapability::PlanCreation));
        assert!(!unfocused.contains(LlmCapability::PlanUpdate));
    }

    #[test]
    fn every_branch_that_carries_the_text_channel_survives_narrowing() {
        // The conversational channel is operational, so a surface narrowed to
        // the text capability keeps every branch that carries it — the answer
        // of a dialogue and the explanation of a change set — instead of
        // dropping them as exhausted foliage.
        let narrowed = repl_capability_expr(None)
            .filter(&|capability| capability == LlmCapability::TextMessage)
            .expect("the narrowing keeps the branches that carry the text channel");
        let branches = match &narrowed {
            CapabilityExpr::Set(items) => match items.as_slice() {
                [CapabilityExpr::AnyOf(_, branches)] => branches,
                other => panic!("expected one branch envelope, got {other:?}"),
            },
            other => panic!("expected a conjunction at the root, got {other:?}"),
        };
        let kinds: Vec<&str> = branches.iter().map(|branch| branch.kind.as_str()).collect();
        assert_eq!(kinds, vec!["edit", "answer"]);
        for branch in branches {
            assert_eq!(branch.expr.leaves(), vec![LlmCapability::TextMessage]);
        }
    }

    /// The loop owns the brief of its contour, so the assembled prompt opens
    /// with the dialogue introduction and states the dialogue rules instead of
    /// the scope of an isolated step.
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
}
