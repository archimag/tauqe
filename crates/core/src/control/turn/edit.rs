//! Atomic disk-side edits and toolchain verification control loop.
//!
//! [`run_edit_loop`] coordinates the application of code changes to disk,
//! project formatting, post-hook execution, deterministic toolchain verification
//! (`[project.verify]`), and multi-round verification healing.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use similar::{ChangeTag, TextDiff};
use tauqe_protocol::{
    ContextAccess, EditFileOp, EditProposal, LlmCapability, LlmCommand, TurnPhase,
    VerificationVerdict,
};

use super::research::{handle_research_commands, ResearchLoopConfig};
use super::staging::{run_staging_loop, StagingLoopConfig};
use crate::control::context::LoopContext;
use crate::control::outcome::{
    FailureReport, LoopResult, LoopSuccess, ResponseEnvelope, SystemError,
};
use crate::edits::StagedEditsState;
use crate::git::state::{changed_worktree_paths, snapshot_worktree};
use crate::providers::StreamEvent;
use crate::toolchain::{
    format_staged_content, run_post_hooks_with_config, run_verification_pipeline_with_config,
    VerificationStatus,
};
use crate::workflow::verification::{
    build_verification_missing_edits_prompt, build_verification_retry_prompt, extract_error_paths,
};

/// Capabilities processed by the atomic edits and verification contour.
pub const EDIT_CAPABILITIES: &[LlmCapability] = &[
    LlmCapability::EditReplace,
    LlmCapability::EditCreate,
    LlmCapability::EditDelete,
    LlmCapability::EditMove,
    LlmCapability::EditMoveDir,
    LlmCapability::EditOverwrite,
];
#[derive(Debug, Clone)]
pub struct EditLoopConfig {
    /// Maximum verification healing attempts allowed when toolchain checks fail.
    pub max_healing_rounds: usize,
    /// In-memory staging and patch retry configuration.
    pub staging: StagingLoopConfig,
    /// Research budgets the healing rounds inherit from the project configuration.
    pub research: ResearchLoopConfig,
}

impl EditLoopConfig {
    /// Creates edit and verification loop configuration from project REPL configuration.
    pub fn from_repl_config(config: &crate::config::ReplConfig) -> Self {
        Self {
            max_healing_rounds: config.max_retries,
            staging: StagingLoopConfig::from_repl_config(config),
            research: ResearchLoopConfig::default(),
        }
    }

    /// Creates edit and verification loop configuration from the authoritative
    /// project configuration.
    pub fn from_app_config(config: &crate::config::AppConfig) -> Self {
        Self {
            max_healing_rounds: config.repl.max_retries,
            staging: StagingLoopConfig::from_repl_config(&config.repl),
            research: ResearchLoopConfig::from_context(&config.context),
        }
    }
}

impl Default for EditLoopConfig {
    fn default() -> Self {
        Self::from_repl_config(&crate::config::ReplConfig::default())
    }
}

/// Artifact produced by a successful edit and verification cycle.
#[derive(Debug, Clone, PartialEq)]
pub struct EditArtifact {
    /// Repository-relative paths touched by the applied change set.
    pub modified_paths: Vec<String>,
    /// Final toolchain verification verdict.
    pub verification: VerificationVerdict,
    /// Final squashed commit hash if changes were committed.
    pub commit_hash: Option<String>,
    /// Diagnostic of a Git transaction that could not be finalized.
    ///
    /// The change set already reached the working tree and was verified, so a
    /// failed finalization never withdraws it: the reason travels with the
    /// artifact instead of being swallowed and reported as a turn without a
    /// commit.
    pub commit_error: Option<String>,
}

/// Local in-memory backup of disk files touched during edit application.
///
/// It is the last-resort rollback: [`rollback_turn`] restores the working tree
/// through the transaction policy and replays this backup only when the policy
/// restored nothing, so a restored tree is never overwritten with a stale copy.
struct DiskBackup {
    root: PathBuf,
    files: HashMap<String, Option<Vec<u8>>>,
}

impl DiskBackup {
    fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            files: HashMap::new(),
        }
    }

    fn record_paths(&mut self, paths: &[String]) {
        for path in paths {
            if !self.files.contains_key(path) {
                let full = self.root.join(path);
                let original = std::fs::read(&full).ok();
                self.files.insert(path.clone(), original);
            }
        }
    }

    fn record_paths_from_git_baseline(&mut self, paths: &[String]) {
        for path in paths {
            if !self.files.contains_key(path) {
                let original =
                    crate::git::cmd::run_git(Some(&self.root), &["show", &format!("HEAD:{path}")])
                        .ok()
                        .map(|s| s.into_bytes());
                self.files.insert(path.clone(), original);
            }
        }
    }

    /// Replays the recorded pre-turn bytes over the working tree.
    ///
    /// This is the fallback of [`rollback_turn`]: it runs only when the
    /// transaction policy restored no tree, so a developer's uncommitted work is
    /// never replaced by the bytes a checkpoint already restored.
    fn rollback(&self) {
        for (path, original) in &self.files {
            let full = self.root.join(path);
            match original {
                Some(bytes) => {
                    if let Some(parent) = full.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    let _ = std::fs::write(&full, bytes);
                }
                None => {
                    let _ = std::fs::remove_file(&full);
                }
            }
        }
    }
}

