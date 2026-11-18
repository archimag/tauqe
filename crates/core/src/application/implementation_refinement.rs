//! Prepare an implementation Architect wave from frozen run configuration.

use crate::api::Application;
use crate::config::AppConfig;
use crate::models::{ArchitectTier, ModelSelection, ReasoningEffort};
use crate::plan::prompt::{
    build_plan_horizon_system_prompt, build_plan_horizon_user_prompt, PlanHorizonDiagnostics,
};
use crate::providers::{format_session_id, SESSION_PREFIX_ARCHITECT, SESSION_PREFIX_REVIEW};
use crate::workflow::implementation::{ExecutionScope, RefinementRequest};
use crate::workflow::WorkflowOptions;
use std::path::Path;

pub struct PreparedImplementationRefinement {
    pub tier: ArchitectTier,
    pub options: WorkflowOptions,
    pub task: String,
    pub intro_header: String,
    pub budget_hint: String,
}

#[derive(Debug)]
pub struct RefinementPreparationError {
    pub reason: String,
    pub tier: ArchitectTier,
}

impl Application {
    pub async fn prepare_implementation_refinement(
        &self,
        root: &Path,
        config: &AppConfig,
        request: &RefinementRequest,
    ) -> Result<PreparedImplementationRefinement, RefinementPreparationError> {
        let tier = architect_tier(config, request.selection.as_ref(), request.reasoning_effort);
        let frame = self
            .implementation_frame(&request.scope, root)
            .await
            .map_err(|failure| RefinementPreparationError {
                reason: failure.into_reason(),
                tier: tier.clone(),
            })?;
        let mut options = WorkflowOptions::from_app_config(config);
        options.session_id = Some(refinement_session(&frame.scope));
        let diagnostics = request
            .failed_step
            .as_deref()
            .map(|step| PlanHorizonDiagnostics {
                blocked_step_id: step,
                blocked_reason: request.failure.as_deref(),
                toolchain_status: None,
            });
        let mut task = build_plan_horizon_system_prompt();
        task.push_str("\n\n---\n\n");
        task.push_str(&build_plan_horizon_user_prompt(
            &frame.plan,
            diagnostics,
            request.instructions.as_deref(),
        ));
        let intro_header = format!("# Automated Plan Replanning: [{}] {}\n*Reprojecting the remaining horizon of this implementation...*\n\n---\n\n", frame.plan.id, frame.plan.title);
        Ok(PreparedImplementationRefinement {
            tier,
            options,
            task,
            intro_header,
            budget_hint: "The plan is too broad and should be manually decomposed or scoped down."
                .to_string(),
        })
    }
}

fn refinement_session(scope: &ExecutionScope) -> String {
    match scope {
        ExecutionScope::Plan { plan_id, .. } => {
            format_session_id(SESSION_PREFIX_ARCHITECT, plan_id)
        }
        ExecutionScope::ReviewGroup { review_id, .. }
        | ExecutionScope::ReviewFinding { review_id, .. } => {
            format_session_id(SESSION_PREFIX_REVIEW, review_id)
        }
    }
}

