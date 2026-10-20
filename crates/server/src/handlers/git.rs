use std::path::Path;
use std::sync::Arc;

use tauqe_core::providers::{create_provider, ChatMessage, LlmProvider, StreamEvent};
use tauqe_protocol::{
    errors, events, Event, GitDiffParams, GitDiffResult, ModelRole, RepositoryListFilesResult,
    Request, Response,
};

use crate::state::AppState;
use crate::usage::UsageGuard;

/// Distinguishes a failure while reading the commit list from a failure while
/// squashing, so each maps to the correct client error code.
enum SquashApplyError {
    Commits(anyhow::Error),
    Squash(anyhow::Error),
}

pub async fn handle_repository_get_state(req: Request, state: &Arc<AppState>) -> Response {
    let repo_root = state.repo_root().await;
    match super::offload(move || tauqe_core::git::get_repository_state(Some(repo_root.as_path())))
        .await
    {
        Ok(repo_state) => Response::ok_typed(req.id, &repo_state),
        Err(err) => Response::err(req.id, errors::REPOSITORY_STATE_FAILED, err),
    }
}

pub async fn handle_repository_init(req: Request, state: &Arc<AppState>) -> Response {
    if let Some(resp) = super::reject_if_operation_active(
        &req.id,
        state,
        "Cannot initialize a repository while a model operation is in progress",
    ) {
        return resp;
    }

    let params: tauqe_protocol::RepositoryInitParams = req
        .params
        .and_then(|p| serde_json::from_value(p).ok())
        .unwrap_or_default();

    let initial_commit = params.initial_commit;
    // The repository must be initialized at the authoritative shared root, not
    // the daemon's process CWD, which can drift from the initialized project.
    let init_root = state.repo_root().await;
    let init_result = super::offload(move || {
        tauqe_core::git::init_repository(Some(init_root.as_path()), initial_commit)
    })
    .await;

    match init_result {
        Ok(Ok(repo_state)) => {
            // A freshly initialized project may now carry its own `tauqe.toml`,
            // so configuration, context, and history are rebound to the new
            // root exactly as `client/initialize` does.
            super::rebind_to_repository(state, Path::new(&repo_state.root)).await;

            state.out.send_event(&Event {
                method: events::GIT_STATE_CHANGED.to_string(),
                params: Some(serde_json::json!({ "repository": &repo_state })),
            });

            let result = tauqe_protocol::RepositoryInitResult {
                repository: repo_state,
            };
            Response::ok_typed(req.id, &result)
        }
        Ok(Err(err)) => Response::err(req.id, errors::INIT_FAILED, err.to_string()),
        Err(err) => Response::err(req.id, errors::INIT_FAILED, err),
    }
}

pub async fn handle_repository_list_files(req: Request, state: &Arc<AppState>) -> Response {
    let repo_root = state.repo_root().await;
    let files =
        super::offload(move || tauqe_core::git::list_repository_files(Some(repo_root.as_path())))
            .await;

    match files {
        Ok(Ok(files)) => Response::ok_typed(req.id, &RepositoryListFilesResult { files }),
        Ok(Err(err)) => Response::err(req.id, errors::LIST_FILES_FAILED, err.to_string()),
        Err(err) => Response::err(req.id, errors::LIST_FILES_FAILED, err),
    }
}

pub async fn handle_git_get_diff(req: Request, state: &Arc<AppState>) -> Response {
    let params: Option<GitDiffParams> = req.params.and_then(|p| serde_json::from_value(p).ok());
    let target_path = params.and_then(|p| p.path);

    let repo_root = state.repo_root().await;
    let diff = super::offload(move || {
        tauqe_core::git::get_diff(repo_root.as_path(), target_path.as_deref())
    })
    .await;

    match diff {
        Ok(Ok(diff)) => Response::ok_typed(req.id, &GitDiffResult { diff }),
        Ok(Err(err)) => Response::err(req.id, errors::GET_DIFF_FAILED, err.to_string()),
        Err(err) => Response::err(req.id, errors::GET_DIFF_FAILED, err),
    }
}