/// Restores the working tree after a change set that must not be committed.
///
/// The transaction policy is the authority on the working tree, so it restores
/// the tree first and the local disk backup answers only when the policy
/// restored nothing: a policy that holds no transaction, or a rollback that
/// failed, leaves the pre-turn bytes to the backup. Writing the backup on top of
/// a restored tree would replace the developer's own uncommitted work with the
/// bytes the turn found, which is exactly what the checkpoint exists to protect.
async fn rollback_turn(ctx: &mut LoopContext<'_>, backup: &DiskBackup) {
    let repo_root = ctx.repo_root;
    let restored = match ctx.policy.rollback(repo_root).await {
        Ok(restored) => restored,
        Err(err) => {
            tracing::error!(
                error = %err,
                "failed to roll back the turn transaction; restoring the pre-turn bytes instead"
            );
            false
        }
    };
    if !restored {
        backup.rollback();
    }
    // The sweep only drops context paths that no longer exist on disk, so it
    // runs off the async executor thread. The auto layer is deliberately kept:
    // only a successful commit may shrink the context.
    crate::blocking::run_blocking(|| ctx.context_manager.prune_missing_files());
}

/// Runs the edit application, staging, toolchain verification, and healing loop.
pub async fn run_edit_loop(
    ctx: &mut LoopContext<'_>,
    initial_envelope: Option<ResponseEnvelope>,
    config: &EditLoopConfig,
) -> Result<LoopResult<EditArtifact>, SystemError> {
    ctx.check_cancelled()?;

    let opening_envelope = match initial_envelope {
        Some(env) => env,
        None => ctx.request_round().await?,
    };
    // The round that opens the edit phase declares what the change set
    // accomplishes. Capturing the summary now keeps it alive across healing
    // rounds, whose envelopes replace the opening one and would otherwise
    // lose the message the model wrote and hand the commit a synthetic label.
    let declared_summary = declared_commit_message(&opening_envelope);
    let mut current_envelope = opening_envelope;

    let project = if let Some(ref p) = ctx.prompt_builder.project_config {
        p.clone()
    } else {
        ctx.prompt_builder
            .input_cache
            .project(ctx.repo_root, Some(&ctx.cancel.receiver()))
            .await
    };
    let initial_worktree = crate::blocking::run_blocking(|| snapshot_worktree(ctx.repo_root));
    let mut backup = DiskBackup::new(ctx.repo_root);
    let mut total_modified_paths = Vec::new();
    let mut healing_attempt = 0usize;
    let mut last_failure_report: Option<String> = None;

    loop {
        ctx.check_cancelled()?;

        // Drain any research commands first if present
        let _ = handle_research_commands(ctx, &mut current_envelope, &config.research).await?;

        // 1. Virtual in-memory staging with patch retry
        let staging_result = run_staging_loop(ctx, current_envelope, &config.staging).await?;

        let staging_success = match staging_result {
            LoopResult::Success(success) => success,
            LoopResult::Diverged(envelope) => {
                if healing_attempt > 0 {
                    if let Some(ref report) = last_failure_report {
                        if healing_attempt >= config.max_healing_rounds {
                            rollback_turn(ctx, &backup).await;
                            return Ok(LoopResult::Exhausted(FailureReport::new(
                                "Code verification failed after healing attempts",
                                healing_attempt,
                                vec![report.clone()],
                            )));
                        }
                        healing_attempt += 1;
                        ctx.events.emit(StreamEvent::TurnPhase {
                            phase: TurnPhase::Healing,
                            round: Some(healing_attempt),
                            max_rounds: Some(config.max_healing_rounds),
                            detail: Some(format!(
                                "Healing round {healing_attempt}/{}",
                                config.max_healing_rounds
                            )),
                        });
                        let retry_prompt = build_verification_missing_edits_prompt(report);
                        current_envelope = ctx.request_healing(&retry_prompt).await?;
                        continue;
                    }
                }
                rollback_turn(ctx, &backup).await;
                return Ok(LoopResult::Diverged(envelope));
            }
            LoopResult::Exhausted(report) => {
                rollback_turn(ctx, &backup).await;
                return Ok(LoopResult::Exhausted(report));
            }
        };

        if !staging_success.value.abandoned_failures.is_empty() {
            let mut lines = Vec::new();
            for (path, reason) in &staging_success.value.abandoned_failures {
                lines.push(format!("- `{path}`: {reason}"));
            }
            let notice = format!(
                "\n\n---\n⚠️ **[Change Set Incomplete]** The corrected edits do not retry the following failed file(s), so they stay out of this change set:\n{}\n\n",
                lines.join("\n")
            );
            ctx.events.emit_text_delta(notice);
        }

        let mut staged_state = staging_success.value.staged_state;
        current_envelope = staging_success.response;

        // 2. Format staged files in memory if project formatter is configured
        format_staged_files(ctx.repo_root, &project, &mut staged_state, ctx.cancel).await;

        // 3. Atomically write staged edits to disk
        let touched_paths = collect_staged_paths(&staged_state);
        backup.record_paths(&touched_paths);

        let proposal = EditProposal {
            summary: format!("edit round {}", healing_attempt + 1),
            edits: staging_success.value.cumulative_edits.clone(),
        };

        let apply_res = ctx
            .policy
            .apply_temporary(ctx.repo_root, &project, ctx.context_manager, &proposal)
            .await;

        if let Err(err) = apply_res {
            rollback_turn(ctx, &backup).await;
            return Ok(LoopResult::Exhausted(FailureReport::new(
                format!("Failed to commit staged edits to disk: {err}"),
                healing_attempt,
                vec![err.to_string()],
            )));
        }

        ctx.events
            .emit(StreamEvent::ContextChanged(ctx.context_manager.get_state()));
        for path in touched_paths {
            if !total_modified_paths.contains(&path) {
                total_modified_paths.push(path);
            }
        }

        // Emit live cumulative diff against pre-turn baseline for all touched files
        emit_cumulative_diff(ctx, &backup);

        // 4. Run project post-hook if configured
        if project.post_hook.is_some() {
            let hook_res = run_post_hooks_with_config(
                ctx.repo_root,
                &project,
                &total_modified_paths,
                Some(&ctx.cancel.receiver()),
            )
            .await;

            match hook_res {
                Ok(resp) if !resp.success => {
                    let hook_error = resp.message.unwrap_or_else(|| "Post-hook failed".into());
                    let report = format!("Project post-hook failed:\n```\n{}\n```\n", hook_error);
                    auto_register_error_files(ctx, &report).await;

                    if healing_attempt >= config.max_healing_rounds {
                        rollback_turn(ctx, &backup).await;
                        return Ok(LoopResult::Exhausted(FailureReport::new(
                            "Project post-hook failed after healing attempts",
                            healing_attempt,
                            vec![report],
                        )));
                    }
                    healing_attempt += 1;
                    last_failure_report = Some(report.clone());
                    ctx.events.emit(StreamEvent::TurnPhase {
                        phase: TurnPhase::Healing,
                        round: Some(healing_attempt),
                        max_rounds: Some(config.max_healing_rounds),
                        detail: Some(format!(
                            "Post-hook healing round {healing_attempt}/{}",
                            config.max_healing_rounds
                        )),
                    });
                    let retry_prompt = build_verification_retry_prompt(&report);
                    current_envelope = ctx.request_healing(&retry_prompt).await?;
                    continue;
                }
                Ok(_) => {
                    let current_worktree =
                        crate::blocking::run_blocking(|| snapshot_worktree(ctx.repo_root));
                    let hook_artifacts =
                        changed_worktree_paths(&initial_worktree, &current_worktree);
                    backup.record_paths_from_git_baseline(&hook_artifacts);
                    for path in hook_artifacts {
                        if !total_modified_paths.contains(&path) {
                            total_modified_paths.push(path);
                        }
                    }
                    emit_cumulative_diff(ctx, &backup);
                }
                Err(err) => {
                    let report = format!("Project post-hook execution error:\n```\n{}\n```\n", err);
                    auto_register_error_files(ctx, &report).await;

                    if healing_attempt >= config.max_healing_rounds {
                        rollback_turn(ctx, &backup).await;
                        return Ok(LoopResult::Exhausted(FailureReport::new(
                            "Project post-hook error after healing attempts",
                            healing_attempt,
                            vec![report],
                        )));
                    }
                    healing_attempt += 1;
                    last_failure_report = Some(report.clone());
                    ctx.events.emit(StreamEvent::TurnPhase {
                        phase: TurnPhase::Healing,
                        round: Some(healing_attempt),
                        max_rounds: Some(config.max_healing_rounds),
                        detail: Some(format!(
                            "Post-hook healing round {healing_attempt}/{}",
                            config.max_healing_rounds
                        )),
                    });
                    let retry_prompt = build_verification_retry_prompt(&report);
                    current_envelope = ctx.request_healing(&retry_prompt).await?;
                    continue;
                }
            }
        }

        // 5. Run toolchain verification pipeline
        ctx.events.emit(StreamEvent::TurnPhase {
            phase: TurnPhase::Verification,
            round: Some(healing_attempt + 1),
            max_rounds: Some(config.max_healing_rounds + 1),
            detail: Some("Verifying project toolchain".into()),
        });
        ctx.events.emit(StreamEvent::ToolchainStarted {
            command: "verify".into(),
        });

        let (status, results) = run_verification_pipeline_with_config(
            ctx.repo_root,
            &project,
            &total_modified_paths,
            Some(&ctx.cancel.receiver()),
        )
        .await;

        if status == VerificationStatus::Cancelled || ctx.cancel.is_cancelled() {
            rollback_turn(ctx, &backup).await;
            ctx.events.emit(StreamEvent::ToolchainFinished {
                command: "verify".into(),
                success: false,
                message: Some("Verification cancelled".into()),
            });
            return Err(SystemError::Cancelled);
        }

        if status.allows_commit() {
            ctx.events.emit(StreamEvent::ToolchainFinished {
                command: "verify".into(),
                success: true,
                message: Some("Verification passed".into()),
            });

            // Re-observe worktree for any drift produced during verification (e.g. Cargo.lock)
            let current_worktree =
                crate::blocking::run_blocking(|| snapshot_worktree(ctx.repo_root));
            let verify_drift = changed_worktree_paths(&initial_worktree, &current_worktree);
            backup.record_paths_from_git_baseline(&verify_drift);
            for path in verify_drift {
                if !total_modified_paths.contains(&path) {
                    total_modified_paths.push(path);
                }
            }

            emit_cumulative_diff(ctx, &backup);
            let summary = declared_summary.as_deref().unwrap_or("REPL changes");
            // The change set is already on disk and verified, so a transaction
            // that could not be finalized must not hide behind a silent `None`
            // hash: the reason travels with the artifact and the verified files
            // stay in the working tree.
            let (commit_hash, commit_error) = match ctx
                .policy
                .apply_final(ctx.repo_root, &total_modified_paths, summary)
                .await
            {
                Ok(commit_hash) => (commit_hash, None),
                Err(err) => {
                    let message = format!(
                        "Changes were applied but the Git transaction could not be finalized: {err}"
                    );
                    tracing::error!(error = %err, "{}", message);
                    (None, Some(message))
                }
            };

            // A committed change set releases the whole transient context: the
            // files discovery gathered and the external documents the registry
            // materialized share one lifecycle, so neither outlives the edits it
            // served. Clearing it recomputes the hierarchical pinned files
            // through disk checks, so the sweep runs off the async executor
            // thread. A failed finalization keeps the material, because the
            // turn's decision is not durable yet.
            if commit_error.is_none() {
                crate::blocking::run_blocking(|| ctx.context_manager.clear_transient());
                ctx.events
                    .emit(StreamEvent::ContextChanged(ctx.context_manager.get_state()));
            }

            return Ok(LoopResult::Success(LoopSuccess::new(
                EditArtifact {
                    modified_paths: total_modified_paths,
                    verification: status.into(),
                    commit_hash,
                    commit_error,
                },
                current_envelope,
            )));
        }

        ctx.events.emit(StreamEvent::ToolchainFinished {
            command: "verify".into(),
            success: false,
            message: Some("Verification failed".into()),
        });

        // Format error report
        let report = format_verification_failure_report(&results);

        // Auto-register error files into editable context
        auto_register_error_files(ctx, &report).await;

        if healing_attempt >= config.max_healing_rounds {
            rollback_turn(ctx, &backup).await;
            return Ok(LoopResult::Exhausted(FailureReport::new(
                "Code verification failed after healing attempts",
                healing_attempt,
                vec![report],
            )));
        }

        healing_attempt += 1;
        last_failure_report = Some(report.clone());
        ctx.events.emit(StreamEvent::TurnPhase {
            phase: TurnPhase::Healing,
            round: Some(healing_attempt),
            max_rounds: Some(config.max_healing_rounds),
            detail: Some(format!(
                "Healing round {healing_attempt}/{}",
                config.max_healing_rounds
            )),
        });

        let retry_prompt = build_verification_retry_prompt(&report);
        current_envelope = ctx.request_healing(&retry_prompt).await?;
    }
}
/// Returns the commit summary a round declared in its terminal success.
///
/// The success message is the model's own account of the change set, so it
/// becomes the summary of the final commit verbatim. A round that stated no
/// message yields `None`, and the caller keeps its own fallback label rather
/// than inventing a summary the model never wrote.
fn declared_commit_message(envelope: &ResponseEnvelope) -> Option<String> {
    envelope
        .commands()
        .iter()
        .find_map(|command| match command {
            LlmCommand::CommitMessage(command) => Some(command.commit_message.trim())
                .filter(|text| !text.is_empty())
                .map(str::to_string),
            _ => None,
        })
}

