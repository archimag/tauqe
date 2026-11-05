//! Universal turn kernel: the round executor shared by every turn contour.
//!
//! A bounded turn interacts with the model exclusively through this kernel. A
//! round is assembled from the active [`PromptProtocol`] and the turn-scoped
//! [`PromptAssembly`] material, streamed through the protocol's
//! [`EditStreamFilter`], and decoded into normalized commands that fold into the
//! turn state and the [`TurnResearchJournal`].
//!
//! The kernel owns the deterministic inputs frozen for the whole turn through
//! [`TurnInputCache`], the shared investigation budget of research and project
//! commands, and the corrective budget of an empty answer, so no phase executor
//! re-derives them. Research commands run through the polymorphic dispatcher of
//! [`crate::workflow::research`]; a round that would breach the files token
//! budget is reported to the caller as a typed signal instead of terminating the
//! turn with hardcoded text.

use std::path::Path;

use tauqe_protocol::{
    ContextAccess, LlmCapability, LlmCommand, ModelRef, ModelResult, ProjectConfig, TurnIntent,
    TurnPhase,
};
use tokio::sync::{mpsc, watch};

use crate::context::{ContextFileContent, ContextManager};
use crate::edits::stream::EditStreamFilter;
use crate::edits::StagedEditsState;
use crate::history::HistoryManager;
use crate::plan::storage::PlanMutationMode;
use crate::prompt::protocol::PromptProtocol;
use crate::prompt::PromptAssembly;
use crate::providers::{LlmProvider, StreamEvent};
use crate::workflow::policy::ApplyPolicy;
use crate::workflow::research::journal::TurnResearchJournal;
use crate::workflow::research::ContextLimitExceeded;
use crate::workflow::WorkflowOptions;

use super::cache::TurnInputCache;
use super::guard::finalize_blocked_turn;
use super::proposal::{
    log_fsm_phase, merge_text, record_review_decisions, ProposalStep, TurnState,
};
use super::FsmTurnOutcome;

/// Universal executor of the model rounds of one bounded turn.
///
/// The kernel is the single point through which a phase executor talks to the
/// model: it composes the round from the frozen turn inputs, streams the
/// response, decodes it into normalized commands, and folds the result into the
/// turn state and the discovery journal. Transactional persistence and
/// verification stay with the phase executors, which own their contour.
pub(super) struct TurnKernel<'a> {
    /// Task statement of the turn, in its original formulation.
    pub(super) prompt: &'a str,
    /// Provider the round is requested from.
    pub(super) provider: &'a dyn LlmProvider,
    /// Effective model of the current tier.
    pub(super) model: &'a ModelRef,
    /// Wire protocol of the turn.
    pub(super) protocol: &'a dyn PromptProtocol,
    /// Session history the turn renders and records into.
    pub(super) history_manager: &'a HistoryManager,
    /// Workflow label used by the observability records.
    pub(super) workflow_name: &'a str,
    /// Stream the filtered round events are forwarded to.
    pub(super) stream_tx: &'a mpsc::Sender<StreamEvent>,
    /// Cancellation signal of the turn.
    pub(super) cancel_rx: &'a watch::Receiver<bool>,
    /// Operational options resolved for the turn.
    pub(super) options: &'a WorkflowOptions,
    /// Repository root the turn operates in.
    pub(super) repo_root: &'a Path,
    /// Declared intent of the turn.
    pub(super) intent: TurnIntent,
    /// Plan mutation mode the turn finalizes its plan payloads with.
    pub(super) plan_mode: PlanMutationMode,
    /// Transactional channel the turn persists validated changes through.
    pub(super) policy: &'a mut dyn ApplyPolicy,
    /// Context of the turn.
    pub(super) context_manager: &'a mut ContextManager,
    /// Deterministic inputs frozen for the whole turn.
    pub(super) input_cache: &'a mut TurnInputCache,
    /// State accumulated across the proposal rounds.
    pub(super) state: TurnState,
    /// Cumulative epistemic tail of the turn.
    pub(super) journal: TurnResearchJournal,
    /// Investigation rounds consumed by the turn so far.
    pub(super) rounds_used: usize,
    /// Conversational text accumulated across every round.
    pub(super) accumulated_text: String,
    /// Corrective budget of an empty answer, owned by the kernel.
    pub(super) empty_steps: EmptyStepGuard,
}

/// Declared external documents one research round resolved.
///
/// A resolved document answers with the cache path the round may now read as
/// an ordinary context request, so the material the model asked for enters
/// the context through the same admission rule as a repository file. Every
/// refusal keeps the classification of its audience, so a trust decision
/// addressed to the developer never merges with a delivery failure addressed
/// to the model.
#[derive(Default)]
struct OuterDocsResolution {
    /// Cache paths that now hold a snapshot the round asked for.
    paths: Vec<String>,
    /// Refusals, each carrying the document that could not be materialized.
    failures: Vec<(String, crate::outer_docs::MaterializeError)>,
}

