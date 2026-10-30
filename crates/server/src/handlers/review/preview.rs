//! Read-only projection of the audit window a review launch would read.
//!
//! `review/preview` describes the composition of the window `review/start`
//! would audit without launching it: how many files the active context
//! contributes, how many the declared paths matched, how many survived the
//! configured limits, and how many the batch and token budgets dropped. The
//! contract mirrors `review/get`: a read RPC claims no operation slot, writes no
//! active pointer, mutates no context, and calls no model, while every
//! repository listing and file read runs on the blocking pool so the stdin read
//! loop keeps accepting requests.

use std::path::Path;
use std::sync::Arc;

use tauqe_core::config::AppConfig;
use tauqe_protocol::review::{
    ReviewPreviewGroup, ReviewPreviewParams, ReviewPreviewResult, ReviewPreviewWindow,
};
use tauqe_protocol::{errors, Request, Response};

use crate::state::AppState;

use super::window::{expand_window, WindowBudgets, WindowContext};

/// One configured preset group as the projection reads it.
///
/// A launch mints fresh group identifiers, so the projection names the
/// configured title instead of an identity that does not exist yet.
struct ConfiguredGroup {
    title: String,
    paths: Vec<String>,
}

/// Projects the audit window `review/start` would read for the same parameters.
pub async fn handle_review_preview(mut req: Request, state: &Arc<AppState>) -> Response {
    // Strict wire-contract parse: a present payload must decode through the typed
    // DTO, so a malformed field answers INVALID_PARAMS instead of projecting a
    // window the launch would never read. An absent payload describes the launch
    // of the whole active context without patterns.
    let params: ReviewPreviewParams = match req.params.take() {
        None => ReviewPreviewParams::default(),
        Some(payload) => match serde_json::from_value(payload) {
            Ok(params) => params,
            Err(err) => {
                return Response::err(
                    req.id,
                    errors::INVALID_PARAMS,
                    format!("Invalid review preview parameters: {}", err),
                );
            }
        },
    };

    // The budgets and the repository root are the only server-side facts the
    // projection reads, and neither is taken exclusively: a concurrently running
    // turn keeps its operation registration and its context mutation rights.
    let config = state.config.lock().await.config.clone();
    let budgets = WindowBudgets {
        files_tokens: config.context.files_token_budget,
    };
    let repo_root = state.context.read().await.repo_root().to_path_buf();

    let preset_name = params
        .preset
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty());

    let projection = match preset_name {
        Some(name) => match configured_groups(&config, name) {
            Some(groups) => preview_preset(&repo_root, &groups, budgets).await,
            None => {
                return Response::err(
                    req.id,
                    errors::NOT_FOUND,
                    format!("Review preset '{}' not found", name),
                );
            }
        },
        None => preview_single(state, &repo_root, &params.path_patterns, budgets).await,
    };

    match projection {
        Ok(result) => Response::ok_typed(req.id, &result),
        Err(err) => Response::err(req.id, errors::REVIEW_PREVIEW_FAILED, err),
    }
}

/// Resolves a configured preset into the groups the projection measures.
///
/// The lookup matches the launch exactly — the trimmed requested name against
/// the trimmed configured name — so a preview never describes a preset the
/// launch would fail to resolve and never falls back to a preset the developer
/// did not name.
fn configured_groups(config: &AppConfig, name: &str) -> Option<Vec<ConfiguredGroup>> {
    let preset = config
        .review
        .presets
        .iter()
        .find(|preset| preset.name.trim() == name)?;
    Some(
        preset
            .groups
            .iter()
            .map(|group| ConfiguredGroup {
                title: group.name.clone(),
                paths: group.paths.clone(),
            })
            .collect(),
    )
}

/// Measures the window a plain launch reads: the active context and patterns.
async fn preview_single(
    state: &Arc<AppState>,
    repo_root: &Path,
    path_patterns: &[String],
    budgets: WindowBudgets,
) -> Result<ReviewPreviewResult, String> {
    let (context_snapshot, context_tokens) = {
        let context = state.context.read().await;
        (
            context.clone(),
            context.get_state().total_estimated_tokens as usize,
        )
    };
    // Reading the active context touches the file system, so it runs on the
    // blocking pool exactly as the launch reads it.
    let files = crate::handlers::offload(move || context_snapshot.read_context_files())
        .await
        .map_err(|err| format!("Failed to read the active context: {}", err))?;

    let expanded = expand_window(
        repo_root,
        WindowContext {
            files: &files,
            tokens: context_tokens,
        },
        path_patterns,
        budgets,
    )
    .await?;

    Ok(ReviewPreviewResult {
        window: expanded.window,
        groups: Vec::new(),
    })
}

