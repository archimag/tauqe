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
    ContextAccess, EditOperation, EditOutcome, EditProposal, ModelResult, TurnPhase, VerifyTarget,
};
use tokio::sync::{mpsc, watch};

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

    // `load_toolchain_config` reads `tauqe.toml` from disk, so it runs off the
    // async executor thread.
    let config_root = ctx.repo_root.to_path_buf();
    let toolchain_config = crate::blocking::run_blocking_async(move || {
        crate::config::load_toolchain_config(&config_root)
    })
    .await
    .unwrap_or_default();
    let current_verify = if ctx.intent.is_isolated_step() {
        // In isolated step / batch execution, verification is strictly determined by project configuration.
        // The model has no capability or authority to configure or weaken toolchain verification.
        if toolchain_config.effective_auto_verify() {
            let target = match toolchain_config.effective_target() {
                VerifyTarget::All => EditVerifyTarget::All,
                VerifyTarget::Check => EditVerifyTarget::Check,
                VerifyTarget::Test => EditVerifyTarget::Test,
                VerifyTarget::Clippy => EditVerifyTarget::Clippy,
            };
            Some(EditVerifyRequest {
                target,
                on_success: EditVerifyOnSuccess::Silent,
            })
        } else {
            None
        }
    } else if toolchain_config.effective_auto_verify() {
        // When automatic verification is enabled, the verification gate is an inviolable project invariant.
        // A model-emitted verify request cannot downgrade the verification target below configured settings.
        let target = match toolchain_config.effective_target() {
            VerifyTarget::All => EditVerifyTarget::All,
            VerifyTarget::Check => EditVerifyTarget::Check,
            VerifyTarget::Test => EditVerifyTarget::Test,
            VerifyTarget::Clippy => EditVerifyTarget::Clippy,
        };
        let on_success = verify_request
            .as_ref()
            .map(|r| r.on_success)
            .unwrap_or(EditVerifyOnSuccess::Silent);
        Some(EditVerifyRequest { target, on_success })
    } else {
        verify_request
    };

    if let Some(mut verify_request) = current_verify {
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
        let mut verify_outcome = run_workflow_verification(
            ctx.repo_root,
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
                has_terminal_success: false,
                plan_saved: false,
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

        // The `[toolchain] auto_heal` flag gates the self-healing retry loop.
        // It defaults to on, preserving the standard bounded-turn behavior.
        let auto_heal = toolchain_config.auto_heal.unwrap_or(true);
        let mut healing_attempt = 0;
        let mut missing_edits = false;
        let mut healing_protocol_errors = Vec::new();
        while !verify_outcome.success && auto_heal && healing_attempt < ctx.options.max_retries {
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
                    has_terminal_success: false,
                    plan_saved: false,
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
                    false,
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
                    if !ctx.intent.is_isolated_step() && !toolchain_config.effective_auto_verify() {
                        if let Some(new_request) = step_out.verify_request() {
                            verify_request = new_request.clone();
                        }
                    }
                    verify_outcome = run_workflow_verification(
                        ctx.repo_root,
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
                            has_terminal_success: false,
                            plan_saved: false,
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
            // Formatting is the last step before the Git commit: it runs only on
            // a verified working tree and never invalidates the build. An
            // interrupted run rolls the turn back instead of committing a
            // partially formatted tree.
            if !toolchain_config.fmt.is_empty()
                && run_best_effort_formatters(ctx.repo_root, &total_changed_files, ctx.cancel_rx)
                    .await
            {
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
                    has_terminal_success: false,
                    plan_saved: false,
                    skip_reason: None,
                });
            }
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
        return Ok(finish_turn(
            std::mem::take(&mut ctx.accumulated_text),
            result,
            ctx.repo_root,
            &ctx.state.plans,
            ctx.plan_mode,
        )
        .await);
    }

    // No verification requested: finalize immediately.
    if let Some(text) = extract_success_summary(ctx.state.terminal_outcome.as_ref()) {
        summary = text;
    }
    if !toolchain_config.fmt.is_empty()
        && run_best_effort_formatters(ctx.repo_root, &total_changed_files, ctx.cancel_rx).await
    {
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
            has_terminal_success: false,
            plan_saved: false,
            skip_reason: None,
        });
    }
    let (commit_hash, commit_error) = guard.finalize(&total_changed_files, &summary).await;
    tracing::info!(
        workflow = ctx.workflow_name,
        model = %ctx.model,
        isolated_step = ctx.intent.is_isolated_step(),
        changed_files = total_changed_files.len(),
        committed = commit_hash.is_some(),
        "bounded turn finalized edits without verification"
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
    Ok(finish_turn(
        std::mem::take(&mut ctx.accumulated_text),
        result,
        ctx.repo_root,
        &ctx.state.plans,
        ctx.plan_mode,
    )
    .await)
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

/// Working-tree content of every changed file a formatter may rewrite.
type ChangedFilesSnapshot = Vec<(String, Option<Vec<u8>>)>;

/// Runs the configured formatters over the changed files as a best-effort step.
///
/// Formatting happens strictly after the build and tests verified the working
/// tree and immediately before the Git commit. The content of the changed files
/// is snapshotted first, so a formatter that fails or exits non-zero has the
/// files it claimed restored: the failure is logged, the verified code is
/// committed untouched, and the model is never asked to repair a formatter.
///
/// The turn cancellation signal is forwarded to every invocation, so a
/// developer interrupt stops formatting immediately rather than after the last
/// configured formatter finished. An interrupted formatter is handled like a
/// failed one — the verified content is restored — and the `true` return value
/// tells the caller to roll the turn back instead of committing.
async fn run_best_effort_formatters(
    repo_root: &Path,
    changed_files: &[String],
    cancel_rx: &watch::Receiver<bool>,
) -> bool {
    if changed_files.is_empty() {
        return false;
    }
    let is_git =
        crate::blocking::run_blocking(|| crate::git::state::is_git_repository(Some(repo_root)));
    let dirty_before = if is_git {
        crate::blocking::run_blocking(|| collect_porcelain_status_files(repo_root))
    } else {
        Vec::new()
    };

    let snapshot =
        crate::blocking::run_blocking(|| snapshot_changed_files(repo_root, changed_files));
    let results = crate::toolchain::run_formatters(repo_root, changed_files, Some(cancel_rx)).await;
    let mut cancelled = false;
    for result in results.iter() {
        if result.is_cancelled {
            cancelled = true;
        }
        if result.success {
            continue;
        }
        tracing::warn!(
            formatter = %result.formatter,
            command = %result.command,
            files = result.files.len(),
            cancelled = result.is_cancelled,
            "formatter failed; restoring the verified content"
        );
        let files = result.files.as_slice();
        crate::blocking::run_blocking(|| restore_changed_files(repo_root, &snapshot, files));
    }

    if is_git && !cancelled {
        crate::blocking::run_blocking(|| {
            revert_foreign_formatter_modifications(repo_root, &dirty_before, changed_files);
        });
    }

    cancelled
}

/// Collects repository-relative paths of modified or untracked files via Git status.
fn collect_porcelain_status_files(repo_root: &Path) -> Vec<String> {
    let output = match crate::git::cmd::run_git(Some(repo_root), &["status", "--porcelain"]) {
        Ok(out) => out,
        Err(_) => return Vec::new(),
    };
    parse_porcelain_paths(&output)
}

fn parse_porcelain_paths(output: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for line in output.lines() {
        if line.len() < 4 {
            continue;
        }
        let file_part = &line[3..];
        let path = if let Some((_, to)) = file_part.split_once(" -> ") {
            to.trim()
        } else {
            file_part.trim()
        };
        let cleaned = path.trim_matches('"');
        if !cleaned.is_empty() {
            paths.push(cleaned.to_string());
        }
    }
    paths
}

/// Reverts any files modified, deleted, or created by formatters that are not part
/// of the turn's intentional `changed_files`.
fn revert_foreign_formatter_modifications(
    repo_root: &Path,
    dirty_before: &[String],
    changed_files: &[String],
) {
    let dirty_after = collect_porcelain_status_files(repo_root);
    for path_str in dirty_after {
        let is_foreign = !dirty_before
            .iter()
            .any(|d| Path::new(d) == Path::new(&path_str))
            && !changed_files
                .iter()
                .any(|c| Path::new(c) == Path::new(&path_str));
        if !is_foreign {
            continue;
        }

        tracing::warn!(
            file = %path_str,
            "reverting unauthorized modification made by formatter outside changed files"
        );

        let checkout_res =
            crate::git::cmd::run_git(Some(repo_root), &["checkout", "HEAD", "--", &path_str]);
        if checkout_res.is_err() {
            let full_path = repo_root.join(&path_str);
            if full_path.is_file() || full_path.is_symlink() {
                let _ = std::fs::remove_file(&full_path);
            } else if full_path.is_dir() {
                let _ = std::fs::remove_dir_all(&full_path);
            }
        }
    }
}

/// Reads the current working-tree content of the changed files.
fn snapshot_changed_files(repo_root: &Path, changed_files: &[String]) -> ChangedFilesSnapshot {
    changed_files
        .iter()
        .map(|file| (file.clone(), std::fs::read(repo_root.join(file)).ok()))
        .collect()
}

/// Restores the snapshotted content of the files a failed formatter claimed.
fn restore_changed_files(repo_root: &Path, snapshot: &ChangedFilesSnapshot, files: &[String]) {
    for (path, content) in snapshot {
        if !files.iter().any(|file| file == path) {
            continue;
        }
        let full = repo_root.join(path);
        let restored = match content {
            Some(bytes) => std::fs::write(&full, bytes),
            None => std::fs::remove_file(&full),
        };
        if let Err(err) = restored {
            tracing::error!(
                file = %path,
                error = %err,
                "failed to restore a file after a formatter failure"
            );
        }
    }
}

/// Registers files referenced by a verification failure into editable context.
///
/// The toolchain report is parsed deterministically for source paths, and each
/// successfully added path is promoted into the editable auto layer so the next
/// healing round can patch it.
async fn auto_register_error_files(
    context_manager: &mut ContextManager,
    report: &str,
    repo_root: &Path,
    stream_tx: &mpsc::Sender<StreamEvent>,
) {
    let available_files = {
        let root = repo_root.to_path_buf();
        crate::blocking::run_blocking_async(move || {
            crate::git::list_repository_files(Some(&root)).unwrap_or_default()
        })
        .await
        .unwrap_or_default()
    };
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
            ("Cargo.toml", "[package]\nname = \"fsm_heal_test\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[lib]\npath = \"a.rs\"\n"),
        ]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        // First attempt emits an edit with a syntax error and requests verification.
        // Cargo check fails, triggering healing. During healing, the provider
        // returns an error (e.g. network drop).
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

    #[test]
    fn test_revert_foreign_formatter_modifications() {
        let (_dir, root) = setup_repo(&[("a.rs", "pub fn a() {}\n"), ("b.rs", "pub fn b() {}\n")]);

        let dirty_before = collect_porcelain_status_files(&root);
        assert!(dirty_before.is_empty());

        // Simulate a formatter modifying both the intentional file (a.rs)
        // and a foreign file (b.rs), as well as creating an untracked artifact.
        std::fs::write(root.join("a.rs"), "pub fn a() { /* formatted */ }\n").unwrap();
        std::fs::write(root.join("b.rs"), "pub fn b() { /* foreign edit */ }\n").unwrap();
        std::fs::write(root.join("untracked.tmp"), "stray formatter artifact\n").unwrap();

        // Revert foreign modifications; only a.rs was intentionally changed.
        revert_foreign_formatter_modifications(&root, &dirty_before, &["a.rs".to_string()]);

        // a.rs retains its modification
        assert_eq!(
            std::fs::read_to_string(root.join("a.rs")).unwrap(),
            "pub fn a() { /* formatted */ }\n"
        );
        // b.rs is reverted to original content from HEAD
        assert_eq!(
            std::fs::read_to_string(root.join("b.rs")).unwrap(),
            "pub fn b() {}\n"
        );
        // untracked artifact is removed
        assert!(!root.join("untracked.tmp").exists());
    }
}
