use std::sync::Arc;

#[cfg(test)]
use tauqe_core::providers::LlmProvider;
use tauqe_protocol::{
    errors, GitDiffParams, GitDiffResult, RepositoryListFilesResult, Request, Response,
};

use crate::state::AppState;

/// Converts the Git snapshot at the RPC boundary.
pub(crate) fn repository_dto(
    state: tauqe_flow::api::RepositoryState,
) -> tauqe_protocol::RepositoryState {
    tauqe_protocol::RepositoryState {
        root: state.root,
        branch: state.branch,
        head: state.head,
        dirty: state.dirty,
    }
}

pub async fn handle_repository_get_state(req: Request, state: &Arc<AppState>) -> Response {
    match state.application.repository_state().await {
        Ok(repository) => Response::ok_typed(req.id, &repository_dto(repository)),
        Err(error) => Response::err(req.id, errors::REPOSITORY_STATE_FAILED, error.to_string()),
    }
}

pub async fn handle_repository_init(req: Request, state: &Arc<AppState>) -> Response {
    // A present payload must decode through the typed DTO: silently defaulting
    // an unparsable one would run a bare init and drop the requested initial
    // commit, which is the silent degradation the strict wire contract forbids.
    let params: tauqe_protocol::RepositoryInitParams = match req.params {
        Some(payload) => match serde_json::from_value(payload) {
            Ok(params) => params,
            Err(err) => {
                return Response::err(
                    req.id,
                    errors::INVALID_PARAMS,
                    format!("Invalid repository init parameters: {}", err),
                );
            }
        },
        None => tauqe_protocol::RepositoryInitParams::default(),
    };

    let observer = Arc::new(crate::publication::Publisher {
        out: state.out.clone(),
    });
    match state
        .application
        .initialize_repository(params.initial_commit, observer)
        .await
    {
        Ok(repository) => Response::ok_typed(
            req.id,
            &tauqe_protocol::RepositoryInitResult {
                repository: repository_dto(repository),
            },
        ),
        Err(tauqe_flow::api::InitializationError::Busy(_)) => Response::err(
            req.id,
            errors::OPERATION_IN_PROGRESS,
            "Cannot initialize a repository while a model operation is in progress",
        ),
        Err(error) => Response::err(req.id, errors::INIT_FAILED, error.to_string()),
    }
}

pub async fn handle_repository_list_files(req: Request, state: &Arc<AppState>) -> Response {
    match state.application.repository_files().await {
        Ok(files) => Response::ok_typed(req.id, &RepositoryListFilesResult { files }),
        Err(error) => Response::err(req.id, errors::LIST_FILES_FAILED, error.to_string()),
    }
}

pub async fn handle_git_get_diff(req: Request, state: &Arc<AppState>) -> Response {
    // A present payload must decode through the typed DTO: silently dropping an
    // unparsable one would answer with a whole-repository diff the client never
    // asked for.
    let params: GitDiffParams = match req.params {
        Some(payload) => match serde_json::from_value(payload) {
            Ok(params) => params,
            Err(err) => {
                return Response::err(
                    req.id,
                    errors::INVALID_PARAMS,
                    format!("Invalid git diff parameters: {}", err),
                );
            }
        },
        None => GitDiffParams::default(),
    };
    match state
        .application
        .repository_diff(params.commit, params.path)
        .await
    {
        Ok(diff) => Response::ok_typed(req.id, &GitDiffResult { diff }),
        Err(error) => repository_error(req.id, errors::GET_DIFF_FAILED, error),
    }
}

pub async fn handle_git_squash_preview(req: Request, state: &Arc<AppState>) -> Response {
    let params: tauqe_protocol::GitSquashPreviewParams = match req.params {
        Some(payload) => match serde_json::from_value(payload) {
            Ok(params) => params,
            Err(err) => {
                return Response::err(
                    req.id,
                    errors::INVALID_PARAMS,
                    format!("Invalid squash preview parameters: {}", err),
                );
            }
        },
        None => tauqe_protocol::GitSquashPreviewParams::default(),
    };

    let preview = match state.application.squash_preview(params.base_ref).await {
        Ok(preview) => preview,
        Err(error) => return repository_error(req.id, errors::SQUASH_PREVIEW_FAILED, error),
    };
    let tauqe_flow::api::SquashPreview {
        base_ref,
        session_base,
        upstream_base,
        commits: commits_ahead,
        diff_stat,
        files,
    } = preview;
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
        files: files
            .into_iter()
            .map(|file| tauqe_protocol::GitSquashFileDiff {
                path: file.path,
                diff: file.diff,
            })
            .collect(),
        suggested_message: None,
    };

    Response::ok_typed(req.id, &preview_res)
}