fn collect_staged_paths(staged: &StagedEditsState) -> Vec<String> {
    let mut paths = Vec::new();
    for p in staged.staged_files.keys() {
        paths.push(p.clone());
    }
    for p in &staged.deleted_paths {
        if !paths.contains(p) {
            paths.push(p.clone());
        }
    }
    paths
}

async fn format_staged_files(
    repo_root: &Path,
    project: &tauqe_protocol::ProjectConfig,
    staged: &mut StagedEditsState,
    cancel: &crate::control::context::CancellationToken,
) {
    if project.format.is_none() {
        return;
    }
    for bytes in staged.staged_files.values_mut() {
        if let Ok(content_str) = std::str::from_utf8(bytes) {
            if let Some(outcome) =
                format_staged_content(repo_root, content_str, Some(&cancel.receiver())).await
            {
                if outcome.formatted {
                    *bytes = outcome.content.into_bytes();
                }
            }
        }
    }
}

fn emit_cumulative_diff(ctx: &LoopContext<'_>, backup: &DiskBackup) {
    let root = ctx.repo_root.to_path_buf();
    let files = backup.files.clone();
    let events =
        crate::blocking::run_blocking(move || compute_cumulative_diff_events(&root, &files));
    for ev in events {
        ctx.events.emit(ev);
    }
}

