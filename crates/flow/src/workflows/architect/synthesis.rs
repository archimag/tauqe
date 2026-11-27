//! Architect horizon decisions over a persistent technical research unit.

use std::path::Path;

use super::ArchitectResearchState;
use crate::application::ArchitectResearchExecution;
use crate::workflows::round::{run_round, RoundServices};
use tauqe_core::context::ContextManager;
use tauqe_core::execution::executor::{ExecuteInput, PromptMessage};
use tauqe_core::execution::{EventSink, ExecutionError};
use tauqe_core::llm::{CapabilityTree, LlmCommand};
use tauqe_core::models::{ArchitectTier, ModelRole};
use tauqe_core::plan::prompt::{
    architect_synthesis_capability_expr, architect_synthesis_terminal_capability_expr, RoundStatus,
    StructuredRefineOutcome, StructuredRefineStatus,
};
use tauqe_core::prompt::TurnBrief;
use tauqe_core::providers::StreamEvent;
use tauqe_core::stop::HardCancellation;
use tauqe_core::workflow::research::journal::TurnResearchJournal;
use tauqe_core::workflow::turn::TurnPhase;
use tokio::sync::mpsc;

const BARREN_ROUND_DIRECTIVE: &str = "<response_directive>\nThe previous discovery round produced no new information: no requested file was added to context, nothing is missing, and no search or targeted contour returned a result. Finalize the horizon now with status 'success', report status 'blocked' with the exact reason, or request concrete paths that exist in <repo_map>.\n</response_directive>";
const RESEARCH_LIMIT_DIRECTIVE: &str = "<response_directive>\nThe research round limit is exhausted: no further files, repository searches, symbol contours, or documentation will be loaded in this turn. Your investigation is complete. Do not request additional paths. Finalize the horizon with status 'success', or report status 'blocked' with the exact reason when the material you already hold is insufficient.\n</response_directive>";
const MAX_EMPTY_RETRIES: usize = 2;
const EMPTY_REMINDER: &str = "Your previous reply produced no actionable result. Provide a concrete result that conforms to the active response schema.";

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_architect_synthesis(
    state: &mut ArchitectResearchState,
    task: &str,
    root: &Path,
    tier: ArchitectTier,
    intro_header: &str,
    stream_tx: mpsc::Sender<StreamEvent>,
    cancel: HardCancellation,
) -> anyhow::Result<StructuredRefineOutcome> {
    let (events, mut rx) = EventSink::channel();
    let forwarding = {
        let tx = stream_tx.clone();
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                if !matches!(event, StreamEvent::Done) && tx.send(event).await.is_err() {
                    break;
                }
            }
        })
    };
    if !intro_header.trim().is_empty() {
        let _ = stream_tx
            .send(StreamEvent::TextDelta(intro_header.into()))
            .await;
    }

    let result = synthesize(
        &mut state.execution,
        &mut state.context,
        task,
        root,
        tier,
        &events,
        &cancel,
    )
    .await;
    drop(events);
    let _ = forwarding.await;
    let _ = stream_tx.send(StreamEvent::Done).await;
    result
}

