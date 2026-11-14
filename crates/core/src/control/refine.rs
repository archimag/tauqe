//! Structured Architect synthesis contour: Schema Healing and tier escalation.
//!
//! Horizon synthesis and review-group decomposition are one Architect operation,
//! so both drive this contour: the modern control loop carries the rounds, prompt
//! assembly, research journal, and corrective budgets, while the contour owns the
//! policy specific to a structured Architect payload.
//!
//! The regime is declared, never inferred: the loop recognizes the turn through
//! [`TurnIntent::ArchitectSynthesis`], and the wire surface is the typed
//! [`PlanRefinePromptProtocol`], so the rendered instructions, the response
//! schema, and the decoder observe one capability surface.
//!
//! Four terminal policies live here:
//!
//! - *Silence* is a transport-level non-answer, so the prompt loop's own budget
//!   resolves it and the running architect rung is never charged for it.
//! - *An unparsable payload* is a schema violation: the decoding diagnostic travels
//!   back to the model as the trailing directive of the next request and spends
//!   one attempt of the rung ([`ArchitectTier::attempts`]). An exhausted rung
//!   climbs the ladder through [`ModelsConfig::next_architect_tier`], applies the
//!   promoted rung's reasoning effort to every later round, and rotates the
//!   provider session identifier with the effective model. A rung served by
//!   another provider is resolved through the injected [`ProviderResolver`].
//! - *A breached context budget* and a *`needs_context` answer that can no longer
//!   be served* are typed terminal outcomes ([`StructuredRefineStatus`]), so a
//!   caller renders them deterministically instead of reconstructing a verdict
//!   from prose.
//! - *A host-environment abort* is the escape hatch of an unattended regime: the
//!   payload asserts that no plan can complete the work in the current
//!   environment, so the contour streams the exact command the developer must
//!   run and ends in [`StructuredRefineStatus::Aborted`] without spending a
//!   generation attempt of the running rung.
//!
//! The contour never records session history: recording a synthesis belongs to the
//! caller. It does release the transient context it widened, both the files its
//! rounds discovered and the external documents they materialized, because that
//! material is the Architect's investigation rather than the developer's dialogue.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use tauqe_protocol::{
    ArchitectTier, CapabilityExpr, LlmCapability, ModelRole, ProviderKind, TurnPhase,
};
use tokio::sync::{mpsc, watch};

use crate::config::ModelsConfig;
use crate::context::ContextManager;
use crate::control::cache::TurnInputCache;
use crate::control::context::{CancellationToken, EventSink, LoopContext};
use crate::control::outcome::{AbortReport, SystemError};
use crate::control::prompt::{execute_round, PromptBuilder};
use crate::plan::prompt::{parse_plan_refinement_output, PlanRefinementOutput};
use crate::prompt::protocol::PromptProtocol;
use crate::prompt::refine::{unsupported_plan_update_reason, PlanRefinePromptProtocol};
use crate::prompt::TurnBrief;
use crate::providers::{format_session_id, LlmProvider, StreamEvent};
use crate::workflow::policy::DirectApplyPolicy;
use crate::workflow::research::journal::TurnResearchJournal;
use crate::workflow::research::{
    execute_research_commands, ContextLimitExceeded, ResearchRoundOutcome,
};
use crate::workflow::WorkflowOptions;

/// Operation label recorded for every round of a synthesis run.
const WORKFLOW_NAME: &str = "architect";

/// Directive appended to the turn journal when a research round produced nothing.
const BARREN_ROUND_DIRECTIVE: &str = "<response_directive>\nThe previous discovery round produced no new information: no requested file was added to context, nothing is missing, and no search or targeted contour returned a result. Finalize the horizon now with status 'success', report status 'blocked' with the exact reason, or request concrete paths that exist in <repo_map>.\n</response_directive>";

