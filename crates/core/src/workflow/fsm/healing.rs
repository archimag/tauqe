//! Verification and healing phase executors for the FSM bounded turn.
//!
//! Once edits are staged, this module applies them into the working tree through
//! a [`TransactionRollbackGuard`], runs the deterministic toolchain check, and
//! drives the self-healing loop that feeds compiler diagnostics back to the
//! model until the build is clean or the retry budget is exhausted. A clean
//! build is finalized into a Git transaction; any failure rolls the working tree
//! back to its pre-turn checkpoint.

use std::path::Path;

use tauqe_protocol::{
    ContextAccess, EditOperation, EditOutcome, EditProposal, ModelResult, TurnPhase,
};
use tokio::sync::mpsc;

use crate::context::ContextManager;
use crate::edits::protocol::{
    VerifyOnSuccess as EditVerifyOnSuccess, VerifyRequest as EditVerifyRequest,
    VerifyTarget as EditVerifyTarget,
};
use crate::providers::StreamEvent;
use crate::workflow::discovery::execute_discovery_commands;
use crate::workflow::verification::{
    build_verification_missing_edits_prompt, build_verification_retry_prompt, extract_error_paths,
    run_workflow_verification, VerificationOutcome,
};

use super::guard::{finish_turn, TransactionRollbackGuard};
use super::proposal::{extract_success_summary, log_fsm_phase, merge_text};
use super::{propose, FsmTurnOutcome, PhaseContext};