#[allow(clippy::too_many_arguments)]
async fn synthesize(
    execution: &mut ArchitectResearchExecution,
    context: &mut ContextManager,
    task: &str,
    root: &Path,
    mut tier: ArchitectTier,
    events: &EventSink,
    cancel: &HardCancellation,
) -> anyhow::Result<StructuredRefineOutcome> {
    let max_rounds = execution.max_turn_rounds();
    let mut journal = TurnResearchJournal::default();
    // A continuation may enter on a different rung than the draft finished on.
    // Rebind before preparing its first request, after the previous usage window drained.
    execution.select_tier(&tier).await?;
    let mut rounds_used = 0;
    let mut attempts_used = 0;
    let mut diagnostic: Option<String> = None;
    let (output, status) = loop {
        if cancel.is_cancelled() {
            break (None, StructuredRefineStatus::Cancelled);
        }
        if rounds_used >= max_rounds {
            journal.push_block(RESEARCH_LIMIT_DIRECTIVE);
        }
        let capabilities = if rounds_used < max_rounds {
            architect_synthesis_capability_expr()
        } else {
            architect_synthesis_terminal_capability_expr()
        };
        events.emit(StreamEvent::TurnPhase {
            phase: TurnPhase::Proposal,
            round: Some(rounds_used + 1),
            max_rounds: None,
            detail: None,
        });

        let mut prompt = task.trim_end().to_owned();
        if let Some(diagnostic) = diagnostic.as_deref() {
            append_feedback(&mut prompt, diagnostic);
        }
        let reply = match request_round(
            execution,
            context,
            root,
            prompt,
            events,
            cancel,
            &mut journal,
            rounds_used + 1,
            &capabilities,
        )
        .await
        {
            Ok(Some(Ok(reply))) => reply,
            Err(ExecutionError::Cancelled) => {
                break (None, StructuredRefineStatus::Cancelled);
            }
            Ok(None) => {
                let message = format!(
                    "The model produced an empty response after {MAX_EMPTY_RETRIES} additional attempt(s)."
                );
                tracing::warn!(workflow = "architect", model = %tier.model, error = %message,
                    "architect produced no actionable payload");
                break (None, StructuredRefineStatus::Blocked { reason: message });
            }
            Err(error) => return Err(error.into()),
            Ok(Some(Err(error))) => {
                attempts_used += 1;
                let budget = tier.attempts() as usize;
                diagnostic = Some(format!(
                    "<response_directive>\nYour previous reply did not satisfy the active response schema ({error}). Regenerate the whole payload strictly according to the active JSON schema. This was attempt {attempts_used} of {budget} on the running architect configuration.\n</response_directive>"
                ));
                events.emit_text_delta(format!(
                    "\n\n---\n⚠️ **[Schema Healing {attempts_used}/{budget}]** {error}\n\n"
                ));
                if attempts_used < budget {
                    continue;
                }
                let next = execution.models().next_architect_tier(Some(&tier));
                if next == tier {
                    let reason = format!(
                        "Failed to parse the architect refinement output after {attempts_used} attempt(s) on {}: {error}",
                        format_tier(&tier)
                    );
                    break (None, StructuredRefineStatus::Blocked { reason });
                }
                let index = execution
                    .models()
                    .architect_tier_index(Some(&next))
                    .unwrap_or_default();
                execution.select_tier(&next).await?;
                events.emit(StreamEvent::TierEscalated {
                    tier: index,
                    model: next.model.clone(),
                    role: ModelRole::Architect,
                    effort: next.effort,
                });
                let previous = format_tier(&tier);
                tier = next;
                attempts_used = 0;
                events.emit_text_delta(format!(
                    "\n\n> Architect level up: {previous} → {}\n\n",
                    format_tier(&tier)
                ));
                continue;
            }
        };
        attempts_used = 0;
        diagnostic = None;
        let (payload, round_status, discovery) = reply;
        match round_status {
            RoundStatus::Success => break (Some(payload), StructuredRefineStatus::Synthesized),
            RoundStatus::Blocked(reason) => {
                break (Some(payload), StructuredRefineStatus::Blocked { reason });
            }
            RoundStatus::Aborted(report) => {
                events.emit_text_delta(format!(
                    "\n\n---\n⛔ **[Execution Aborted]**\n\n{}\n\n",
                    report.to_message()
                ));
                break (Some(payload), StructuredRefineStatus::Aborted(report));
            }
            RoundStatus::PlanUpdateRefused(reason) => {
                events.emit_text_delta(format!(
                    "\n\n---\n⛔ **[Plan Mutation Refused]**\n\n{reason}\n\n"
                ));
                break (Some(payload), StructuredRefineStatus::Blocked { reason });
            }
            RoundStatus::NeedsContext => {
                if cancel.is_cancelled() {
                    break (None, StructuredRefineStatus::Cancelled);
                }
                rounds_used += 1;
                // The shared research step opens its own boundary when there are
                // commands. A barren needs_context still spends one research round.
                if discovery.kind == Default::default() {
                    journal.begin_round(rounds_used);
                }
                events.emit(StreamEvent::TurnPhase {
                    phase: TurnPhase::Research,
                    round: Some(rounds_used),
                    max_rounds: Some(max_rounds),
                    detail: None,
                });
                if !discovery.outcome.has_progress(true) {
                    journal.push_block(BARREN_ROUND_DIRECTIVE);
                }
                let summary = discovery.outcome.architect_banner_summary();
                let body = if summary.is_empty() {
                    "The requested material produced no new information."
                } else {
                    &summary
                };
                events.emit_text_delta(format!("\n\n---\n**[Round {rounds_used}]** {body}\n\n"));
            }
        }
    };

    Ok(StructuredRefineOutcome {
        output,
        tier,
        status,
    })
}

fn append_feedback(task: &mut String, block: &str) {
    task.push_str("\n\n");
    task.push_str(block);
}