pub async fn handle_git_squash_preview(req: Request, state: &Arc<AppState>) -> Response {
    if state.is_operation_active() {
        return Response::err(
            req.id,
            errors::OPERATION_IN_PROGRESS,
            "Cannot squash commits while a model operation is in progress",
        );
    }

    let params: Option<tauqe_protocol::GitSquashPreviewParams> =
        req.params.and_then(|p| serde_json::from_value(p).ok());

    let repo_root = state.repo_root().await;
    let configured_upstream = state.config.lock().await.config.git.upstream.clone();
    let requested_base = params
        .and_then(|p| p.base_ref)
        .filter(|b| !b.trim().is_empty());

    let preview = super::offload(move || {
        let session_base =
            tauqe_core::git::find_last_non_ai_commit(repo_root.as_path()).unwrap_or(None);
        let upstream_base = tauqe_core::git::detect_upstream_branch(
            repo_root.as_path(),
            configured_upstream.as_deref(),
        );

        let base_ref = requested_base
            .clone()
            .or_else(|| session_base.clone())
            .or_else(|| upstream_base.clone())
            .unwrap_or_else(|| "HEAD~1".to_string());

        // Propagate git failures instead of hiding them behind an empty
        // preview; a swallowed error would silently corrupt the squash scope.
        let commits_ahead = tauqe_core::git::get_commits_ahead(repo_root.as_path(), &base_ref)?;
        let diff_stat = tauqe_core::git::get_diff_stat(repo_root.as_path(), &base_ref)?;
        let files = tauqe_core::git::get_cumulative_file_diffs(repo_root.as_path(), &base_ref)?;

        Ok::<_, anyhow::Error>((
            base_ref,
            session_base,
            upstream_base,
            commits_ahead,
            diff_stat,
            files,
        ))
    })
    .await;

    let (base_ref, session_base, upstream_base, commits_ahead, diff_stat, files) = match preview {
        Ok(Ok(preview)) => preview,
        Ok(Err(err)) => {
            return Response::err(req.id, errors::SQUASH_PREVIEW_FAILED, err.to_string());
        }
        Err(err) => return Response::err(req.id, errors::SQUASH_PREVIEW_FAILED, err),
    };

    let items: Vec<tauqe_protocol::GitSquashCommitItem> = commits_ahead
        .into_iter()
        .map(|c| tauqe_protocol::GitSquashCommitItem {
            hash: c.hash,
            author: c.author,
            date: c.date,
            subject: c.subject,
        })
        .collect();

    let preview_res = tauqe_protocol::GitSquashPreviewResult {
        base_ref,
        session_base,
        upstream_base,
        commits: items,
        diff_stat,
        files,
        suggested_message: None,
    };

    Response::ok_typed(req.id, &preview_res)
}

