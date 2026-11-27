//! One Architect horizon wave, independent of the document that will store it.
pub(crate) mod critique;
use self::critique::run_critique_wave;
use super::{
    publish_architect_started, publish_horizon_terminal, run_contour_with_state,
    ArchitectResearchState, ArchitectScope, HorizonVerdict, PlanHorizonContour,
};
use crate::api::{ApplicationEvent, ApplicationObserver, OperationGuard};
use crate::application::context::ContextSession;
use crate::application::usage_totals::UsageReceipt;
use crate::application::{ExecutionSession, PlanExecutionEnvironment};
use std::sync::Arc;
use std::time::Duration;
use tauqe_core::models::{ArchitectTier, ModelSelection};
use tauqe_core::plan::find_duplicate_item_ids;
use tauqe_core::plan::prompt::{
    build_plan_horizon_system_prompt, build_plan_reconciliation_prompt, format_architect_tier,
    PlanRefinementOutput, StructuredRefineOutcome, StructuredRefineStatus,
};
use tauqe_core::plan::Plan;
use tauqe_core::stop::HardCancellation;

pub(crate) struct PlannerWave {
    pub output: PlanRefinementOutput,
    pub tier: ArchitectTier,
    pub research: ArchitectResearchState,
    pub receipt: Option<UsageReceipt>,
    pub terminal: DeferredTerminal,
}

/// The caller owns the document and supplies its ephemeral planning coordinate.
pub(crate) struct HorizonSynthesisRequest {
    pub plan: Plan,
    pub draft_task: String,
    pub directives: Option<String>,
    pub scope: ArchitectScope,
    pub planner_model: Option<ModelSelection>,
    pub critic_model: Option<ModelSelection>,
    pub skip_critique: bool,
    pub operation_title: &'static str,
}

pub(crate) struct SynthesizedHorizon {
    pub output: PlanRefinementOutput,
    pub tier: ArchitectTier,
    pub receipts: Vec<UsageReceipt>,
    pub terminal: DeferredTerminal,
}

/// The document owner releases the final successful phase after saving.
pub(crate) struct DeferredTerminal {
    id: String,
    text: String,
}

impl DeferredTerminal {
    pub(super) fn new(id: String, text: String) -> Self {
        Self { id, text }
    }

    pub fn publish(self, observer: &dyn ApplicationObserver) {
        observer.publish(&self.id, ApplicationEvent::Finished(self.text));
    }
}

/// Run the only draft, optional critique, and single reconciliation sequence.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn synthesize_horizon(
    resources: &ExecutionSession,
    context: &ContextSession,
    operation: &OperationGuard,
    environment: &PlanExecutionEnvironment,
    observer: Arc<dyn ApplicationObserver>,
    deadline: Duration,
    request: HorizonSynthesisRequest,
) -> Result<SynthesizedHorizon, WaveFailure> {
    let result = synthesize_horizon_inner(
        resources,
        context,
        operation,
        environment,
        observer,
        deadline,
        request,
    )
    .await;
    if let Err(error) = context.clear_transient_context().await {
        tracing::warn!(%error, "Failed to clear synthesis transient context");
    }
    result
}