#[allow(clippy::too_many_arguments)]
async fn request_round(
    execution: &mut ArchitectResearchExecution,
    context: &mut ContextManager,
    _root: &Path,
    task: String,
    events: &EventSink,
    cancel: &HardCancellation,
    journal: &mut TurnResearchJournal,
    _research_round: usize,
    capabilities: &CapabilityTree,
) -> Result<
    Option<
        Result<
            (
                tauqe_core::plan::prompt::PlanRefinementOutput,
                RoundStatus,
                tauqe_core::workflow::research::step::ResearchStep,
            ),
            String,
        >,
    >,
    ExecutionError,
> {
    if cancel.is_cancelled() {
        return Err(ExecutionError::Cancelled);
    }
    execution.executor.emit_repo_map_tokens(events);

    for attempt in 0..=MAX_EMPTY_RETRIES {
        if cancel.is_cancelled() {
            return Err(ExecutionError::Cancelled);
        }
        let mut task = task.clone();
        if attempt > 0 {
            append_feedback(&mut task, EMPTY_REMINDER);
        }
        let evidence_journal = journal.clone();
        let reply = run_round(
            &mut execution.executor,
            ExecuteInput {
                message: PromptMessage::Task(task.clone()),
                directives: None,
                round: None,
                context_items: context.get_state().items,
                history: None,
                capabilities: capabilities.clone(),
                brief: Some(TurnBrief::architect()),
                feedback: None,
                staged: None,
                created_paths: Vec::new(),
                journal: Some(&evidence_journal),
            },
            RoundServices {
                context,
                journal,
                repairing: false,
                edits: None,
            },
            events,
            cancel,
        )
        .await;
        let reply = match reply {
            Ok(reply) => reply,
            Err(ExecutionError::EmptyResponse) => {
                continue;
            }
            Err(ExecutionError::Protocol(error)) => {
                return Ok(Some(Err(error)));
            }
            Err(error) => return Err(error),
        };
        let round = reply.iter().find_map(|command| match command {
            LlmCommand::ArchitectRound { output, status } => Some((output.clone(), status.clone())),
            _ => None,
        });
        if let Some((output, status)) = round {
            return Ok(Some(Ok((output, status, journal.last_step().clone()))));
        }
        return Ok(Some(Err(
            "Architect response contained no ArchitectRound command".into(),
        )));
    }
    Ok(None)
}

fn format_tier(tier: &ArchitectTier) -> String {
    match tier.effort {
        Some(effort) => format!("{} ({effort})", tier.model),
        None => tier.model.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Application;
    use crate::application::PlanExecutionEnvironment;
    use crate::workflows::architect::ArchitectScope;
    use std::sync::Arc;
    use tauqe_core::config::AppConfig;
    use tauqe_core::models::ModelRef;
    use tauqe_core::providers::mock::MockLlmProvider;

    #[tokio::test]
    async fn complete_architect_input_is_refused_before_transport() {
        struct Recorder(std::sync::Mutex<Vec<tauqe_core::providers::ModelUsageInfo>>);
        impl tauqe_core::execution::usage::UsageSink for Recorder {
            fn record(&self, report: &tauqe_core::providers::ModelUsageInfo) {
                self.0.lock().unwrap().push(report.clone());
            }
        }
        let root = tempfile::tempdir().unwrap();
        let mut config = AppConfig::default();
        config.context.include_repo_map = false;
        config.request.max_input_tokens = 1;
        let (history, _) = mpsc::unbounded_channel();
        let app = Application::for_repository(config, root.path().into(), history);
        let provider = Arc::new(MockLlmProvider::new());
        app.test_set_provider(provider.clone()).await;
        let operation = app.try_begin_operation().unwrap();
        let environment = PlanExecutionEnvironment::new(app.configuration().await, &operation);
        let tier = ArchitectTier::from(ModelRef::openrouter("mock/model"));
        let mut execution = environment
            .for_architect_research(
                &app.execution_session(),
                &operation,
                &tier,
                &ArchitectScope::Plan("budget".into()),
                root.path(),
            )
            .await
            .unwrap();
        let scope = tauqe_core::execution::usage::UsageScope::new(Arc::new(Recorder(
            std::sync::Mutex::new(Vec::new()),
        )));
        scope.activate();
        execution
            .bind_usage(tauqe_core::execution::usage::UsageBinding::Bound(scope))
            .unwrap();
        let mut context = ContextManager::new(root.path().into());
        let result = synthesize(
            &mut execution,
            &mut context,
            "Keep the full task",
            root.path(),
            tier,
            &EventSink::noop(),
            &HardCancellation::never(),
        )
        .await;
        assert!(result.unwrap_err().to_string().contains("input"));
        assert!(provider.requests().is_empty());
        assert_eq!(app.active_operation_id(), Some(operation.id()));
    }
}