pub async fn handle_git_squash_generate_message(req: Request, state: &Arc<AppState>) -> Response {
    let params: tauqe_protocol::GitSquashGenerateMessageParams =
        match req.params.and_then(|p| serde_json::from_value(p).ok()) {
            Some(p) => p,
            None => {
                return Response::err(req.id, errors::INVALID_PARAMS, "Missing base_ref parameter");
            }
        };

    let repo_root = state.repo_root().await;
    let base_ref = params.base_ref.clone();

    let git_data = super::offload(move || {
        let commits = tauqe_core::git::get_commits_ahead(repo_root.as_path(), &base_ref);
        let diff = tauqe_core::git::get_cumulative_diff(repo_root.as_path(), &base_ref)
            .unwrap_or_default();
        (commits, diff)
    })
    .await;

    let (commits_ahead, cumulative_diff) = match git_data {
        Ok((Ok(commits), diff)) => (commits, diff),
        Ok((Err(err), _)) => {
            return Response::err(
                req.id,
                errors::COMMITS_AHEAD_FAILED,
                format!(
                    "Failed to get commits ahead of {}: {}",
                    params.base_ref, err
                ),
            );
        }
        Err(err) => return Response::err(req.id, errors::COMMITS_AHEAD_FAILED, err),
    };

    let (ctx_snapshot, pinned_paths) = {
        let ctx = state.context.read().await;
        let state_ctx = ctx.get_state();
        let pinned_paths: std::collections::HashSet<String> = state_ctx
            .items
            .into_iter()
            .filter(|i| i.layer == tauqe_protocol::ContextLayer::Pinned)
            .map(|i| i.path)
            .collect();
        (ctx.clone(), pinned_paths)
    };

    let pinned_files = super::offload(move || {
        let all_files = ctx_snapshot.read_context_files();
        all_files
            .into_iter()
            .filter(|f| pinned_paths.contains(&f.path))
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();

    let prompt = tauqe_core::prompt::build_squash_commit_prompt(
        &commits_ahead,
        &cumulative_diff,
        &pinned_files,
    );

    let (provider_res, model) = {
        let cfg = state.config.lock().await.config.clone();
        let m = cfg.resolve_role(ModelRole::Scribe);
        let p = resolve_squash_provider(state, &cfg, m.provider).await;
        (p, m)
    };

    let (message, usage_snapshot) = if let Ok(provider) = provider_res {
        let operation = match state.try_begin_operation() {
            Ok(guard) => guard,
            Err(()) => {
                return Response::err(
                    req.id,
                    errors::OPERATION_IN_PROGRESS,
                    "Cannot generate a squash message while a model operation is in progress",
                );
            }
        };
        // The identifier and the usage window exist before the first streamed
        // byte, so a cancellation delivered before the stream starts still
        // closes exactly one accounted operation.
        let op_id = format!("op-{}", crate::state::next_operation_id());
        let usage_guard = state.begin_usage().await;
        let cancel_rx = operation.cancel_receiver();
        // The signal is also observed after the event loop, so a provider that
        // stops streaming on cancellation without reporting it is still
        // recognized as a cancelled turn.
        let cancel_observer = cancel_rx.clone();
        let (tx, mut rx) = tokio::sync::mpsc::channel(50);
        let msgs = vec![ChatMessage::user(&prompt)];
        let model_name = model.name.clone();
        let response_format = tauqe_core::prompt::squash_commit_response_format();
        let stream_task = tokio::spawn(async move {
            provider
                .stream_chat(&model_name, msgs, Some(response_format), tx, cancel_rx)
                .await
        });
        // The handle lets the last-resort watchdog interrupt a provider request
        // that never observes the cancellation signal.
        operation.set_abort_handle(stream_task.abort_handle());
        let mut full_text = String::new();
        let mut stream_error: Option<String> = None;
        let mut cancelled = false;
        while let Some(ev) = rx.recv().await {
            match ev {
                StreamEvent::TextDelta(delta) => {
                    full_text.push_str(&delta);
                }
                // Providers may report usage on intermediate chunks. It stays
                // in the in-flight window and is folded into the session totals
                // only by the single terminal flush below, so an intermediate
                // report can never be counted twice.
                StreamEvent::Usage(reported) => {
                    usage_guard.observe(&reported).await;
                }
                StreamEvent::Error(msg) => {
                    stream_error = Some(msg);
                    break;
                }
                // An explicit provider cancellation is reported as `CANCELLED`
                // instead of being swallowed by the catch-all arm and surfacing
                // later as a generic internal failure.
                StreamEvent::Cancelled => {
                    cancelled = true;
                    break;
                }
                _ => {}
            }
        }

        // The signal itself is authoritative: a provider may end the stream on
        // cancellation without ever reporting a cancellation event.
        let cancelled = cancelled || *cancel_observer.borrow();

        // Surface both a cancelled turn and a streamed provider error so a
        // broken or interrupted provider never masquerades as a successful
        // fallback message. The abandoned request is interrupted and awaited
        // before the outcome is reported, so no provider call outlives the
        // response.
        if cancelled || stream_error.is_some() {
            stream_task.abort();
            let _ = stream_task.await;
            operation.clear_abort_handle();
            close_squash_usage(state, usage_guard, &op_id).await;
            let (code, message) = if cancelled {
                (
                    errors::CANCELLED,
                    "Squash message generation was cancelled".to_string(),
                )
            } else {
                (
                    errors::INTERNAL_ERROR,
                    stream_error.unwrap_or_else(|| "Squash message stream failed".to_string()),
                )
            };
            return Response::err(req.id, code, message);
        }

        let stream_result = stream_task.await;
        operation.clear_abort_handle();
        match stream_result {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                close_squash_usage(state, usage_guard, &op_id).await;
                return Response::err(req.id, errors::INTERNAL_ERROR, err.to_string());
            }
            Err(join_err) => {
                close_squash_usage(state, usage_guard, &op_id).await;
                return Response::err(
                    req.id,
                    errors::INTERNAL_ERROR,
                    format!("Squash message stream task failed: {}", join_err),
                );
            }
        }
        let msg =
            tauqe_core::prompt::parse_squash_commit_message(&full_text).unwrap_or_else(|| {
                commits_ahead
                    .first()
                    .map(|c| c.subject.clone())
                    .unwrap_or_else(|| "Squashed commits".to_string())
            });
        let snapshot = close_squash_usage(state, usage_guard, &op_id).await;
        (msg, Some(snapshot))
    } else {
        // Without a provider no operation ever opens, so the zero operation
        // neither reaches the session totals nor increments the operation count.
        (
            commits_ahead
                .first()
                .map(|c| c.subject.clone())
                .unwrap_or_else(|| "Squashed commits".to_string()),
            None,
        )
    };

    // The legacy fields are projected from the authoritative snapshot — the
    // last completed operation and the session total — instead of a locally
    // re-summed counter, so both channels always agree.
    let result = match usage_snapshot {
        Some(snapshot) => {
            let totals = snapshot.operation_totals().clone();
            tauqe_protocol::GitSquashGenerateMessageResult {
                message,
                usage: Some(totals.to_usage_info()),
                session_total_cost: Some(snapshot.session.cost),
                current_cost: Some(totals.cost),
            }
        }
        None => tauqe_protocol::GitSquashGenerateMessageResult {
            message,
            usage: None,
            session_total_cost: None,
            current_cost: None,
        },
    };
    Response::ok_typed(req.id, &result)
}

/// Resolves the LLM provider for a squash message.
///
/// Deterministic unit tests inject a scripted provider through the state's test
/// seam; production builds construct a real network provider from configuration.
async fn resolve_squash_provider(
    _state: &Arc<AppState>,
    config: &tauqe_core::config::AppConfig,
    kind: tauqe_protocol::ProviderKind,
) -> Result<Arc<dyn LlmProvider>, String> {
    #[cfg(test)]
    {
        if let Some(provider) = _state.provider_override.lock().await.clone() {
            return Ok(provider);
        }
    }
    create_provider(kind, config)
        .map(Arc::from)
        .map_err(|err| err.to_string())
}

/// Closes the squash operation's usage window and publishes the snapshot.
///
/// The flush is the single point where the operation folds into the session
/// totals, so the cancelled, failed, and successful paths all reach the
/// accumulator exactly once. The legacy per-operation usage event is mirrored
/// from the same snapshot.
async fn close_squash_usage(
    state: &Arc<AppState>,
    guard: UsageGuard,
    op_id: &str,
) -> tauqe_protocol::UsageTotalsSnapshot {
    guard.flush().await;
    let snapshot = state.usage.lock().await.clone();
    let totals = snapshot.operation_totals().clone();
    state.out.send_event(&Event {
        method: events::MODEL_USAGE.to_string(),
        params: serde_json::to_value(tauqe_protocol::ModelUsageEvent {
            operation_id: op_id.to_string(),
            usage: totals.to_usage_info(),
            session_total_cost: snapshot.session.cost,
            current_cost: Some(totals.cost),
        })
        .ok(),
    });
    snapshot
}

pub async fn handle_git_squash_apply(req: Request, state: &Arc<AppState>) -> Response {
    if state.is_operation_active() {
        return Response::err(
            req.id,
            errors::OPERATION_IN_PROGRESS,
            "Cannot squash commits while a model operation is in progress",
        );
    }

    let params: tauqe_protocol::GitSquashApplyParams =
        match req.params.and_then(|p| serde_json::from_value(p).ok()) {
            Some(p) => p,
            None => {
                return Response::err(
                    req.id,
                    errors::INVALID_PARAMS,
                    "Missing base_ref or message",
                );
            }
        };

    // Squashing rewrites history, so the client-supplied inputs are validated
    // before any git mutation: an empty message or a revision that looks like
    // an option (leading '-') is rejected instead of being forwarded to git.
    if params.message.trim().is_empty() {
        return Response::err(
            req.id,
            errors::INVALID_PARAMS,
            "Commit message cannot be empty",
        );
    }
    let trimmed_base_ref = params.base_ref.trim();
    if trimmed_base_ref.is_empty() || trimmed_base_ref.starts_with('-') {
        return Response::err(
            req.id,
            errors::INVALID_PARAMS,
            "Invalid base_ref: expected a non-empty revision not starting with '-'",
        );
    }

    let repo_root = state.repo_root().await;
    let base_ref = trimmed_base_ref.to_string();
    let message = params.message.clone();

    let applied = super::offload(move || {
        match tauqe_core::git::get_commits_ahead(repo_root.as_path(), &base_ref) {
            Ok(commits_ahead) => {
                let count = commits_ahead.len();
                match tauqe_core::git::squash_to_single_commit(
                    repo_root.as_path(),
                    &base_ref,
                    &message,
                ) {
                    Ok(hash) => Ok((count, hash)),
                    Err(err) => Err(SquashApplyError::Squash(err)),
                }
            }
            Err(err) => Err(SquashApplyError::Commits(err)),
        }
    })
    .await;

    let (commits_count, squashed_hash) = match applied {
        Ok(Ok(pair)) => pair,
        Ok(Err(SquashApplyError::Commits(err))) => {
            return Response::err(
                req.id,
                errors::COMMITS_AHEAD_FAILED,
                format!(
                    "Failed to get commits ahead of {}: {}",
                    params.base_ref, err
                ),
            );
        }
        Ok(Err(SquashApplyError::Squash(err))) => {
            return Response::err(req.id, errors::SQUASH_FAILED, err.to_string());
        }
        Err(err) => return Response::err(req.id, errors::SQUASH_FAILED, err),
    };

    // The squash moved HEAD, so the repository state is read again on the
    // blocking pool from the authoritative root.
    let repo_root = state.repo_root().await;
    let new_repo_state = match super::offload(move || {
        tauqe_core::git::get_repository_state(Some(repo_root.as_path()))
    })
    .await
    {
        Ok(repo_state) => repo_state,
        Err(err) => return Response::err(req.id, errors::SQUASH_FAILED, err),
    };

    let first_line = params
        .message
        .lines()
        .next()
        .unwrap_or("Squash commits")
        .to_string();
    let msg_text = format!(
        "Squashed {} commits ahead of {} into {}.",
        commits_count, params.base_ref, squashed_hash
    );

    // Recording the response appends to `history.jsonl` behind an `fsync`
    // barrier, so the mutation runs on the blocking pool with the exclusive
    // history guard held instead of stalling an async worker.
    let response_commit = squashed_hash.clone();
    let recorded = crate::handlers::with_history_mutation(state, move |history| {
        history.record_response(
            &msg_text,
            Some(response_commit),
            Some(first_line),
            Vec::new(),
        )
    })
    .await;
    if let Err(err) = recorded {
        tracing::warn!(
            error = %err,
            "Failed to record the squashed commit in the interaction history"
        );
    }

    state.out.send_event(&Event {
        method: events::GIT_STATE_CHANGED.to_string(),
        params: Some(serde_json::json!({ "repository": new_repo_state })),
    });

    let apply_result = tauqe_protocol::GitSquashApplyResult {
        squashed_commit: squashed_hash,
        message: params.message,
        base_ref: params.base_ref,
    };

    state.out.send_event(&Event {
        method: events::GIT_SQUASH_COMPLETED.to_string(),
        params: serde_json::to_value(&apply_result).ok(),
    });

    Response::ok_typed(req.id, &apply_result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_core::config::AppConfig;
    use tauqe_core::providers::mock::{MockLlmProvider, MockResponse};
    use tauqe_protocol::{methods, RepositoryInitParams, RequestId};

    /// Initialization must target the authoritative shared root and never the
    /// process CWD: a repository created under the wrong directory silently
    /// desynchronizes every later handler that resolves paths from
    /// `state.repo_root()`. The initialized project must also have its
    /// configuration reloaded and rebound.
    #[tokio::test]
    async fn repository_init_targets_authoritative_root_not_cwd() {
        let dir = std::env::temp_dir().join(format!(
            "tauqe-test-init-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::create_dir_all(&dir);

        let (out, _rx) = crate::transport::OutChannel::new();
        let (history_sender, _hrx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(AppConfig::default(), dir.clone(), history_sender, out);

        let req = Request {
            id: RequestId::Number(1),
            method: methods::REPOSITORY_INIT.to_string(),
            params: Some(
                serde_json::to_value(RepositoryInitParams {
                    initial_commit: false,
                })
                .unwrap(),
            ),
        };

        let resp = handle_repository_init(req, &state).await;
        assert!(resp.error.is_none());
        // The Git directory is created at the authoritative root, not the CWD.
        assert!(dir.join(".git").exists());
        assert_eq!(state.repo_root().await, dir);
        // The history manager is rebuilt against the new root instead of being
        // left anchored to the previous one.
        assert_eq!(state.history.lock().await.repo_root(), dir.as_path());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Temporary git repository carrying one commit, so a squash request can
    /// resolve its base revision.
    struct SquashTestRepo(std::path::PathBuf);

    impl SquashTestRepo {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "tauqe-squash-usage-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::create_dir_all(&root);
            tauqe_core::git::init_repository(Some(&root), true).unwrap();
            std::fs::write(root.join("README.md"), "# tauqe\n").unwrap();
            tauqe_core::git::cmd::stage_files(&root, &["README.md".to_string()]).unwrap();
            tauqe_core::git::cmd::commit_with_developer_author(&root, "Add readme", false).unwrap();
            Self(root)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for SquashTestRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn squash_message_request() -> Request {
        Request {
            id: RequestId::Number(1),
            method: methods::GIT_SQUASH_GENERATE_MESSAGE.to_string(),
            params: Some(serde_json::json!({ "base_ref": "HEAD" })),
        }
    }

    /// A generated squash message is one accounted operation: its usage folds
    /// into the session totals exactly once, the window closes, and the legacy
    /// result fields are projected from the same snapshot.
    #[tokio::test]
    async fn squash_message_generation_is_one_completed_operation() {
        let repo = SquashTestRepo::new();
        let root = repo.path().to_path_buf();

        let usage = tauqe_protocol::ModelUsageInfo {
            prompt_tokens: 30,
            completion_tokens: 10,
            total_tokens: 40,
            cost: Some(0.25),
            ..tauqe_protocol::ModelUsageInfo::default()
        };
        let provider: Arc<dyn LlmProvider> =
            Arc::new(MockLlmProvider::scripted([MockResponse::text(
                "squash the session history",
            )
            .with_usage(usage)]));

        let (out, _rx) = crate::transport::OutChannel::new();
        let (history_sender, _history_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut config = AppConfig::default();
        config.models.scribe = Some(tauqe_protocol::ModelRef::openrouter("test/scribe"));
        let state = AppState::new(config, root, history_sender, out);
        *state.provider_override.lock().await = Some(provider);

        let resp = handle_git_squash_generate_message(squash_message_request(), &state).await;
        assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);

        let result: tauqe_protocol::GitSquashGenerateMessageResult =
            serde_json::from_value(resp.result.unwrap()).unwrap();
        assert!(!result.message.is_empty(), "a message is always returned");
        assert_eq!(result.usage.as_ref().map(|u| u.total_tokens), Some(40));
        assert_eq!(result.session_total_cost, Some(0.25));
        assert_eq!(result.current_cost, Some(0.25));

        let snapshot = state.usage.lock().await.clone();
        assert_eq!(snapshot.operations, 1, "one terminal flush per operation");
        assert!(snapshot.cur.is_none(), "the flush closes the usage window");
        assert_eq!(snapshot.session.total_tokens, 40);
        assert_eq!(snapshot.prev.total_tokens, 40);
        assert!((snapshot.session.cost - 0.25).abs() < f64::EPSILON);
        assert!(
            !state.is_operation_active(),
            "the operation slot is released"
        );
    }

    /// Without a provider no operation opens, so nothing reaches the session
    /// totals, the operation count stays zero, and the response carries no
    /// usage at all.
    #[tokio::test]
    async fn squash_message_without_provider_accounts_no_operation() {
        let repo = SquashTestRepo::new();
        let root = repo.path().to_path_buf();

        let (out, _rx) = crate::transport::OutChannel::new();
        let (history_sender, _history_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(AppConfig::default(), root, history_sender, out);

        let resp = handle_git_squash_generate_message(squash_message_request(), &state).await;
        assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);

        let result: tauqe_protocol::GitSquashGenerateMessageResult =
            serde_json::from_value(resp.result.unwrap()).unwrap();
        assert!(result.usage.is_none());
        assert!(result.session_total_cost.is_none());
        assert!(result.current_cost.is_none());

        let snapshot = state.usage.lock().await.clone();
        assert_eq!(snapshot.operations, 0, "a zero operation never counts");
        assert!(snapshot.session.is_empty());
        assert!(snapshot.prev.is_empty());
        assert!(snapshot.cur.is_none());
        assert!(!state.is_operation_active());
    }
}
