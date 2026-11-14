//! Autonomous bounded turn control loop and submodules.
//!
//! [`run_turn_loop`] orchestrates the prompt round, context research via
//! [`run_research_loop`], virtual staging, toolchain verification, and
//! verification healing via [`run_edit_loop`].

pub mod edit;
pub mod research;
pub mod staging;

use tauqe_protocol::{CapabilityExpr, LlmCapability, TurnPhase, VerificationVerdict};

use super::context::LoopContext;
use super::outcome::{LoopResult, LoopSuccess, ResponseEnvelope, SystemError};
use crate::providers::StreamEvent;

pub use edit::{run_edit_loop, EditArtifact, EditLoopConfig, EDIT_CAPABILITIES};
pub use research::{
    handle_research_commands, run_research_loop, ResearchArtifact, ResearchFeedback,
    ResearchLoopConfig, RESEARCH_CAPABILITIES,
};
pub use staging::{run_staging_loop, StagingArtifact, StagingLoopConfig};

/// Builds the algebraic capability expression for an autonomous bounded turn.
///
/// The surface is the one an isolated step owns: the turn executes an approved
/// design inside the codebase, so it carries the dialogue head minus plan
/// authoring, the internal documentation of the harness, toolchain
/// verification, and conversational text, and reports through the short
/// description of its change set instead.
pub fn turn_capability_expr() -> CapabilityExpr {
    let narrowed = super::repl::dialogue_capability_expr(false, false)
        .filter(&|capability| {
            !matches!(
                capability,
                LlmCapability::PlanCreation
                    | LlmCapability::DocRequest
                    | LlmCapability::ToolchainVerify
                    | LlmCapability::TextMessage
                    | LlmCapability::ShortDescription
            )
        })
        .unwrap_or_else(|| CapabilityExpr::set(std::iter::empty::<CapabilityExpr>()));
    // The short description is the conversational channel of a step, so it joins
    // the ambient leaves beside the ones the head already declares instead of
    // opening a branch of its own. The branch channels are narrowed above, so
    // the step still declares exactly one `message` field.
    super::repl::append_leaf(narrowed, LlmCapability::ShortDescription)
}

/// Configuration for the bounded turn loop.
#[derive(Debug, Clone, Default)]
pub struct TurnLoopConfig {
    /// Configuration for research and context expansion rounds.
    pub research: ResearchLoopConfig,
    /// Configuration for in-memory staging and toolchain verification.
    pub edit: EditLoopConfig,
}

impl TurnLoopConfig {
    /// Creates turn loop configuration from authoritative project configuration.
    pub fn from_app_config(config: &crate::config::AppConfig) -> Self {
        Self {
            research: ResearchLoopConfig::from_context(&config.context),
            edit: EditLoopConfig::from_app_config(config),
        }
    }
}

/// Artifact produced by a successful turn execution cycle.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnArtifact {
    /// Repository-relative paths touched by code edits during this turn.
    pub modified_paths: Vec<String>,
    /// Git commit hash if changes were committed and squashed.
    pub commit_hash: Option<String>,
    /// Diagnostic of a Git transaction that could not be finalized.
    pub commit_error: Option<String>,
    /// Final toolchain verification verdict if edits were performed.
    pub verification: Option<VerificationVerdict>,
    /// Number of research rounds executed.
    pub rounds_spent: usize,
    /// Remaining response envelope with companion capabilities.
    pub envelope: ResponseEnvelope,
}