pub async fn handle_git_squash_generate_message(req: Request, state: &Arc<AppState>) -> Response {
    let params: tauqe_protocol::GitSquashGenerateMessageParams = match req
        .params
        .and_then(|p| serde_json::from_value(p).ok())
    {
        Some(p) => p,
        None => return Response::err(req.id, errors::INVALID_PARAMS, "Missing base_ref parameter"),
    };
    let observer = Arc::new(crate::publication::Publisher {
        out: state.out.clone(),
    });
    let result = match state
        .application
        .generate_squash_message(
            &params.base_ref,
            observer,
            crate::reap::task_exit_deadline(),
        )
        .await
    {
        Ok(r) => r,
        Err(e) => return squash_error(req.id, e),
    };
    let usage = result
        .usage
        .as_ref()
        .map(|s| crate::usage::wire_usage(&s.operation_totals().to_usage_info()));
    Response::ok_typed(
        req.id,
        &tauqe_protocol::GitSquashGenerateMessageResult {
            message: result.message,
            usage,
            session_total_cost: result.usage.as_ref().map(|s| s.session.cost),
            current_cost: result.usage.as_ref().map(|s| s.operation_totals().cost),
        },
    )
}
fn repository_error(
    id: tauqe_protocol::RequestId,
    failed: &str,
    error: tauqe_flow::api::RepositoryError,
) -> Response {
    let code = match error {
        tauqe_flow::api::RepositoryError::Busy => errors::OPERATION_IN_PROGRESS,
        tauqe_flow::api::RepositoryError::Invalid(_) => errors::INVALID_PARAMS,
        _ => failed,
    };
    Response::err(id, code, error.to_string())
}
fn squash_error(
    id: tauqe_protocol::RequestId,
    error: tauqe_flow::api::SquashMessageError,
) -> Response {
    let code = match &error {
        tauqe_flow::api::SquashMessageError::Busy => errors::OPERATION_IN_PROGRESS,
        tauqe_flow::api::SquashMessageError::Invalid(_) => errors::INVALID_PARAMS,
        tauqe_flow::api::SquashMessageError::Commits(_) => errors::COMMITS_AHEAD_FAILED,
        tauqe_flow::api::SquashMessageError::Model(_) => errors::INTERNAL_ERROR,
        tauqe_flow::api::SquashMessageError::Cancelled(_) => errors::CANCELLED,
    };
    Response::err(id, code, error.to_string())
}
pub async fn handle_git_squash_apply(req: Request, state: &Arc<AppState>) -> Response {
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

    let observer = crate::publication::Publisher {
        out: state.out.clone(),
    };
    let applied = match state
        .application
        .apply_squash(params.base_ref, params.message, &observer)
        .await
    {
        Ok(result) => result,
        Err(error) => {
            let code = match &error {
                tauqe_flow::api::SquashApplyError::Busy => errors::OPERATION_IN_PROGRESS,
                tauqe_flow::api::SquashApplyError::Invalid(_) => errors::INVALID_PARAMS,
                tauqe_flow::api::SquashApplyError::Commits(_) => errors::COMMITS_AHEAD_FAILED,
                tauqe_flow::api::SquashApplyError::Squash(_) => errors::SQUASH_FAILED,
            };
            return Response::err(req.id, code, error.to_string());
        }
    };
    Response::ok_typed(req.id, &squash_result_dto(applied))
}

