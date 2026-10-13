use std::path::Path;
use std::sync::Arc;

use tauqe_core::providers::{create_provider, ChatMessage, StreamEvent};
use tauqe_protocol::{
    errors, events, Event, GitDiffParams, GitDiffResult, ModelRole, RepositoryListFilesResult,
    Request, Response,
};

use crate::state::AppState;

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
    let params: tauqe_protocol::RepositoryInitParams = req
        .params
        .and_then(|p| serde_json::from_value(p).ok())
        .unwrap_or_default();

    let initial_commit = params.initial_commit;
    let init_result =
        super::offload(move || tauqe_core::git::init_repository(None, initial_commit)).await;

    match init_result {
        Ok(Ok(repo_state)) => {
            let repo_path = Path::new(&repo_state.root);
            if !repo_path.as_os_str().is_empty() {
                {
                    let mut ctx = state.context.write().await;
                    ctx.set_repo_root(repo_path.to_path_buf());
                }
                let mut hm = tauqe_core::history::HistoryManager::new(repo_path.to_path_buf());
                let sender = state.history_sender.clone();
                hm.set_listener(move |item| {
                    let _ = sender.send(item);
                });
                let mut history = state.history.lock().await;
                *history = hm;
            }

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
    let files = super::offload(move || {
        tauqe_core::git::list_repository_files(Some(repo_root.as_path()))
    })
    .await;

    match files {
        Ok(Ok(files)) => Response::ok_typed(req.id, &RepositoryListFilesResult { files }),
        Ok(Err(err)) => Response::err(req.id, errors::LIST_FILES_FAILED, err.to_string()),
        Err(err) => Response::err(req.id, errors::LIST_FILES_FAILED, err),
    }
}

pub async fn handle_git_undo(req: Request, state: &Arc<AppState>) -> Response {
    if state.is_operation_active() {
        return Response::err(
            req.id,
            errors::OPERATION_IN_PROGRESS,
            "Cannot undo while a model operation is in progress",
        );
    }

    let repo_root = state.repo_root().await;

    // The undo itself is a git transaction; re-reading the repository state
    // afterwards is also blocking, so both run on the same blocking task.
    let undo = super::offload(move || {
        let undo_result = tauqe_core::git::undo_last_ai_commit(&repo_root);
        let repo_state = tauqe_core::git::get_repository_state(Some(repo_root.as_path()));
        (undo_result, repo_state)
    })
    .await;

    let (undo_result, new_repo_state) = match undo {
        Ok((Ok(undo_result), repo_state)) => (undo_result, repo_state),
        Ok((Err(err), _)) => {
            return Response::err(req.id, errors::UNDO_FAILED, err.to_string());
        }
        Err(err) => return Response::err(req.id, errors::UNDO_FAILED, err),
    };

    let ctx_state = {
        {
            let mut history = state.history.lock().await;
            let _ = history.record_undo(
                &undo_result.undone_commit,
                undo_result.restored_checkpoint,
            );
        }
        let mut ctx = state.context.write().await;
        ctx.prune_missing_files();
        ctx.get_state()
    };

    state.out.send_event(&Event {
        method: events::GIT_STATE_CHANGED.to_string(),
        params: Some(serde_json::json!({ "repository": new_repo_state })),
    });

    state.out.send_event(&Event {
        method: events::CONTEXT_CHANGED.to_string(),
        params: Some(serde_json::json!({ "state": ctx_state })),
    });

    state.out.send_event(&Event {
        method: events::GIT_UNDO_COMPLETED.to_string(),
        params: serde_json::to_value(&undo_result).ok(),
    });

    Response::ok_typed(req.id, &undo_result)
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
                return Response::err(
                    req.id,
                    errors::INVALID_PARAMS,
                    "Missing base_ref parameter",
                );
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
                format!("Failed to get commits ahead of {}: {}", params.base_ref, err),
            );
        }
        Err(err) => return Response::err(req.id, errors::COMMITS_AHEAD_FAILED, err),
    };

    let pinned_files = {
        let ctx = state.context.read().await;
        let state_ctx = ctx.get_state();
        let pinned_paths: std::collections::HashSet<String> = state_ctx
            .items
            .into_iter()
            .filter(|i| i.layer == tauqe_protocol::ContextLayer::Pinned)
            .map(|i| i.path)
            .collect();
        let all_files = ctx.read_context_files();
        all_files
            .into_iter()
            .filter(|f| pinned_paths.contains(&f.path))
            .collect::<Vec<_>>()
    };

    let prompt = tauqe_core::prompt::build_squash_commit_prompt(
        &commits_ahead,
        &cumulative_diff,
        &pinned_files,
    );

    let (provider_res, model) = {
        let cfg = state.config.lock().await.config.clone();
        let m = cfg.resolve_role(ModelRole::Scribe);
        let p = create_provider(m.provider, &cfg);
        (p, m)
    };

    let (message, usage_info) = if let Ok(provider) = provider_res {
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
        let cancel_rx = operation.cancel_receiver();
        let (tx, mut rx) = tokio::sync::mpsc::channel(50);
        let msgs = vec![ChatMessage::user(&prompt)];
        let model_name = model.name.clone();
        let stream_task =
            tokio::spawn(
                async move { provider.stream_text(&model_name, msgs, tx, cancel_rx).await },
            );
        let mut full_text = String::new();
        let mut usage: Option<tauqe_protocol::ModelUsageInfo> = None;
        let mut stream_error: Option<String> = None;
        while let Some(ev) = rx.recv().await {
            match ev {
                StreamEvent::TextDelta(delta) => {
                    full_text.push_str(&delta);
                }
                StreamEvent::Usage(u) => {
                    usage = Some(u);
                }
                StreamEvent::Error(msg) => {
                    stream_error = Some(msg);
                }
                _ => {}
            }
        }

        // Surface both a streamed provider error and a failed streaming task so
        // a broken provider never masquerades as a successful fallback message.
        if let Some(msg) = stream_error {
            return Response::err(req.id, errors::INTERNAL_ERROR, msg);
        }
        match stream_task.await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                return Response::err(req.id, errors::INTERNAL_ERROR, err.to_string());
            }
            Err(join_err) => {
                return Response::err(
                    req.id,
                    errors::INTERNAL_ERROR,
                    format!("Squash message stream task failed: {}", join_err),
                );
            }
        }
        let trimmed = full_text.trim();
        let cleaned = trimmed
            .strip_prefix("```")
            .and_then(|s| s.strip_suffix("```"))
            .unwrap_or(trimmed)
            .trim()
            .to_string();
        let msg = if cleaned.is_empty() {
            commits_ahead
                .first()
                .map(|c| c.subject.clone())
                .unwrap_or_else(|| "Squashed commits".to_string())
        } else {
            cleaned
        };
        (msg, usage)
    } else {
        (
            commits_ahead
                .first()
                .map(|c| c.subject.clone())
                .unwrap_or_else(|| "Squashed commits".to_string()),
            None,
        )
    };

    let usage = usage_info.unwrap_or_default();
    let cost = usage.cost.unwrap_or(0.0);
    let total_cost = {
        let mut total = state.total_cost.lock().await;
        *total += cost;
        *total
    };

    let op_id = format!("op-{}", crate::state::next_operation_id());
    state.out.send_event(&Event {
        method: events::MODEL_USAGE.to_string(),
        params: serde_json::to_value(tauqe_protocol::ModelUsageEvent {
            operation_id: op_id,
            usage: usage.clone(),
            session_total_cost: total_cost,
            current_cost: Some(cost),
        })
        .ok(),
    });

    let result = tauqe_protocol::GitSquashGenerateMessageResult {
        message,
        usage: Some(usage),
        session_total_cost: Some(total_cost),
        current_cost: Some(cost),
    };
    Response::ok_typed(req.id, &result)
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
                format!("Failed to get commits ahead of {}: {}", params.base_ref, err),
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

    {
        let mut history = state.history.lock().await;
        let _ = history.record_response(
            &msg_text,
            Some(squashed_hash.clone()),
            Some(first_line),
            Vec::new(),
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