#[allow(clippy::too_many_arguments)]
async fn synthesize_horizon_inner(
    resources: &ExecutionSession,
    context: &ContextSession,
    operation: &OperationGuard,
    environment: &PlanExecutionEnvironment,
    observer: Arc<dyn ApplicationObserver>,
    deadline: Duration,
    request: HorizonSynthesisRequest,
) -> Result<SynthesizedHorizon, WaveFailure> {
    let HorizonSynthesisRequest {
        plan,
        draft_task,
        directives,
        scope,
        planner_model,
        critic_model,
        skip_critique,
        operation_title,
    } = request;
    let cfg = environment.config();
    let (planner_tier, _) = tauqe_core::plan::prompt::resolve_architect_tier(
        cfg,
        plan.implementation.architect.as_ref(),
        planner_model.as_ref(),
        None,
        plan.implementation.last_replanned_step.as_deref(),
    );
    let critic_tier = critic_model
        .as_ref()
        .filter(|_| !skip_critique)
        .map(|selection| cfg.models.resolve_selection_architect_tier(selection));
    let header = |phase: &str, tier: &ArchitectTier| {
        format!(
            "# {}: [{}] {}\n*{} with {}...*\n\n---\n\n",
            operation_title,
            plan.id,
            plan.title,
            phase,
            format_architect_tier(tier)
        )
    };
    let mut wave = run_planner_wave(
        resources,
        context,
        operation,
        environment,
        observer.clone(),
        draft_task.clone(),
        header("Drafting the plan horizon", &planner_tier),
        planner_tier,
        scope.clone(),
        None,
    )
    .await?;
    let mut receipts = Vec::new();
    if let Some(receipt) = wave.receipt.clone() {
        receipts.push(receipt);
    }
    let mut terminal = wave.terminal;
    if let Some(critic_tier) = critic_tier.as_ref() {
        ensure_phase_can_start(&operation.hard_cancellation())?;
        let evidence = wave
            .research
            .execution
            .executor
            .evidence_snapshot()
            .ok_or_else(|| WaveFailure::Failed("Architect research evidence was lost".into()))?;
        terminal.publish(observer.as_ref());
        let critique = run_critique_wave(
            resources,
            context,
            observer.clone(),
            deadline,
            operation,
            environment,
            &plan,
            &wave.output,
            directives.as_deref(),
            &draft_task,
            &evidence,
            critic_tier,
            &scope,
        )
        .await?;
        if let Some(receipt) = critique.receipt {
            receipts.push(receipt);
        }
        terminal = critique.terminal;
        if !critique.output.is_approved() {
            ensure_phase_can_start(&operation.hard_cancellation())?;
            terminal.publish(observer.as_ref());
            let mut task = build_plan_horizon_system_prompt();
            task.push_str("\n\n---\n\n");
            task.push_str(&draft_task);
            task.push_str("\n\n---\n\n");
            task.push_str(&build_plan_reconciliation_prompt(
                &plan,
                &wave.output,
                &critique.output,
                directives.as_deref(),
            ));
            let tier = wave.tier.clone();
            wave = run_planner_wave(
                resources,
                context,
                operation,
                environment,
                observer.clone(),
                task,
                header("Reconciling the independent critique", &tier),
                tier,
                scope,
                Some(wave.research),
            )
            .await?;
            if let Some(receipt) = wave.receipt.clone() {
                receipts.push(receipt);
            }
            terminal = wave.terminal;
        }
    }
    Ok(SynthesizedHorizon {
        output: wave.output,
        tier: wave.tier,
        receipts,
        terminal,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VerdictKind {
    Blocked,
    Aborted,
}

impl VerdictKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Blocked => "Blocked",
            Self::Aborted => "Aborted",
        }
    }
}

pub(crate) enum WaveFailure {
    Cancelled,
    Failed(String),
    EmptyHorizon,
    InvalidTree(String),
    Verdict { kind: VerdictKind, message: String },
}

fn ensure_phase_can_start(cancel: &HardCancellation) -> Result<(), WaveFailure> {
    if cancel.is_cancelled() {
        Err(WaveFailure::Cancelled)
    } else {
        Ok(())
    }
}

fn classify_outcome(
    result: anyhow::Result<StructuredRefineOutcome>,
    cancelled: bool,
    error: Option<&str>,
) -> HorizonVerdict {
    match result {
        _ if cancelled => HorizonVerdict::Cancelled,
        Err(failure) => HorizonVerdict::Failed(
            error
                .map(str::to_owned)
                .unwrap_or_else(|| failure.to_string()),
        ),
        Ok(StructuredRefineOutcome {
            output,
            tier,
            status,
        }) => match status {
            StructuredRefineStatus::Cancelled => HorizonVerdict::Cancelled,
            StructuredRefineStatus::Blocked { reason } => HorizonVerdict::Blocked { reason, tier },
            StructuredRefineStatus::Aborted(report) => HorizonVerdict::Aborted { report, tier },
            StructuredRefineStatus::Synthesized => match output {
                Some(output) if !output.items.is_empty() => {
                    if let Some(duplicate) = find_duplicate_item_ids(&output.items) {
                        HorizonVerdict::InvalidTree { duplicate, tier }
                    } else {
                        HorizonVerdict::Synthesized { output, tier }
                    }
                }
                _ => HorizonVerdict::EmptyHorizon { tier },
            },
        },
    }
}