impl TurnKernel<'_> {
    /// Returns the project configuration frozen at the start of the turn.
    ///
    /// The first proposal round resolves the effective configuration — reading
    /// `tauqe.toml` and, when a task runner is declared, running its dump once —
    /// and the turn cache freezes that result. Every later phase reads the same
    /// snapshot, so a patch retry, a healing round, and a project-command round
    /// never re-read the declaration or re-run the runner.
    pub(super) async fn project(&mut self) -> ProjectConfig {
        self.input_cache
            .project(self.repo_root, Some(self.cancel_rx))
            .await
    }

    /// Reports whether the step's cancel signal is already raised.
    ///
    /// Phase boundaries that are about to produce work read it, so a cancellation
    /// observed inside a phase ends the step before the next phase starts instead
    /// of only interrupting the provider stream.
    pub(super) fn cancel_requested(&self) -> bool {
        *self.cancel_rx.borrow()
    }

    /// Spends one shared investigation round and reports its ordinal.
    ///
    /// Research rounds and project-command rounds draw from one budget: both
    /// widen what the model knows through a provider round trip, so the model may
    /// freely alternate file requests, searches, documentation, and diagnostic
    /// command runs without one kind of investigation starving the other. `None`
    /// reports that the budget is spent.
    pub(super) fn spend_investigation_round(&mut self) -> Option<usize> {
        if self.rounds_used >= self.options.max_research_rounds {
            return None;
        }
        self.rounds_used += 1;
        Some(self.rounds_used)
    }

    /// Materializes the declared external documents a round requested.
    ///
    /// A declared document has no file on disk until the harness materializes
    /// it, so the round resolves both request spellings before the research
    /// dispatcher looks for any file: a context request that addresses the
    /// outer documentation cache names a cache path directly, while an
    /// outer-document request names the declaration and the relative document it
    /// wants and keeps the cache layout an implementation detail of the harness.
    /// Every refusal keeps its classification: a trust refusal reaches the round
    /// outcome as a developer-addressed verdict, a delivery failure as a
    /// model-addressed one.
    async fn materialize_requested_outer_docs(
        &self,
        requests: &[crate::edits::protocol::ContextRequest],
        declared: &[String],
    ) -> OuterDocsResolution {
        let mut resolved = OuterDocsResolution::default();
        let Some(config) = self.options.app_config.as_ref() else {
            return resolved;
        };
        if config.outer_docs.is_empty() {
            return resolved;
        }
        let registry = crate::outer_docs::OuterDocsRegistry::new(&config.outer_docs);
        for request in requests {
            if !crate::outer_docs::is_outer_docs_cache_path(&request.path) {
                continue;
            }
            let materialized = registry
                .materialize_request(
                    self.repo_root,
                    &request.path,
                    crate::outer_docs::MAX_DOCUMENT_BYTES,
                    Some(self.cancel_rx),
                )
                .await;
            match materialized {
                Ok(Some(cache_path)) => {
                    tracing::info!(
                        path = cache_path.as_str(),
                        "outer documentation materialized for the research round"
                    );
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(
                        path = request.path.as_str(),
                        trust_refusal = error.trust().is_some(),
                        reason = %error.reason(),
                        "outer documentation request was refused"
                    );
                    resolved.failures.push((request.path.clone(), error));
                }
            }
        }
        for id in declared {
            let materialized = registry
                .materialize_declared(
                    self.repo_root,
                    id,
                    crate::outer_docs::MAX_DOCUMENT_BYTES,
                    Some(self.cancel_rx),
                )
                .await;
            match materialized {
                Ok(cache_path) => {
                    tracing::info!(
                        document = id.as_str(),
                        path = cache_path.as_str(),
                        "outer documentation materialized for the research round"
                    );
                    resolved.paths.push(cache_path);
                }
                Err(error) => {
                    tracing::warn!(
                        document = id.as_str(),
                        trust_refusal = error.trust().is_some(),
                        reason = %error.reason(),
                        "outer documentation request was refused"
                    );
                    resolved.failures.push((id.clone(), error));
                }
            }
        }
        resolved
    }
    /// Executes one proposal round with the accumulated turn state.
    ///
    /// The phase executors call this instead of [`propose_round`] directly, so
    /// the turn-scoped language directive and scratchpad are always injected
    /// without repeating the extraction at every call site.
    pub(super) async fn propose(
        &mut self,
        prompt: &str,
        staged: Option<&StagedEditsState>,
    ) -> anyhow::Result<ProposalStep> {
        propose_round(
            prompt,
            self.provider,
            self.model,
            &*self.context_manager,
            self.history_manager,
            self.protocol,
            self.workflow_name,
            self.stream_tx,
            self.cancel_rx,
            self.options,
            &mut *self.input_cache,
            staged,
            &self.state,
            self.intent,
        )
        .await
    }

    /// Runs the research phase: resolves context and documentation requests.
    ///
    /// Each round folds the model's requested files into the auto context layer
    /// and injects requested documentation before re-asking the provider for a
    /// proposal, feeding back the list of missing paths. A round that only repeats
    /// requests the context already satisfies is not a dead end: it delivers the
    /// aggregate hint and one bounded round asks the model for the analysis it
    /// owes. The phase returns the last proposal round so the caller can stage its
    /// edits. A breached file token budget becomes a typed signal the caller
    /// renders; a cancellation or a semantic block terminates the turn instead.
    pub(super) async fn run_research_phase(
        &mut self,
        initial: ProposalStep,
    ) -> anyhow::Result<ResearchPhase> {
        let mut step = initial;
        let mut satisfied_rounds = 0usize;
        // Prose the model itself authored, tracked apart from the accumulated turn
        // text: that text also carries the harness banners of every round, so it
        // proves that the turn ran but never that the model wrote anything. The
        // phase enters with the text of the round that produced `initial`, so that
        // text seeds the flag before the first research round runs.
        let mut has_model_prose = !step.text().trim().is_empty();
        while self.rounds_used < self.options.max_research_rounds
            && step.edits().is_empty()
            && step.protocol_errors().is_empty()
        {
            // The round's own buckets decide whether there is research to run:
            // one predicate serves the phase loop, the healing phase, and the
            // empty-round guard, so a capability can never be advertised,
            // decoded, and then silently ignored here.
            let has_research = step.has_research_commands();
            if !has_research {
                break;
            }

            // A declared external document has no file on disk until the
            // harness materializes it, so the requests addressing the outer
            // documentation cache are resolved before the dispatcher looks for
            // their files. A refusal keeps its audience: a trust decision
            // reaches the outcome as a developer-addressed verdict, a delivery
            // failure as a model-addressed one.
            let declared = step.outer_doc_requests().to_vec();
            let outer_docs = self
                .materialize_requested_outer_docs(step.context_requests(), &declared)
                .await;
            // A document the round asked for by its mount point carries no
            // context request of its own, so the cache path the harness derived
            // for it joins the round as one. The dispatcher then prices it like
            // every other candidate and admits it as read-only reference.
            let mut commands = step.commands().to_vec();
            for path in &outer_docs.paths {
                commands.push(LlmCommand::ContextRequest {
                    path: path.clone(),
                    access: ContextAccess::ReadOnly,
                });
            }
            let mut outcome = crate::workflow::research::execute_research_commands(
                &mut *self.context_manager,
                self.repo_root,
                &commands,
                self.options.token_budget,
                self.options.repomap_token_budget,
                self.options.research_limits,
                // A repeated request that the context already satisfies still
                // teaches the model something: the aggregate note explains that
                // the files are visible, so such a round can be answered instead
                // of being mistaken for a dead end.
                true,
                // The round races the turn's own cancel signal, so an abandoned
                // research request stops waiting for a slow reference host.
                Some(self.cancel_rx),
            )
            .await;
            for (path, error) in outer_docs.failures {
                outcome.record_outer_docs_failure(&path, &error);
            }

            // A round that would breach the files token budget is reported to the
            // caller as a typed signal: the kernel never closes the turn with
            // hardcoded text of its own.
            if let Some(limit) = &outcome.limit_exceeded {
                return Ok(ResearchPhase::BudgetExceeded(limit.clone()));
            }

            let widened = outcome.has_progress(false);
            // Repeating a request the context already satisfies widens nothing,
            // yet the model still deserves the explanation that those files are
            // visible. The round is only answered while the model itself has
            // written no prose: an analysis it authored earlier already concluded
            // the investigation, and buying another provider round trip for it
            // would spend the turn on a repeated conclusion. The guard reads the
            // authored-prose flag instead of the accumulated turn text, because
            // that text also carries the harness banners merged into it by every
            // earlier round.
            let hint_only = !widened
                && !outcome.already_satisfied_files.is_empty()
                && step.text().trim().is_empty()
                && !has_model_prose;
            if !widened && !hint_only {
                break;
            }
            if hint_only {
                // One explanation is enough to deliver the hint, so a later
                // repetition of the same requests closes the phase and leaves the
                // written answer to the turn itself.
                if satisfied_rounds >= MAX_SATISFIED_RESEARCH_ROUNDS {
                    break;
                }
                satisfied_rounds += 1;
            }

            // A research round draws from the same investigation budget as a
            // project command round, so the shared spender is the single place
            // that accounts for it. The loop head guarantees an available round;
            // should it ever be spent, the phase keeps the proposal it already
            // decoded instead of widening nothing.
            let Some(research_round) = self.spend_investigation_round() else {
                break;
            };
            log_fsm_phase(
                self.workflow_name,
                self.model,
                self.intent.is_isolated_step(),
                TurnPhase::Research,
                Some(research_round),
                Some(self.options.max_research_rounds),
            );
            tracing::info!(
                workflow = self.workflow_name,
                model = %self.model,
                round = research_round,
                added = outcome.added_files.len(),
                missing = outcome.missing_files.len(),
                searches = outcome.searches_executed.len(),
                doc_topics = outcome.doc_topics_loaded.len(),
                "context research round completed"
            );
            let _ = self
                .stream_tx
                .send(StreamEvent::TurnPhase {
                    phase: TurnPhase::Research,
                    round: Some(research_round),
                    max_rounds: Some(self.options.max_research_rounds),
                    detail: None,
                })
                .await;

            // Research findings are not repository files, so they accumulate in
            // the turn-scoped journal, an append-only epistemic tail of the user
            // message, instead of being folded into the context that the patching
            // phase reads.
            self.journal.record_outcome(&outcome);
            if hint_only {
                // The repeated request leaves nothing new to inspect, so the next
                // round owes an answer. The directive joins the journal, where it
                // stays visible for the remaining rounds of the turn.
                self.journal.push_block(ALREADY_SATISFIED_DIRECTIVE);
            }

            let ctx_state = self.context_manager.get_state();
            let _ = self
                .stream_tx
                .send(StreamEvent::ContextChanged(ctx_state))
                .await;

            let banner_text = outcome.fsm_banner_summary();

            let banner = format!(
                "\n\n---\n**[Round {}]** {}\n\n",
                research_round + 1,
                banner_text
            );
            let _ = self
                .stream_tx
                .send(StreamEvent::TextDelta(banner.clone()))
                .await;
            log_fsm_phase(
                self.workflow_name,
                self.model,
                self.intent.is_isolated_step(),
                TurnPhase::Proposal,
                Some(research_round + 1),
                None,
            );
            let _ = self
                .stream_tx
                .send(StreamEvent::TurnPhase {
                    phase: TurnPhase::Proposal,
                    round: Some(research_round + 1),
                    max_rounds: None,
                    detail: None,
                })
                .await;

            let round_prompt = self.journal.compose_user_prompt(self.prompt, None);
            step = self.propose(&round_prompt, None).await?;

            if step.is_cancelled {
                let _ = self.stream_tx.send(StreamEvent::Done).await;
                return Ok(ResearchPhase::Terminate(Box::new(FsmTurnOutcome {
                    assistant_text: std::mem::take(&mut self.accumulated_text),
                    result: ModelResult::Answer {
                        text: String::new(),
                    },
                    is_cancelled: true,
                    blocked: None,
                    abort: None,
                    has_terminal_success: false,
                    plan_saved: false,
                    plan_errors: Vec::new(),
                    plan_warnings: Vec::new(),
                    skip_reason: None,
                })));
            }

            self.state.absorb(&step);
            let step_text = step.text();
            // Prose authored in this round keeps the satisfied-hint guard closed
            // in every following round of the phase.
            has_model_prose |= !step_text.trim().is_empty();
            self.accumulated_text = merge_text(
                std::mem::take(&mut self.accumulated_text),
                &banner,
                step_text,
            );

            if let Some(reason) = self.state.blocked.clone() {
                let accumulated = std::mem::take(&mut self.accumulated_text);
                let outcome = finalize_blocked_turn(
                    self.policy,
                    self.context_manager,
                    self.stream_tx,
                    self.repo_root,
                    reason,
                    accumulated,
                    &self.state.plans,
                    self.plan_mode,
                )
                .await;
                return Ok(ResearchPhase::Terminate(Box::new(outcome)));
            }
        }

        if self.research_limit_reached(&step) {
            let warn = format!(
                "Research round limit reached (max_research_rounds = {}). The model asked for more repository material, but no further files, searches, listings, or documentation will be loaded in this turn.",
                self.options.max_research_rounds
            );
            tracing::warn!(
                workflow = self.workflow_name,
                model = %self.model,
                rounds_used = self.rounds_used,
                max_research_rounds = self.options.max_research_rounds,
                "research round limit reached; no further material will be loaded"
            );
            let banner = format!("\n\n---\n⚠️ {}\n\n", warn);
            let _ = self
                .stream_tx
                .send(StreamEvent::TextDelta(banner.clone()))
                .await;
            self.accumulated_text =
                merge_text(std::mem::take(&mut self.accumulated_text), &banner, "");
            // The directive is what the following rounds read: it closes the phase
            // with an answer contract instead of leaving the request unanswered.
            self.journal.push_block(RESEARCH_LIMIT_DIRECTIVE);
        }

        Ok(ResearchPhase::Continue(Box::new(step)))
    }

    /// Reports whether research stopped only because the round limit was spent.
    ///
    /// The loop leaves for several reasons; only an exhausted limit deserves a
    /// warning, and only when the model still asked for repository material that
    /// will never arrive. A round that carries edits or protocol errors is a
    /// proposal the harness can act upon, not a truncated investigation.
    fn research_limit_reached(&self, step: &ProposalStep) -> bool {
        self.rounds_used >= self.options.max_research_rounds
            && step.edits().is_empty()
            && step.protocol_errors().is_empty()
            && step.has_research_commands()
    }

    /// Re-asks the model after it produced a round with nothing actionable.
    ///
    /// An empty generation is a transport-level non-answer, so decoding it as a
    /// validated `ModelResult::Answer` would record silence as a conclusion. The
    /// [`EmptyStepGuard`] spends one attempt of the kernel's own corrective
    /// budget and asks once more with the accumulated journal in view; a
    /// cancellation or a semantic block terminates the turn instead. The budget
    /// is deliberately separate from the attempts a contour grants to a model
    /// tier: silence never escalates a model.
    pub(super) async fn reask_after_empty_round(&mut self) -> anyhow::Result<EmptyStepRetry> {
        let Some(attempt) = self.empty_steps.grant() else {
            return Ok(EmptyStepRetry::Exhausted);
        };
        let budget = self.empty_steps.budget();

        tracing::warn!(
            workflow = self.workflow_name,
            model = %self.model,
            attempt = attempt,
            max_retries = budget,
            "empty proposal round; re-asking the model"
        );

        let detail = format!("Empty response retry {}/{}", attempt, budget);
        let banner = format!(
            "\n\n---\n**[Empty Response {}/{}]** The previous reply carried nothing actionable; asking for a concrete proposal...\n\n",
            attempt, budget
        );
        let _ = self
            .stream_tx
            .send(StreamEvent::TextDelta(banner.clone()))
            .await;
        log_fsm_phase(
            self.workflow_name,
            self.model,
            self.intent.is_isolated_step(),
            TurnPhase::Proposal,
            Some(attempt),
            Some(budget),
        );
        let _ = self
            .stream_tx
            .send(StreamEvent::TurnPhase {
                phase: TurnPhase::Proposal,
                round: Some(attempt),
                max_rounds: Some(budget),
                detail: Some(detail),
            })
            .await;

        // The corrective request is composed from the task and the turn journal
        // every time, so the findings Discovery and the project commands already
        // produced are never dropped by a re-ask.
        let reminders = empty_step_reminder(self.protocol.capabilities()).repeat(attempt);
        let prompt = self
            .journal
            .compose_user_prompt(self.prompt, Some(&reminders));
        let step = self.propose(&prompt, None).await?;

        if step.is_cancelled {
            let _ = self.stream_tx.send(StreamEvent::Done).await;
            return Ok(EmptyStepRetry::Terminate(Box::new(FsmTurnOutcome {
                assistant_text: std::mem::take(&mut self.accumulated_text),
                result: ModelResult::Answer {
                    text: String::new(),
                },
                is_cancelled: true,
                blocked: None,
                abort: None,
                has_terminal_success: false,
                plan_saved: false,
                plan_errors: Vec::new(),
                plan_warnings: Vec::new(),
                skip_reason: None,
            })));
        }

        self.state.absorb(&step);
        self.accumulated_text = merge_text(
            std::mem::take(&mut self.accumulated_text),
            &banner,
            step.text(),
        );

        if let Some(reason) = self.state.blocked.clone() {
            let accumulated = std::mem::take(&mut self.accumulated_text);
            let outcome = finalize_blocked_turn(
                self.policy,
                self.context_manager,
                self.stream_tx,
                self.repo_root,
                reason,
                accumulated,
                &self.state.plans,
                self.plan_mode,
            )
            .await;
            return Ok(EmptyStepRetry::Terminate(Box::new(outcome)));
        }

        Ok(EmptyStepRetry::Proposed(Box::new(step)))
    }

    /// Re-asks the model after the probe refused its plan payload.
    ///
    /// The refused payload is replaced rather than accumulated: the corrective round
    /// answers for the document it repairs, so the mutations the refused round
    /// carried leave the turn state before the new round is absorbed, and no mutation
    /// is ever applied in its uncorrected form. The round itself stays narrow — only
    /// its plan payload and its text join the turn — so a repair cannot widen the turn
    /// it repairs. The directive joins the turn journal, where it stays visible for the
    /// rest of the turn, and the corrective request is composed from the task and that
    /// journal exactly like every other round.
    pub(super) async fn reask_after_failed_plan(
        &mut self,
        directive: &str,
        attempt: usize,
        budget: usize,
    ) -> anyhow::Result<PlanRetry> {
        tracing::warn!(
            workflow = self.workflow_name,
            model = %self.model,
            attempt = attempt,
            max_retries = budget,
            "plan payload refused; asking the model for a corrected mutation"
        );

        let detail = format!("Plan payload repair {}/{}", attempt, budget);
        let banner = format!(
            "\n\n---\n**[Plan Payload Repair {}/{}]** The plan mutation of the previous reply could not be applied; requesting a corrected payload...\n\n",
            attempt, budget
        );
        let _ = self
            .stream_tx
            .send(StreamEvent::TextDelta(banner.clone()))
            .await;
        log_fsm_phase(
            self.workflow_name,
            self.model,
            self.intent.is_isolated_step(),
            TurnPhase::Proposal,
            Some(attempt),
            Some(budget),
        );
        let _ = self
            .stream_tx
            .send(StreamEvent::TurnPhase {
                phase: TurnPhase::Proposal,
                round: Some(attempt),
                max_rounds: Some(budget),
                detail: Some(detail),
            })
            .await;

        // The refused payload is replaced, never accumulated, so the mutations it
        // carried leave the turn state before the corrected round is absorbed.
        self.state.clear_plans();
        self.journal.push_block(directive);

        let prompt = self.journal.compose_user_prompt(self.prompt, None);
        let step = self.propose(&prompt, None).await?;

        if step.is_cancelled {
            let _ = self.stream_tx.send(StreamEvent::Done).await;
            return Ok(PlanRetry::Terminate(Box::new(FsmTurnOutcome::cancelled(
                std::mem::take(&mut self.accumulated_text),
            ))));
        }

        // The corrective round answers for the plan payload alone: its mutations
        // replace the refused ones and its text joins the turn, while the rest of the
        // surface stays untouched. A repair round therefore cannot turn the answer
        // into a block, cannot demand a host action, and cannot widen the turn it
        // repairs.
        self.state.plans.extend(step.plans().iter().cloned());
        self.accumulated_text = merge_text(
            std::mem::take(&mut self.accumulated_text),
            &banner,
            step.text(),
        );

        Ok(PlanRetry::Proposed(Box::new(step)))
    }
}

