//! Public scenario entry points and session services used by workflows.

pub(crate) mod discussion_prompt;

mod infrastructure;
pub(crate) use infrastructure::execution::{
    ArchitectResearchExecution, BoundExecution, ExecutionSession, PlanExecutionEnvironment,
    StructuredExecution,
};
#[cfg(test)]
pub(crate) use infrastructure::execution::{ExecutionEnvironment, ExecutionFactory};
// Scenario migrations will consume the prepared accounting scope incrementally.
#[allow(dead_code)]
pub(crate) mod execution_scope;
pub(crate) mod operation;
pub(crate) mod session;

pub(crate) mod shutdown;

pub(crate) mod dialogue;
pub(crate) mod focus;

pub(crate) mod configuration;

pub(crate) mod configuration_files;

pub(crate) mod system_status;

pub(crate) mod configuration_operation;

pub(crate) mod history;

mod history_compaction;

pub(crate) mod history_operation;

pub(crate) mod context;
mod context_publication;

pub(crate) mod context_operation;

pub(crate) mod outer_docs_cache;

pub(crate) mod outer_docs_preload;

pub(crate) mod turn_setup;

pub(crate) mod interactive_history;

pub(crate) mod interactive_completion;

pub(crate) mod interactive_task;

pub(crate) mod usage;

pub(crate) mod usage_totals;

pub(crate) mod repository_startup;

pub(crate) mod task_exit;

pub(crate) mod review_observer;

pub(crate) mod locale_generation;

pub(crate) mod git_squash;

pub(crate) mod preflight;

pub(crate) mod plan_batch;

impl crate::api::Application {
    pub async fn synthesize_plan(
        &self,
        input: crate::api::PlanSynthesisInput,
        observer: std::sync::Arc<dyn crate::api::ApplicationObserver>,
        exit_deadline: std::time::Duration,
    ) -> Result<tauqe_core::plan::PlanSynthesisResult, crate::api::PlanSynthesisError> {
        self.plan_resources()
            .synthesize(self.configuration_session(), input, observer, exit_deadline)
            .await
    }
}

#[cfg(feature = "test-support")]
impl crate::api::Application {
    pub async fn test_run_architect_contour(
        &self,
        operation: &crate::api::OperationGuard,
        work: crate::api::PlanHorizonContour,
        observer: std::sync::Arc<dyn crate::api::ApplicationObserver>,
    ) -> (
        anyhow::Result<tauqe_core::plan::prompt::StructuredRefineOutcome>,
        crate::api::ContourRelaySummary,
    ) {
        let environment = PlanExecutionEnvironment::new(self.configuration().await, operation);
        crate::workflows::architect::run_contour(
            &self.execution_session(),
            self.context_session(),
            operation,
            &environment,
            work,
            observer,
        )
        .await
    }
}

mod review_batch;

pub(crate) mod interactive_updates;

pub(crate) mod documents;
pub(crate) mod repository;

pub(crate) mod ask;
pub(crate) mod edit_cycle;
pub(crate) mod events;
mod review_entry;
pub(crate) mod settings;

#[cfg(test)]
mod scenario_tests;

#[cfg(test)]
pub(crate) fn outer_doc_entry(
    id: &str,
    title: &str,
    source: &str,
) -> tauqe_core::config::OuterDocEntry {
    serde_json::from_value(serde_json::json!({ "id": id, "title": title, "source": source }))
        .unwrap()
}