/// Directive appended to the turn journal once the research budget is spent.
const RESEARCH_LIMIT_DIRECTIVE: &str = "<response_directive>\nThe research round limit is exhausted: no further files, repository searches, symbol contours, or documentation will be loaded in this turn. Your investigation is complete. Do not request additional paths. Finalize the horizon with status 'success', or report status 'blocked' with the exact reason when the material you already hold is insufficient.\n</response_directive>";
/// Composes the capability surface of an Architect horizon synthesis.
///
/// A synthesis projects a horizon instead of patching a tree, so file edits and
/// toolchain verification stay excluded. The abort escape hatch stays included,
/// because a synthesis runs unattended exactly as an isolated step does: a
/// host-environment impossibility only the developer can repair is reported
/// through `abort` instead of an improvised surrogate or a misreported blocker.
pub(crate) fn architect_synthesis_capability_expr() -> CapabilityExpr {
    CapabilityExpr::from_leaves([
        LlmCapability::TextMessage,
        LlmCapability::ContextRequest,
        LlmCapability::RepoSearch,
        LlmCapability::RepoDirs,
        LlmCapability::RepoMap,
        LlmCapability::DirList,
        LlmCapability::DocRequest,
        LlmCapability::OuterDocRequest,
        LlmCapability::PlanCreation,
        LlmCapability::Scratchpad,
        LlmCapability::CommitMessage,
        LlmCapability::TurnError,
        LlmCapability::TurnSkip,
        LlmCapability::Blocked,
        LlmCapability::Abort,
        LlmCapability::PlanUpdate,
    ])
}

/// Typed resolver of the provider client that serves another provider.
#[async_trait]
pub trait ProviderResolver: Send + Sync {
    /// Resolves the client of `kind`.
    async fn resolve(&self, kind: ProviderKind) -> anyhow::Result<Arc<dyn LlmProvider>>;
}

/// Static configuration of one structured synthesis run.
pub struct StructuredRefineConfig<'a> {
    /// Repository root the synthesis operates in.
    pub repo_root: &'a Path,
    /// Frozen operational budgets of the run.
    pub options: &'a WorkflowOptions,
    /// Architect ladder consulted once a rung has spent its attempts.
    pub models: &'a ModelsConfig,
    /// Rung the synthesis starts on.
    pub tier: ArchitectTier,
    /// Resolver of a client served by another provider.
    pub providers: &'a dyn ProviderResolver,
    /// Header the synthesis opens with.
    pub intro_header: &'a str,
    /// Diagnostic appended when the files token budget is breached.
    pub budget_hint: &'a str,
}

/// Typed terminal status of one structured synthesis run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StructuredRefineStatus {
    /// The Architect finalized the horizon.
    Synthesized,
    /// The Architect demanded more context than `files_token_budget` allows.
    BudgetExceeded {
        /// Deterministic report the caller surfaces.
        diagnostic: String,
    },
    /// The synthesis could not finalize within the approved design.
    Blocked {
        /// Deterministic reason the caller reports.
        reason: String,
    },
    /// The synthesis needs an action only the developer can perform on the host.
    Aborted(AbortReport),
    /// The turn was cancelled before the Architect finalized.
    Cancelled,
}

impl StructuredRefineStatus {
    /// Reports whether the synthesis produced an accepted horizon.
    pub fn is_synthesized(&self) -> bool {
        matches!(self, Self::Synthesized)
    }
}

/// Result of one structured synthesis run.
#[derive(Debug, Clone)]
pub struct StructuredRefineOutcome {
    /// Typed payload of the round that ended the run, when one parsed.
    pub output: Option<PlanRefinementOutput>,
    /// Rung the run ended on.
    pub tier: ArchitectTier,
    /// Typed terminal status of the run.
    pub status: StructuredRefineStatus,
}

/// Accepted terminal status of one synthesis round.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RoundStatus {
    /// The Architect finalized the horizon.
    Success,
    /// The Architect refused to plan and stated its reason.
    Blocked,
    /// The Architect aborted: only the developer can unblock the host.
    Aborted,
    /// The round emitted a plan mutation this contour does not accept.
    PlanUpdateRefused(String),
    /// The Architect named the evidence it still needs.
    NeedsContext,
}