/// Outcome of the research phase of a bounded turn.
pub(super) enum ResearchPhase {
    /// Research finished; carries the final proposal round to stage.
    ///
    /// The proposal is boxed so this variant stays small relative to
    /// `Terminate`, keeping the enum compact.
    Continue(Box<ProposalStep>),
    /// The round would breach `files_token_budget`; the caller renders the
    /// rejection, because its wording belongs to the contour, not to the kernel.
    BudgetExceeded(ContextLimitExceeded),
    /// The turn must terminate immediately with the carried outcome.
    Terminate(Box<FsmTurnOutcome>),
}

/// Outcome of the corrective proposal round requested after an empty round.
pub(super) enum EmptyStepRetry {
    /// The model answered the corrective request; carries the new round.
    Proposed(Box<ProposalStep>),
    /// The corrective budget is spent; the caller renders the terminal failure.
    Exhausted,
    /// The turn must terminate immediately with the carried outcome.
    Terminate(Box<FsmTurnOutcome>),
}

/// Outcome of the corrective proposal round requested after a refused plan payload.
pub(super) enum PlanRetry {
    /// The model answered the corrective request; carries the new round.
    Proposed(Box<ProposalStep>),
    /// The turn must terminate immediately with the carried outcome.
    Terminate(Box<FsmTurnOutcome>),
}