fn compute_cumulative_diff_events(
    root: &Path,
    backup_files: &HashMap<String, Option<Vec<u8>>>,
) -> Vec<StreamEvent> {
    let mut paths: Vec<&String> = backup_files.keys().collect();
    paths.sort();

    if paths.is_empty() {
        return Vec::new();
    }

    let mut events = Vec::new();
    events.push(StreamEvent::EditStarted);

    for path in paths {
        let original = backup_files.get(path).and_then(|opt| opt.as_deref());
        let full_path = root.join(path);
        let current = std::fs::read(&full_path).ok();

        if original == current.as_deref() {
            continue;
        }

        let op_type = match (original, current.as_deref()) {
            (None, Some(_)) => EditFileOp::Create,
            (Some(_), None) => EditFileOp::Delete,
            _ => EditFileOp::Replace,
        };

        events.push(StreamEvent::EditFileStarted {
            path: path.clone(),
            op_type,
        });

        let old_text = original.and_then(|b| std::str::from_utf8(b).ok());
        let new_text = current.as_deref().and_then(|b| std::str::from_utf8(b).ok());

        let hunks = match (old_text, new_text) {
            (Some(old_str), Some(new_str)) => compute_file_hunks(path, old_str, new_str, op_type),
            (None, Some(new_str)) if op_type == EditFileOp::Create => {
                compute_file_hunks(path, "", new_str, op_type)
            }
            (Some(old_str), None) if op_type == EditFileOp::Delete => {
                compute_file_hunks(path, old_str, "", op_type)
            }
            _ => Vec::new(),
        };

        let hunks_count = hunks.len();
        events.extend(hunks);

        events.push(StreamEvent::EditFileDone {
            path: path.clone(),
            status: tauqe_protocol::EditFileStatus::Ok,
            error: None,
            hunks_count,
        });
    }

    events
}

