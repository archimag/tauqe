//! Shared step execution: one technical attempt, bounded retries, and queue traversal.

pub(crate) mod attempts;
pub(crate) mod completion;
pub(crate) mod queue;
pub(crate) mod run;

use super::round::{RoundInput, RoundWorkflow};
use super::state::TurnState;
use super::turn::{TurnDisk, TurnSettlement, TurnWorkflow};
use std::path::PathBuf;
use tauqe_core::config::AppConfig;
use tauqe_core::context::ContextManager;
use tauqe_core::execution::{CancellationToken, EventSink, PromptExecutor};
use tauqe_core::llm::CapabilityTree;
use tauqe_core::prompt::TurnBrief;
use tauqe_core::providers::StreamEvent;
use tauqe_core::workflow::research::step::ResearchLoopConfig;
use tauqe_core::workflow::WorkflowOptions;

pub(crate) struct StepAttempt {
    pub root: PathBuf,
    pub task: String,
    pub context: ContextManager,
    pub capabilities: CapabilityTree,
    pub config: AppConfig,
    pub executor: PromptExecutor,
    pub cancel: CancellationToken,
    pub events: EventSink,
}

impl StepAttempt {
    pub(crate) async fn run(self, disk: TurnDisk) -> (TurnSettlement, ContextManager) {
        let Self {
            root,
            task,
            context,
            capabilities,
            config,
            mut executor,
            cancel,
            events,
        } = self;
        let mut state = TurnState::new(
            context,
            capabilities,
            None,
            Some(WorkflowOptions::from_app_config(&config).max_turn_rounds()),
            config.context.max_scratchpad_chars(),
        );
        let project = executor.project(&root, Some(&cancel.receiver())).await;
        let (repo_map, repo_dirs) = if config.context.include_repo_map {
            (
                executor
                    .repo_map(&root, config.context.repomap_token_budget())
                    .await,
                None,
            )
        } else {
            (
                None,
                executor
                    .repo_dirs(&root, state.context.pinned_patterns())
                    .await,
            )
        };
        events.emit(StreamEvent::RepoMapTokens(
            repo_map
                .as_deref()
                .or(repo_dirs.as_deref())
                .map(tauqe_core::prompt::estimate_tokens)
                .unwrap_or(0),
        ));
        let settlement = TurnWorkflow {
            round: RoundWorkflow {
                executor: &mut executor,
                events: &events,
                cancel: &cancel,
            },
            disk: &disk,
            research: &ResearchLoopConfig::from_context(&config.context),
        }
        .run(
            &mut state,
            RoundInput {
                task: &task,
                repo_root: &root,
                project: &project,
                repo_map: repo_map.as_deref(),
                repo_dirs: repo_dirs.as_deref(),
                outer_docs: &config.outer_docs,
                brief: Some(&TurnBrief::step()),
                feedback: None,
                reason: None,
                staged: None,
            },
        )
        .await;
        (settlement, state.context)
    }
}