/// Number of additional proposal attempts granted to a model that produced an
/// empty round: no text, no edits, and no protocol command.
///
/// It is the default budget of [`EmptyStepGuard`], the component that spends it,
/// so the limit and the accounting of the corrective rounds live together.
pub(super) const MAX_EMPTY_STEP_RETRIES: usize = 2;

/// Corrective budget granted to a model that produced an empty round.
///
/// Silence is a transport-level non-answer, so the kernel resolves it itself:
/// the guard owns its own limit and never spends an attempt of the model tier,
/// which is what lets an empty answer be re-asked without escalating a model
/// that simply returned nothing.
pub(super) struct EmptyStepGuard {
    /// Corrective rounds already granted.
    used: usize,
    /// Corrective rounds the guard may grant.
    budget: usize,
}

impl Default for EmptyStepGuard {
    fn default() -> Self {
        Self {
            used: 0,
            budget: MAX_EMPTY_STEP_RETRIES,
        }
    }
}

impl EmptyStepGuard {
    /// Spends one corrective round and reports its ordinal.
    ///
    /// `None` reports that the budget is spent, so the caller renders the
    /// terminal failure instead of asking the model again.
    pub(super) fn grant(&mut self) -> Option<usize> {
        if self.used >= self.budget {
            return None;
        }
        self.used += 1;
        Some(self.used)
    }