/// Runs the verification and healing phases and finalizes the turn.
///
/// Applies the staged edits into the working tree, resolves the effective
/// verification request (falling back to the toolchain auto-verify default),
/// runs the deterministic check, and heals compiler failures by re-prompting the
/// model up to `max_retries`. A clean build is finalized into a transaction; a
/// failed or cancelled verification rolls the working tree back. The returned
/// outcome is always terminal for the turn.
pub(super) async fn run_apply_and_verify_phase(
    ctx: &mut PhaseContext<'_>,
    cumulative_edits: Vec<EditOperation>,
    verify_request: Option<EditVerifyRequest>,
) -> anyhow::Result<FsmTurnOutcome> {
    let mut summary = extract_success_summary(ctx.state.terminal_outcome.as_ref())
        .unwrap_or_else(|| "Apply FSM turn edits".to_string());

    let proposal = EditProposal {
        summary: summary.clone(),
        edits: cumulative_edits.clone(),
    };

    let repo_root = ctx.repo_root;
    let mut guard = TransactionRollbackGuard::new(
        ctx.policy,
        ctx.context_manager,
        repo_root,
        ctx.cancel_rx.clone(),
    );

    let mut total_changed_files = match guard.apply_temporary(&proposal).await {
        Ok(files) => {
            guard.arm();
            files
        }
        Err(error) => {
            guard.rollback().await;
            let ctx_state = guard.context_manager.get_state();
            let _ = ctx
                .stream_tx
                .send(StreamEvent::ContextChanged(ctx_state))
                .await;
            let _ = ctx.stream_tx.send(StreamEvent::Done).await;
            let result = ModelResult::Edit {
                summary,
                edits: cumulative_edits,
                proposal: None,
                outcome: EditOutcome {
                    applied: false,
                    error: Some(error.to_string()),
                    changed_files: Vec::new(),
                    commit_hash: None,
                },
            };
            return Ok(finish_turn(
                std::mem::take(&mut ctx.accumulated_text),
                result,
                ctx.repo_root,
                &ctx.state.plans,
                ctx.plan_mode,
            )
            .await);
        }
    };

    // Verification is an inviolable project invariant: the declared
    // `[project.verify]` lifecycle runs after every change set, so the request
    // always exists and the model shapes only whether a passing run is reported.
    // Because the project declares one lifecycle command, the requested target is
    // a label rather than a command selector and can never weaken the gate.
    let verify_request = verify_request.unwrap_or(EditVerifyRequest {
        target: EditVerifyTarget::All,
        on_success: EditVerifyOnSuccess::Silent,
    });

    // The verification phase owns the open transaction until it finalizes or
    // rolls the working tree back, so it runs as one scoped stage.
    {
        log_fsm_phase(
            ctx.workflow_name,
            ctx.model,
            ctx.intent.is_isolated_step(),
            TurnPhase::Verification,
            None,
            None,
        );
        let _ = ctx
            .stream_tx
            .send(StreamEvent::TurnPhase {
                phase: TurnPhase::Verification,
                round: None,
                max_rounds: None,
                detail: None,
            })
            .await;
        // The change set is the harness's authoritative answer to what this turn
        // touched: the staged proposal, the post-hook artifacts, the paths a
        // repair round wrote, and every working-copy path the transaction gained.
        // The project dispatcher receives it on its standard input and narrows the
        // checks from it, so it is assembled before the first round runs.
        let mut verify_files = total_changed_files.clone();
        merge_changed_files(&mut verify_files, guard.worktree_changes());
        let mut verify_outcome = run_workflow_verification(
            ctx.repo_root,
            &verify_files,
            &verify_request,
            ctx.stream_tx,
            false,
            Some(ctx.cancel_rx),
        )
        .await;
        if verify_outcome.is_cancelled {
            if guard.rollback().await {
                let ctx_state = guard.context_manager.get_state();
                let _ = ctx
                    .stream_tx
                    .send(StreamEvent::ContextChanged(ctx_state))
                    .await;
            }
            let _ = ctx.stream_tx.send(StreamEvent::Done).await;
            return Ok(FsmTurnOutcome {
                assistant_text: std::mem::take(&mut ctx.accumulated_text),
                result: ModelResult::Answer {
                    text: String::new(),
                },
                is_cancelled: true,
                blocked: None,
                abort: None,
                has_terminal_success: false,
                plan_saved: false,
                plan_errors: Vec::new(),
                skip_reason: None,
            });
        }
        if !verify_outcome.success {
            auto_register_error_files(
                guard.context_manager,
                &verify_outcome.report,
                ctx.repo_root,
                ctx.stream_tx,
            )
            .await;
        }

        // The healing loop is bounded by the edit retry budget: a verification
        // failure always receives the same number of repair rounds as a failed
        // patch, and a repository can never switch repair off entirely.
        let mut healing_attempt = 0;
        let mut missing_edits = false;
        let mut healing_protocol_errors = Vec::new();
        while !verify_outcome.success && healing_attempt < ctx.options.max_retries {
            healing_attempt += 1;
            log_fsm_phase(
                ctx.workflow_name,
                ctx.model,
                ctx.intent.is_isolated_step(),
                TurnPhase::Healing,
                Some(healing_attempt),
                Some(ctx.options.max_retries),
            );
            let _ = ctx
                .stream_tx
                .send(StreamEvent::TurnPhase {
                    phase: TurnPhase::Healing,
                    round: Some(healing_attempt),
                    max_rounds: Some(ctx.options.max_retries),
                    detail: None,
                })
                .await;

            let retry_banner = format!(
                "\n\n---\n**[Verification Retry {}/{}]** Correcting verification failures...\n\n",
                healing_attempt, ctx.options.max_retries
            );
            let _ = ctx
                .stream_tx
                .send(StreamEvent::TextDelta(retry_banner.clone()))
                .await;

            let retry_diagnostics = if !healing_protocol_errors.is_empty() {
                format!(
                    "The proposed verification repair edits could not be parsed due to protocol syntax errors:\n{}\n\nPlease provide corrected edit blocks to fix the verification failure:\n{}",
                    healing_protocol_errors.iter().map(|e| format!("- {}", e)).collect::<Vec<_>>().join("\n"),
                    verify_outcome.report
                )
            } else if missing_edits {
                build_verification_missing_edits_prompt(
                    &verify_outcome.report,
                    ctx.state.language.as_deref(),
                )
            } else {
                build_verification_retry_prompt(
                    &verify_outcome.report,
                    ctx.state.language.as_deref(),
                )
            };
            // The healing round replays the knowledge Discovery already paid
            // for: the task first, the accumulated journal next, and the
            // compiler diagnostics last.
            let retry_prompt = ctx
                .journal
                .compose_user_prompt(ctx.prompt, Some(&retry_diagnostics));
            let step_out = propose(
                ctx.provider,
                ctx.model,
                ctx.protocol,
                ctx.history_manager,
                &*guard.context_manager,
                ctx.workflow_name,
                ctx.stream_tx,
                ctx.cancel_rx,
                ctx.options,
                &mut *ctx.input_cache,
                ctx.state,
                ctx.intent,
                &retry_prompt,
                None,
            )
            .await;

            // A provider stream failure here must roll the open transaction
            // back in async code: the guard's `Drop` no longer performs I/O, so
            // deferring rollback to it would leave the working tree dirty until
            // crash recovery on the next startup.
            let step_out = match step_out {
                Ok(step) => step,
                Err(err) => {
                    if guard.rollback().await {
                        let ctx_state = guard.context_manager.get_state();
                        let _ = ctx
                            .stream_tx
                            .send(StreamEvent::ContextChanged(ctx_state))
                            .await;
                    }
                    return Err(err);
                }
            };

            if step_out.is_cancelled {
                if guard.rollback().await {
                    let ctx_state = guard.context_manager.get_state();
                    let _ = ctx
                        .stream_tx
                        .send(StreamEvent::ContextChanged(ctx_state))
                        .await;
                }
                let _ = ctx.stream_tx.send(StreamEvent::Done).await;
                return Ok(FsmTurnOutcome {
                    assistant_text: std::mem::take(&mut ctx.accumulated_text),
                    result: ModelResult::Answer {
                        text: String::new(),
                    },
                    is_cancelled: true,
                    blocked: None,
                    abort: None,
                    has_terminal_success: false,
                    plan_saved: false,
                    plan_errors: Vec::new(),
                    skip_reason: None,
                });
            }
            ctx.state.absorb(&step_out);
            let step_text = step_out.text();
            ctx.accumulated_text = merge_text(
                std::mem::take(&mut ctx.accumulated_text),
                &retry_banner,
                step_text,
            );

            if let Some(abort) = ctx.state.abort.clone() {
                // An abort asserts that no code change can complete the work:
                // the host environment itself must change first. The repair loop
                // therefore stops before the next verification round, no further
                // repair request is issued, and the open transaction is rolled
                // back so the repository stays exactly as the turn found it.
                let notice = format!(
                    "\n\n---\n⛔ **[Execution Aborted]**\n\n{}\n\n",
                    abort.to_message()
                );
                let _ = ctx.stream_tx.send(StreamEvent::TextDelta(notice)).await;
                tracing::error!(
                    workflow = ctx.workflow_name,
                    model = %ctx.model,
                    isolated_step = ctx.intent.is_isolated_step(),
                    reason = %abort.reason,
                    "bounded turn aborted during verification healing"
                );
                guard.rollback().await;
                let ctx_state = guard.context_manager.get_state();
                let _ = ctx
                    .stream_tx
                    .send(StreamEvent::ContextChanged(ctx_state))
                    .await;
                let _ = ctx.stream_tx.send(StreamEvent::Done).await;
                let result = ModelResult::Edit {
                    summary: "Execution aborted".to_string(),
                    edits: Vec::new(),
                    proposal: None,
                    outcome: EditOutcome {
                        applied: false,
                        error: Some(abort.to_message()),
                        changed_files: Vec::new(),
                        commit_hash: None,
                    },
                };
                let mut outcome = finish_turn(
                    std::mem::take(&mut ctx.accumulated_text),
                    result,
                    ctx.repo_root,
                    &ctx.state.plans,
                    ctx.plan_mode,
                )
                .await;
                outcome.abort = Some(abort);
                return Ok(outcome);
            }

            if let Some(reason) = ctx.state.blocked.clone() {
                guard.rollback().await;
                let ctx_state = guard.context_manager.get_state();
                let _ = ctx
                    .stream_tx
                    .send(StreamEvent::ContextChanged(ctx_state))
                    .await;
                let _ = ctx.stream_tx.send(StreamEvent::Done).await;
                let result = ModelResult::Edit {
                    summary: "Execution blocked".to_string(),
                    edits: Vec::new(),
                    proposal: None,
                    outcome: EditOutcome {
                        applied: false,
                        error: Some(reason.clone()),
                        changed_files: Vec::new(),
                        commit_hash: None,
                    },
                };
                let mut outcome = finish_turn(
                    std::mem::take(&mut ctx.accumulated_text),
                    result,
                    ctx.repo_root,
                    &ctx.state.plans,
                    ctx.plan_mode,
                )
                .await;
                outcome.blocked = Some(reason);
                return Ok(outcome);
            }

            // Discovery commands emitted during healing run through the same
            // dispatcher as the Discovery phase, so every capability (context
            // files, repository searches, documentation) behaves identically and
            // owns its own blocking offload and budget checks.
            if step_out.has_discovery_commands() {
                let discovery_outcome = execute_discovery_commands(
                    guard.context_manager,
                    ctx.repo_root,
                    step_out.commands(),
                    ctx.options.files_token_budget,
                    ctx.options.repomap_token_budget,
                    ctx.options.discovery_limits,
                    false,
                    // The healing round races the turn's own cancel signal, so
                    // an abandoned repair stops waiting for a slow reference host.
                    Some(ctx.cancel_rx),
                )
                .await;
                // Healing findings join the turn journal, so a later healing
                // round still sees the documentation or outline this one asked
                // for.
                ctx.journal.record_outcome(&discovery_outcome);
                if !discovery_outcome.added_files.is_empty() {
                    let ctx_state = guard.context_manager.get_state();
                    let _ = ctx
                        .stream_tx
                        .send(StreamEvent::ContextChanged(ctx_state))
                        .await;
                }
            }

            let retry_edits = step_out.edits().to_vec();
            healing_protocol_errors = step_out.protocol_errors().to_vec();
            if retry_edits.is_empty() {
                missing_edits = true;
                continue;
            }
            missing_edits = false;

            let retry_proposal = EditProposal {
                summary: summary.clone(),
                edits: retry_edits,
            };
            // The working copy is snapshotted immediately before the attempt, so
            // a repair that fails halfway cannot strand the files it already
            // wrote: the paths it left dirty are collected below and join the
            // change set like any other artifact.
            let snapshot_root = ctx.repo_root;
            let worktree_before = crate::blocking::run_blocking(|| {
                crate::git::state::snapshot_worktree(snapshot_root)
            });
            match guard.apply_temporary(&retry_proposal).await {
                Ok(new_files) => {
                    merge_changed_files(&mut total_changed_files, new_files);
                    // The repair widened the change set, and the failed round may
                    // have left artifacts of its own behind, so the next
                    // verification receives the whole list instead of the
                    // repaired files alone.
                    verify_files = total_changed_files.clone();
                    merge_changed_files(&mut verify_files, guard.worktree_changes());
                    verify_outcome = run_workflow_verification(
                        ctx.repo_root,
                        &verify_files,
                        &verify_request,
                        ctx.stream_tx,
                        false,
                        Some(ctx.cancel_rx),
                    )
                    .await;
                    if verify_outcome.is_cancelled {
                        if guard.rollback().await {
                            let ctx_state = guard.context_manager.get_state();
                            let _ = ctx
                                .stream_tx
                                .send(StreamEvent::ContextChanged(ctx_state))
                                .await;
                        }
                        let _ = ctx.stream_tx.send(StreamEvent::Done).await;
                        return Ok(FsmTurnOutcome {
                            assistant_text: std::mem::take(&mut ctx.accumulated_text),
                            result: ModelResult::Answer {
                                text: String::new(),
                            },
                            is_cancelled: true,
                            blocked: None,
                            abort: None,
                            has_terminal_success: false,
                            plan_saved: false,
                            plan_errors: Vec::new(),
                            skip_reason: None,
                        });
                    }
                    if !verify_outcome.success {
                        auto_register_error_files(
                            guard.context_manager,
                            &verify_outcome.report,
                            ctx.repo_root,
                            ctx.stream_tx,
                        )
                        .await;
                    }
                }
                Err(error) => {
                    // A repair rejected midway may still have written some of its
                    // files. Those paths stay dirty in the working copy, so they
                    // must join the change set: otherwise the final commit would
                    // leave them unaccounted for while the turn reports a clean,
                    // verified tree.
                    let snapshot_root = ctx.repo_root;
                    let worktree_after = crate::blocking::run_blocking(|| {
                        crate::git::state::snapshot_worktree(snapshot_root)
                    });
                    let dirty = crate::git::state::changed_worktree_paths(
                        &worktree_before,
                        &worktree_after,
                    );
                    merge_changed_files(&mut total_changed_files, dirty);
                    verify_outcome = VerificationOutcome {
                        success: false,
                        is_cancelled: false,
                        report: format!("Failed to apply repair edits: {}", error),
                    };
                }
            }
        }

        if verify_outcome.success {
            if let Some(text) = extract_success_summary(ctx.state.terminal_outcome.as_ref()) {
                summary = text;
            }
            // The working copy now holds more than the model proposed: the
            // toolchain may have refreshed a lock file or regenerated a
            // snapshot while it verified the tree. The guard baselined the
            // working copy once, so its differential keeps the atomic commit
            // complete instead of leaving verified artifacts outside it.
            merge_changed_files(&mut total_changed_files, guard.worktree_changes());
            let (commit_hash, commit_error) = guard.finalize(&total_changed_files, &summary).await;
            tracing::info!(
                workflow = ctx.workflow_name,
                model = %ctx.model,
                isolated_step = ctx.intent.is_isolated_step(),
                changed_files = total_changed_files.len(),
                committed = commit_hash.is_some(),
                "bounded turn applied and verified edits"
            );
            let ctx_state = guard.context_manager.get_state();
            let _ = ctx
                .stream_tx
                .send(StreamEvent::ContextChanged(ctx_state))
                .await;
            let _ = ctx.stream_tx.send(StreamEvent::Done).await;
            let result = ModelResult::Edit {
                summary,
                edits: cumulative_edits,
                proposal: None,
                outcome: EditOutcome {
                    applied: true,
                    error: commit_error,
                    changed_files: total_changed_files,
                    commit_hash,
                },
            };
            return Ok(finish_turn(
                std::mem::take(&mut ctx.accumulated_text),
                result,
                ctx.repo_root,
                &ctx.state.plans,
                ctx.plan_mode,
            )
            .await);
        }

        let changed_files = if guard.rollback().await {
            let ctx_state = guard.context_manager.get_state();
            let _ = ctx
                .stream_tx
                .send(StreamEvent::ContextChanged(ctx_state))
                .await;
            Vec::new()
        } else {
            total_changed_files
        };
        tracing::warn!(
            workflow = ctx.workflow_name,
            model = %ctx.model,
            isolated_step = ctx.intent.is_isolated_step(),
            "verification failed after healing attempts"
        );
        let _ = ctx.stream_tx.send(StreamEvent::Done).await;
        let result = ModelResult::Edit {
            summary,
            edits: cumulative_edits,
            proposal: None,
            outcome: EditOutcome {
                applied: false,
                error: Some(format!(
                    "Code verification failed:\n{}",
                    verify_outcome.report
                )),
                changed_files,
                commit_hash: None,
            },
        };
        Ok(finish_turn(
            std::mem::take(&mut ctx.accumulated_text),
            result,
            ctx.repo_root,
            &ctx.state.plans,
            ctx.plan_mode,
        )
        .await)
    }
}

