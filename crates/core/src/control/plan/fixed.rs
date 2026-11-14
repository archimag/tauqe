//! Fixed plan execution loop.
//!
//! [`run_fixed_plan_loop`] drives an entire fixed plan sequentially
//! by collecting all executable `Todo` leaf steps in preorder (DFS),
//! executing them via [`run_plan_sequence_loop`], and refreshing
//! container statuses upon completion.

use tokio::sync::watch;

use tauqe_protocol::{PlanItem, PlanItemStatus};

use crate::control::context::LoopContext;
use crate::control::outcome::{LoopResult, LoopSuccess, ResponseEnvelope, SystemError};
use crate::control::plan::sequence::{
    run_plan_sequence_loop, PlanSequenceArtifact, PlanSequenceLoopConfig,
};
use crate::plan::storage::PlanStorage;

/// Configuration for running a fixed plan.
#[derive(Debug, Clone)]
pub struct FixedPlanLoopConfig {
    /// Configuration passed to the underlying sequence loop.
    pub sequence: PlanSequenceLoopConfig,
}

impl From<PlanSequenceLoopConfig> for FixedPlanLoopConfig {
    fn from(sequence: PlanSequenceLoopConfig) -> Self {
        Self { sequence }
    }
}

/// Artifact of running a fixed plan to completion or stop.
#[derive(Debug, Clone, PartialEq)]
pub struct FixedPlanArtifact {
    /// Sequence artifact capturing executed steps and stop reason.
    pub sequence: PlanSequenceArtifact,
}

/// Traverses plan items in preorder (DFS) and collects IDs of executable `Todo` leaves.
fn collect_todo_leaves(items: &[PlanItem], leaves: &mut Vec<String>) {
    for item in items {
        if item.is_leaf() {
            if item.status == PlanItemStatus::Todo {
                leaves.push(item.id.clone());
            }
        } else {
            collect_todo_leaves(&item.children, leaves);
        }
    }
}