    /// Total corrective rounds the guard may grant.
    pub(super) fn budget(&self) -> usize {
        self.budget
    }
}

/// Number of Research rounds granted to a proposal that only repeated
/// requests the context already satisfied.
///
/// Such a round widens nothing, but the model still deserves the explanation
/// that the files are visible. One round is enough to deliver it: repeating the
/// hint would spend provider round trips without teaching the model anything.
const MAX_SATISFIED_RESEARCH_ROUNDS: usize = 1;

/// Directive appended to the journal when a round only repeated requests the
/// context already satisfied.
///
/// It turns a dead-end repetition into an explicit answer contract, so the next
/// proposal round concludes with prose instead of asking for the same files.
const ALREADY_SATISFIED_DIRECTIVE: &str = "<response_directive>\nEvery file you requested is already loaded in <context>. Do not request those paths again: answer the task from the material you already hold. Write the analysis or the conclusion in plain conversational text, and emit file edits only when the code itself must change.\n</response_directive>";

/// Directive appended to the journal when the research round limit is
/// exhausted while the model still asked for more repository material.
///
/// It turns the stop condition into an explicit answer contract: the next
/// proposal round must conclude from the material already gathered instead of
/// requesting paths the harness will no longer load.
const RESEARCH_LIMIT_DIRECTIVE: &str = "<response_directive>\nThe research round limit is exhausted: no further files, repository searches, directory listings, or documentation will be loaded in this turn. Your investigation for this turn is complete. Do not request additional paths. Answer the task from the material you already hold, stating the conclusions or emitting the file edits the gathered context supports.\n</response_directive>";

/// Corrective suffix appended to the request after an empty proposal round.
///
/// The suffix extends the previous request instead of replacing it, so the
/// corrective round keeps the provider-cached prompt prefix and the model keeps
/// the original task statement in view. It names the plain-text analysis as a
/// valid conclusion, so a model that has finished its investigation answers the
/// task instead of believing that only file edits count as a result, and it
/// states the payload contract of a terminal outcome, because a status without
/// its payload is as empty as a reply without content.
const EMPTY_STEP_REMINDER: &str = "\n\nYour previous reply was empty: it carried no text, no file edits, no research command, and no terminal outcome with its payload, so nothing could be acted upon. Respond again with a concrete result: when the context you already hold answers the task, write the analysis or the conclusion in plain conversational text; when the code must change, emit the file edits; otherwise declare an explicit terminal outcome and state the payload of the status you declare, because a terminal status without its payload is a protocol defect rather than a result.";
/// Corrective suffix for a contour that cannot edit files.
///
/// The wording follows the capability surface of the active contour
/// (`crates/core/Conventions.md` §2.4): a contour without the edit capability is
/// never told to emit a file edit its decoder would reject, so the reminder
/// names only the results that contour can actually deliver.
const EMPTY_STEP_REMINDER_WITHOUT_EDITS: &str = "\n\nYour previous reply was empty: it carried no text, no protocol command, and no terminal outcome with its payload, so nothing could be acted upon. Respond again with a concrete result: state the analysis or the conclusion the task asks for, or declare an explicit terminal outcome and state the payload of the status you declare, because a terminal status without its payload is a protocol defect rather than a result.";

/// Selects the corrective suffix for the capability surface of a contour.
fn empty_step_reminder(capabilities: &[LlmCapability]) -> &'static str {
    if capabilities.contains(&LlmCapability::FileEdits) {
        EMPTY_STEP_REMINDER
    } else {
        EMPTY_STEP_REMINDER_WITHOUT_EDITS
    }
}
/// Operation label recorded when an offloaded repository-state read fails.
const REPOSITORY_STATE_OPERATION: &str = "repository state resolution";

/// Offloads blocking work and records a failed offload before degrading.
///
/// The returned `Option` reports whether the offload completed at all: `Some`
/// wraps whatever the closure produced, while `None` reports a panicked or
/// cancelled blocking task. The `JoinError` always travels through
/// [`crate::blocking::log_join_error`] before the caller degrades, so a failed
/// offload never hides behind a legitimately absent result.
async fn offload_or_record<T, F>(operation: &str, work: F) -> Option<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    match crate::blocking::run_blocking_async(work).await {
        Ok(value) => Some(value),
        Err(error) => {
            crate::blocking::log_join_error(&error, operation);
            None
        }
    }
}

