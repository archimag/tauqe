//! Interactive REPL control loop.
//!
//! [`run_repl_loop`] orchestrates interactive developer dialogue, research and
//! discovery context enrichment via [`run_research_loop`], and code modification
//! with toolchain verification via [`run_edit_loop`].

use tauqe_protocol::{CapabilitySet, LlmCapability, LlmCommand, TurnPhase};

use super::context::LoopContext;
use super::edit::{run_edit_loop, EditLoopConfig, EDIT_CAPABILITIES};
use super::outcome::{LoopResult, LoopSuccess, ResponseEnvelope, SystemError};
use super::research::{run_research_loop, ResearchLoopConfig, RESEARCH_CAPABILITIES};
use crate::providers::StreamEvent;

/// Configuration for the interactive REPL loop.
#[derive(Debug, Clone, Default)]
pub struct ReplLoopConfig {
    /// Configuration for research and context expansion rounds.
    pub research: ResearchLoopConfig,
    /// Configuration for in-memory staging and toolchain verification.
    pub edit: EditLoopConfig,
}

impl ReplLoopConfig {
    /// Creates REPL loop configuration from authoritative project configuration.
    pub fn from_app_config(config: &crate::config::AppConfig) -> Self {
        Self {
            research: ResearchLoopConfig::from_context(&config.context),
            edit: EditLoopConfig::from_repl_config(&config.repl),
        }
    }
}

/// Capabilities managed directly by the interactive REPL dialogue loop.
pub const REPL_OWN_CAPABILITIES: &[LlmCapability] = &[
    LlmCapability::TextMessage,
    LlmCapability::PlanCreation,
    LlmCapability::Outcome,
    LlmCapability::UserLanguage,
];

