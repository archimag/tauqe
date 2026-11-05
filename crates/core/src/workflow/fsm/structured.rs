//! Structured Architect synthesis contour: Schema Healing and tier escalation.
//!
//! Horizon synthesis and review-group decomposition are one Architect operation,
//! so both drive this contour: the universal turn kernel carries the rounds, the
//! prompt assembly, the research journal, and the corrective budget of an empty
//! answer, while the contour owns the policy that is specific to a structured
//! Architect payload.
//!
//! The regime is declared, never inferred: the kernel recognizes the turn through
//! [`TurnIntent::ArchitectSynthesis`], and the wire surface is the typed
//! [`PlanRefinePromptProtocol`], so the rendered instructions, the response
//! schema, and the decoder observe one capability surface.
//!
//! Four terminal policies live here:
//!
//! - *Silence* is a transport-level non-answer, so the kernel's own budget
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
use tauqe_protocol::{ArchitectTier, ModelRole, ProviderKind, TurnIntent, TurnPhase};
use tokio::sync::{mpsc, watch};

use crate::config::ModelsConfig;
use crate::context::ContextManager;
use crate::history::HistoryManager;
use crate::plan::prompt::{parse_plan_refinement_output, PlanRefinementOutput};
use crate::plan::storage::PlanMutationMode;
use crate::prompt::refine::{unsupported_plan_update_reason, PlanRefinePromptProtocol};
use crate::providers::{format_session_id, LlmProvider, StreamEvent};
use crate::workflow::policy::DirectApplyPolicy;
use crate::workflow::research::journal::TurnResearchJournal;
use crate::workflow::research::{
    execute_research_commands, ContextLimitExceeded, ResearchRoundOutcome,
};
use crate::workflow::WorkflowOptions;

use super::cache::TurnInputCache;
use super::kernel::{EmptyStepGuard, EmptyStepRetry, TurnKernel};
use super::proposal::{log_fsm_phase, ProposalStep, TurnState};
use super::{AbortReport, FsmTurnOutcome};

/// Operation label recorded for every round of a synthesis run.
const WORKFLOW_NAME: &str = "architect";

/// Directive appended to the turn journal when a research round produced nothing.
///
/// A barren round is neither progress nor a dead end: the harness answered every
/// request with nothing, so the round is still charged and the model receives an
/// explicit answer contract instead of an exhausted-budget verdict.
const BARREN_ROUND_DIRECTIVE: &str = "<response_directive>\nThe previous discovery round produced no new information: no requested file was added to context, nothing is missing, and no search or targeted contour returned a result. Finalize the horizon now with status 'success', report status 'blocked' with the exact reason, or request concrete paths that exist in <repo_map>.\n</response_directive>";

/// Directive appended to the turn journal once the research budget is spent.
const RESEARCH_LIMIT_DIRECTIVE: &str = "<response_directive>\nThe research round limit is exhausted: no further files, repository searches, symbol contours, or documentation will be loaded in this turn. Your investigation is complete. Do not request additional paths. Finalize the horizon with status 'success', or report status 'blocked' with the exact reason when the material you already hold is insufficient.\n</response_directive>";

/// Typed resolver of the provider client that serves another provider.
///
/// A provider client is bound to one provider kind, so a rung served by another
/// provider needs a freshly resolved client instead of a reused one. The
/// injection keeps the engine free of provider construction policy: the adapter
/// that owns credentials and transport configuration is the only place that
/// knows how a client is built.
#[async_trait]
pub trait ProviderResolver: Send + Sync {
    /// Resolves the client of `kind`.
    ///
    /// The hook is asynchronous because it is consulted from inside a proposal
    /// round: the adapter's resolution future is awaited there, so the seam
    /// imposes no runtime-flavor requirement on the host that polls the contour.
    /// The `Send + Sync` bound lets a contour carry the resolver into the task
    /// that drives the synthesis, exactly like the client it returns.
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
    ///
    /// An abort is not a blocker: it states that no plan can complete the work
    /// in the current environment, so the report carries the exact command the
    /// developer must run. The variant wraps the very [`AbortReport`] the edit
    /// contour publishes, so both contours name the same two fields.
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
    ///
    /// The typed refusal the refinement contour states instead of decoding a
    /// horizon that silently dropped the mutation.
    PlanUpdateRefused(String),
    /// The Architect named the evidence it still needs.
    NeedsContext,
}