/// Runs a single atomic model interaction and decodes it into commands.
///
/// This is the piece-wise form of [`TurnKernel::propose`]: a phase executor that
/// holds an open transaction guard owns the context borrow for the guard's whole
/// lifetime, so it addresses the kernel fields it needs directly instead of the
/// kernel as a whole.
#[allow(clippy::too_many_arguments)]
pub(super) async fn propose_round(
    prompt: &str,
    provider: &dyn LlmProvider,
    model: &ModelRef,
    context_manager: &ContextManager,
    history_manager: &HistoryManager,
    protocol: &dyn PromptProtocol,
    workflow_name: &str,
    stream_tx: &mpsc::Sender<StreamEvent>,
    cancel_rx: &watch::Receiver<bool>,
    options: &WorkflowOptions,
    cache: &mut TurnInputCache,
    staged_state: Option<&StagedEditsState>,
    state: &TurnState,
    intent: TurnIntent,
) -> anyhow::Result<ProposalStep> {
    let repo_root = context_manager.repo_root().to_path_buf();

    // Reading the repository state shells out to git; the call runs off the
    // async executor thread. A repository that cannot be inspected degrades to
    // an absent state block instead of failing the turn, and a failed offload
    // is recorded before the block goes absent, so a panicked read never looks
    // like a repository that was simply never inspected.
    let root = repo_root.clone();
    let repo_state = offload_or_record(REPOSITORY_STATE_OPERATION, move || {
        crate::git::state::get_repository_state(Some(&root))
    })
    .await;

    let ctx_state = context_manager.get_state();
    // The turn memoizes the body of every context file it already read, so a
    // round pays only for the paths discovery just added or the change set just
    // rewrote. The staged-content override below still wins over the memo.
    let mut context_files = cache.context_files(&repo_root, &ctx_state.items).await?;

    if let Some(staged) = staged_state {
        // Staged content is merged in place: a file already present in context
        // keeps its position, its neighbors keep their tokens, and the round
        // extends the previous prompt instead of reshaping it. Files created
        // during the turn are appended after the existing block in path order,
        // which keeps the appended tail deterministic across rounds. The staged
        // state keeps bytes, so only its lossless text view joins the prompt: a
        // payload that does not decode as UTF-8 stays out of it instead of being
        // rendered lossily.
        let staged_text = staged.staged_text_files();
        let mut appended = Vec::new();
        for (path, content) in &staged_text {
            if let Some(file) = context_files.iter_mut().find(|file| &file.path == path) {
                file.content = content.clone();
            } else {
                appended.push(ContextFileContent {
                    path: path.clone(),
                    access: ContextAccess::Editable,
                    content: content.clone(),
                    // A file created during the turn never entered a context
                    // layer, so it carries the unrecorded arrival.
                    insertion_seq: 0,
                });
            }
        }
        appended.sort_by(|a, b| a.path.cmp(&b.path));
        context_files.extend(appended);
        context_files.retain(|file| !staged.deleted_paths.contains(&file.path));
    }

    // Every file an edit may address this turn: the whole context block, not
    // only the editable layer, because a file marked read-only becomes editable
    // as soon as an edit proposes a change to it. One list feeds the streaming
    // filter, the response schema, and the decoder, so the path a preview
    // reports while streaming is byte-identical to the path carried by the
    // decoded command.
    let context_paths: Vec<String> = context_files.iter().map(|file| file.path.clone()).collect();

    // The cache freezes the map on the first round of the turn, so later rounds
    // receive the identical string even after discovery widened the context.
    // The eager symbol map and the compact directory outline are mutually
    // exclusive: the model reads either symbol contours or the folder layout,
    // never both, so exactly one outline enters the prompt.
    let (repo_map, repo_dirs) = if options.include_repo_map {
        let map = cache
            .repo_map(&repo_root, &context_paths, options.repomap_token_budget)
            .await;
        (map, None)
    } else {
        (
            None,
            cache
                .repo_dirs(&repo_root, context_manager.pinned_patterns())
                .await,
        )
    };

    let repomap_tokens = repo_map
        .as_deref()
        .or(repo_dirs.as_deref())
        .map(crate::prompt::estimate_tokens)
        .unwrap_or(0);
    tracing::info!(
        workflow = workflow_name,
        model = %model,
        isolated_step = intent.is_isolated_step(),
        repo_map_tokens = repomap_tokens,
        "proposal round prepared"
    );
    let _ = stream_tx
        .send(StreamEvent::RepoMapTokens(repomap_tokens))
        .await;

    let history = cache.history_snapshot(history_manager, intent).await;
    let project = cache.project(&repo_root, Some(cancel_rx)).await;
    // The declared external documentation is a project fact rather than a round
    // decision, so the catalog the prompt renders comes from the same frozen
    // configuration snapshot as the project command allowlist.
    let outer_docs = options
        .app_config
        .as_ref()
        .map(|config| config.outer_docs.clone())
        .unwrap_or_default();

    // The declared patch strategy decides how the rendered context addresses the
    // lines of every file, so it travels from the project configuration into the
    // assembly instead of being re-derived by the prompt layer.
    let patch_strategy = options.patch_strategy();

    let mut assembly = PromptAssembly::new(
        repo_state,
        ctx_state.revision,
        context_files,
        workflow_name,
        protocol.kind(),
        history,
        repo_map,
    )
    .with_repo_dirs(repo_dirs)
    .with_project(Some(project))
    .with_patch_strategy(patch_strategy)
    .with_outer_docs(outer_docs);
    assembly.target_language = state.language.clone();
    assembly.intent = intent;
    assembly.turn_scratchpad = state.scratchpad.clone();
    assembly.max_scratchpad_chars = options.max_scratchpad_chars;
    // The layout policy resolved from the governing configuration composes the
    // message sequence of the round: an interactive dialogue that keeps a
    // growing history builds the chronological transcript, while every other
    // intent keeps the semantic block order.
    assembly.prompt_layout = options.prompt_layout(intent);

    // The declared turn intent narrows the protocol once, so the rendered
    // instructions, the structured response schema, the stream filter, and the
    // decoder all observe the same capability surface: nothing is advertised
    // without being decodable, and nothing is accepted without being advertised.
    let scoped_protocol = protocol.with_capability_set(assembly.effective_capabilities(protocol));
    let protocol: &dyn PromptProtocol = scoped_protocol.as_ref();

    // The turn-scoped memo hands back the elements an earlier round already
    // rendered, so the context block only grows at its tail and the provider
    // keeps matching the prompt prefix it already cached.
    let messages =
        assembly.assemble_chat_messages_with_context_cache(prompt, protocol, cache.context_cache());
    // The composed schema and the rendered prompt rules observe one project
    // declaration: the frozen snapshot the assembly carries is the very value
    // handed to the projection, so a field the prompt never mentions can never
    // be mandated.
    let response_format =
        protocol.response_format(&context_paths, patch_strategy, assembly.project.as_ref());

    let (llm_tx, llm_rx) = mpsc::channel::<StreamEvent>(100);
    // A session identifier pins the proposal round to one provider backend and
    // keeps its prompt-prefix cache warm across the discovery, patch-retry, and
    // healing rounds of the same turn. Without one the provider serves the
    // request statelessly.
    let client_call = provider.stream_chat_with_session(
        &model.name,
        messages,
        response_format,
        options.reasoning_effort,
        options.session_id.as_deref(),
        llm_tx,
        cancel_rx.clone(),
    );

    let staged_for_filter = staged_state
        .map(StagedEditsState::staged_text_files)
        .unwrap_or_default();
    // The streaming preview receives the same context list as the decoder, so a
    // reported edit and the staged edit always name the same file. The seed is
    // the lossless text view of the staged state, so a payload that is not text
    // is absent from the preview exactly as it is absent from the prompt.
    let stream_filter =
        protocol.create_stream_filter(context_paths.clone(), repo_root, staged_for_filter);
    let forward_stream_tx = stream_tx.clone();

    // The forwarder owns the protocol filter for the whole round, so it resets
    // it together with the decoded accumulator whenever the transport voids an
    // attempt and starts the next one.
    let forward_task = tokio::spawn(async move {
        forward_provider_stream(llm_rx, stream_filter, forward_stream_tx).await
    });

    let client_result = client_call.await;
    let (assistant_text, cancelled, stream_error, completed) =
        forward_task.await.map_err(|join_error| {
            tracing::error!(
                "Proposal streaming task terminated unexpectedly: {}",
                join_error
            );
            anyhow::anyhow!(
                "Proposal streaming task terminated unexpectedly: {}",
                join_error
            )
        })?;
    client_result?;

    if let Some(err) = stream_error {
        anyhow::bail!("LLM stream error: {}", err);
    }

    if cancelled {
        return Ok(ProposalStep {
            is_cancelled: true,
            ..Default::default()
        });
    }

    if !completed {
        anyhow::bail!("LLM stream closed unexpectedly without completion");
    }

    // The decoder resolves edit paths against the identical context list the
    // stream filter received, so a decoded edit names the file the preview
    // already certified as edited.
    //
    // A payload with no content carries no command to decode. Silence is a
    // transport-level non-answer, so decoding it would manufacture a protocol
    // error the model never wrote and charge the running model tier for it; the
    // round therefore stays empty and the kernel's own corrective budget
    // resolves it (`EmptyStepGuard`).
    if assistant_text.trim().is_empty() {
        return Ok(ProposalStep::default().with_raw_text(assistant_text));
    }
    let commands = protocol.decode(&assistant_text, &context_paths);
    // The decoded commands serve the phases; a contour that owns a typed payload
    // reads the very bytes this round was decoded from through the single
    // deserializer of its own schema, so the text travels with the proposal.
    let step = ProposalStep::from_commands(commands).with_raw_text(assistant_text);
    // A decision the round recorded for a review finding is durable state
    // rather than a proposal: it is applied at the round boundary that decoded
    // it, so a later phase failure can never discard the engineer's decision.
    record_review_decisions(&step, options, context_manager.repo_root());
    Ok(step)
}