/// Runs the bounded turn lifecycle (Prompt -> Research -> Edit/Verify/Heal -> Commit).
pub async fn run_turn_loop(
    ctx: &mut LoopContext<'_>,
    initial_envelope: Option<ResponseEnvelope>,
    config: &TurnLoopConfig,
) -> Result<LoopResult<TurnArtifact>, SystemError> {
    ctx.check_cancelled()?;

    let opening_envelope = match initial_envelope {
        Some(env) => env,
        None => {
            ctx.events.emit(StreamEvent::TurnPhase {
                phase: TurnPhase::Proposal,
                round: Some(1),
                max_rounds: None,
                detail: Some("Awaiting initial response".into()),
            });
            ctx.request_round().await?
        }
    };

    // 1. Run research loop to handle discovery, doc requests, and custom tools
    let research_result = run_research_loop(ctx, opening_envelope, &config.research).await?;

    let (mut envelope, rounds_spent) = match research_result {
        LoopResult::Success(success) => (success.response, success.value.rounds_spent),
        LoopResult::Diverged(env) => return Ok(LoopResult::Diverged(env)),
        LoopResult::Exhausted(report) => return Ok(LoopResult::Exhausted(report)),
    };

    // 2. Delegate to run_edit_loop if change set is present
    let mut modified_paths = Vec::new();
    let mut commit_hash = None;
    let mut commit_error = None;
    let mut verification = None;

    if EDIT_CAPABILITIES.iter().any(|cap| envelope.has(*cap)) {
        let edit_result = run_edit_loop(ctx, Some(envelope), &config.edit).await?;
        match edit_result {
            LoopResult::Success(edit_success) => {
                modified_paths = edit_success.value.modified_paths;
                commit_hash = edit_success.value.commit_hash;
                commit_error = edit_success.value.commit_error;
                verification = Some(edit_success.value.verification);
                envelope = edit_success.response;
            }
            LoopResult::Diverged(env) => return Ok(LoopResult::Diverged(env)),
            LoopResult::Exhausted(report) => return Ok(LoopResult::Exhausted(report)),
        }
    }

    Ok(LoopResult::Success(LoopSuccess::new(
        TurnArtifact {
            modified_paths,
            commit_hash,
            commit_error,
            verification,
            rounds_spent,
            envelope: envelope.clone(),
        },
        envelope,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ContextManager;
    use crate::control::context::{CancellationToken, EventSink};
    use crate::prompt::json::JsonPromptProtocol;
    use crate::providers::mock::{MockLlmProvider, MockResponse};
    use crate::workflow::research::journal::TurnResearchJournal;
    use tauqe_protocol::LlmCapability;
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
            for (path, _) in files {
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
    async fn turn_loop_returns_pure_text_response() {
        let mut fixture = Fixture::new(&[], vec![MockResponse::text("Turn text result")]);
        let mut ctx = fixture.context();

        let outcome = run_turn_loop(&mut ctx, None, &TurnLoopConfig::default())
            .await
            .expect("runs loop");
        assert!(outcome.is_success());
        let success = outcome.into_success().expect("success");
        assert!(success.value.modified_paths.is_empty());
        assert!(success.response.has(LlmCapability::TextMessage));
    }

    #[tokio::test]
    async fn turn_loop_executes_edits_and_commits() {
        let mut fixture = Fixture::new(
            &[("hello.rs", "fn hello() {}\n")],
            vec![MockResponse::text(
                serde_json::json!({
                    "message": "Update hello",
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
                    "reason": ""
                })
                .to_string(),
            )],
        );
        let mut ctx = fixture.context();

        let outcome = run_turn_loop(&mut ctx, None, &TurnLoopConfig::default())
            .await
            .expect("runs loop");
        assert!(outcome.is_success());
        let success = outcome.into_success().expect("success");
        assert_eq!(success.value.modified_paths, vec!["hello.rs"]);
        assert!(success.value.commit_hash.is_some());
        assert_eq!(
            std::fs::read_to_string(fixture.dir.path().join("hello.rs")).unwrap(),
            "fn hello() -> i32 { 1 }\n"
        );
    }

    #[test]
    fn turn_capability_expr_declares_the_terminal_branches() {
        let expr = turn_capability_expr();
        let leaves = expr.leaves();
        assert!(leaves.contains(&LlmCapability::ShortDescription));
        assert!(leaves.contains(&LlmCapability::EditReplace));
        assert!(leaves.contains(&LlmCapability::ContextRequest));
        assert!(leaves.contains(&LlmCapability::Blocked));
        assert!(leaves.contains(&LlmCapability::Abort));
        assert!(!leaves.contains(&LlmCapability::PlanCreation));
        assert!(!leaves.contains(&LlmCapability::PlanUpdate));
        assert!(!leaves.contains(&LlmCapability::ReviewUpdate));
        let CapabilityExpr::Set(items) = expr else {
            panic!("expected Set at root");
        };
        let CapabilityExpr::AnyOf(label, branches) = &items[1] else {
            panic!("expected AnyOf for action branches");
        };
        assert_eq!(label, "action");
        let kinds: Vec<&str> = branches.iter().map(|branch| branch.kind.as_str()).collect();
        assert_eq!(kinds, vec!["research", "edit", "blocked", "abort"]);
        // The edit branch keeps the outcome a change set commits with, while
        // the terminal signals live in branches of their own.
        assert!(branches[1].expr.contains(LlmCapability::CommitMessage));
        assert!(!branches[1].expr.contains(LlmCapability::Blocked));
    }
}