fn compute_file_hunks(
    path: &str,
    old_str: &str,
    new_str: &str,
    op_type: EditFileOp,
) -> Vec<StreamEvent> {
    if op_type == EditFileOp::Create {
        let new_lines = new_str.lines().count();
        return vec![StreamEvent::EditHunk {
            path: path.to_string(),
            hunk_index: 0,
            old_start: 0,
            old_lines: 0,
            new_start: if new_lines > 0 { 1 } else { 0 },
            new_lines,
            context_hint: None,
            old_text: String::new(),
            new_text: new_str.to_string(),
        }];
    }

    if op_type == EditFileOp::Delete {
        let old_lines = old_str.lines().count();
        return vec![StreamEvent::EditHunk {
            path: path.to_string(),
            hunk_index: 0,
            old_start: if old_lines > 0 { 1 } else { 0 },
            old_lines,
            new_start: 0,
            new_lines: 0,
            context_hint: None,
            old_text: old_str.to_string(),
            new_text: String::new(),
        }];
    }

    let diff = TextDiff::from_lines(old_str, new_str);
    let mut hunks = Vec::new();

    for (hunk_index, group) in diff.grouped_ops(3).into_iter().enumerate() {
        let Some(first_op) = group.first() else {
            continue;
        };
        let Some(last_op) = group.last() else {
            continue;
        };

        let old_start_idx = first_op.old_range().start;
        let old_end_idx = last_op.old_range().end;
        let new_start_idx = first_op.new_range().start;
        let new_end_idx = last_op.new_range().end;

        let old_lines_count = old_end_idx.saturating_sub(old_start_idx);
        let new_lines_count = new_end_idx.saturating_sub(new_start_idx);

        let old_start = if old_lines_count > 0 {
            old_start_idx + 1
        } else {
            0
        };
        let new_start = if new_lines_count > 0 {
            new_start_idx + 1
        } else {
            0
        };

        let mut old_chunk = String::new();
        let mut new_chunk = String::new();

        for op in &group {
            for change in diff.iter_changes(op) {
                match change.tag() {
                    ChangeTag::Equal => {
                        old_chunk.push_str(change.value());
                        new_chunk.push_str(change.value());
                    }
                    ChangeTag::Delete => {
                        old_chunk.push_str(change.value());
                    }
                    ChangeTag::Insert => {
                        new_chunk.push_str(change.value());
                    }
                }
            }
        }

        let context_hint = old_str
            .lines()
            .take(old_start_idx)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .find_map(|line| {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    Some(trimmed.chars().take(120).collect())
                } else {
                    None
                }
            });

        hunks.push(StreamEvent::EditHunk {
            path: path.to_string(),
            hunk_index,
            old_start,
            old_lines: old_lines_count,
            new_start,
            new_lines: new_lines_count,
            context_hint,
            old_text: old_chunk,
            new_text: new_chunk,
        });
    }

    hunks
}