/// Drains one proposal stream, forwarding the filtered events to the client.
///
/// A retried attempt starts from a clean slate: `ResetStream` clears the decoded
/// accumulator together with the protocol filter, so neither the decoded
/// commands nor the streamed preview can inherit a fragment of the attempt the
/// transport voided. The filter keeps what describes the turn rather than the
/// attempt - the turn marker and the editable path list.
async fn forward_provider_stream(
    mut llm_rx: mpsc::Receiver<StreamEvent>,
    mut stream_filter: Box<dyn EditStreamFilter>,
    forward_stream_tx: mpsc::Sender<StreamEvent>,
) -> (String, bool, Option<String>, bool) {
    let mut assistant_text = String::new();
    let mut cancelled = false;
    let mut stream_error = None;
    let mut completed = false;
    while let Some(event) = llm_rx.recv().await {
        match event {
            StreamEvent::TextDelta(ref delta) => {
                assistant_text.push_str(delta);
                for forwarded in stream_filter.push_chunk(delta) {
                    if forward_stream_tx.send(forwarded).await.is_err() {
                        break;
                    }
                }
            }
            StreamEvent::Cancelled => {
                cancelled = true;
                let _ = forward_stream_tx.send(StreamEvent::Cancelled).await;
            }
            StreamEvent::Error(ref err) => {
                stream_error = Some(err.clone());
                let _ = forward_stream_tx
                    .send(StreamEvent::Error(err.clone()))
                    .await;
            }
            // The transport decorator discarded the fragments of the
            // interrupted attempt and re-issued the request, so the text
            // accumulated for decoding is discarded with them. The protocol
            // filter is reset beside it, because a preview the client already
            // reset must not be completed by the bytes of the attempt that
            // replaces it.
            StreamEvent::ResetStream => {
                assistant_text.clear();
                stream_filter.reset();
                let _ = forward_stream_tx.send(StreamEvent::ResetStream).await;
            }
            StreamEvent::Done => {
                completed = true;
            }
            // Events the view does not model are forwarded as-is. A client
            // that stopped listening only silences the forwarding: the round
            // keeps draining the provider, so its output is still decoded and
            // the turn reaches a deterministic outcome instead of being
            // reported as an interrupted stream.
            other => {
                let _ = forward_stream_tx.send(other).await;
            }
        }
    }
    for forwarded in stream_filter.finish() {
        let _ = forward_stream_tx.send(forwarded).await;
    }
    (assistant_text, cancelled, stream_error, completed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_empty_step_reminder_requires_a_result_not_a_silent_status() {
        // The corrective round owes a result, and a payload-free terminal status
        // is named as a non-result, so a model cannot satisfy the reminder by
        // declaring an empty outcome.
        assert!(super::EMPTY_STEP_REMINDER.contains("Your previous reply was empty"));
        assert!(super::EMPTY_STEP_REMINDER.contains("plain conversational text"));
        assert!(super::EMPTY_STEP_REMINDER.contains("emit the file edits"));
        assert!(super::EMPTY_STEP_REMINDER.contains("terminal outcome"));
        assert!(super::EMPTY_STEP_REMINDER.contains("payload"));
        assert!(super::EMPTY_STEP_REMINDER.contains("protocol defect"));
    }

    #[test]
    fn the_empty_step_reminder_follows_the_capability_surface() {
        // A contour that can edit files is told that edits are a valid result.
        let with_edits = [LlmCapability::TextMessage, LlmCapability::FileEdits];
        assert_eq!(empty_step_reminder(&with_edits), super::EMPTY_STEP_REMINDER);

        // A contour that cannot edit files is never asked for one: the reminder
        // names only the results its decoder would accept.
        let without_edits = [LlmCapability::TextMessage, LlmCapability::Research];
        assert_eq!(
            empty_step_reminder(&without_edits),
            super::EMPTY_STEP_REMINDER_WITHOUT_EDITS
        );
        assert!(!empty_step_reminder(&without_edits).contains("file edits"));
        assert!(empty_step_reminder(&without_edits).contains("terminal outcome"));
        assert!(empty_step_reminder(&without_edits).contains("payload"));
    }
    /// Drives one proposal stream over `filter`, returning the events the
    /// client observed and the text the turn would decode.
    async fn drive_forward_stream(
        filter: Box<dyn EditStreamFilter>,
        events: Vec<StreamEvent>,
    ) -> (Vec<StreamEvent>, String) {
        let (llm_tx, llm_rx) = mpsc::channel(64);
        let (stream_tx, mut stream_rx) = mpsc::channel(64);
        let forward =
            tokio::spawn(async move { forward_provider_stream(llm_rx, filter, stream_tx).await });
        for event in events {
            llm_tx
                .send(event)
                .await
                .expect("the forwarder is listening");
        }
        drop(llm_tx);

        let (assistant_text, ..) = forward.await.expect("the forward task joins");
        let mut observed = Vec::new();
        while let Some(event) = stream_rx.recv().await {
            observed.push(event);
        }
        (observed, assistant_text)
    }

    /// Returns the events forwarded after the transport reset the stream.
    fn events_after_reset(events: &[StreamEvent]) -> &[StreamEvent] {
        let reset_at = events
            .iter()
            .position(|event| matches!(event, StreamEvent::ResetStream))
            .expect("the transport must void the interrupted attempt");
        &events[reset_at + 1..]
    }

    /// Concatenates the conversational text of the given events.
    fn streamed_text(events: &[StreamEvent]) -> String {
        events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::TextDelta(text) => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Collects the terminal event of every previewed file.
    fn file_completions(events: &[StreamEvent]) -> Vec<(tauqe_protocol::EditFileStatus, usize)> {
        events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::EditFileDone {
                    status,
                    hunks_count,
                    ..
                } => Some((*status, *hunks_count)),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn a_reset_stream_restarts_the_xml_preview_from_a_clean_filter() {
        use std::collections::HashMap;

        use crate::edits::stream::XmlStreamFilter;

        let filter = Box::new(
            XmlStreamFilter::new(vec!["a.rs".to_string()], std::env::temp_dir())
                .with_staged_contents(HashMap::from([(
                    "a.rs".to_string(),
                    "fn old() {}\n".to_string(),
                )])),
        );

        // The interrupted attempt opens a file block and stops in the middle of
        // a replacement, leaving the filter inside the edit container with an
        // open file and a partially scanned hunk.
        let interrupted = concat!(
            "First try.\n",
            "<tauqe_edits><edit path=\"a.rs\">\n",
            "<search>\nfn old() {}\n</search>\n<replace>\npartial"
        );
        let retried = concat!(
            "<tauqe_edits><edit path=\"a.rs\">\n",
            "<search>\nfn old() {}\n</search>\n<replace>\nfn new() {}\n</replace>\n",
            "</edit></tauqe_edits>\nAll done."
        );

        let (events, assistant_text) = drive_forward_stream(
            filter,
            vec![
                StreamEvent::TextDelta(interrupted.to_string()),
                StreamEvent::ResetStream,
                StreamEvent::TextDelta(retried.to_string()),
                StreamEvent::Done,
            ],
        )
        .await;

        // The decoded accumulator restarts with the filter, so the retried
        // attempt is the only text the turn decodes.
        assert!(!assistant_text.contains("First try."));
        assert!(assistant_text.contains("All done."));

        let retried_events = events_after_reset(&events);
        assert_eq!(streamed_text(retried_events).trim(), "All done.");
        assert_eq!(
            file_completions(retried_events),
            vec![(tauqe_protocol::EditFileStatus::Ok, 1)],
            "the retried replacement applies to the untouched snapshot"
        );
        assert!(!retried_events.iter().any(|event| matches!(
            event,
            StreamEvent::EditHunk { new_text, .. } if new_text.contains("partial")
        )));
    }

    #[tokio::test]
    async fn a_reset_stream_restarts_the_json_preview_from_a_clean_filter() {
        use std::collections::HashMap;

        use crate::edits::stream::JsonStreamFilter;

        let filter = Box::new(
            JsonStreamFilter::new(vec!["a.rs".to_string()], std::env::temp_dir())
                .with_staged_contents(HashMap::from([(
                    "a.rs".to_string(),
                    "alpha\nbeta\ngamma\n".to_string(),
                )])),
        );

        // The interrupted attempt completes one change and stops inside the
        // `message` value, so the cascaded cursor and the staging simulator both
        // carry state of the voided attempt.
        let interrupted = concat!(
            "{\"changes\":[{\"op\":\"replace\",\"path\":\"a.rs\",",
            "\"old_text\":\"alpha\",\"new_text\":\"ALPHA\"}],",
            "\"message\":\"First try."
        );
        let retried = concat!(
            "{\"message\":\"Second try.\",\"changes\":[{\"op\":\"replace\",",
            "\"path\":\"a.rs\",\"old_text\":\"alpha\",\"new_text\":\"ALPHA\"}]}"
        );

        let (events, assistant_text) = drive_forward_stream(
            filter,
            vec![
                StreamEvent::TextDelta(interrupted.to_string()),
                StreamEvent::ResetStream,
                StreamEvent::TextDelta(retried.to_string()),
                StreamEvent::Done,
            ],
        )
        .await;

        assert!(!assistant_text.contains("First try."));

        let retried_events = events_after_reset(&events);
        assert_eq!(streamed_text(retried_events), "Second try.");
        assert_eq!(
            file_completions(retried_events),
            vec![(tauqe_protocol::EditFileStatus::Ok, 1)],
            "the retried change applies to the snapshot, not to the voided hunk"
        );
        assert!(!retried_events.iter().any(|event| matches!(
            event,
            StreamEvent::EditFileDone { status, .. }
                if *status == tauqe_protocol::EditFileStatus::Error
        )));
    }

    /// A failed offload must never degrade silently: the record emitted by
    /// `log_join_error` keeps a panicked repository-state read distinguishable
    /// from a repository that was never inspected.
    #[tokio::test]
    async fn a_failed_offload_leaves_an_observable_record() {
        let (buffer, _guard) = crate::blocking::log_capture::install();

        let state = offload_or_record(REPOSITORY_STATE_OPERATION, || -> u32 {
            panic!("intentional panic for test")
        })
        .await;

        assert!(state.is_none(), "a failed offload degrades to `None`");
        let logged = crate::blocking::log_capture::contents(&buffer);
        assert!(
            logged.contains("offloaded blocking task failed"),
            "the degradation must be observable: {logged}"
        );
        assert!(
            logged.contains(REPOSITORY_STATE_OPERATION),
            "the record must name the failed operation: {logged}"
        );
        assert!(
            logged.contains("panicked"),
            "the record must report the failure kind: {logged}"
        );
    }
}