/// Runs one structured Architect synthesis on the control loop engine.
pub async fn run_structured_refine(
    context_manager: &mut ContextManager,
    provider: Arc<dyn LlmProvider>,
    task: &str,
    config: StructuredRefineConfig<'_>,
    stream_tx: mpsc::Sender<StreamEvent>,
    cancel_rx: watch::Receiver<bool>,
) -> anyhow::Result<StructuredRefineOutcome> {
    let repo_root = config.repo_root;
    let protocol: Box<dyn PromptProtocol> =
        PlanRefinePromptProtocol::new().with_capability_expr(architect_synthesis_capability_expr());

    let mut policy = DirectApplyPolicy::new();
    let mut input_cache = TurnInputCache::default();
    let mut journal = TurnResearchJournal::default();
    let mut rounds_used = 0usize;

    let base_effort = config.options.reasoning_effort;
    let base_session = config.options.session_id.clone();
    let mut options = config.options.clone();
    let mut provider = provider;
    let mut tier = config.tier.clone();
    options.reasoning_effort = tier.effort.or(base_effort);
    let mut tier_attempts = tier.attempts() as usize;
    let mut attempts_used = 0usize;
    let mut phase_diagnostics: Option<String> = None;

    if !config.intro_header.trim().is_empty() {
        let _ = stream_tx
            .send(StreamEvent::TextDelta(config.intro_header.to_string()))
            .await;
    }

    let cancel_token = CancellationToken::from_receiver(cancel_rx.clone());
    let (internal_tx, mut internal_rx) = mpsc::unbounded_channel::<StreamEvent>();
    let stream_tx_clone = stream_tx.clone();
    let forward_task = tokio::spawn(async move {
        while let Some(event) = internal_rx.recv().await {
            if matches!(event, StreamEvent::Done) {
                continue;
            }
            if stream_tx_clone.send(event).await.is_err() {
                break;
            }
        }
    });
    let event_sink = EventSink::from_sender(internal_tx.clone());

    let (output, status) = loop {
        if cancel_token.is_cancelled() {
            break (None, StructuredRefineStatus::Cancelled);
        }

        if rounds_used >= options.max_research_rounds {
            journal.push_block(RESEARCH_LIMIT_DIRECTIVE);
        }

        let _ = stream_tx
            .send(StreamEvent::TurnPhase {
                phase: TurnPhase::Proposal,
                round: Some(rounds_used + 1),
                max_rounds: None,
                detail: None,
            })
            .await;

        let envelope = {
            // The synthesis owns the brief of its contour: the architect
            // introduction opens the prompt and the architect rules of the
            // regime follow it, never an inference from the leaf composition.
            let prompt_builder = PromptBuilder::new(
                provider.as_ref(),
                &tier.model,
                protocol.as_ref(),
                &mut input_cache,
            )
            .with_session_id(options.session_id.clone())
            .with_prompt(Some(task.to_string()))
            .with_repo_map_config(options.include_repo_map, options.repomap_token_budget)
            .with_patch_strategy(options.patch_strategy())
            .with_prompt_layout(Some(options.prompt_layout(false)))
            .with_turn_brief(Some(TurnBrief::architect()))
            .with_reasoning_effort(options.reasoning_effort);

            let mut ctx = LoopContext::new(
                &cancel_token,
                &event_sink,
                repo_root,
                context_manager,
                &mut journal,
                &mut policy,
                protocol.capability_expr(),
                prompt_builder,
            );

            execute_round(&mut ctx, phase_diagnostics.as_deref(), None).await
        };

        let envelope = match envelope {
            Ok(env) => env,
            Err(SystemError::Cancelled) => {
                break (None, StructuredRefineStatus::Cancelled);
            }
            Err(SystemError::Other(msg)) => {
                tracing::warn!(
                    workflow = WORKFLOW_NAME,
                    model = %tier.model,
                    error = %msg,
                    "architect produced no actionable payload"
                );
                break (None, StructuredRefineStatus::Blocked { reason: msg });
            }
            Err(err) => {
                return Err(anyhow::anyhow!(err));
            }
        };

        let parsed = parse_round(&envelope.raw_text);
        let (output, round_status) = match parsed {
            Ok(parsed) => {
                attempts_used = 0;
                phase_diagnostics = None;
                parsed
            }
            Err(err) => {
                attempts_used += 1;
                let feedback = schema_healing_feedback(&err, attempts_used, tier_attempts);
                let banner = format!(
                    "\n\n---\n⚠️ **[Schema Healing {}/{}]** {}\n\n",
                    attempts_used, tier_attempts, err
                );
                let _ = stream_tx.send(StreamEvent::TextDelta(banner)).await;
                phase_diagnostics = Some(feedback);

                if attempts_used < tier_attempts {
                    tracing::warn!(
                        workflow = WORKFLOW_NAME,
                        round = rounds_used + 1,
                        attempt = attempts_used,
                        max_attempts = tier_attempts,
                        model = %tier.model,
                        error = %err,
                        "architect payload violated the response schema; regenerating the same tier"
                    );
                    continue;
                }

                let next = config.models.next_architect_tier(Some(&tier));
                if next == tier {
                    tracing::error!(
                        workflow = WORKFLOW_NAME,
                        attempts = attempts_used,
                        max_attempts = tier_attempts,
                        model = %tier.model,
                        error = %err,
                        "architect ladder is saturated without a payload the schema accepts"
                    );
                    break (
                        None,
                        StructuredRefineStatus::Blocked {
                            reason: format!(
                                "Failed to parse the architect refinement output after {} attempt(s) on {}: {}",
                                attempts_used,
                                format_tier(&tier),
                                err
                            ),
                        },
                    );
                }

                let promoted_from = format_tier(&tier);
                let tier_index = config
                    .models
                    .architect_tier_index(Some(&next))
                    .unwrap_or_default();
                tracing::warn!(
                    workflow = WORKFLOW_NAME,
                    round = rounds_used + 1,
                    attempts = attempts_used,
                    max_attempts = tier_attempts,
                    from_model = %tier.model,
                    to_model = %next.model,
                    error = %err,
                    "architect tier exhausted its attempts; escalating the architect ladder"
                );
                let _ = stream_tx
                    .send(StreamEvent::TierEscalated {
                        tier: tier_index,
                        model: next.model.clone(),
                        role: ModelRole::Architect,
                        effort: next.effort,
                    })
                    .await;
                if next.model.provider != tier.model.provider {
                    provider = config.providers.resolve(next.model.provider).await?;
                }
                if next.model != tier.model {
                    options.session_id = base_session
                        .as_ref()
                        .map(|session| format_session_id(session, &format!("tier{tier_index}")));
                }
                options.reasoning_effort = next.effort.or(base_effort);
                tier = next;
                tier_attempts = tier.attempts() as usize;
                attempts_used = 0;

                let banner = format!(
                    "\n\n> Architect level up: {} → {}\n\n",
                    promoted_from,
                    format_tier(&tier)
                );
                let _ = stream_tx.send(StreamEvent::TextDelta(banner)).await;
                continue;
            }
        };

        match round_status {
            RoundStatus::Success => break (Some(output), StructuredRefineStatus::Synthesized),
            RoundStatus::Blocked => {
                let reason = blocked_reason(&output);
                break (Some(output), StructuredRefineStatus::Blocked { reason });
            }
            RoundStatus::Aborted => {
                let report = abort_report(&output);
                tracing::error!(
                    workflow = WORKFLOW_NAME,
                    model = %tier.model,
                    round = rounds_used + 1,
                    reason = %report.reason,
                    "architect synthesis terminated by an abort outcome"
                );
                let notice = format!(
                    "\n\n---\n⛔ **[Execution Aborted]**\n\n{}\n\n",
                    report.to_message()
                );
                let _ = stream_tx.send(StreamEvent::TextDelta(notice)).await;
                break (Some(output), StructuredRefineStatus::Aborted(report));
            }
            RoundStatus::PlanUpdateRefused(reason) => {
                tracing::error!(
                    workflow = WORKFLOW_NAME,
                    model = %tier.model,
                    round = rounds_used + 1,
                    reason = %reason,
                    "architect synthesis refused a payload that mutates the plan"
                );
                let notice = format!("\n\n---\n⛔ **[Plan Mutation Refused]**\n\n{reason}\n\n");
                let _ = stream_tx.send(StreamEvent::TextDelta(notice)).await;
                break (Some(output), StructuredRefineStatus::Blocked { reason });
            }
            RoundStatus::NeedsContext => {
                if rounds_used >= options.max_research_rounds {
                    break (
                        Some(output),
                        StructuredRefineStatus::Blocked {
                            reason: research_limit_reason(options.max_research_rounds),
                        },
                    );
                }

                let outcome = execute_research_commands(
                    context_manager,
                    repo_root,
                    envelope.commands(),
                    options.token_budget,
                    options.repomap_token_budget,
                    options.research_limits,
                    true,
                    Some(&cancel_rx),
                )
                .await;

                if let Some(limit) = outcome.limit_exceeded.as_ref() {
                    let diagnostic = budget_diagnostic(limit, config.budget_hint);
                    tracing::warn!(
                        workflow = WORKFLOW_NAME,
                        token_budget = limit.token_budget,
                        requested_tokens = limit.requested_tokens,
                        current_tokens = limit.current_tokens,
                        "context files token budget exceeded"
                    );
                    let banner = format!("\n\n---\n⚠️ {diagnostic}\n\n");
                    let _ = stream_tx.send(StreamEvent::TextDelta(banner)).await;
                    break (
                        Some(output),
                        StructuredRefineStatus::BudgetExceeded { diagnostic },
                    );
                }

                rounds_used += 1;
                let _ = stream_tx
                    .send(StreamEvent::TurnPhase {
                        phase: TurnPhase::Research,
                        round: Some(rounds_used),
                        max_rounds: Some(options.max_research_rounds),
                        detail: Some("Research round completed".to_string()),
                    })
                    .await;
                if !outcome.has_progress(true) {
                    journal.push_block(BARREN_ROUND_DIRECTIVE);
                }
                stream_round_banner(&stream_tx, rounds_used, &outcome).await;
                journal.record_outcome(&outcome);
                continue;
            }
        }
    };

    drop(internal_tx);
    drop(event_sink);
    let _ = forward_task.await;

    crate::blocking::run_blocking(|| {
        context_manager.clear_transient();
    });
    let _ = stream_tx.send(StreamEvent::Done).await;

    Ok(StructuredRefineOutcome {
        output,
        tier,
        status,
    })
}