/// Merges newly produced paths into the turn's change set.
///
/// The change set is the single source of truth for what the turn touched:
/// post-hook artifacts, repair-round edits, and the paths a failed repair left
/// dirty all arrive here. The final atomic commit stages exactly this list, so a
/// file that is dirty in the working copy can never fall outside it. Ordering is
/// preserved and each path is recorded once, which keeps the commit
/// deterministic across healing rounds.
fn merge_changed_files<I>(total: &mut Vec<String>, new_files: I)
where
    I: IntoIterator<Item = String>,
{
    for file in new_files {
        if !total.contains(&file) {
            total.push(file);
        }
    }
}

/// Registers files referenced by a verification failure into editable context.
///
/// The toolchain report is parsed deterministically for source paths, and each
/// successfully added path is promoted into the editable auto layer so the next
/// healing round can patch it. A repository that cannot be inspected skips the
/// registration, and the failed listing is logged first, so the skip stays an
/// observable degradation instead of looking like a report without resolvable
/// paths.
async fn auto_register_error_files(
    context_manager: &mut ContextManager,
    report: &str,
    repo_root: &Path,
    stream_tx: &mpsc::Sender<StreamEvent>,
) {
    // The failure arms are recorded inside `resolve_repository_files`
    // before `None` is returned, so the downgrade here stays observable.
    let available_files = crate::workflow::discovery::resolve_repository_files(repo_root)
        .await
        .unwrap_or_default();
    let error_paths = extract_error_paths(report, &available_files);
    // `add_auto_file` performs a filesystem metadata check and path
    // canonicalization, so the loop runs off the async executor thread.
    let added = crate::blocking::run_blocking(|| {
        let mut added = Vec::new();
        for path in error_paths {
            if context_manager.effective_access_for(&path) != Some(ContextAccess::Editable)
                && context_manager
                    .add_auto_file(&path, ContextAccess::Editable)
                    .is_ok()
            {
                added.push(path);
            }
        }
        added
    });
    if !added.is_empty() {
        let ctx_state = context_manager.get_state();
        let _ = stream_tx.send(StreamEvent::ContextChanged(ctx_state)).await;
        let notice = format!(
            "\n\n---\nAdded {} error file(s) to editable context: {}\n\n",
            added.len(),
            added.join(", ")
        );
        let _ = stream_tx.send(StreamEvent::TextDelta(notice)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::super::run_fsm_turn;
    use super::*;
    use crate::context::ContextManager;
    use crate::history::HistoryManager;
    use crate::prompt::xml::XmlPromptProtocol;
    use crate::providers::mock::{MockLlmProvider, MockResponse};
    use crate::workflow::WorkflowOptions;
    use tauqe_protocol::{ContextAccess, ModelRef};
    use tokio::sync::{mpsc, watch};

    fn setup_repo(files: &[(&str, &str)]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        crate::git::init_repository(Some(&root), true).unwrap();
        for (path, content) in files {
            let full = root.join(path);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(full, content).unwrap();
        }
        let owned: Vec<String> = files.iter().map(|(path, _)| path.to_string()).collect();
        crate::git::cmd::stage_files(&root, &owned).unwrap();
        crate::git::cmd::commit_with_developer_author(&root, "Add fixtures", false).unwrap();
        (dir, root)
    }

    fn context_with_target(root: &std::path::Path) -> ContextManager {
        let mut context = ContextManager::new(root.to_path_buf());
        context.add_file("a.rs", ContextAccess::Editable).unwrap();
        context
    }

    #[tokio::test]
    async fn error_during_healing_triggers_automatic_rollback() {
        let (_dir, root) = setup_repo(&[
            ("a.rs", "pub fn compute() -> i32 { 0 }\n"),
            (
                "tauqe.toml",
                concat!(
                    "[models]\n",
                    "primary = [\"openrouter:mock/model\"]\n",
                    "\n",
                    "[project.verify]\n",
                    "command = \"false\"\n",
                ),
            ),
        ]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        // The first attempt applies its edit, the declared verification fails,
        // and healing begins. During that healing round the provider returns an
        // error (e.g. a network drop).
        let bad_edit = concat!(
            "<tauqe_edits summary=\"Edit\">\n",
            "<edit path=\"a.rs\">\n",
            "<search>\npub fn compute() -> i32 { 0 }\n</search>\n",
            "<replace>\npub fn compute() -> i32 { SYNTAX_ERROR }\n</replace>\n",
            "</edit>\n",
            "</tauqe_edits>\n",
            "<verify target=\"check\" on_success=\"silent\" />",
        );
        let provider = MockLlmProvider::scripted([
            MockResponse::text(bad_edit),
            MockResponse::error("OpenRouter connection reset"),
        ]);
        let model = ModelRef::openrouter("mock/model");
        let protocol = XmlPromptProtocol;
        let mut policy = crate::workflow::git_policy::GitTransactionPolicy::new();
        let options = WorkflowOptions::default();
        let (stream_tx, rx) = mpsc::channel(256);
        drop(rx);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let outcome_result = run_fsm_turn(
            "Change compute",
            tauqe_protocol::TurnIntent::InteractiveRepl,
            &provider,
            &model,
            &mut context,
            &mut history,
            &protocol,
            &mut policy,
            "git",
            stream_tx,
            cancel_rx,
            &options,
        )
        .await;

        assert!(
            outcome_result.is_err(),
            "turn must propagate provider error"
        );
        // Verify working tree has been rolled back to pristine pre-turn state.
        assert_eq!(
            std::fs::read_to_string(root.join("a.rs")).unwrap(),
            "pub fn compute() -> i32 { 0 }\n"
        );
    }

    /// A declared verification command reads the whole turn change set on its
    /// standard input: the model's edit and the artifact the post-hook produced
    /// arrive as one repository-relative path per line.
    #[cfg(unix)]
    #[tokio::test]
    async fn verification_receives_the_full_change_set_on_stdin() {
        let capture = tempfile::tempdir().unwrap();
        let capture_path = capture.path().join("change-set.txt");
        let config = format!(
            concat!(
                "[models]\n",
                "primary = [\"openrouter:mock/model\"]\n",
                "\n",
                "[project.verify]\n",
                "command = \"sh\"\n",
                "args = ['-c', 'tee {} > /dev/null']\n",
                "\n",
                "[project.post_hook]\n",
                "command = \"sh\"\n",
                "args = ['-c', 'printf \"generated\\n\" > generated.rs']\n",
            ),
            capture_path.display()
        );
        let (_dir, root) = setup_repo(&[("a.rs", "one\n")]);
        std::fs::write(root.join("tauqe.toml"), &config).expect("write the project declaration");
        let mut context = ContextManager::new(root.clone());
        context.add_file("a.rs", ContextAccess::Editable).unwrap();
        let mut history = HistoryManager::new(root.clone());

        let response = concat!(
            "<tauqe_edits summary=\"Edit\">\n",
            "<edit path=\"a.rs\">\n",
            "<search>\none\n</search>\n",
            "<replace>\ntwo\n</replace>\n",
            "</edit>\n",
            "</tauqe_edits>\n",
            "<verify target=\"check\" on_success=\"silent\" />",
        );
        let provider = MockLlmProvider::scripted([MockResponse::text(response)]);
        let model = ModelRef::openrouter("mock/model");
        let protocol = XmlPromptProtocol;
        let mut policy = crate::workflow::git_policy::GitTransactionPolicy::new();
        let options = WorkflowOptions::default();
        let (stream_tx, rx) = mpsc::channel(256);
        drop(rx);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let turn = run_fsm_turn(
            "Rewrite a.rs",
            tauqe_protocol::TurnIntent::InteractiveRepl,
            &provider,
            &model,
            &mut context,
            &mut history,
            &protocol,
            &mut policy,
            "git",
            stream_tx,
            cancel_rx,
            &options,
        )
        .await
        .expect("the bounded turn executes");

        match &turn.result {
            ModelResult::Edit { outcome, .. } => {
                assert!(
                    outcome.error.is_none(),
                    "unexpected error: {:?}",
                    outcome.error
                );
                assert!(outcome.applied, "the verified change set must be committed");
            }
            other => panic!("expected applied edits, got {other:?}"),
        }

        let captured = std::fs::read_to_string(&capture_path)
            .expect("the verification command captured its standard input");
        let mut delivered: Vec<&str> = captured
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect();
        delivered.sort();
        assert_eq!(
            delivered,
            vec!["a.rs", "generated.rs"],
            "the change set must reach the verification command whole: {captured:?}"
        );
    }

    #[test]
    fn test_merge_changed_files_keeps_order_and_drops_duplicates() {
        let mut total: Vec<String> = vec!["src/lib.rs".to_string()];

        merge_changed_files(
            &mut total,
            vec!["generated.rs".to_string(), "src/lib.rs".to_string()],
        );
        merge_changed_files(&mut total, Vec::new());

        assert_eq!(
            total,
            vec!["src/lib.rs".to_string(), "generated.rs".to_string()]
        );
    }

    /// Returns the repository-relative paths touched by the commit at HEAD.
    #[cfg(unix)]
    fn committed_paths(root: &std::path::Path) -> Vec<String> {
        let mut paths: Vec<String> = crate::git::cmd::run_git(
            Some(root),
            &["show", "--name-only", "--pretty=format:", "HEAD"],
        )
        .expect("read the committed paths")
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();
        paths.sort();
        paths
    }

    /// Counts the commits reachable from HEAD.
    #[cfg(unix)]
    fn commit_count(root: &std::path::Path) -> usize {
        crate::git::cmd::run_git(Some(root), &["rev-list", "--count", "HEAD"])
            .expect("count the commits")
            .trim()
            .parse()
            .expect("parse the commit count")
    }

    /// A verification command that rewrites a companion artifact on every run
    /// leaves that path inside the single commit of the healed turn, and the
    /// working copy stays clean afterwards.
    ///
    /// This is the shape of a real project check: a lock file is refreshed while
    /// the tree is verified, so the artifact the check produced belongs to the
    /// turn that paid for it instead of surviving as untracked residue.
    #[cfg(unix)]
    #[tokio::test]
    async fn verification_artifacts_join_the_single_commit_after_healing() {
        let (_dir, root) = setup_repo(&[
            ("a.rs", "pub fn compute() -> i32 { 0 }\n"),
            (
                "tauqe.toml",
                concat!(
                    "[models]\n",
                    "primary = [\"openrouter:mock/model\"]\n",
                    "\n",
                    "[project.verify]\n",
                    "command = \"sh\"\n",
                    "args = ['-c', 'echo run >> Cargo.lock; grep -q \"pub fn compute() -> i32 { 7 }\" a.rs']\n",
                ),
            ),
        ]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        // The first attempt repairs the signature but still fails the project
        // check, so the turn heals; the second attempt satisfies it.
        let first = concat!(
            "<tauqe_edits summary=\"Repair compute\">\n",
            "<edit path=\"a.rs\">\n",
            "<search>\npub fn compute() -> i32 { 0 }\n</search>\n",
            "<replace>\npub fn compute() -> i32 { 1 }\n</replace>\n",
            "</edit>\n",
            "</tauqe_edits>\n",
            "<verify target=\"check\" on_success=\"silent\" />",
        );
        let second = concat!(
            "<tauqe_edits summary=\"Repair compute\">\n",
            "<edit path=\"a.rs\">\n",
            "<search>\npub fn compute() -> i32 { 1 }\n</search>\n",
            "<replace>\npub fn compute() -> i32 { 7 }\n</replace>\n",
            "</edit>\n",
            "</tauqe_edits>\n",
            "<verify target=\"check\" on_success=\"silent\" />",
        );
        let provider =
            MockLlmProvider::scripted([MockResponse::text(first), MockResponse::text(second)]);
        let model = ModelRef::openrouter("mock/model");
        let protocol = XmlPromptProtocol;
        let mut policy = crate::workflow::git_policy::GitTransactionPolicy::new();
        let options = WorkflowOptions::default();
        let (stream_tx, rx) = mpsc::channel(256);
        drop(rx);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let baseline = commit_count(&root);
        let turn = run_fsm_turn(
            "Repair the mismatched return type in a.rs",
            tauqe_protocol::TurnIntent::InteractiveRepl,
            &provider,
            &model,
            &mut context,
            &mut history,
            &protocol,
            &mut policy,
            "git",
            stream_tx,
            cancel_rx,
            &options,
        )
        .await
        .expect("the bounded turn executes");

        match &turn.result {
            ModelResult::Edit { outcome, .. } => {
                assert!(
                    outcome.error.is_none(),
                    "unexpected error: {:?}",
                    outcome.error
                );
                assert!(outcome.applied, "the healed change set must be committed");
                let mut changed = outcome.changed_files.clone();
                changed.sort();
                assert_eq!(
                    changed,
                    vec!["Cargo.lock".to_string(), "a.rs".to_string()],
                    "the artifact produced by verification must join the change set"
                );
            }
            other => panic!("expected applied edits, got {other:?}"),
        }
        assert_eq!(
            provider.requests().len(),
            2,
            "one initial attempt and one healing round"
        );
        assert_eq!(
            committed_paths(&root),
            vec!["Cargo.lock".to_string(), "a.rs".to_string()],
            "the verification artifact must land in the same commit as the edit"
        );
        assert_eq!(
            commit_count(&root),
            baseline + 1,
            "the healed turn must still produce exactly one commit"
        );
        assert_eq!(
            crate::git::cmd::run_git(Some(&root), &["show", "HEAD:Cargo.lock"])
                .expect("read the committed artifact"),
            "run\nrun\n",
            "the artifact rewritten by the healing round must be committed"
        );
        assert!(
            crate::git::cmd::run_git(Some(&root), &["status", "--porcelain"])
                .expect("read the working copy status")
                .trim()
                .is_empty(),
            "the turn must not leave unaccounted files behind"
        );
    }

    /// A repository that cannot be inspected must not register verification
    /// error files, and the skipped registration must leave a warn record
    /// instead of silently degrading to an empty path list.
    #[tokio::test]
    async fn uninspectable_repository_skips_error_file_registration_with_a_record() {
        let dir = tempfile::tempdir().unwrap();
        let missing_root = dir.path().join("no-such-repository");
        let mut context = ContextManager::new(dir.path().to_path_buf());
        let (stream_tx, _rx) = mpsc::channel(64);

        let (buffer, _guard) = crate::blocking::log_capture::install();
        auto_register_error_files(
            &mut context,
            "error: a.rs:3:5: mismatched types",
            &missing_root,
            &stream_tx,
        )
        .await;

        assert!(context.auto_files().is_empty());
        let logs = crate::blocking::log_capture::contents(&buffer);
        assert!(
            logs.contains("failed to list repository files"),
            "the degradation must be observable: {logs}"
        );
    }
}