/// Resolves the Architect rung a wave runs on from the request.
///
/// The pipeline already resolved the ladder and applied the lazy level-up
/// invariant, so the request names the exact rung to execute; the host never
/// re-derives the escalation policy locally.
///
/// The rung travels as the pair of its model and the reasoning depth the
/// request states, and it is resolved against the configured Architect
/// entries: the attempt budget and the escalation marker declared for that
/// pair reach the synthesis contour, so the wave runs within the rounds it is
/// granted and a rung the developer reserved for manual runs stays saturated.
/// A pair the configuration does not declare keeps the requested depth and
/// resolves no runtime parameters, exactly as a plain tier carries them.
fn architect_tier(
    config: &AppConfig,
    selection: Option<&ModelSelection>,
    effort: Option<ReasoningEffort>,
) -> ArchitectTier {
    let model = match selection {
        Some(selection) => config.resolve_model(selection),
        None => config.models.architect_tier(0).model,
    };
    let rung = ModelSelection::Tier { model, effort };
    config.models.resolve_selection_architect_tier(&rung)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ModelsConfig;
    use crate::models::{ModelEntry, ModelRef};
    /// A wave requests the whole rung by its `(model, effort)` pair, so the
    /// runtime parameters declared for that exact Architect entry reach the
    /// synthesis contour instead of being reset by a reconstructed tier.
    #[test]
    fn the_wave_tier_keeps_the_configured_parameters_of_its_rung() {
        let shared = ModelRef::openrouter("architect/shared");
        let mut manual = ModelEntry::new(shared.clone(), Some(ReasoningEffort::High));
        manual.attempts = Some(4);
        manual.level_up = Some(false);
        let config = AppConfig {
            models: ModelsConfig {
                repl: vec![ModelEntry::from(ModelRef::openrouter("base/primary"))],
                architect: Some(vec![
                    ModelEntry::new(shared.clone(), Some(ReasoningEffort::Low)),
                    manual,
                ]),
                ..ModelsConfig::default()
            },
            ..AppConfig::default()
        };

        let selection = ModelSelection::Tier {
            model: shared.clone(),
            effort: Some(ReasoningEffort::High),
        };
        let tier = architect_tier(&config, Some(&selection), Some(ReasoningEffort::High));
        assert_eq!(tier.model, shared);
        assert_eq!(tier.effort, Some(ReasoningEffort::High));
        assert_eq!(tier.attempts, Some(4));
        assert_eq!(tier.attempts(), 4);
        assert_eq!(tier.level_up, Some(false));

        // A pair the configuration does not declare keeps the requested depth
        // and resolves no runtime parameters, exactly as a plain tier carried
        // them before the rung identity was introduced.
        let outside = ModelSelection::Tier {
            model: ModelRef::openrouter("architect/outside"),
            effort: Some(ReasoningEffort::Max),
        };
        let tier = architect_tier(&config, Some(&outside), Some(ReasoningEffort::Max));
        assert_eq!(tier.model, ModelRef::openrouter("architect/outside"));
        assert_eq!(tier.effort, Some(ReasoningEffort::Max));
        assert_eq!(tier.attempts, None);
        assert_eq!(tier.level_up, None);

        // An absent selection still resolves the base rung of the ladder.
        let tier = architect_tier(&config, None, Some(ReasoningEffort::Low));
        assert_eq!(tier.model, ModelRef::openrouter("architect/shared"));
        assert_eq!(tier.effort, Some(ReasoningEffort::Low));
    }
    fn request(scope: ExecutionScope) -> RefinementRequest {
        RefinementRequest {
            scope,
            failed_step: Some("1".into()),
            failure: Some("compiler failure marker".into()),
            selection: None,
            reasoning_effort: None,
            instructions: Some("Keep the public interface".into()),
        }
    }

    #[test]
    fn refinement_affinity_is_scoped_to_the_plan_or_review_audit() {
        assert_eq!(
            refinement_session(&ExecutionScope::Plan {
                plan_id: "plan-1".into(),
                step_id: Some("step-2".into())
            }),
            "tauqe-architect-plan-1"
        );
        for scope in [
            ExecutionScope::ReviewGroup {
                review_id: "audit-1".into(),
                group_id: "group-1".into(),
            },
            ExecutionScope::ReviewFinding {
                review_id: "audit-1".into(),
                item_id: 7,
            },
        ] {
            assert_eq!(refinement_session(&scope), "tauqe-review-audit-1");
        }
    }

    #[tokio::test]
    async fn preparation_keeps_stored_scope_diagnostics_and_frozen_budgets() {
        let dir = tempfile::tempdir().unwrap();
        let plan = crate::plan::Plan::from_parts(
            "plan-preparation".into(),
            "Stored horizon".into(),
            None,
            0,
            0,
            Vec::new(),
            vec![crate::plan::PlanItem {
                id: "1".into(),
                title: "Implement the client".into(),
                files: vec!["client.rs".into()],
                ..Default::default()
            }],
        );
        crate::plan::storage::PlanStorage::save_plan(dir.path(), &plan).unwrap();
        let mut config = AppConfig::default();
        config.context.token_budget = 7;
        config.repl.max_turn_rounds = 3;
        let app = Application::default();
        let request = request(ExecutionScope::Plan {
            plan_id: "plan-preparation".into(),
            step_id: None,
        });
        let prepared = app
            .prepare_implementation_refinement(dir.path(), &config, &request)
            .await
            .unwrap();
        assert_eq!(prepared.options.token_budget, 7);
        assert_eq!(prepared.options.max_turn_rounds, 3);
        assert_eq!(
            prepared.options.session_id.as_deref(),
            Some("tauqe-architect-plan-preparation")
        );
        assert!(prepared.task.contains("compiler failure marker"));
        assert!(prepared.task.contains("Keep the public interface"));
        assert!(prepared.task.contains("Implement the client"));
        assert_eq!(prepared.intro_header, "# Automated Plan Replanning: [plan-preparation] Stored horizon\n*Reprojecting the remaining horizon of this implementation...*\n\n---\n\n");
    }

    #[tokio::test]
    async fn failed_frame_resolution_keeps_the_requested_tier_for_the_blocked_result() {
        let dir = tempfile::tempdir().unwrap();
        let app = Application::default();
        let mut request = request(ExecutionScope::Plan {
            plan_id: "missing".into(),
            step_id: None,
        });
        let model = ModelRef::openrouter("architect/manual");
        request.selection = Some(ModelSelection::Tier {
            model: model.clone(),
            effort: Some(ReasoningEffort::High),
        });
        request.reasoning_effort = Some(ReasoningEffort::High);
        let error = match app
            .prepare_implementation_refinement(dir.path(), &AppConfig::default(), &request)
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("a missing frame cannot prepare a refinement"),
        };
        assert_eq!(error.reason, "Plan 'missing' not found");
        assert_eq!(error.tier.model, model);
        assert_eq!(error.tier.effort, Some(ReasoningEffort::High));
    }
}