/// Assembles the complete capability set supported by the interactive REPL loop
/// from research, edits, conversational dialogue, plan creation, and optional
/// focused capabilities (plan modification or review decisions).
pub fn repl_capabilities(focus: Option<&tauqe_protocol::DiscussionTarget>) -> CapabilitySet {
    let mut caps = CapabilitySet::from_slice(RESEARCH_CAPABILITIES);
    for cap in EDIT_CAPABILITIES {
        caps.insert(*cap);
    }
    for cap in REPL_OWN_CAPABILITIES {
        caps.insert(*cap);
    }
    match focus {
        Some(tauqe_protocol::DiscussionTarget::Plan { .. }) => {
            caps.insert(LlmCapability::PlanUpdate);
        }
        Some(tauqe_protocol::DiscussionTarget::Review { .. })
        | Some(tauqe_protocol::DiscussionTarget::ReviewGroup { .. }) => {
            caps.insert(LlmCapability::ReviewUpdate);
        }
        None => {}
    }
    caps
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
    // 1. Initial request to the model
    ctx.events.emit(StreamEvent::TurnPhase {
        phase: TurnPhase::Proposal,
        round: Some(1),
        max_rounds: None,
        detail: Some("Awaiting initial response".into()),
    });

    let initial_envelope = ctx.request_round().await?;
    // 2. Run research loop to handle discovery, doc requests, and custom tools
    let research_result = run_research_loop(&mut ctx, initial_envelope, &config.research).await?;

    let mut envelope = match research_result {
        LoopResult::Success(success) => success.response,
        LoopResult::Diverged(env) => return Ok(LoopResult::Diverged(env)),
        LoopResult::Exhausted(report) => return Ok(LoopResult::Exhausted(report)),
    };

    // 3. Delegate to run_edit_loop if change set is present
    let mut modified_paths = Vec::new();
    let mut commit_hash = None;
    if envelope.has(LlmCapability::FileEdits) {
        let edit_result = run_edit_loop(&mut ctx, Some(envelope), &config.edit).await?;
        match edit_result {
            LoopResult::Success(edit_success) => {
                modified_paths = edit_success.value.modified_paths;
                commit_hash = edit_success.value.commit_hash;
                envelope = edit_success.response;
            }
            LoopResult::Diverged(env) => return Ok(LoopResult::Diverged(env)),
            LoopResult::Exhausted(report) => return Ok(LoopResult::Exhausted(report)),
        }
    }

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
    let _ = envelope.take(LlmCapability::Outcome);
    // 5. Anti-leak invariant (§13.5): log any unhandled capabilities
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
            modified_paths,
            plans_applied,
            plan_rejections,
            plan_warnings,
            review_decisions_applied,
            envelope: envelope.clone(),
            commit_hash,
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
    let text_cmds = envelope.take(LlmCapability::TextMessage);
    let mut combined = String::new();
    for cmd in text_cmds {
        if let LlmCommand::Text { content } = cmd {
            if !combined.is_empty() {
                combined.push('\n');
            }
            combined.push_str(&content);
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
    use crate::prompt::xml::XmlPromptProtocol;
    use crate::providers::mock::{MockLlmProvider, MockResponse};
    use crate::workflow::research::journal::TurnResearchJournal;
    use tauqe_protocol::{CapabilitySet, ContextAccess, ModelRef};

    struct Fixture {
        dir: tempfile::TempDir,
        context: ContextManager,
        journal: TurnResearchJournal,
        provider: MockLlmProvider,
        model: ModelRef,
        protocol: XmlPromptProtocol,
        cache: crate::workflow::fsm::cache::TurnInputCache,
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
                protocol: XmlPromptProtocol,
                cache: crate::workflow::fsm::cache::TurnInputCache::default(),
                cancel: CancellationToken::new(),
                events: EventSink::noop(),
                policy: crate::workflow::git_policy::GitTransactionPolicy::new(),
            }
        }

        fn context(&mut self) -> LoopContext<'_> {
            let caps = CapabilitySet::from_slice(&[
                LlmCapability::TextMessage,
                LlmCapability::FileEdits,
                LlmCapability::Research,
            ]);
            let prompt_builder = super::super::prompt::PromptBuilder::new(
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
            vec![MockResponse::text(concat!(
                "<edits summary=\"Update hello\">\n",
                "  <edit path=\"hello.rs\">\n",
                "    <search>\n",
                "fn hello() {}\n",
                "    </search>\n",
                "    <replace>\n",
                "fn hello() -> i32 { 1 }\n",
                "    </replace>\n",
                "  </edit>\n",
                "</edits>\n",
            ))],
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
            vec![MockResponse::text(concat!(
                "<edits summary=\"Update hello\">\n",
                "  <edit path=\"hello.rs\">\n",
                "    <search>\n",
                "fn hello() {}\n",
                "    </search>\n",
                "    <replace>\n",
                "fn hello() -> i32 { 1 }\n",
                "    </replace>\n",
                "  </edit>\n",
                "</edits>\n",
                "<success>feat(hello): return a value</success>\n",
            ))],
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
            vec![MockResponse::text(concat!(
                "<edits summary=\"Update hello\">\n",
                "  <edit path=\"hello.rs\">\n",
                "    <search>\n",
                "fn hello() {}\n",
                "    </search>\n",
                "    <replace>\n",
                "fn hello() -> i32 { 1 }\n",
                "    </replace>\n",
                "  </edit>\n",
                "</edits>\n",
            ))],
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
        assert_eq!(repl_config.research.max_rounds, 8);
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
        assert_eq!(repl_config.edit.max_healing_rounds, 7);
        assert_eq!(repl_config.edit.staging.max_retries, 7);
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
        let plan_xml = r#"
<plan action="save" id="test-repl-plan" title="Test REPL Plan">
  <concept>
    <concept_item id="c-1" title="Concept 1" />
  </concept>
  <item id="1" title="Step 1" status="todo" files="src/step1.rs" />
</plan>
"#;
        let mut fixture = Fixture::new(&[], vec![MockResponse::text(plan_xml)]);
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
        assert!(!base.contains(LlmCapability::Blocked));
        assert!(!base.contains(LlmCapability::Abort));

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
}