pub(crate) fn squash_result_dto(
    applied: tauqe_flow::api::AppliedSquash,
) -> tauqe_protocol::GitSquashApplyResult {
    tauqe_protocol::GitSquashApplyResult {
        squashed_commit: applied.commit,
        message: applied.message,
        base_ref: applied.base_ref,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::common::test_support::scratch_root;
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
        let dir = scratch_root("init");

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
        assert_eq!(
            state
                .application
                .test_mutate_history(|history| history.repo_root().to_path_buf())
                .await
                .unwrap(),
            dir.as_path()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Temporary git repository carrying one commit, so a squash request can
    /// resolve its base revision.
    struct SquashTestRepo(std::path::PathBuf);

    impl SquashTestRepo {
        fn new() -> Self {
            let root = scratch_root("squash-usage");
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

        let usage = tauqe_core::providers::ModelUsageInfo {
            prompt_tokens: 30,
            completion_tokens: 10,
            total_tokens: 40,
            cost: Some(0.25),
            ..tauqe_core::providers::ModelUsageInfo::default()
        };
        let provider: Arc<dyn LlmProvider> =
            Arc::new(MockLlmProvider::scripted([MockResponse::text(
                "squash the session history",
            )
            .with_usage(usage)]));

        let (out, _rx) = crate::transport::OutChannel::new();
        let (history_sender, _history_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut config = AppConfig::default();
        config.models.scribe = Some(tauqe_core::models::ModelRef::openrouter("test/scribe"));
        let state = AppState::new(config, root, history_sender, out);
        state.application.test_set_provider(provider).await;

        let resp = handle_git_squash_generate_message(squash_message_request(), &state).await;
        assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);

        let result: tauqe_protocol::GitSquashGenerateMessageResult =
            serde_json::from_value(resp.result.unwrap()).unwrap();
        assert!(!result.message.is_empty(), "a message is always returned");
        assert_eq!(result.usage.as_ref().map(|u| u.total_tokens), Some(40));
        assert_eq!(result.session_total_cost, Some(0.25));
        assert_eq!(result.current_cost, Some(0.25));

        let snapshot = state.usage_snapshot().await;
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

        let snapshot = state.usage_snapshot().await;
        assert_eq!(snapshot.operations, 0, "a zero operation never counts");
        assert!(snapshot.session.is_empty());
        assert!(snapshot.prev.is_empty());
        assert!(snapshot.cur.is_none());
        assert!(!state.is_operation_active());
    }
    /// Squashing rewrites HEAD, so it must own the operation slot rather than
    /// sample it once: a turn registering right after a plain check would be
    /// committing against history this handler is rewriting.
    #[tokio::test]
    async fn squash_apply_is_refused_while_an_operation_is_registered() {
        let repo = SquashTestRepo::new();
        let root = repo.path().to_path_buf();

        let (out, _rx) = crate::transport::OutChannel::new();
        let (history_sender, _history_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(AppConfig::default(), root.clone(), history_sender, out);
        let operation = state.try_begin_operation().expect("the slot is free");
        let head_before = tauqe_core::git::get_repository_state(Some(&root)).head;

        let req = Request {
            id: RequestId::Number(5),
            method: methods::GIT_SQUASH_APPLY.to_string(),
            params: Some(serde_json::json!({
                "base_ref": "HEAD",
                "message": "squashed while a turn was running",
            })),
        };
        let resp = handle_git_squash_apply(req, &state).await;
        assert_eq!(
            resp.error.expect("the squash must be refused").code,
            errors::OPERATION_IN_PROGRESS
        );

        assert_eq!(
            tauqe_core::git::get_repository_state(Some(&root)).head,
            head_before,
            "a refused squash must leave HEAD untouched"
        );
        assert!(
            state.is_operation_active(),
            "the running operation must keep owning the slot"
        );

        drop(operation);
        assert!(!state.is_operation_active());
    }

    /// The message generator reads the commit list and the cumulative diff a
    /// running turn may be rewriting, so it owns the operation slot for the
    /// whole handler rather than claiming it once the reads are done.
    ///
    /// The probe is a root that is not a repository: were the reads still
    /// outside the slot, they would fail first and answer `COMMITS_AHEAD_FAILED`
    /// instead of the refusal.
    #[tokio::test]
    async fn squash_message_generation_is_refused_before_reading_the_repository() {
        let dir = scratch_root("squash-generate-exclusive");

        let (out, _rx) = crate::transport::OutChannel::new();
        let (history_sender, _history_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(AppConfig::default(), dir.clone(), history_sender, out);
        let operation = state.try_begin_operation().expect("the slot is free");

        let resp = handle_git_squash_generate_message(squash_message_request(), &state).await;
        assert_eq!(
            resp.error.expect("the generation must be refused").code,
            errors::OPERATION_IN_PROGRESS,
            "the slot must be claimed before the repository is read"
        );
        assert!(
            state.is_operation_active(),
            "the running operation must keep owning the slot"
        );
        let snapshot = state.usage_snapshot().await;
        assert_eq!(
            snapshot.operations, 0,
            "a refused generation accounts no operation"
        );
        assert!(snapshot.cur.is_none());

        drop(operation);
        assert!(!state.is_operation_active());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Builds one squash request carrying the given `base_ref`.
    fn squash_base_request(method: &str, base_ref: &str) -> Request {
        Request {
            id: RequestId::Number(11),
            method: method.to_string(),
            params: Some(serde_json::json!({
                "base_ref": base_ref,
                "message": "squashed",
            })),
        }
    }

    /// Asserts that a squash endpoint refused the request as invalid input.
    fn assert_invalid_params(resp: Response, base_ref: &str) {
        let error = resp
            .error
            .unwrap_or_else(|| panic!("the endpoint accepted the base_ref {base_ref:?}"));
        assert_eq!(error.code, errors::INVALID_PARAMS, "{base_ref:?}");
    }

    /// An option-shaped `base_ref` is refused identically on every squash
    /// endpoint, before any Git plumbing runs.
    ///
    /// `--output=<path>` is the sharpest probe: reaching `git log` or
    /// `git diff` as an option would create the named file, so its absence
    /// proves the value never left the shared gate.
    #[tokio::test]
    async fn option_shaped_base_ref_is_refused_on_every_squash_endpoint() {
        let repo = SquashTestRepo::new();
        let root = repo.path().to_path_buf();
        let probe = root.join("injected-output.txt");

        let (out, _rx) = crate::transport::OutChannel::new();
        let (history_sender, _history_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(AppConfig::default(), root, history_sender, out);

        let output_option = format!("--output={}", probe.display());
        for base_ref in [output_option.as_str(), "-x"] {
            let preview = handle_git_squash_preview(
                squash_base_request(methods::GIT_SQUASH_PREVIEW, base_ref),
                &state,
            )
            .await;
            assert_invalid_params(preview, base_ref);

            let generated = handle_git_squash_generate_message(
                squash_base_request(methods::GIT_SQUASH_GENERATE_MESSAGE, base_ref),
                &state,
            )
            .await;
            assert_invalid_params(generated, base_ref);

            let applied = handle_git_squash_apply(
                squash_base_request(methods::GIT_SQUASH_APPLY, base_ref),
                &state,
            )
            .await;
            assert_invalid_params(applied, base_ref);
        }

        assert!(
            !probe.exists(),
            "an option-shaped base_ref must never reach git"
        );
        assert!(
            !state.is_operation_active(),
            "a refused call must release the operation slot"
        );
    }

    /// A supplied but empty revision is refused as well, while an absent
    /// optional base of the preview keeps its automatic meaning.
    #[tokio::test]
    async fn blank_base_ref_is_refused_on_every_squash_endpoint() {
        let repo = SquashTestRepo::new();
        let root = repo.path().to_path_buf();

        let (out, _rx) = crate::transport::OutChannel::new();
        let (history_sender, _history_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(AppConfig::default(), root, history_sender, out);

        for base_ref in ["", "   "] {
            let preview = handle_git_squash_preview(
                squash_base_request(methods::GIT_SQUASH_PREVIEW, base_ref),
                &state,
            )
            .await;
            assert_invalid_params(preview, base_ref);

            let generated = handle_git_squash_generate_message(
                squash_base_request(methods::GIT_SQUASH_GENERATE_MESSAGE, base_ref),
                &state,
            )
            .await;
            assert_invalid_params(generated, base_ref);

            let applied = handle_git_squash_apply(
                squash_base_request(methods::GIT_SQUASH_APPLY, base_ref),
                &state,
            )
            .await;
            assert_invalid_params(applied, base_ref);
        }
    }
    /// A malformed `initial_commit` must be refused instead of silently running
    /// a bare init that drops the commit the client asked for.
    #[tokio::test]
    async fn repository_init_rejects_malformed_payloads() {
        let dir = scratch_root("init-invalid");

        let (out, _rx) = crate::transport::OutChannel::new();
        let (history_sender, _hrx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(AppConfig::default(), dir.clone(), history_sender, out);

        let req = Request {
            id: RequestId::Number(2),
            method: methods::REPOSITORY_INIT.to_string(),
            params: Some(serde_json::json!({ "initial_commit": "yes" })),
        };
        let resp = handle_repository_init(req, &state).await;
        assert_eq!(
            resp.error
                .expect("the malformed payload must be refused")
                .code,
            errors::INVALID_PARAMS
        );
        assert!(
            !dir.join(".git").exists(),
            "a refused init must not touch the repository"
        );
        assert!(
            !state.is_operation_active(),
            "a refused init must release the operation slot"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An unparsable `path` must be refused instead of answering with a
    /// whole-repository diff the client never asked for.
    #[tokio::test]
    async fn git_get_diff_rejects_malformed_payloads() {
        let repo = SquashTestRepo::new();
        let root = repo.path().to_path_buf();

        let (out, _rx) = crate::transport::OutChannel::new();
        let (history_sender, _history_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(AppConfig::default(), root.clone(), history_sender, out);

        let malformed = Request {
            id: RequestId::Number(3),
            method: methods::GIT_GET_DIFF.to_string(),
            params: Some(serde_json::json!({ "path": 7 })),
        };
        let resp = handle_git_get_diff(malformed, &state).await;
        assert_eq!(
            resp.error
                .expect("the malformed payload must be refused")
                .code,
            errors::INVALID_PARAMS
        );

        // An absent payload keeps its documented meaning: the whole worktree.
        let absent = Request {
            id: RequestId::Number(4),
            method: methods::GIT_GET_DIFF.to_string(),
            params: None,
        };
        let resp = handle_git_get_diff(absent, &state).await;
        assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
        let result: GitDiffResult =
            serde_json::from_value(resp.result.expect("git/getDiff result")).unwrap();
        assert_eq!(
            result.diff,
            tauqe_core::git::get_diff(root.as_path(), None).expect("the repository diff"),
            "an absent payload must answer the repository diff"
        );
    }
    /// A commit-scoped diff answers exactly the named commit, and the commit
    /// reference passes the same revision gate as every other reference.
    #[tokio::test]
    async fn git_get_diff_answers_a_requested_commit() {
        let repo = SquashTestRepo::new();
        let root = repo.path().to_path_buf();
        let probe = root.join("injected-output.txt");
        let head = tauqe_core::git::run_git(Some(&root), &["rev-parse", "HEAD"])
            .expect("the commit hash")
            .trim()
            .to_string();

        let (out, _rx) = crate::transport::OutChannel::new();
        let (history_sender, _history_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(AppConfig::default(), root, history_sender, out);

        let request = |commit: &str| Request {
            id: RequestId::Number(7),
            method: methods::GIT_GET_DIFF.to_string(),
            params: Some(serde_json::json!({ "commit": commit })),
        };

        let resp = handle_git_get_diff(request(&head), &state).await;
        assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
        let result: GitDiffResult =
            serde_json::from_value(resp.result.expect("git/getDiff result")).unwrap();
        assert!(
            result.diff.contains("+# tauqe"),
            "the commit diff must carry the change of that commit: {}",
            result.diff
        );

        // An option-shaped value is refused before any Git plumbing runs, so
        // the named probe file is never created.
        let refused =
            handle_git_get_diff(request(&format!("--output={}", probe.display())), &state).await;
        let error = refused
            .error
            .expect("an option-shaped commit must be refused");
        assert_eq!(error.code, errors::INVALID_PARAMS);
        assert!(
            !probe.exists(),
            "an option-shaped commit must never reach git"
        );
    }
}