/// Result of one proposal round after the kernel's corrective budget ran.
enum RoundOutcome {
    /// The round produced a proposal to interpret.
    Proposed(Box<ProposalStep>),
    /// The kernel's corrective budget is spent: the model stayed silent.
    Silent,
    /// The turn terminated inside the kernel.
    Terminated(Box<FsmTurnOutcome>),
}

/// Runs one structured Architect synthesis on the universal turn kernel.
///
/// `task` is the complete task statement of the run: the caller composes it from
/// the domain framing, the architectural concept, the completed foundation, and
/// the rendered repository outline. The contour composes every later round from
/// that statement and the accumulated discovery journal, so the provider keeps
/// matching the prompt prefix it already cached.
pub async fn run_structured_refine(
    context_manager: &mut ContextManager,
    provider: Arc<dyn LlmProvider>,
    task: &str,
    config: StructuredRefineConfig<'_>,
    stream_tx: mpsc::Sender<StreamEvent>,
    cancel_rx: watch::Receiver<bool>,
) -> anyhow::Result<StructuredRefineOutcome> {
    let repo_root = config.repo_root;
    let protocol = PlanRefinePromptProtocol::new();
    // The synthesis renders no session history, so this manager is never read; it
    // exists because the shared round executor takes the dialogue it would
    // otherwise freeze for the turn.
    let history_manager = HistoryManager::new(repo_root.to_path_buf());

    let mut policy = DirectApplyPolicy::new();
    let mut input_cache = TurnInputCache::default();
    let mut state = TurnState::default();
    let mut journal = TurnResearchJournal::default();
    let mut accumulated_text = String::new();
    let mut rounds_used = 0usize;
    let mut empty_steps = EmptyStepGuard::default();

    let base_effort = config.options.reasoning_effort;
    let base_session = config.options.session_id.clone();
    let mut options = config.options.clone();
    let mut provider = provider;
    let mut tier = config.tier.clone();
    options.reasoning_effort = tier.effort.or(base_effort);
    let mut tier_attempts = tier.attempts() as usize;
    let mut attempts_used = 0usize;
    let mut phase_diagnostics: Option<String> = None;

    // The opening header reaches the client once, as the first event of the run.
    if !config.intro_header.trim().is_empty() {
        let _ = stream_tx
            .send(StreamEvent::TextDelta(config.intro_header.to_string()))
            .await;
    }

    let (output, status) = loop {
        // A cancel raised during research, while a reference document streamed,
        // or between two provider requests ends the synthesis before another
        // round of it is spent.
        if *cancel_rx.borrow() {
            break (None, StructuredRefineStatus::Cancelled);
        }

        // The closing directive is what a model that still needs material reads:
        // it turns an exhausted budget into an explicit answer contract instead of
        // leaving the request unanswered.
        if rounds_used >= options.max_research_rounds {
            journal.push_block(RESEARCH_LIMIT_DIRECTIVE);
        }

        let round_prompt = journal.compose_user_prompt(task, phase_diagnostics.as_deref());
        log_fsm_phase(
            WORKFLOW_NAME,
            &tier.model,
            false,
            TurnPhase::Proposal,
            Some(rounds_used + 1),
            None,
        );
        let _ = stream_tx
            .send(StreamEvent::TurnPhase {
                phase: TurnPhase::Proposal,
                round: Some(rounds_used + 1),
                max_rounds: None,
                detail: None,
            })
            .await;

        // The kernel is re-bound for every round: a promoted rung changes the
        // effective model, its reasoning effort, and — when the provider changes —
        // the client itself. The turn-scoped state travels across bindings, so the
        // journal, the round budget, and the corrective budget of an empty answer
        // survive the promotion.
        let round = {
            let mut kernel = TurnKernel {
                prompt: task,
                provider: &*provider,
                model: &tier.model,
                protocol: &protocol,
                history_manager: &history_manager,
                workflow_name: WORKFLOW_NAME,
                stream_tx: &stream_tx,
                cancel_rx: &cancel_rx,
                options: &options,
                repo_root,
                intent: TurnIntent::ArchitectSynthesis,
                plan_mode: PlanMutationMode::Discussion,
                policy: &mut policy,
                context_manager,
                input_cache: &mut input_cache,
                state,
                journal,
                rounds_used,
                accumulated_text,
                empty_steps,
            };
            let round = run_round(&mut kernel, &round_prompt).await;
            state = kernel.state;
            journal = kernel.journal;
            rounds_used = kernel.rounds_used;
            accumulated_text = kernel.accumulated_text;
            empty_steps = kernel.empty_steps;
            round?
        };

        let step = match round {
            RoundOutcome::Proposed(step) => *step,
            RoundOutcome::Silent => {
                let attempts = empty_steps.budget();
                tracing::warn!(
                    workflow = WORKFLOW_NAME,
                    model = %tier.model,
                    attempts,
                    "architect produced no payload after every corrective round"
                );
                break (
                    None,
                    StructuredRefineStatus::Blocked {
                        reason: format!(
                            "The model returned an empty response after {attempts} additional attempt(s) and produced no horizon payload."
                        ),
                    },
                );
            }
            RoundOutcome::Terminated(turn) => {
                let turn = *turn;
                if turn.is_cancelled {
                    break (None, StructuredRefineStatus::Cancelled);
                }
                break (
                    None,
                    StructuredRefineStatus::Blocked {
                        reason: turn.blocked.unwrap_or_else(|| {
                            "The synthesis terminated without a horizon payload.".to_string()
                        }),
                    },
                );
            }
        };

        let parsed = parse_round(&step);
        let (output, round_status) = match parsed {
            Ok(parsed) => {
                // A parseable round is progress, so the running rung regains its
                // whole attempt budget: a `needs_context` answer must never count
                // as a failure.
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

                // The rung spent its whole generation budget. The ladder either
                // hands out the next configuration or stays where it is, which
                // means the synthesis cannot climb any further.
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
                // A client is bound to one provider kind, so a rung served by
                // another provider needs a freshly resolved client instead of the
                // current one.
                if next.model.provider != tier.model.provider {
                    provider = config.providers.resolve(next.model.provider).await?;
                }
                // A promoted model addresses a backend node whose prefix cache was
                // warmed by that very model, so the session identifier rotates with
                // it (crates/core/Conventions.md §9.9).
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
                // An abort supersedes every other reading of the payload, so it
                // is resolved before the horizon is inspected: the report names
                // the host action the developer must perform, the developer sees
                // that exact command while the run settles, and the turn ends on
                // the typed verdict without spending an attempt of the rung.
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
                // The round asked for a mutation this contour cannot perform, so
                // the verdict is a typed refusal: the developer sees the exact
                // contract the payload violated while the run settles, and the
                // round spends no generation attempt of the rung.
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
                    // The budget is spent while the Architect still needs the
                    // material it names: a typed block, never a payload that
                    // silently claims to be a horizon.
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
                    step.commands(),
                    options.token_budget,
                    options.repomap_token_budget,
                    options.research_limits,
                    true,
                    // The round races the turn's own cancel signal, so an
                    // abandoned request stops waiting for a slow reference host.
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

                // Every `needs_context` answer is charged as one investigation
                // round: it is a provider round trip that asked for material,
                // whether or not it widened anything. A round that produced
                // nothing is therefore never mistaken for an exhausted budget.
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
                // The findings join the turn journal, so a later round still reads
                // the search reports and contours this one paid for.
                journal.record_outcome(&outcome);
                continue;
            }
        }
    };

    // The synthesis owns the transient context it widened: the files its
    // research rounds discovered and the external documents they materialized
    // are the Architect's investigation, not the developer's dialogue, so the
    // one primitive that owns that lifetime releases both layers together
    // (crates/core/Conventions.md §12). A release drops the context entries and
    // never deletes a materialized snapshot, which stays admissible from the
    // cache without another transfer. Recomputing pinned files reads the disk,
    // so the sweep runs off the async executor thread.
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

/// Executes one proposal round and resolves the kernel's corrective budget.
///
/// Silence is a transport-level non-answer, so the kernel's own budget resolves
/// it: the model tier is never charged for a reply that carried nothing
/// actionable, which is what keeps an empty generation from consuming an attempt
/// of the running architect rung.
async fn run_round(
    kernel: &mut TurnKernel<'_>,
    round_prompt: &str,
) -> anyhow::Result<RoundOutcome> {
    let mut step = kernel.propose(round_prompt, None).await?;
    loop {
        if step.is_cancelled {
            let accumulated = std::mem::take(&mut kernel.accumulated_text);
            return Ok(RoundOutcome::Terminated(Box::new(
                FsmTurnOutcome::cancelled(accumulated),
            )));
        }
        if !step.is_empty_step() {
            return Ok(RoundOutcome::Proposed(Box::new(step)));
        }
        match kernel.reask_after_empty_round().await? {
            EmptyStepRetry::Proposed(next) => step = *next,
            EmptyStepRetry::Exhausted => return Ok(RoundOutcome::Silent),
            EmptyStepRetry::Terminate(outcome) => {
                return Ok(RoundOutcome::Terminated(outcome));
            }
        }
    }
}

/// Decodes the typed payload of one round.
///
/// The single deserializer of the contour owns both the payload shape and the
/// accepted status vocabulary, so a payload that violates either is one schema
/// violation and receives the same healing treatment.
fn parse_round(step: &ProposalStep) -> Result<(PlanRefinementOutput, RoundStatus), String> {
    let output = parse_plan_refinement_output(step.raw_text()).map_err(|err| err.to_string())?;
    // An active abort supersedes the declared status: the payload asserts that no
    // plan can complete the work, so a `success` carrying items is void and an
    // unrecognized status is a verdict rather than a schema violation to heal.
    if declares_abort(&output) {
        return Ok((output, RoundStatus::Aborted));
    }
    // The synthesis contour owns the horizon alone: a payload that mutates the
    // plan is refused as a typed verdict instead of being decoded into a horizon
    // that silently dropped the mutation.
    if let Some(reason) = unsupported_plan_update_reason(step.raw_text()) {
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

/// Reports whether the payload declares an active host-environment abort.
fn declares_abort(output: &PlanRefinementOutput) -> bool {
    output.abort.as_ref().is_some_and(|abort| abort.is_aborted)
}

/// Reads the abort report of a payload, with the edit contour's defaults.
///
/// The two contours publish one escape hatch, so a blank reason or
/// recommendation resolves to the very default the structured edit contour
/// substitutes instead of ending the synthesis on an empty payload.
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

/// Renders the model-facing diagnostic of a schema violation.
fn schema_healing_feedback(err: &str, attempt: usize, budget: usize) -> String {
    format!(
        "<response_directive>\nYour previous reply did not satisfy the response schema ({err}). Regenerate the whole payload strictly according to the specified JSON schema: `status`, `explanation`, `reason`, `context_requests`, `repo_searches`, `repo_maps`, and `items` are all required. This was attempt {attempt} of {budget} on the running architect configuration.\n</response_directive>"
    )
}

/// Formats an architect rung for a developer-facing message.
fn format_tier(tier: &ArchitectTier) -> String {
    match tier.effort {
        Some(effort) => format!("{} ({effort})", tier.model),
        None => tier.model.to_string(),
    }
}

/// Reads the reason an Architect payload states for its blocker.
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

/// Renders the typed reason of a synthesis that ran out of research rounds.
fn research_limit_reason(limit: usize) -> String {
    format!(
        "The Architect still required repository material after {limit} research round(s), and no further files, searches, contours, or documentation can be loaded in this turn."
    )
}

/// Renders the typed report of a breached context budget.
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

/// Streams the developer-facing summary of one discovery round.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::mock::{MockLlmProvider, MockResponse};
    use tauqe_protocol::{ContextAccess, ContextLayer, ModelRef, ReasoningEffort};

    /// Creates a repository the synthesis can inspect.
    fn repo() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        crate::git::init_repository(Some(&root), true).unwrap();
        std::fs::write(root.join("a.rs"), "pub fn alpha() {}\n").unwrap();
        crate::git::cmd::stage_files(&root, &["a.rs".to_string()]).unwrap();
        crate::git::cmd::commit_with_developer_author(&root, "Add fixture", false).unwrap();
        (dir, root)
    }

    /// Resolver a single-provider synthesis must never consult.
    struct NoResolver;

    #[async_trait]
    impl ProviderResolver for NoResolver {
        async fn resolve(&self, kind: ProviderKind) -> anyhow::Result<Arc<dyn LlmProvider>> {
            anyhow::bail!("a single-provider synthesis must not resolve {kind}")
        }
    }

    /// Builds a models section declaring the given architect ladder.
    fn models(architect: &str) -> ModelsConfig {
        let declaration =
            format!("[models]\nprimary = [\"openrouter:mock/base\"]\narchitect = [{architect}]\n");
        let config: crate::config::AppConfig =
            toml::from_str(&declaration).expect("parse the architect ladder");
        config.models
    }

    /// Builds one architect payload with the given status and explanation.
    fn payload(status: &str, explanation: &str) -> String {
        serde_json::json!({
            "status": status,
            "explanation": explanation,
            "reason": null,
            "context_requests": [],
            "repo_searches": [],
            "repo_maps": [],
            "items": []
        })
        .to_string()
    }

    /// A barren `needs_context` round widens nothing, mentions nothing missing,
    /// and returns no search or contour result. It must be charged as one
    /// investigation round and answered with a corrective directive instead of
    /// ending the synthesis with a false exhausted-budget verdict.
    #[tokio::test]
    async fn a_barren_needs_context_round_is_charged_and_directed() {
        let (_dir, root) = repo();
        let models = models("");
        let provider = Arc::new(MockLlmProvider::scripted([
            MockResponse::text(payload("needs_context", "The contract is not verified yet")),
            MockResponse::text(payload(
                "success",
                "Horizon synthesized from the held material",
            )),
        ]));
        let client: Arc<dyn LlmProvider> = provider.clone();
        let mut context = ContextManager::new(root.clone());
        let options = WorkflowOptions::default();
        let (stream_tx, mut stream_rx) = mpsc::channel(256);
        let collector = tokio::spawn(async move {
            let mut events = Vec::new();
            while let Some(event) = stream_rx.recv().await {
                events.push(event);
            }
            events
        });
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let outcome = run_structured_refine(
            &mut context,
            client,
            "Synthesize the unfinished horizon",
            StructuredRefineConfig {
                repo_root: &root,
                options: &options,
                models: &models,
                tier: models.architect_tier(0),
                providers: &NoResolver,
                intro_header: "",
                budget_hint: "",
            },
            stream_tx,
            cancel_rx,
        )
        .await
        .expect("the synthesis runs to completion");

        assert_eq!(outcome.status, StructuredRefineStatus::Synthesized);
        assert!(outcome.output.is_some());
        assert_eq!(
            provider.requests().len(),
            2,
            "the barren round is charged as one investigation round"
        );

        let second = &provider.requests()[1];
        assert!(
            second
                .messages
                .iter()
                .any(|message| message.content.contains("produced no new information")),
            "the barren round must be answered with a corrective directive"
        );
        assert!(
            second
                .messages
                .iter()
                .any(|message| message.content.contains("status 'success'")),
            "the directive must name the final statuses"
        );
        assert!(
            second
                .messages
                .iter()
                .any(|message| message.content.contains("<repo_map>")),
            "the directive must route a further request to the rendered outline"
        );

        drop(_cancel_tx);
        let events = collector.await.expect("the collector joins");
        assert!(events
            .iter()
            .any(|event| matches!(event, StreamEvent::Done)));
    }

    /// An unparsable payload spends one attempt of the running rung; an exhausted
    /// rung climbs the architect ladder with the promoted rung's reasoning effort.
    #[tokio::test]
    async fn an_exhausted_rung_climbs_the_architect_ladder() {
        let (_dir, root) = repo();
        let models = models(
            "{ model = \"openrouter:mock/base\", attempts = 1 }, \
             { model = \"openrouter:mock/strong\", attempts = 1, effort = \"high\" }",
        );
        let provider = Arc::new(MockLlmProvider::scripted([
            MockResponse::text("not a refinement payload"),
            MockResponse::text(payload("success", "Horizon synthesized on the strong rung")),
        ]));
        let client: Arc<dyn LlmProvider> = provider.clone();
        let mut context = ContextManager::new(root.clone());
        let options = WorkflowOptions::default();
        let (stream_tx, mut stream_rx) = mpsc::channel(256);
        let collector = tokio::spawn(async move {
            let mut events = Vec::new();
            while let Some(event) = stream_rx.recv().await {
                events.push(event);
            }
            events
        });
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let outcome = run_structured_refine(
            &mut context,
            client,
            "Synthesize the unfinished horizon",
            StructuredRefineConfig {
                repo_root: &root,
                options: &options,
                models: &models,
                tier: models.architect_tier(0),
                providers: &NoResolver,
                intro_header: "",
                budget_hint: "",
            },
            stream_tx,
            cancel_rx,
        )
        .await
        .expect("the synthesis runs to completion");

        assert_eq!(outcome.status, StructuredRefineStatus::Synthesized);
        assert_eq!(outcome.tier.model, ModelRef::openrouter("mock/strong"));
        assert_eq!(provider.requests().len(), 2, "one attempt per rung");
        assert_eq!(
            provider.requests()[1].reasoning_effort,
            Some(ReasoningEffort::High),
            "the promoted rung applies its own reasoning effort"
        );

        drop(_cancel_tx);
        let events = collector.await.expect("the collector joins");
        assert!(
            events.iter().any(|event| matches!(
                event,
                StreamEvent::TierEscalated { model, effort, .. }
                    if *model == ModelRef::openrouter("mock/strong")
                        && *effort == Some(ReasoningEffort::High)
            )),
            "the promotion must travel as a semantic event"
        );
    }

    /// Builds one architect payload that declares a host-environment abort.
    fn abort_payload(status: &str, items: serde_json::Value) -> String {
        serde_json::json!({
            "status": status,
            "explanation": "The host environment must be repaired before the horizon exists",
            "reason": null,
            "abort": {
                "is_aborted": true,
                "reason": "  native build approval required  ",
                "recommendation": "  pnpm approve-builds  "
            },
            "context_requests": [],
            "repo_searches": [],
            "repo_maps": [],
            "items": items
        })
        .to_string()
    }

    /// An abort declared beside a `blocked` verdict ends the synthesis on the
    /// typed status: the rung keeps its generation budget, the ladder is never
    /// consulted, and the exact host action reaches the developer live.
    #[tokio::test]
    async fn an_abort_declared_beside_a_blocked_verdict_ends_the_synthesis() {
        let (_dir, root) = repo();
        let models = models(
            "{ model = \"openrouter:mock/base\", attempts = 1 }, \
             { model = \"openrouter:mock/strong\", attempts = 1 }",
        );
        let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
            abort_payload("blocked", serde_json::json!([])),
        )]));
        let client: Arc<dyn LlmProvider> = provider.clone();
        let mut context = ContextManager::new(root.clone());
        let options = WorkflowOptions::default();
        let (stream_tx, mut stream_rx) = mpsc::channel(256);
        let collector = tokio::spawn(async move {
            let mut events = Vec::new();
            while let Some(event) = stream_rx.recv().await {
                events.push(event);
            }
            events
        });
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let outcome = run_structured_refine(
            &mut context,
            client,
            "Synthesize the unfinished horizon",
            StructuredRefineConfig {
                repo_root: &root,
                options: &options,
                models: &models,
                tier: models.architect_tier(0),
                providers: &NoResolver,
                intro_header: "",
                budget_hint: "",
            },
            stream_tx,
            cancel_rx,
        )
        .await
        .expect("the synthesis runs to completion");

        assert_eq!(
            outcome.status,
            StructuredRefineStatus::Aborted(AbortReport {
                reason: "native build approval required".to_string(),
                recommendation: "pnpm approve-builds".to_string(),
            })
        );
        // The abort consumes no generation attempt of the running rung, so the
        // stronger rung of the ladder is never entered.
        assert_eq!(provider.requests().len(), 1);
        assert_eq!(outcome.tier.model, ModelRef::openrouter("mock/base"));

        drop(_cancel_tx);
        let events = collector.await.expect("the collector joins");
        assert!(
            events.iter().any(|event| matches!(
                event,
                StreamEvent::TextDelta(delta)
                    if delta.contains("[Execution Aborted]")
                        && delta.contains("pnpm approve-builds")
            )),
            "the abort banner must reach the developer with the exact host action"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, StreamEvent::TierEscalated { .. })),
            "an abort must never escalate the ladder"
        );
        assert!(events
            .iter()
            .any(|event| matches!(event, StreamEvent::Done)));
    }

    /// An abort supersedes a declared `success`: the horizon that arrived with
    /// the verdict is never treated as a result of the synthesis.
    #[tokio::test]
    async fn an_abort_supersedes_a_declared_success_horizon() {
        let (_dir, root) = repo();
        let models = models(
            "{ model = \"openrouter:mock/base\", attempts = 1 }, \
             { model = \"openrouter:mock/strong\", attempts = 1 }",
        );
        let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
            abort_payload(
                "success",
                serde_json::json!([{
                    "id": "1",
                    "title": "Step 1",
                    "status": "todo",
                    "files": ["src/step-1.rs"],
                    "readonly_files": [],
                    "concept_refs": [],
                    "depends_on": [],
                    "children": []
                }]),
            ),
        )]));
        let client: Arc<dyn LlmProvider> = provider.clone();
        let mut context = ContextManager::new(root.clone());
        let options = WorkflowOptions::default();
        let (stream_tx, mut stream_rx) = mpsc::channel(256);
        let collector = tokio::spawn(async move { while stream_rx.recv().await.is_some() {} });
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let outcome = run_structured_refine(
            &mut context,
            client,
            "Synthesize the unfinished horizon",
            StructuredRefineConfig {
                repo_root: &root,
                options: &options,
                models: &models,
                tier: models.architect_tier(0),
                providers: &NoResolver,
                intro_header: "",
                budget_hint: "",
            },
            stream_tx,
            cancel_rx,
        )
        .await
        .expect("the synthesis runs to completion");

        match &outcome.status {
            StructuredRefineStatus::Aborted(report) => {
                assert_eq!(report.reason, "native build approval required");
                assert_eq!(report.recommendation, "pnpm approve-builds");
            }
            other => panic!("expected a typed abort, got {other:?}"),
        }
        let output = outcome
            .output
            .expect("the payload travels with the verdict");
        assert_eq!(
            output.items.len(),
            1,
            "the abort supersedes the horizon the payload carried"
        );
        assert_eq!(provider.requests().len(), 1);

        drop(_cancel_tx);
        collector.await.expect("the collector joins");
    }

    /// A synthesis round that emits a plan mutation is refused as a typed
    /// verdict: the plan is authored by the plan dialogue, so a horizon carrying
    /// `plan_update` would silently drop the mutation the model attempted.
    #[tokio::test]
    async fn a_synthesis_payload_that_mutates_the_plan_is_refused() {
        let (_dir, root) = repo();
        let models = models("");
        let mut mutating: serde_json::Value = serde_json::from_str(&payload(
            "success",
            "Horizon synthesized from the held material",
        ))
        .expect("the fixture is a refinement payload");
        mutating["plan_update"] =
            serde_json::json!({"action": "update", "id": "p-1", "title": "P1"});
        let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
            mutating.to_string(),
        )]));
        let client: Arc<dyn LlmProvider> = provider.clone();
        let mut context = ContextManager::new(root.clone());
        let options = WorkflowOptions::default();
        let (stream_tx, mut stream_rx) = mpsc::channel(256);
        let collector = tokio::spawn(async move {
            let mut events = Vec::new();
            while let Some(event) = stream_rx.recv().await {
                events.push(event);
            }
            events
        });
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let outcome = run_structured_refine(
            &mut context,
            client,
            "Synthesize the unfinished horizon",
            StructuredRefineConfig {
                repo_root: &root,
                options: &options,
                models: &models,
                tier: models.architect_tier(0),
                providers: &NoResolver,
                intro_header: "",
                budget_hint: "",
            },
            stream_tx,
            cancel_rx,
        )
        .await
        .expect("the synthesis runs to completion");

        match &outcome.status {
            StructuredRefineStatus::Blocked { reason } => {
                assert!(reason.contains("plan_update"), "{reason}");
            }
            other => panic!("expected a typed refusal, got {other:?}"),
        }
        // The refusal is a verdict of the round, so no second generation of the
        // same payload is requested.
        assert_eq!(provider.requests().len(), 1);

        drop(_cancel_tx);
        let events = collector.await.expect("the collector joins");
        assert!(
            events.iter().any(|event| matches!(
                event,
                StreamEvent::TextDelta(delta) if delta.contains("[Plan Mutation Refused]")
            )),
            "the refusal must reach the developer live"
        );
        assert!(events
            .iter()
            .any(|event| matches!(event, StreamEvent::Done)));
    }

    /// A finished synthesis releases the whole transient context: the files its
    /// research rounds discovered and the external documents it materialized
    /// share one lifetime, so no material of either layer survives the run,
    /// while the cached snapshot outlives the release and a later round admits
    /// it again without a transfer.
    #[tokio::test]
    async fn a_finished_synthesis_releases_the_whole_transient_context() {
        let (_dir, root) = repo();
        let models = models("");
        let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(payload(
            "success",
            "Horizon synthesized from the held material",
        ))]));
        let client: Arc<dyn LlmProvider> = provider.clone();
        let mut context = ContextManager::new(root.clone());

        // A research round widened both transient layers: a discovered
        // repository file and an external document materialized into the
        // isolated cache the registry owns.
        context
            .add_auto_file("a.rs", ContextAccess::ReadOnly)
            .expect("the discovered file enters the auto layer");
        let snapshot = format!("{}/index.md", crate::outer_docs::document_dir("mylib"));
        let snapshot_path = root.join(&snapshot);
        std::fs::create_dir_all(snapshot_path.parent().expect("the snapshot has a parent"))
            .expect("the cache directory is created");
        std::fs::write(&snapshot_path, "# MyLib\n").expect("the snapshot is written");
        context
            .add_outer_docs_file(&snapshot)
            .expect("the materialized snapshot enters the outer docs layer");

        let options = WorkflowOptions::default();
        let (stream_tx, mut stream_rx) = mpsc::channel(256);
        let collector = tokio::spawn(async move { while stream_rx.recv().await.is_some() {} });
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let outcome = run_structured_refine(
            &mut context,
            client,
            "Synthesize the unfinished horizon",
            StructuredRefineConfig {
                repo_root: &root,
                options: &options,
                models: &models,
                tier: models.architect_tier(0),
                providers: &NoResolver,
                intro_header: "",
                budget_hint: "",
            },
            stream_tx,
            cancel_rx,
        )
        .await
        .expect("the synthesis runs to completion");

        assert_eq!(outcome.status, StructuredRefineStatus::Synthesized);
        assert!(
            context.auto_files().is_empty(),
            "the files discovered for the synthesis never survive it"
        );
        assert!(
            context.outer_docs_files().is_empty(),
            "the documents materialized for the synthesis never survive it"
        );
        assert!(
            snapshot_path.is_file(),
            "the release drops the context entry, never the cached snapshot"
        );

        // The snapshot stays admissible: a later round reads the very bytes the
        // cache holds instead of paying for another transfer.
        let readmitted = context
            .add_outer_docs_file(&snapshot)
            .expect("the cached snapshot is admitted again");
        assert_eq!(readmitted.layer, ContextLayer::OuterDocs);
        assert_eq!(readmitted.access, ContextAccess::ReadOnly);
        assert_eq!(readmitted.path, "mylib/index.md");
        assert_eq!(
            context.outer_docs_files(),
            vec!["mylib/index.md".to_string()]
        );

        drop(_cancel_tx);
        collector.await.expect("the collector joins");
    }
}