fn format_verification_failure_report(
    results: &[crate::toolchain::ToolchainCheckResult],
) -> String {
    let mut report = String::new();
    for r in results.iter().filter(|r| !r.success) {
        let body = if r.diagnostics.is_empty() {
            r.combined_output.trim().to_string()
        } else {
            tauqe_protocol::format_diagnostics(&r.diagnostics)
        };
        report.push_str(&format!(
            "Command `{}` failed:\n```\n{}\n```\n\n",
            r.command, body
        ));
    }
    if report.is_empty() {
        "Verification command returned non-zero exit status".into()
    } else {
        report
    }
}

async fn auto_register_error_files(ctx: &mut LoopContext<'_>, report: &str) {
    let available_files = crate::workflow::research::resolve_repository_files(ctx.repo_root)
        .await
        .unwrap_or_default();
    let error_paths = extract_error_paths(report, &available_files);
    let added = crate::blocking::run_blocking(|| {
        let mut added = Vec::new();
        for path in error_paths {
            if ctx.context_manager.effective_access_for(&path) != Some(ContextAccess::Editable)
                && ctx
                    .context_manager
                    .add_auto_file(&path, ContextAccess::Editable)
                    .is_ok()
            {
                added.push(path);
            }
        }
        added
    });
    if !added.is_empty() {
        ctx.events
            .emit(StreamEvent::ContextChanged(ctx.context_manager.get_state()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ContextManager;
    use crate::control::context::{CancellationToken, EventSink};
    use crate::prompt::json::JsonPromptProtocol;
    use crate::providers::mock::{MockLlmProvider, MockResponse};
    use crate::workflow::research::journal::TurnResearchJournal;
    use tauqe_protocol::{
        CapabilityExpr, EditReplace, LlmCapability, LlmCommand, ModelRef, TextCommand,
    };
    struct Fixture {
        dir: tempfile::TempDir,
        context: ContextManager,
        journal: TurnResearchJournal,
        provider: MockLlmProvider,
        model: ModelRef,
        protocol: JsonPromptProtocol,
        cache: crate::control::cache::TurnInputCache,
        cancel: CancellationToken,
        events: EventSink,
        policy: crate::workflow::git_policy::GitTransactionPolicy,
    }

    impl Fixture {
        fn new(files: &[(&str, &str)], responses: Vec<MockResponse>) -> Self {
            let dir = tempfile::tempdir().expect("scratch directory");
            let root = dir.path().to_path_buf();
            crate::git::init_repository(Some(&root), true).expect("repository");
            for (path, content) in files {
                let full = root.join(path);
                if let Some(parent) = full.parent() {
                    std::fs::create_dir_all(parent).expect("dir");
                }
                std::fs::write(full, content).expect("file");
            }
            let mut context = ContextManager::new(root);
            for (path, _) in files {
                context
                    .add_file(path, ContextAccess::Editable)
                    .expect("context file");
            }
            Self {
                dir,
                context,
                journal: TurnResearchJournal::default(),
                provider: MockLlmProvider::scripted(responses),
                model: ModelRef::openrouter("mock/model"),
                protocol: JsonPromptProtocol::edits(),
                cache: crate::control::cache::TurnInputCache::default(),
                cancel: CancellationToken::new(),
                events: EventSink::noop(),
                policy: crate::workflow::git_policy::GitTransactionPolicy::new(),
            }
        }

        fn context(&mut self) -> LoopContext<'_> {
            let caps = CapabilityExpr::set(
                EDIT_CAPABILITIES
                    .iter()
                    .copied()
                    .chain([LlmCapability::TextMessage])
                    .map(CapabilityExpr::leaf),
            );
            let prompt_builder = crate::control::prompt::PromptBuilder::new(
                &self.provider,
                &self.model,
                &self.protocol,
                &mut self.cache,
            );
            LoopContext::new(
                &self.cancel,
                &self.events,
                self.dir.path(),
                &mut self.context,
                &mut self.journal,
                &mut self.policy,
                caps,
                prompt_builder,
            )
        }
    }

    #[tokio::test]
    async fn edit_loop_diverges_when_envelope_has_no_edits() {
        let mut fixture = Fixture::new(&[("a.rs", "pub fn test() {}")], Vec::new());
        let envelope = ResponseEnvelope::from_commands(&[LlmCommand::Text(TextCommand {
            content: "no edits here".to_string(),
        })]);
        let mut ctx = fixture.context();

        let outcome = run_edit_loop(&mut ctx, Some(envelope), &EditLoopConfig::default())
            .await
            .expect("runs loop");
        assert!(outcome.is_diverged());
    }

    #[tokio::test]
    async fn edit_loop_succeeds_when_verification_not_configured() {
        let mut fixture = Fixture::new(&[("a.rs", "fn compute() -> i32 { 0 }")], Vec::new());
        let envelope = ResponseEnvelope::from_commands(&[LlmCommand::EditReplace(EditReplace {
            path: "a.rs".to_string(),
            old_text: "fn compute() -> i32 { 0 }".to_string(),
            new_text: "fn compute() -> i32 { 42 }".to_string(),
        })]);
        let mut ctx = fixture.context();

        let outcome = run_edit_loop(&mut ctx, Some(envelope), &EditLoopConfig::default())
            .await
            .expect("runs loop");
        assert!(outcome.is_success());
        let success = outcome.into_success().expect("success");
        assert_eq!(success.value.modified_paths, vec!["a.rs"]);
        assert_eq!(
            std::fs::read_to_string(fixture.dir.path().join("a.rs")).unwrap(),
            "fn compute() -> i32 { 42 }"
        );
    }

    #[tokio::test]
    async fn edit_loop_emits_cumulative_diff_events() {
        let mut fixture = Fixture::new(&[("a.rs", "fn compute() -> i32 { 0 }\n")], Vec::new());
        let envelope = ResponseEnvelope::from_commands(&[LlmCommand::EditReplace(EditReplace {
            path: "a.rs".to_string(),
            old_text: "fn compute() -> i32 { 0 }\n".to_string(),
            new_text: "fn compute() -> i32 { 42 }\n".to_string(),
        })]);
        let (events, mut rx) = EventSink::channel();
        let mut cache = crate::control::cache::TurnInputCache::default();
        let caps = CapabilityExpr::set(EDIT_CAPABILITIES.iter().copied().map(CapabilityExpr::leaf));
        let prompt_builder = crate::control::prompt::PromptBuilder::new(
            &fixture.provider,
            &fixture.model,
            &fixture.protocol,
            &mut cache,
        );
        let mut policy = crate::workflow::git_policy::GitTransactionPolicy::new();
        let mut ctx = LoopContext::new(
            &fixture.cancel,
            &events,
            fixture.dir.path(),
            &mut fixture.context,
            &mut fixture.journal,
            &mut policy,
            caps,
            prompt_builder,
        );

        let outcome = run_edit_loop(&mut ctx, Some(envelope), &EditLoopConfig::default())
            .await
            .expect("runs loop");
        assert!(outcome.is_success());

        let mut received = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            received.push(ev);
        }

        let has_edit_started = received
            .iter()
            .any(|e| matches!(e, StreamEvent::EditStarted));
        let has_file_started = received.iter().any(|e| match e {
            StreamEvent::EditFileStarted { path, op_type } => {
                path == "a.rs" && *op_type == EditFileOp::Replace
            }
            _ => false,
        });
        let has_hunk = received.iter().any(|e| match e {
            StreamEvent::EditHunk {
                path,
                old_text,
                new_text,
                ..
            } => {
                path == "a.rs"
                    && old_text.contains("fn compute() -> i32 { 0 }")
                    && new_text.contains("fn compute() -> i32 { 42 }")
            }
            _ => false,
        });
        let has_file_done = received.iter().any(|e| match e {
            StreamEvent::EditFileDone { path, status, .. } => {
                path == "a.rs" && *status == tauqe_protocol::EditFileStatus::Ok
            }
            _ => false,
        });

        assert!(has_edit_started, "expected EditStarted event");
        assert!(has_file_started, "expected EditFileStarted event");
        assert!(has_hunk, "expected EditHunk event with cumulative patch");
        assert!(has_file_done, "expected EditFileDone event");
    }

    #[test]
    fn edit_loop_config_from_repl_config_inherits_max_retries() {
        let repl_config = crate::config::ReplConfig {
            max_retries: 5,
            ..Default::default()
        };
        let edit_config = EditLoopConfig::from_repl_config(&repl_config);
        assert_eq!(edit_config.max_healing_rounds, 5);
        assert_eq!(edit_config.staging.max_retries, 5);
        assert_eq!(
            edit_config.staging.patch_strategy,
            repl_config.patch_strategy
        );
    }

    #[tokio::test]
    async fn edit_loop_emits_change_set_incomplete_notice_on_abandoned_failures() {
        let fix_a_only = serde_json::json!({
            "message": "Edit",
            "changes": [{
                "op": "replace",
                "path": "a.rs",
                "to": "",
                "start": "",
                "end": "",
                "content": "",
                "old_text": "fn compute_a() -> i32 { 0 }\n",
                "new_text": "fn compute_a() -> i32 { 10 }\n"
            }],
            "commit_message": "Edit",
            "reason": ""
        })
        .to_string();
        let mut fixture = Fixture::new(
            &[
                ("a.rs", "fn compute_a() -> i32 { 0 }\n"),
                ("b.rs", "fn compute_b() -> i32 { 0 }\n"),
            ],
            vec![MockResponse::text(fix_a_only)],
        );
        let (events, mut rx) = EventSink::channel();
        let mut cache = crate::control::cache::TurnInputCache::default();
        let caps = CapabilityExpr::set(EDIT_CAPABILITIES.iter().copied().map(CapabilityExpr::leaf));
        let prompt_builder = crate::control::prompt::PromptBuilder::new(
            &fixture.provider,
            &fixture.model,
            &fixture.protocol,
            &mut cache,
        );
        let mut policy = crate::workflow::git_policy::GitTransactionPolicy::new();
        let mut ctx = LoopContext::new(
            &fixture.cancel,
            &events,
            fixture.dir.path(),
            &mut fixture.context,
            &mut fixture.journal,
            &mut policy,
            caps,
            prompt_builder,
        );
        let initial_envelope = ResponseEnvelope::from_commands(&[
            LlmCommand::EditReplace(EditReplace {
                path: "a.rs".to_string(),
                old_text: "nonexistent a".to_string(),
                new_text: "irrelevant".to_string(),
            }),
            LlmCommand::EditReplace(EditReplace {
                path: "b.rs".to_string(),
                old_text: "nonexistent b".to_string(),
                new_text: "irrelevant".to_string(),
            }),
        ]);

        let outcome = run_edit_loop(&mut ctx, Some(initial_envelope), &EditLoopConfig::default())
            .await
            .expect("runs loop");
        assert!(outcome.is_success());

        let mut notices = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let StreamEvent::TextDelta(t) = ev {
                if t.contains("Change Set Incomplete") {
                    notices.push(t);
                }
            }
        }
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("b.rs"));
    }

    #[tokio::test]
    async fn edit_loop_prompts_with_missing_edits_when_healing_round_returns_no_edits() {
        let mut fixture = Fixture::new(
            &[
                ("a.rs", "fn compute() -> i32 { 0 }\n"),
                ("tauqe.toml", "[project.verify]\ncommand = \"false\"\n"),
            ],
            vec![
                MockResponse::text("I understand the error, here is my explanation without edits."),
                MockResponse::text("Still just explaining."),
            ],
        );
        let initial_envelope =
            ResponseEnvelope::from_commands(&[LlmCommand::EditReplace(EditReplace {
                path: "a.rs".to_string(),
                old_text: "fn compute() -> i32 { 0 }\n".to_string(),
                new_text: "fn compute() -> i32 { 1 }\n".to_string(),
            })]);
        let mut ctx = fixture.context();
        let config = EditLoopConfig {
            max_healing_rounds: 2,
            ..EditLoopConfig::default()
        };

        let outcome = run_edit_loop(&mut ctx, Some(initial_envelope), &config)
            .await
            .expect("runs loop");
        assert!(outcome.is_exhausted());

        let requests = fixture.provider.requests();
        assert_eq!(requests.len(), 2);
        let first_prompt = requests[0]
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(first_prompt.contains("Code verification failed"));
        assert!(!first_prompt.contains("did not include any code edits"));

        let second_prompt = requests[1]
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(second_prompt.contains("did not include any code edits"));
    }
}