fn parse_round(raw_text: &str) -> Result<(PlanRefinementOutput, RoundStatus), String> {
    let output = parse_plan_refinement_output(raw_text).map_err(|err| err.to_string())?;
    if declares_abort(&output) {
        return Ok((output, RoundStatus::Aborted));
    }
    if let Some(reason) = unsupported_plan_update_reason(raw_text) {
        return Ok((output, RoundStatus::PlanUpdateRefused(reason)));
    }
    let status = match output.status.trim().to_ascii_lowercase().as_str() {
        "success" => RoundStatus::Success,
        "blocked" => RoundStatus::Blocked,
        "needs_context" => RoundStatus::NeedsContext,
        other => return Err(format!("Unsupported plan refinement status '{other}'")),
    };
    Ok((output, status))
}

fn declares_abort(output: &PlanRefinementOutput) -> bool {
    output.abort.as_ref().is_some_and(|abort| abort.is_aborted)
}

fn abort_report(output: &PlanRefinementOutput) -> AbortReport {
    let abort = output.abort.as_ref();
    let reason = abort
        .map(|abort| abort.reason.trim())
        .filter(|reason| !reason.is_empty())
        .unwrap_or("Execution aborted");
    let recommendation = abort
        .map(|abort| abort.recommendation.trim())
        .filter(|recommendation| !recommendation.is_empty())
        .unwrap_or("No recommendation was stated.");
    AbortReport {
        reason: reason.to_string(),
        recommendation: recommendation.to_string(),
    }
}