/// Drives a fixed plan sequentially by executing all `Todo` leaves in order.
///
/// 1. Loads the plan from [`PlanStorage`].
/// 2. Performs a preorder (DFS) walk of the plan items to extract leaf steps with status `Todo`.
/// 3. Executes the sequence via [`run_plan_sequence_loop`], respecting soft-stop and cancellation signals.
/// 4. Re-reads and refreshes aggregated container statuses via `refresh_parent_statuses()` in [`PlanStorage`].
pub async fn run_fixed_plan_loop(
    ctx: &mut LoopContext<'_>,
    plan_id: &str,
    config: &FixedPlanLoopConfig,
    soft_stop_rx: Option<&watch::Receiver<bool>>,
) -> Result<LoopResult<FixedPlanArtifact>, SystemError> {
    ctx.check_cancelled()?;

    // 1. Load the plan to identify pending leaves.
    let plan = PlanStorage::load_plan(ctx.repo_root, plan_id)
        .map_err(|err| SystemError::Other(err.to_string()))?
        .ok_or_else(|| SystemError::Other(format!("Plan '{plan_id}' not found")))?;

    let mut todo_leaf_ids = Vec::new();
    collect_todo_leaves(&plan.implementation.items, &mut todo_leaf_ids);

    if todo_leaf_ids.is_empty() {
        return Ok(LoopResult::Success(LoopSuccess::new(
            FixedPlanArtifact {
                sequence: PlanSequenceArtifact {
                    executed_steps: Vec::new(),
                    stop: crate::control::plan::sequence::PlanSequenceStop::Completed,
                },
            },
            ResponseEnvelope::default(),
        )));
    }

    // 2. Execute the sequence of leaf steps.
    let sequence_result =
        run_plan_sequence_loop(ctx, plan_id, &todo_leaf_ids, &config.sequence, soft_stop_rx)
            .await?;

    let sequence_artifact = match sequence_result {
        LoopResult::Success(success) => success.value,
        LoopResult::Diverged(envelope) => return Ok(LoopResult::Diverged(envelope)),
        LoopResult::Exhausted(report) => return Ok(LoopResult::Exhausted(report)),
    };

    // 3. Post-execution status normalization: refresh container aggregate statuses.
    if let Ok(Some(mut current_plan)) = PlanStorage::load_plan(ctx.repo_root, plan_id) {
        current_plan.implementation.refresh_parent_statuses();
        let _ = PlanStorage::save_plan(ctx.repo_root, &current_plan);
    }

    Ok(LoopResult::Success(LoopSuccess::new(
        FixedPlanArtifact {
            sequence: sequence_artifact,
        },
        ResponseEnvelope::default(),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use tauqe_protocol::{
        CapabilityExpr, ConceptItem, EditProtocolKind, LlmCapability, ModelRef, Plan, PlanItem,
        PlanItemStatus,
    };

    use crate::config::ModelsConfig;
    use crate::context::ContextManager;
    use crate::control::context::{CancellationToken, EventSink, LoopContext};
    use crate::control::plan::step::PlanStepLoopConfig;
    use crate::control::prompt::PromptBuilder;
    use crate::control::turn::TurnLoopConfig;
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
            Self {
                dir,
                context: ContextManager::new(root),
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
            let caps = CapabilityExpr::set([CapabilityExpr::leaf(LlmCapability::TextMessage)]);
            let prompt_builder =
                PromptBuilder::new(&self.provider, &self.model, &self.protocol, &mut self.cache);
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

    fn test_config() -> FixedPlanLoopConfig {
        FixedPlanLoopConfig {
            sequence: PlanSequenceLoopConfig {
                step: PlanStepLoopConfig {
                    turn: TurnLoopConfig::default(),
                    models: ModelsConfig::default(),
                    protocol: EditProtocolKind::Structured,
                    marker: None,
                    selection: None,
                },
            },
        }
    }

    fn stored_hierarchical_plan(root: &Path, id: &str) -> Plan {
        let plan = Plan::from_parts(
            id.to_string(),
            "Fixed Plan".to_string(),
            Some("Hierarchical test plan".to_string()),
            0,
            0,
            vec![ConceptItem {
                id: "c-1".to_string(),
                title: "Concept".to_string(),
                details: None,
                children: Vec::new(),
            }],
            vec![PlanItem {
                id: "parent-1".to_string(),
                title: "Parent milestone".to_string(),
                status: PlanItemStatus::Todo,
                children: vec![
                    PlanItem {
                        id: "1.1".to_string(),
                        title: "Substep 1.1".to_string(),
                        status: PlanItemStatus::Todo,
                        files: vec!["one.rs".to_string()],
                        ..PlanItem::default()
                    },
                    PlanItem {
                        id: "1.2".to_string(),
                        title: "Substep 1.2".to_string(),
                        status: PlanItemStatus::Todo,
                        files: vec!["two.rs".to_string()],
                        ..PlanItem::default()
                    },
                ],
                ..PlanItem::default()
            }],
        );
        PlanStorage::save_plan(root, &plan).expect("save");
        plan
    }

    #[tokio::test]
    async fn fixed_plan_loop_executes_preorder_leaves_and_refreshes_parent() {
        let mut fixture = Fixture::new(
            &[("one.rs", "fn one() {}\n"), ("two.rs", "fn two() {}\n")],
            vec![
                MockResponse::text(
                    serde_json::json!({
                        "message": "Update one",
                        "action": {
                            "kind": "edit",
                            "changes": [{
                                "op": "replace",
                                "path": "one.rs",
                                "to": "",
                                "start": "",
                                "end": "",
                                "content": "",
                                "old_text": "fn one() {}\n",
                                "new_text": "fn one() -> i32 { 1 }\n"
                            }],
                            "commit_message": "feat(one): update one",
                            "message": "",
                            "reason": ""
                        }
                    })
                    .to_string(),
                ),
                MockResponse::text(
                    serde_json::json!({
                        "message": "Update two",
                        "action": {
                            "kind": "edit",
                            "changes": [{
                                "op": "replace",
                                "path": "two.rs",
                                "to": "",
                                "start": "",
                                "end": "",
                                "content": "",
                                "old_text": "fn two() {}\n",
                                "new_text": "fn two() -> i32 { 2 }\n"
                            }],
                            "commit_message": "feat(two): update two",
                            "message": "",
                            "reason": ""
                        }
                    })
                    .to_string(),
                ),
            ],
        );
        let root = fixture.dir.path().to_path_buf();
        stored_hierarchical_plan(&root, "fixed-plan");

        let mut ctx = fixture.context();
        let result = run_fixed_plan_loop(&mut ctx, "fixed-plan", &test_config(), None)
            .await
            .expect("fixed plan loop succeeds");

        assert!(result.is_success());
        let success = result.into_success().expect("success");
        assert_eq!(
            success.value.sequence.stop,
            crate::control::plan::sequence::PlanSequenceStop::Completed
        );
        assert_eq!(success.value.sequence.executed_steps.len(), 2);
        assert_eq!(success.value.sequence.executed_steps[0].step_id, "1.1");
        assert_eq!(success.value.sequence.executed_steps[1].step_id, "1.2");

        // Check that parent status was refreshed to Done in storage
        let reloaded = PlanStorage::load_plan(&root, "fixed-plan")
            .expect("load plan")
            .expect("plan exists");
        assert_eq!(
            reloaded.implementation.items[0].status,
            PlanItemStatus::Done
        );
    }
}
