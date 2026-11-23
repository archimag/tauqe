//! Architect horizon decisions over a persistent technical research unit.

use std::path::Path;

use crate::application::ArchitectResearchExecution;
use tauqe_core::context::ContextManager;
use tauqe_core::execution::transcript::TurnTranscript;
use tauqe_core::execution::{
    CancellationToken, EventSink, ExecutionError, PromptInput, PromptSequence,
};
use tauqe_core::models::{ArchitectTier, ModelRole};
use tauqe_core::plan::prompt::{
    architect_synthesis_capability_expr, RoundStatus, StructuredRefineOutcome,
    StructuredRefineStatus,
};
use tauqe_core::prompt::refine::PlanRefineRound;
use tauqe_core::prompt::TurnBrief;
use tauqe_core::providers::StreamEvent;
use tauqe_core::workflow::research::{
    execute_research_commands, journal::TurnResearchJournal, ResearchLimits,
};
use tauqe_core::workflow::turn::TurnPhase;
use tokio::sync::{mpsc, watch};

const BARREN_ROUND_DIRECTIVE: &str = "<response_directive>\nThe previous discovery round produced no new information: no requested file was added to context, nothing is missing, and no search or targeted contour returned a result. Finalize the horizon now with status 'success', report status 'blocked' with the exact reason, or request concrete paths that exist in <repo_map>.\n</response_directive>";
const RESEARCH_LIMIT_DIRECTIVE: &str = "<response_directive>\nThe research round limit is exhausted: no further files, repository searches, symbol contours, or documentation will be loaded in this turn. Your investigation is complete. Do not request additional paths. Finalize the horizon with status 'success', or report status 'blocked' with the exact reason when the material you already hold is insufficient.\n</response_directive>";
const MAX_EMPTY_RETRIES: usize = 2;
const EMPTY_REMINDER: &str = "Your previous reply produced no actionable result. Provide a concrete result that conforms to the active response schema.";

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_architect_synthesis(
    mut execution: ArchitectResearchExecution,
    context: &mut ContextManager,
    task: &str,
    root: &Path,
    tier: ArchitectTier,
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

    let result = synthesize(
        &mut execution,
        context,
        task,
        root,
        tier,
        budget_hint,
        &events,
        &cancel,
    )
    .await;
    drop(events);
    let _ = forwarding.await;
    tauqe_core::blocking::run_blocking(|| context.clear_transient());
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
    budget_hint: &str,
    events: &EventSink,
    cancel: &CancellationToken,
) -> anyhow::Result<StructuredRefineOutcome> {
    let max_rounds = execution.max_turn_rounds();
    let config = execution.config().context.clone();
    let limits = ResearchLimits::from_context_config(&config);
    let mut journal = TurnResearchJournal::default();
    let mut transcript = TurnTranscript::default();
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
        events.emit(StreamEvent::TurnPhase {
            phase: TurnPhase::Proposal,
            round: Some(rounds_used + 1),
            max_rounds: None,
            detail: None,
        });

        let mut prompt = task.trim_end().to_owned();
        if let Some(feedback) = journal.rendered() {
            append_feedback(&mut prompt, &feedback);
        }
        if let Some(diagnostic) = diagnostic.as_deref() {
            append_feedback(&mut prompt, diagnostic);
        }
        let reply = match request_round(
            execution,
            &mut transcript,
            context,
            root,
            &config,
            prompt,
            events,
            cancel,
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
                    "<response_directive>\nYour previous reply did not satisfy the response schema ({error}). Regenerate the whole payload strictly according to the specified JSON schema: `status`, `explanation`, `reason`, `context_requests`, `repo_searches`, `repo_maps`, and `items` are all required. This was attempt {attempts_used} of {budget} on the running architect configuration.\n</response_directive>"
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
        let PlanRefineRound {
            output: payload,
            status: round_status,
            commands,
        } = reply;
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
                if rounds_used >= max_rounds {
                    let reason = format!("The Architect still required repository material after {max_rounds} research round(s), and no further files, searches, contours, or documentation can be loaded in this turn.");
                    break (Some(payload), StructuredRefineStatus::Blocked { reason });
                }
                let progress = execute_research_commands(
                    context,
                    root,
                    &commands,
                    config.token_budget,
                    config.repomap_token_budget(),
                    limits,
                    true,
                    Some(&cancel.receiver()),
                )
                .await;
                if cancel.is_cancelled() {
                    break (None, StructuredRefineStatus::Cancelled);
                }
                if let Some(limit) = progress.limit_exceeded() {
                    let mut text = format!("Context token budget exceeded (token_budget = {}). The Architect requested {} token(s) in {} new file(s) [{}], but the context already holds {} token(s).", limit.token_budget, limit.requested_tokens, limit.requested_files.len(), limit.requested_files.join(", "), limit.current_tokens);
                    if !budget_hint.trim().is_empty() {
                        text.push(' ');
                        text.push_str(budget_hint.trim());
                    }
                    events.emit_text_delta(format!("\n\n---\n⚠️ {text}\n\n"));
                    break (
                        Some(payload),
                        StructuredRefineStatus::BudgetExceeded { diagnostic: text },
                    );
                }
                rounds_used += 1;
                journal.begin_round(rounds_used);
                journal.record_outcome(&progress);
                events.emit(StreamEvent::TurnPhase {
                    phase: TurnPhase::Research,
                    round: Some(rounds_used),
                    max_rounds: Some(max_rounds),
                    detail: None,
                });
                if !progress.has_progress(true) {
                    journal.push_block(BARREN_ROUND_DIRECTIVE);
                }
                let summary = progress.architect_banner_summary();
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
    transcript: &mut TurnTranscript,
    context: &ContextManager,
    root: &Path,
    config: &tauqe_core::config::ContextConfig,
    task: String,
    events: &EventSink,
    cancel: &CancellationToken,
) -> Result<Option<Result<PlanRefineRound, String>>, ExecutionError> {
    if cancel.is_cancelled() {
        return Err(ExecutionError::Cancelled);
    }
    let executor = &mut execution.executor;
    let files = executor
        .context_files(root, &context.get_state().items)
        .await
        .map_err(|error| ExecutionError::Other(error.to_string()))?;
    let strategy = executor.patch_strategy();
    let files = tauqe_core::blocking::run_blocking(|| {
        transcript.project_files(files, None, root, strategy)
    });
    let (repo_map, repo_dirs) = if config.include_repo_map {
        (
            executor.repo_map(root, config.repomap_token_budget()).await,
            None,
        )
    } else {
        (
            None,
            executor.repo_dirs(root, context.pinned_patterns()).await,
        )
    };
    events.emit(StreamEvent::RepoMapTokens(
        repo_map
            .as_deref()
            .or(repo_dirs.as_deref())
            .map(tauqe_core::prompt::estimate_tokens)
            .unwrap_or_default(),
    ));
    let project = executor.project(root, Some(&cancel.receiver())).await;

    for attempt in 0..=MAX_EMPTY_RETRIES {
        if cancel.is_cancelled() {
            return Err(ExecutionError::Cancelled);
        }
        let mut task = task.clone();
        if attempt > 0 {
            append_feedback(&mut task, EMPTY_REMINDER);
        }
        if let Some(versions) = &files.current_versions {
            append_feedback(&mut task, versions);
        }
        let input = PromptInput {
            task,
            originals: files.originals.clone(),
            current_files: files.current.clone(),
            history: None,
            capabilities: architect_synthesis_capability_expr(),
            project: project.clone(),
            repo_root: root.to_path_buf(),
            repo_map: repo_map.clone(),
            repo_dirs: repo_dirs.clone(),
            outer_docs: Vec::new(),
            brief: Some(TurnBrief::architect()),
            sequence: PromptSequence::Assemble { continuation: None },
            tail: Vec::new(),
            excluded_disk_sources: Vec::new(),
        };
        let result = execution.execute_round(input, events, cancel).await?;
        match result {
            Some(Ok(round)) if round.commands.is_empty() => {}
            Some(result) => return Ok(Some(result)),
            None => {}
        }
        tracing::warn!(repo_root = %root.display(), attempt = attempt + 1,
            "architect request produced no actionable response");
    }
    Ok(None)
}

fn format_tier(tier: &ArchitectTier) -> String {
    match tier.effort {
        Some(effort) => format!("{} ({effort})", tier.model),
        None => tier.model.to_string(),
    }
}
