//! A persistent technical unit for structured requests with repository context.

use std::path::Path;

use crate::context::ContextManager;
use crate::control::context::{CancellationToken, EventSink};
use crate::control::execution::{ExecutionUnit, TurnExecutionConfig};
use crate::control::outcome::SystemError;
use crate::llm::{CapabilityTree, LlmCommand};
use crate::workflow::policy::DirectApplyPolicy;
use crate::workflow::research::{execute_research_commands, ContextLimitExceeded};
use crate::workflow::WorkflowOptions;

/// A decoded round and the research commands admitted by its protocol.
pub struct StructuredResearchReply<T> {
    pub payload: T,
    pub commands: Vec<LlmCommand>,
}

/// Parsing is distinct from provider or cancellation failures: the caller may
/// spend a schema-healing attempt without charging the research-round budget.
#[derive(Debug)]
pub enum StructuredResearchError {
    System(SystemError),
    Decode(String),
}

/// Facts returned by one research round, without a domain decision about them.
pub struct StructuredResearchProgress {
    pub has_progress: bool,
    pub limit_exceeded: Option<ContextLimitExceeded>,
    pub summary: String,
}

/// Cache and epistemic journal survive all requests in this unit. A new unit
/// starts both afresh; context cleanup remains the caller's responsibility.
#[derive(Default)]
pub struct StructuredResearchUnit {
    execution: ExecutionUnit,
}

impl StructuredResearchUnit {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feedback(&mut self, text: &str) {
        self.execution.journal.push_block(text);
    }

    /// Freeze the caller's grant and current context for this request. The
    /// decoder runs inside core so callers never inspect provider JSON.
    #[allow(clippy::too_many_arguments)]
    pub async fn request<T>(
        &mut self,
        execution: TurnExecutionConfig<'_>,
        context: &mut ContextManager,
        options: &WorkflowOptions,
        task: &str,
        grant: CapabilityTree,
        feedback: Option<&str>,
        decode: fn(&str) -> Result<T, String>,
        cancel: &CancellationToken,
        events: &EventSink,
    ) -> Result<StructuredResearchReply<T>, StructuredResearchError> {
        let mut policy = DirectApplyPolicy::new();
        let mut ctx = self.execution.prepare_rebuilt(
            execution,
            context,
            options,
            task,
            grant,
            &mut policy,
            cancel,
            events,
        );
        let envelope = super::prompt::execute_rebuilt_round(&mut ctx, feedback, None)
            .await
            .map_err(StructuredResearchError::System)?;
        let payload = decode(&envelope.raw_text).map_err(StructuredResearchError::Decode)?;
        Ok(StructuredResearchReply {
            payload,
            commands: envelope.commands().to_vec(),
        })
    }

    /// Widen the supplied context and retain the resulting feedback for the
    /// next rebuilt request. No round count or schema-healing policy lives here.
    pub async fn research(
        &mut self,
        context: &mut ContextManager,
        repo_root: &Path,
        commands: &[LlmCommand],
        options: &WorkflowOptions,
        cancel: &CancellationToken,
    ) -> StructuredResearchProgress {
        let outcome = execute_research_commands(
            context,
            repo_root,
            commands,
            options.token_budget(),
            options.repomap_token_budget(),
            options.research_limits(),
            true,
            Some(&cancel.receiver()),
        )
        .await;
        let progress = StructuredResearchProgress {
            has_progress: outcome.has_progress(true),
            limit_exceeded: outcome.limit_exceeded.clone(),
            summary: outcome.architect_banner_summary(),
        };
        self.execution.journal.record_outcome(&outcome);
        progress
    }
}
