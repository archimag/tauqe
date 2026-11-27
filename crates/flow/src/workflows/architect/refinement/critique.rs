//! Independent read-only critique of an Architect horizon.
use super::{DeferredTerminal, VerdictKind, WaveFailure};
use crate::api::{ApplicationEvent, ApplicationObserver, OperationGuard};
use crate::application::context::ContextSession;
use crate::application::usage_totals::UsageReceipt;
use crate::application::{ExecutionSession, PlanExecutionEnvironment, StructuredExecution};
use crate::workflows::architect::{publish_architect_started, ArchitectScope};
use crate::workflows::structured::{self, StructuredFailure};
use std::sync::Arc;
use std::time::Duration;
use tauqe_core::execution::ResearchEvidence;
use tauqe_core::models::ArchitectTier;
use tauqe_core::plan::prompt::critique::{build_plan_critique_message, plan_critique_contract};
use tauqe_core::plan::prompt::{PlanCritiqueOutput, PlanRefinementOutput};
use tauqe_core::plan::Plan;
use tauqe_core::providers::{ChatMessage, StreamEvent};

enum CritiqueFailure {
    Cancelled,
    Failed(String),
    Unreadable(String),
}

pub(crate) struct CritiqueWave {
    pub output: PlanCritiqueOutput,
    pub receipt: Option<UsageReceipt>,
    pub terminal: DeferredTerminal,
}

/// Audit one draft without changing its research state or issuing a plan.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_critique_wave(
    resources: &ExecutionSession,
    context: &ContextSession,
    observer: Arc<dyn ApplicationObserver>,
    deadline: Duration,
    operation: &OperationGuard,
    environment: &PlanExecutionEnvironment,
    plan: &Plan,
    draft: &PlanRefinementOutput,
    directives: Option<&str>,
    draft_task: &str,
    evidence: &ResearchEvidence,
    tier: &ArchitectTier,
    scope: &ArchitectScope,
) -> Result<CritiqueWave, WaveFailure> {
    let execution = environment
        .for_structured(resources, operation, tier, scope)
        .await
        .map_err(|error| WaveFailure::Failed(error.to_string()))?;

    let op_id = format!("op-{}", crate::api::next_operation_id());
    publish_architect_started(observer.as_ref(), &op_id, tier, false);
    tracing::info!(plan_id = %plan.id, model = %tier.model, "independent plan critique started");

    let mut message = build_plan_critique_message(
        plan,
        draft,
        directives,
        evidence,
        environment.config().repl.patch_strategy(),
    );
    message.push_str("\n## Original Draft Task:\n");
    message.push_str(draft_task);
    let messages = vec![ChatMessage::user(message)];
    match collect_structured_round(
        resources,
        context,
        observer.clone(),
        deadline,
        operation,
        &execution,
        messages,
        &op_id,
    )
    .await
    {
        Ok((critique, receipt)) => Ok(CritiqueWave {
            terminal: DeferredTerminal::new(op_id, critique_banner(&critique)),
            output: critique,
            receipt,
        }),
        Err(CritiqueFailure::Cancelled) => {
            observer.publish(&op_id, ApplicationEvent::Stream(StreamEvent::Cancelled));
            Err(WaveFailure::Cancelled)
        }
        Err(CritiqueFailure::Failed(message)) => {
            observer.publish(
                &op_id,
                ApplicationEvent::Stream(StreamEvent::Error(message.clone())),
            );
            Err(WaveFailure::Failed(message))
        }
        Err(CritiqueFailure::Unreadable(message)) => {
            let banner =
                format!("\n\n---\n⛔ **[Independent Critique Unavailable]** {message}\n\n");
            observer.publish(&op_id, ApplicationEvent::Finished(banner));
            Err(WaveFailure::Verdict {
                kind: VerdictKind::Blocked,
                message,
            })
        }
    }
}

/// The silent contract keeps partial payloads and resets out of the client stream.
#[allow(clippy::too_many_arguments)]
async fn collect_structured_round(
    resources: &ExecutionSession,
    context: &ContextSession,
    observer: Arc<dyn ApplicationObserver>,
    deadline: Duration,
    operation: &OperationGuard,
    execution: &StructuredExecution,
    messages: Vec<ChatMessage>,
    op_id: &str,
) -> Result<(PlanCritiqueOutput, Option<UsageReceipt>), CritiqueFailure> {
    let publisher = observer.clone();
    let completed = structured::run_bound(
        resources,
        context,
        operation,
        execution,
        plan_critique_contract(),
        messages,
        op_id,
        deadline,
        move |id, snapshot| publisher.publish(id, ApplicationEvent::UsageTotals(snapshot.clone())),
        |_| async {},
    )
    .await;
    let (completion, receipt) = match completed {
        Ok((completion, receipt)) => {
            observer.publish(op_id, ApplicationEvent::Usage(receipt.snapshot.clone()));
            (completion, Some(receipt))
        }
        Err(error) => (Err(error), None),
    };
    match completion {
        Ok(output) => Ok((output, receipt)),
        Err(error) => match error.classify("The independent critique") {
            StructuredFailure::Cancelled => Err(CritiqueFailure::Cancelled),
            StructuredFailure::Empty => Err(CritiqueFailure::Unreadable(
                "Structured request returned an empty payload".into(),
            )),
            StructuredFailure::Decode(err) => Err(CritiqueFailure::Unreadable(err)),
            StructuredFailure::InputLimit(err) => Err(CritiqueFailure::Failed(err.to_string())),
            StructuredFailure::Failed(err) => Err(CritiqueFailure::Failed(err)),
        },
    }
}

fn critique_banner(critique: &PlanCritiqueOutput) -> String {
    let mut banner = format!(
        "\n\n---\n**[Independent Critique]** `{}`\n",
        critique.verdict.as_str()
    );
    let summary = critique.summary.trim();
    if !summary.is_empty() {
        banner.push_str(&format!("\n{summary}\n"));
    }
    for finding in &critique.findings {
        let target = finding
            .target
            .as_deref()
            .map(str::trim)
            .filter(|target| !target.is_empty())
            .map(|target| format!("#{target}"))
            .unwrap_or_else(|| "(whole draft)".to_string());
        banner.push_str(&format!(
            "\n- [{}] {}: {}",
            finding.category.as_str(),
            target,
            finding.detail.trim()
        ));
    }
    banner.push_str("\n\n");
    banner
}