fn schema_healing_feedback(err: &str, attempt: usize, budget: usize) -> String {
    format!(
        "<response_directive>\nYour previous reply did not satisfy the response schema ({err}). Regenerate the whole payload strictly according to the specified JSON schema: `status`, `explanation`, `reason`, `context_requests`, `repo_searches`, `repo_maps`, and `items` are all required. This was attempt {attempt} of {budget} on the running architect configuration.\n</response_directive>"
    )
}

fn format_tier(tier: &ArchitectTier) -> String {
    match tier.effort {
        Some(effort) => format!("{} ({effort})", tier.model),
        None => tier.model.to_string(),
    }
}

fn blocked_reason(output: &PlanRefinementOutput) -> String {
    output
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|reason| !reason.is_empty())
        .map(str::to_string)
        .or_else(|| {
            let explanation = output.explanation.trim();
            (!explanation.is_empty()).then(|| explanation.to_string())
        })
        .unwrap_or_else(|| "The Architect reported a blocker without stating a reason.".to_string())
}

fn research_limit_reason(limit: usize) -> String {
    format!(
        "The Architect still required repository material after {limit} research round(s), and no further files, searches, contours, or documentation can be loaded in this turn."
    )
}

fn budget_diagnostic(limit: &ContextLimitExceeded, hint: &str) -> String {
    let mut diagnostic = format!(
        "Context token budget exceeded (token_budget = {}). The Architect requested {} token(s) in {} new file(s) [{}], but the context already holds {} token(s).",
        limit.token_budget,
        limit.requested_tokens,
        limit.requested_files.len(),
        limit.requested_files.join(", "),
        limit.current_tokens
    );
    let hint = hint.trim();
    if !hint.is_empty() {
        diagnostic.push(' ');
        diagnostic.push_str(hint);
    }
    diagnostic
}

async fn stream_round_banner(
    stream_tx: &mpsc::Sender<StreamEvent>,
    round: usize,
    outcome: &ResearchRoundOutcome,
) {
    let summary = outcome.architect_banner_summary();
    let summary = summary.trim();
    let body = if summary.is_empty() {
        "The requested material produced no new information."
    } else {
        summary
    };
    let banner = format!("\n\n---\n**[Round {}]** {}\n\n", round, body);
    let _ = stream_tx.send(StreamEvent::TextDelta(banner)).await;
}