/// Measures every configured group and folds the windows into one projection.
///
/// Groups audit their own slices, so the aggregate sums the counters of the
/// per-group windows: the confirmation a client renders then describes the whole
/// session instead of one slice of it.
async fn preview_preset(
    repo_root: &Path,
    groups: &[ConfiguredGroup],
    budgets: WindowBudgets,
) -> Result<ReviewPreviewResult, String> {
    let mut aggregated = ReviewPreviewWindow::default();
    let mut projected = Vec::with_capacity(groups.len());
    for group in groups {
        // A preset launch reads no interactive context, so every group expands
        // from an empty base exactly as the grouped audit does.
        let expanded =
            expand_window(repo_root, WindowContext::empty(), &group.paths, budgets).await?;
        accumulate(&mut aggregated, &expanded.window);
        projected.push(ReviewPreviewGroup {
            title: group.title.clone(),
            window: expanded.window,
        });
    }
    // Every group window obeys its own configured file budget, so the aggregate
    // allowance is what each group may spend; comparing a summed spend against a
    // single group's limit would misreport the whole session.
    aggregated.files_token_budget = budgets.files_tokens.saturating_mul(groups.len());
    Ok(ReviewPreviewResult {
        window: aggregated,
        groups: projected,
    })
}

/// Folds one group window into the aggregate projection.
fn accumulate(total: &mut ReviewPreviewWindow, window: &ReviewPreviewWindow) {
    total.context_files = total.context_files.saturating_add(window.context_files);
    total.context_tokens = total.context_tokens.saturating_add(window.context_tokens);
    total.matched_files = total.matched_files.saturating_add(window.matched_files);
    total.selected_files = total.selected_files.saturating_add(window.selected_files);
    total.selected_tokens = total.selected_tokens.saturating_add(window.selected_tokens);
    total.omitted_files = total.omitted_files.saturating_add(window.omitted_files);
    total.omitted_by_budget = total
        .omitted_by_budget
        .saturating_add(window.omitted_by_budget);
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::handlers::common::test_support::{scratch_root, snapshot_tree};
    use crate::transport::OutChannel;
    use tauqe_protocol::{methods, RequestId};

    /// Builds the daemon state a projection test runs against.
    fn test_state(root: &Path, config: AppConfig) -> Arc<AppState> {
        let (out, _events) = OutChannel::new();
        let (history_sender, _history_rx) = tokio::sync::mpsc::unbounded_channel();
        AppState::new(config, root.to_path_buf(), history_sender, out)
    }

    /// Builds a `review/preview` request carrying the given parameters.
    fn preview_request(params: serde_json::Value) -> Request {
        Request {
            id: RequestId::Number(1),
            method: methods::REVIEW_PREVIEW.to_string(),
            params: Some(params),
        }
    }

    /// Decodes the projection a successful preview answered with.
    fn projection(response: Response) -> ReviewPreviewResult {
        assert!(
            response.error.is_none(),
            "preview failed: {:?}",
            response.error
        );
        serde_json::from_value(response.result.expect("preview result"))
            .expect("the preview projection decodes")
    }

    /// Creates a scratch repository holding the given files, committed so the
    /// repository listing the projection reads reports them as tracked.
    fn init_repository_with_files(root: &Path, files: &[&str]) {
        tauqe_core::git::init_repository(Some(root), true).unwrap();
        let mut staged = Vec::new();
        for path in files {
            let full = root.join(path);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&full, "pub fn target() {}\n").unwrap();
            staged.push((*path).to_string());
        }
        tauqe_core::git::cmd::stage_files(root, &staged).unwrap();
        tauqe_core::git::cmd::commit_with_developer_author(root, "Add preview fixtures", false)
            .unwrap();
    }

    /// Builds the `[review]` configuration section a preset fixture declares.
    fn preset_config(presets: serde_json::Value) -> AppConfig {
        AppConfig {
            review: serde_json::from_value(presets).expect("the preset fixture decodes"),
            ..Default::default()
        }
    }

    /// An unknown preset is refused before anything is measured, and the read
    /// path leaves every persisted file byte-identical.
    #[tokio::test]
    async fn an_unknown_preset_is_refused_without_touching_the_repository() {
        let root = scratch_root("review-preview-unknown-preset");
        let state = test_state(&root, preset_config(serde_json::json!({ "presets": [] })));
        let before = snapshot_tree(&root);

        let response = handle_review_preview(
            preview_request(serde_json::json!({ "preset": "missing-preset" })),
            &state,
        )
        .await;

        let error = response.error.expect("an unknown preset is refused");
        assert_eq!(error.code, errors::NOT_FOUND);
        assert_eq!(
            snapshot_tree(&root),
            before,
            "a read RPC must leave every persisted file byte-identical"
        );
        assert!(
            !state.is_operation_active(),
            "a read RPC never claims the operation slot"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A preset projection reports every group and folds their counters into one
    /// aggregate, so the developer sees the whole coverage of the launch.
    #[tokio::test]
    async fn a_preset_projection_aggregates_every_group_and_reports_what_was_dropped() {
        let root = scratch_root("review-preview-preset");
        init_repository_with_files(
            &root,
            &["alpha/a.rs", "alpha/b.rs", "beta/c.rs", "beta/d.rs"],
        );

        let mut config = preset_config(serde_json::json!({
            "presets": [{
                "name": "layered",
                "groups": [
                    { "name": "Alpha", "paths": ["alpha/"] },
                    { "name": "Beta", "paths": ["beta/"] },
                ],
            }],
        }));
        // One matched file per group fits the file token budget, so the
        // projection must report the dropped files per group and in aggregate.
        let content_tokens =
            tauqe_core::prompt::estimate_tokens_from_bytes("pub fn target() {}\n".len() as u64)
                as usize;
        let wrapper = tauqe_core::prompt::context_element_overhead_tokens(
            "alpha/a.rs",
            tauqe_protocol::ContextAccess::ReadOnly,
        );
        config.context.files_token_budget = content_tokens.saturating_add(wrapper);
        let budget = config.context.files_token_budget;
        let state = test_state(&root, config);

        let result = projection(
            handle_review_preview(
                preview_request(serde_json::json!({ "preset": "layered" })),
                &state,
            )
            .await,
        );

        assert_eq!(result.groups.len(), 2);
        assert_eq!(result.groups[0].title, "Alpha");
        assert_eq!(result.groups[1].title, "Beta");
        for group in &result.groups {
            assert_eq!(group.window.matched_files, 2);
            assert_eq!(group.window.selected_files, 1);
            assert_eq!(group.window.omitted_by_budget, 1);
        }

        let window = &result.window;
        assert_eq!(window.context_files, 0, "a preset launch reads no context");
        assert_eq!(window.matched_files, 4);
        assert_eq!(window.selected_files, 2);
        assert_eq!(window.omitted_by_budget, 2);
        assert_eq!(window.omitted_files, 2);
        assert_eq!(window.files_token_budget, budget.saturating_mul(2));
        assert!(window.selected_tokens > 0);
        assert!(window.is_truncated());

        assert!(!state.is_operation_active());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A plain projection applies the same normalization the launch does: blank
    /// lines narrow nothing and every matched file is counted once.
    #[tokio::test]
    async fn a_single_projection_counts_pattern_matches_without_truncating_them() {
        let root = scratch_root("review-preview-single");
        init_repository_with_files(&root, &["src/a.rs", "src/b.rs", "docs/c.md"]);
        let state = test_state(&root, AppConfig::default());

        let result = projection(
            handle_review_preview(
                preview_request(serde_json::json!({ "path_patterns": ["src/", "  "] })),
                &state,
            )
            .await,
        );

        assert!(
            result.groups.is_empty(),
            "a plain launch declares no groups"
        );
        let window = &result.window;
        assert_eq!(window.matched_files, 2);
        assert_eq!(window.selected_files, 2);
        assert_eq!(window.omitted_files, 0);
        assert!(!window.is_truncated());
        assert!(window.selected_tokens > 0);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A payload that does not decode through the typed DTO is refused rather
    /// than projected from partially understood parameters.
    #[tokio::test]
    async fn a_malformed_payload_is_refused() {
        let root = scratch_root("review-preview-malformed");
        let state = test_state(&root, AppConfig::default());

        let response = handle_review_preview(
            preview_request(serde_json::json!({ "path_patterns": 7 })),
            &state,
        )
        .await;

        let error = response.error.expect("a malformed payload is refused");
        assert_eq!(error.code, errors::INVALID_PARAMS);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The method is wired into the dispatcher: it answers with a projection
    /// instead of falling through to `METHOD_NOT_FOUND`.
    #[tokio::test]
    async fn the_preview_method_is_dispatched_to_the_projection() {
        let root = scratch_root("review-preview-dispatch");
        let state = test_state(&root, AppConfig::default());

        let response = crate::handlers::handle_request(
            Request {
                id: RequestId::Number(3),
                method: methods::REVIEW_PREVIEW.to_string(),
                params: Some(serde_json::json!({})),
            },
            &state,
        )
        .await;

        let result = projection(response);
        assert!(!result.window.is_truncated());
        assert!(result.groups.is_empty());

        let _ = std::fs::remove_dir_all(&root);
    }
}