/// Publish and account for one contour; the caller owns persistence and context cleanup.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_planner_wave(
    resources: &ExecutionSession,
    shared_context: &ContextSession,
    operation: &OperationGuard,
    environment: &PlanExecutionEnvironment,
    observer: Arc<dyn ApplicationObserver>,
    task: String,
    intro_header: String,
    tier: ArchitectTier,
    scope: ArchitectScope,
    previous: Option<ArchitectResearchState>,
) -> Result<PlannerWave, WaveFailure> {
    let op_id = format!("op-{}", crate::api::next_operation_id());
    publish_architect_started(observer.as_ref(), &op_id, &tier, false);

    let context = match previous.as_ref() {
        Some(state) => state.context.clone(),
        None => shared_context.context_for_turn().await,
    };
    let (result, summary, research) = run_contour_with_state(
        resources,
        shared_context,
        operation,
        environment,
        PlanHorizonContour {
            operation_id: op_id.clone(),
            task,
            context,
            scope,
            tier,
            intro_header,
        },
        observer.clone(),
        previous,
    )
    .await;

    let verdict = classify_outcome(
        result,
        summary.cancelled || operation.hard_cancellation().is_cancelled(),
        summary.error.as_deref(),
    );
    if matches!(verdict, HorizonVerdict::Synthesized { .. }) {
        if let Some(receipt) = &summary.receipt {
            observer.publish(&op_id, ApplicationEvent::Usage(receipt.snapshot.clone()));
        }
    } else {
        publish_horizon_terminal(observer.as_ref(), &op_id, &verdict, &summary);
    }
    let terminal = DeferredTerminal::new(op_id.clone(), summary.assistant_text.clone());
    let receipt = summary.receipt.clone();
    // The contour revoked its detached source. Restore shared publication before
    // critique or cleanup; reconciliation activates its retained research copy.
    if !shared_context.activate_manual_publication(operation, observer) {
        return Err(WaveFailure::Failed(
            "Could not restore plan synthesis context publication".into(),
        ));
    }
    match verdict {
        HorizonVerdict::Synthesized { output, tier } => match research {
            Some(research) => Ok(PlannerWave {
                output,
                tier,
                research,
                receipt,
                terminal,
            }),
            None => Err(WaveFailure::Failed(
                "Architect research state was lost".into(),
            )),
        },
        HorizonVerdict::Cancelled => Err(WaveFailure::Cancelled),
        HorizonVerdict::Failed(reason) => Err(WaveFailure::Failed(reason)),
        HorizonVerdict::EmptyHorizon { .. } => Err(WaveFailure::EmptyHorizon),
        HorizonVerdict::InvalidTree { duplicate, .. } => Err(WaveFailure::InvalidTree(duplicate)),
        HorizonVerdict::Blocked { reason, .. } => Err(WaveFailure::Verdict {
            kind: VerdictKind::Blocked,
            message: reason,
        }),
        HorizonVerdict::Aborted { report, .. } => Err(WaveFailure::Verdict {
            kind: VerdictKind::Aborted,
            message: report.to_message(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_core::models::ModelRef;
    use tauqe_core::plan::prompt::AbortReport;
    use tauqe_core::plan::PlanItem;
    use tauqe_core::stop::StopSource;

    #[test]
    fn hard_requested_between_phases_prevents_the_next_phase() {
        let source = StopSource::new();
        let cancel = source.token().hard_cancellation();
        source.request_soft();
        assert!(ensure_phase_can_start(&cancel).is_ok());
        source.request_hard();
        assert!(matches!(
            ensure_phase_can_start(&cancel),
            Err(WaveFailure::Cancelled)
        ));
        // A late observer also sees Hard before the next phase starts.
        assert!(matches!(
            ensure_phase_can_start(&source.token().hard_cancellation()),
            Err(WaveFailure::Cancelled)
        ));
    }

    #[test]
    fn wave_classifies_terminal_status_and_invalid_horizons() {
        let tier = ArchitectTier::from(ModelRef::openrouter("planner/model"));
        let output = |items| {
            serde_json::from_value::<PlanRefinementOutput>(serde_json::json!({
                "status": "success", "explanation": "done", "items": items
            }))
            .unwrap()
        };
        let outcome = |status, items| {
            Ok(StructuredRefineOutcome {
                output: Some(output(items)),
                tier: tier.clone(),
                status,
            })
        };
        assert!(matches!(
            classify_outcome(
                outcome(StructuredRefineStatus::Synthesized, vec![]),
                false,
                None
            ),
            HorizonVerdict::EmptyHorizon { .. }
        ));
        let duplicate = vec![
            PlanItem {
                id: "1".into(),
                ..Default::default()
            };
            2
        ];
        assert!(
            matches!(classify_outcome(outcome(StructuredRefineStatus::Synthesized, duplicate), false, None), HorizonVerdict::InvalidTree { duplicate, .. } if duplicate == "1")
        );
        assert!(matches!(
            classify_outcome(
                outcome(
                    StructuredRefineStatus::Synthesized,
                    vec![PlanItem {
                        id: "1".into(),
                        ..Default::default()
                    }]
                ),
                false,
                None
            ),
            HorizonVerdict::Synthesized { .. }
        ));
        assert!(
            matches!(classify_outcome(outcome(StructuredRefineStatus::Blocked { reason: "stop".into() }, vec![]), false, None), HorizonVerdict::Blocked { reason, .. } if reason == "stop")
        );
        assert!(matches!(
            classify_outcome(
                outcome(
                    StructuredRefineStatus::Aborted(AbortReport {
                        reason: "host".into(),
                        recommendation: "install".into()
                    }),
                    vec![]
                ),
                false,
                None
            ),
            HorizonVerdict::Aborted { .. }
        ));
        assert!(matches!(
            classify_outcome(
                outcome(StructuredRefineStatus::Cancelled, vec![]),
                false,
                None
            ),
            HorizonVerdict::Cancelled
        ));
        assert!(
            matches!(classify_outcome(Err(anyhow::anyhow!("worker")), false, Some("transport")), HorizonVerdict::Failed(reason) if reason == "transport")
        );
        assert!(matches!(
            classify_outcome(Err(anyhow::anyhow!("worker")), true, None),
            HorizonVerdict::Cancelled
        ));
    }
}
