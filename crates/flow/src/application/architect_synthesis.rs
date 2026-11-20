//! Architect horizon decisions over a persistent technical research unit.

use std::path::Path;

use super::infrastructure::execution::ArchitectResearchExecution;
use tauqe_core::context::ContextManager;
use tauqe_core::control::context::{CancellationToken, EventSink};
use tauqe_core::control::outcome::SystemError;
use tauqe_core::control::refine::{
    abort_report, architect_synthesis_capability_expr, blocked_reason, parse_round, RoundStatus,
    StructuredRefineOutcome, StructuredRefineStatus,
};
use tauqe_core::control::research_unit::StructuredResearchError;
use tauqe_core::models::{ArchitectTier, ModelRole};
use tauqe_core::providers::StreamEvent;
use tauqe_core::workflow::turn::TurnPhase;
use tokio::sync::{mpsc, watch};

const BARREN_ROUND_DIRECTIVE: &str = "<response_directive>\nThe previous discovery round produced no new information: no requested file was added to context, nothing is missing, and no search or targeted contour returned a result. Finalize the horizon now with status 'success', report status 'blocked' with the exact reason, or request concrete paths that exist in <repo_map>.\n</response_directive>";
const RESEARCH_LIMIT_DIRECTIVE: &str = "<response_directive>\nThe research round limit is exhausted: no further files, repository searches, symbol contours, or documentation will be loaded in this turn. Your investigation is complete. Do not request additional paths. Finalize the horizon with status 'success', or report status 'blocked' with the exact reason when the material you already hold is insufficient.\n</response_directive>";

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_architect_synthesis(
    mut execution: ArchitectResearchExecution,
    context: &mut ContextManager,
    task: &str,
    root: &Path,
    mut tier: ArchitectTier,
    intro_header: &str,
    budget_hint: &str,
    stream_tx: mpsc::Sender<StreamEvent>,
    cancel_rx: watch::Receiver<bool>,
) -> anyhow::Result<StructuredRefineOutcome> {
    let cancel = CancellationToken::from_receiver(cancel_rx.clone());
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

    let mut rounds_used = 0;
    let mut attempts_used = 0;
    let mut diagnostic: Option<String> = None;
    let (output, status) = loop {
        if *cancel_rx.borrow() {
            break (None, StructuredRefineStatus::Cancelled);
        }
        if rounds_used >= execution.max_turn_rounds() {
            execution.feedback(RESEARCH_LIMIT_DIRECTIVE);
        }
        let _ = stream_tx
            .send(StreamEvent::TurnPhase {
                phase: TurnPhase::Proposal,
                round: Some(rounds_used + 1),
                max_rounds: None,
                detail: None,
            })
            .await;

        let reply = execution
            .request(
                root,
                context,
                task,
                &tier,
                architect_synthesis_capability_expr(),
                diagnostic.as_deref(),
                parse_round,
                &cancel,
                &events,
            )
            .await;
        let reply = match reply {
            Ok(reply) => reply,
            Err(StructuredResearchError::System(SystemError::Cancelled)) => {
                break (None, StructuredRefineStatus::Cancelled);
            }
            Err(StructuredResearchError::System(SystemError::Other(message))) => {
                tracing::warn!(workflow = "architect", model = %tier.model, error = %message,
                    "architect produced no actionable payload");
                break (None, StructuredRefineStatus::Blocked { reason: message });
            }
            Err(StructuredResearchError::System(error)) => {
                drop(events);
                let _ = forwarding.await;
                return Err(anyhow::anyhow!(error));
            }
            Err(StructuredResearchError::Decode(error)) => {
                attempts_used += 1;
                let budget = tier.attempts() as usize;
                diagnostic = Some(format!(
                    "<response_directive>\nYour previous reply did not satisfy the response schema ({error}). Regenerate the whole payload strictly according to the specified JSON schema: `status`, `explanation`, `reason`, `context_requests`, `repo_searches`, `repo_maps`, and `items` are all required. This was attempt {attempts_used} of {budget} on the running architect configuration.\n</response_directive>"
                ));
                let _ = stream_tx
                    .send(StreamEvent::TextDelta(format!(
                        "\n\n---\n⚠️ **[Schema Healing {attempts_used}/{budget}]** {error}\n\n"
                    )))
                    .await;
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
                let _ = stream_tx
                    .send(StreamEvent::TierEscalated {
                        tier: index,
                        model: next.model.clone(),
                        role: ModelRole::Architect,
                        effort: next.effort,
                    })
                    .await;
                let previous = format_tier(&tier);
                tier = next;
                attempts_used = 0;
                let _ = stream_tx
                    .send(StreamEvent::TextDelta(format!(
                        "\n\n> Architect level up: {previous} → {}\n\n",
                        format_tier(&tier)
                    )))
                    .await;
                continue;
            }
        };
        attempts_used = 0;
        diagnostic = None;
        let (payload, round_status) = reply.payload;
        match round_status {
            RoundStatus::Success => break (Some(payload), StructuredRefineStatus::Synthesized),
            RoundStatus::Blocked => {
                let reason = blocked_reason(&payload);
                break (Some(payload), StructuredRefineStatus::Blocked { reason });
            }
            RoundStatus::Aborted => {
                let report = abort_report(&payload);
                let _ = stream_tx
                    .send(StreamEvent::TextDelta(format!(
                        "\n\n---\n⛔ **[Execution Aborted]**\n\n{}\n\n",
                        report.to_message()
                    )))
                    .await;
                break (Some(payload), StructuredRefineStatus::Aborted(report));
            }
            RoundStatus::PlanUpdateRefused(reason) => {
                let _ = stream_tx
                    .send(StreamEvent::TextDelta(format!(
                        "\n\n---\n⛔ **[Plan Mutation Refused]**\n\n{reason}\n\n"
                    )))
                    .await;
                break (Some(payload), StructuredRefineStatus::Blocked { reason });
            }
            RoundStatus::NeedsContext => {
                if rounds_used >= execution.max_turn_rounds() {
                    let reason = format!("The Architect still required repository material after {} research round(s), and no further files, searches, contours, or documentation can be loaded in this turn.", execution.max_turn_rounds());
                    break (Some(payload), StructuredRefineStatus::Blocked { reason });
                }
                let progress = execution
                    .research(context, root, &reply.commands, &cancel)
                    .await;
                if let Some(limit) = progress.limit_exceeded {
                    let mut text = format!("Context token budget exceeded (token_budget = {}). The Architect requested {} token(s) in {} new file(s) [{}], but the context already holds {} token(s).", limit.token_budget, limit.requested_tokens, limit.requested_files.len(), limit.requested_files.join(", "), limit.current_tokens);
                    if !budget_hint.trim().is_empty() {
                        text.push(' ');
                        text.push_str(budget_hint.trim());
                    }
                    let _ = stream_tx
                        .send(StreamEvent::TextDelta(format!("\n\n---\n⚠️ {text}\n\n")))
                        .await;
                    break (
                        Some(payload),
                        StructuredRefineStatus::BudgetExceeded { diagnostic: text },
                    );
                }
                rounds_used += 1;
                let _ = stream_tx
                    .send(StreamEvent::TurnPhase {
                        phase: TurnPhase::Research,
                        round: Some(rounds_used),
                        max_rounds: Some(execution.max_turn_rounds()),
                        detail: None,
                    })
                    .await;
                if !progress.has_progress {
                    execution.feedback(BARREN_ROUND_DIRECTIVE);
                }
                let summary = progress.summary.trim();
                let body = if summary.is_empty() {
                    "The requested material produced no new information."
                } else {
                    summary
                };
                let _ = stream_tx
                    .send(StreamEvent::TextDelta(format!(
                        "\n\n---\n**[Round {rounds_used}]** {body}\n\n"
                    )))
                    .await;
            }
        }
    };

    drop(events);
    let _ = forwarding.await;
    tauqe_core::blocking::run_blocking(|| context.clear_transient());
    let _ = stream_tx.send(StreamEvent::Done).await;
    Ok(StructuredRefineOutcome {
        output,
        tier,
        status,
    })
}

fn format_tier(tier: &ArchitectTier) -> String {
    match tier.effort {
        Some(effort) => format!("{} ({effort})", tier.model),
        None => tier.model.to_string(),
    }
}
