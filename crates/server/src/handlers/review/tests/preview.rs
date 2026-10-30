//! `review/preview`: the deterministic projection of the audit window.
//!
//! The projection is a pre-computation of what a launch reads, never a second
//! implementation of the selection: the same order and the same limits decide
//! the window, and the counters report separately what each limit dropped.
//! Because the method is a read RPC, it must leave the context, the working
//! tree, and the operation slot untouched.

use std::sync::Arc;

use super::super::handle_review_preview;
use super::{init_repo_with_files, init_repo_with_uniform_files, test_config, TestDir};

use crate::state::AppState;
use crate::transport::OutChannel;
use tauqe_core::config::AppConfig;
use tauqe_protocol::review::ReviewPreviewResult;
use tauqe_protocol::{methods, ContextAccess, Request, RequestId, Response};

/// Content of every fixture file.
///
/// Identical size keeps the token estimate of each fixture the same, so a
/// budget assertion is expressed in whole files instead of an opaque constant.
const TARGET_FILE: &str = "pub fn target() {}\n";

/// Builds a `review/preview` request carrying the given payload.
fn preview_request(params: serde_json::Value) -> Request {
    Request {
        id: RequestId::Number(1),
        method: methods::REVIEW_PREVIEW.to_string(),
        params: Some(params),
    }
}

/// Builds the daemon state a projection test runs against.
fn state_for(dir: &TestDir, config: AppConfig) -> Arc<AppState> {
    let (out, _events) = OutChannel::new();
    let (history_sender, _history_rx) = tokio::sync::mpsc::unbounded_channel();
    AppState::new(config, dir.path().to_path_buf(), history_sender, out)
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

/// Token estimate of one fixture file, the unit every budget is expressed in.
fn per_file_tokens() -> usize {
    let tokens = tauqe_core::prompt::estimate_tokens_from_bytes(TARGET_FILE.len() as u64) as usize;
    assert!(
        tokens > 0,
        "the fixture must occupy a positive token estimate"
    );
    tokens
}

/// Price one fixture file pays against the file token budget: its content plus
/// the wrapper its `<file>` element renders.
fn per_file_cost() -> usize {
    per_file_tokens()
        + tauqe_core::prompt::context_element_overhead_tokens("src/a.rs", ContextAccess::ReadOnly)
}

/// Snapshot of the working tree, excluding the Git metadata a read-only listing
/// may refresh without changing the repository content.
fn working_tree(dir: &TestDir) -> Vec<(String, Vec<u8>)> {
    crate::handlers::common::test_support::snapshot_tree(dir.path())
        .into_iter()
        .filter(|(path, _)| !path.starts_with(".git/"))
        .collect()
}

/// A launch over the active context alone expands no pattern, so the projection
/// reports the context it holds and drops nothing (#c-1).
#[tokio::test]
async fn preview_without_patterns_reports_the_active_context_and_omits_nothing() {
    let dir = TestDir::new();
    init_repo_with_files(&dir, &[("src/lib.rs", TARGET_FILE)]);
    let state = state_for(&dir, test_config());
    state
        .context
        .write()
        .await
        .add_file("src/lib.rs", ContextAccess::ReadOnly)
        .unwrap();

    let result =
        projection(handle_review_preview(preview_request(serde_json::json!({})), &state).await);

    assert!(
        result.groups.is_empty(),
        "a plain launch declares no groups"
    );
    let window = &result.window;
    assert_eq!(window.context_files, 1);
    assert!(window.context_tokens > 0);
    assert_eq!(window.matched_files, 0);
    assert_eq!(window.selected_files, 0);
    assert_eq!(window.omitted_files, 0);
    assert!(!window.is_truncated());
    assert!(
        !state.is_operation_active(),
        "a read RPC claims no operation slot"
    );
}

/// The number of matched files is no limit of its own: every match the file
/// token budget pays for is selected (#c-1).
#[tokio::test]
async fn preview_selects_every_matched_file_the_budget_pays_for() {
    let dir = TestDir::new();
    init_repo_with_files(
        &dir,
        &[
            ("src/a.rs", TARGET_FILE),
            ("src/b.rs", TARGET_FILE),
            ("src/c.rs", TARGET_FILE),
        ],
    );
    let mut config = test_config();
    config.context.files_token_budget = per_file_cost().saturating_mul(3);
    let budget = config.context.files_token_budget;
    let state = state_for(&dir, config);

    let result = projection(
        handle_review_preview(
            preview_request(serde_json::json!({ "path_patterns": ["src/"] })),
            &state,
        )
        .await,
    );

    let window = &result.window;
    assert_eq!(window.matched_files, 3);
    assert_eq!(window.selected_files, 3);
    assert_eq!(window.omitted_by_budget, 0);
    assert_eq!(window.omitted_files, 0);
    assert_eq!(window.files_token_budget, budget);
    assert!(!window.is_truncated());
}

/// An exhausted file token budget is reported as the omission, so a
/// confirmation can state how much the budget dropped (#c-1, #c-3).
#[tokio::test]
async fn preview_reports_the_token_budget_omission() {
    let dir = TestDir::new();
    init_repo_with_files(
        &dir,
        &[
            ("src/a.rs", TARGET_FILE),
            ("src/b.rs", TARGET_FILE),
            ("src/c.rs", TARGET_FILE),
        ],
    );
    let per_file = per_file_cost();
    let mut config = test_config();
    config.context.files_token_budget = per_file.saturating_mul(2);
    let state = state_for(&dir, config);

    let result = projection(
        handle_review_preview(
            preview_request(serde_json::json!({ "path_patterns": ["src/"] })),
            &state,
        )
        .await,
    );

    let window = &result.window;
    assert_eq!(window.matched_files, 3);
    assert_eq!(window.selected_files, 2);
    assert_eq!(window.omitted_by_budget, 1);
    assert_eq!(window.omitted_files, 1);
    assert_eq!(window.files_token_budget, per_file.saturating_mul(2));
    assert_eq!(window.selected_tokens, per_file.saturating_mul(2));
    assert!(window.is_truncated());
}

/// The projection reads the repository and the context without writing either:
/// a preview leaves the context and the persisted working tree byte-identical
/// and the operation slot free (#c-2).
#[tokio::test]
async fn preview_leaves_the_context_and_the_working_tree_untouched() {
    let dir = TestDir::new();
    init_repo_with_files(
        &dir,
        &[
            ("src/a.rs", TARGET_FILE),
            ("src/b.rs", TARGET_FILE),
            ("src/c.rs", TARGET_FILE),
        ],
    );
    let mut config = test_config();
    // The audit window shares the file token budget: the file the context
    // already holds plus one matched file fit, so exactly one further match is
    // dropped by the budget and the number of files is never a limit.
    config.context.files_token_budget = per_file_tokens().saturating_add(per_file_cost());
    let state = state_for(&dir, config);
    state
        .context
        .write()
        .await
        .add_file("src/a.rs", ContextAccess::ReadOnly)
        .unwrap();

    let context_before = serde_json::to_value(state.context.read().await.get_state())
        .expect("the context snapshot serializes");
    let tree_before = working_tree(&dir);

    let result = projection(
        handle_review_preview(
            preview_request(serde_json::json!({ "path_patterns": ["src/"] })),
            &state,
        )
        .await,
    );

    // A file the context already carries is found by the patterns and is not
    // read a second time, so matches are counted before the context filter and
    // only the fresh candidates compete for the file token budget.
    assert_eq!(result.window.context_files, 1);
    assert_eq!(result.window.matched_files, 3);
    assert_eq!(result.window.selected_files, 1);
    assert_eq!(result.window.omitted_by_budget, 1);
    assert_eq!(result.window.omitted_files, 1);

    let context_after = serde_json::to_value(state.context.read().await.get_state())
        .expect("the context snapshot serializes");
    assert_eq!(
        context_after, context_before,
        "a read RPC never mutates the context"
    );

    assert_eq!(
        working_tree(&dir),
        tree_before,
        "a read RPC never writes the working tree"
    );
    assert!(!state.is_operation_active());
}

/// A preset audit expands every group independently, so its projection folds
/// the per-group windows into one aggregate while the token budget of the whole
/// session is what each group may spend (#c-5).
#[tokio::test]
async fn a_preset_preview_aggregates_the_token_budget_omissions_of_its_groups() {
    let dir = TestDir::new();
    init_repo_with_files(
        &dir,
        &[
            ("alpha/a.rs", TARGET_FILE),
            ("alpha/b.rs", TARGET_FILE),
            ("beta/c.rs", TARGET_FILE),
        ],
    );
    // The budget pays for one matched file per group, priced exactly as the
    // window charges it: its content plus the wrapper its own `<file>` element
    // renders.
    let per_file = per_file_tokens()
        + tauqe_core::prompt::context_element_overhead_tokens(
            "alpha/a.rs",
            ContextAccess::ReadOnly,
        );
    let mut config = test_config();
    config.review = serde_json::from_value(serde_json::json!({
        "presets": [{
            "name": "layered",
            "groups": [
                { "name": "Alpha", "paths": ["alpha/"] },
                { "name": "Beta", "paths": ["beta/"] },
            ],
        }],
    }))
    .expect("the preset fixture decodes");
    config.context.files_token_budget = per_file;
    let state = state_for(&dir, config);

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
    assert_eq!(result.groups[0].window.matched_files, 2);
    assert_eq!(result.groups[0].window.selected_files, 1);
    assert_eq!(result.groups[0].window.omitted_by_budget, 1);
    assert_eq!(result.groups[1].window.matched_files, 1);
    assert_eq!(result.groups[1].window.selected_files, 1);
    assert_eq!(result.groups[1].window.omitted_by_budget, 0);

    let window = &result.window;
    assert_eq!(window.context_files, 0, "a preset launch reads no context");
    assert_eq!(window.matched_files, 3);
    assert_eq!(window.selected_files, 2);
    assert_eq!(window.omitted_by_budget, 1);
    assert_eq!(window.omitted_files, 1);
    assert_eq!(window.files_token_budget, per_file.saturating_mul(2));
    assert!(window.is_truncated());
    assert!(!state.is_operation_active());
}

/// The price of a file is its content plus the wrapper its own `<file>` element
/// renders, so identical content behind a longer path costs more and the budget
/// measures what the audit actually reads (#c-2).
#[tokio::test]
async fn preview_prices_the_wrapper_of_each_path_into_the_budget() {
    let long_path = "src/a_considerably_longer_module_name.rs";
    let short_cost = per_file_cost();
    let long_cost = per_file_tokens()
        + tauqe_core::prompt::context_element_overhead_tokens(long_path, ContextAccess::ReadOnly);
    assert!(
        long_cost > short_cost,
        "a longer path must render a longer wrapper"
    );

    let mut config = test_config();
    config.context.files_token_budget = short_cost;

    let short_dir = TestDir::new();
    init_repo_with_files(&short_dir, &[("src/a.rs", TARGET_FILE)]);
    let short_state = state_for(&short_dir, config.clone());
    let short_window = projection(
        handle_review_preview(
            preview_request(serde_json::json!({ "path_patterns": ["src/"] })),
            &short_state,
        )
        .await,
    )
    .window;
    assert_eq!(short_window.selected_files, 1);
    assert_eq!(
        short_window.selected_tokens, short_cost,
        "the window charges the wrapper beside the content"
    );
    assert!(!short_window.is_truncated());

    let long_dir = TestDir::new();
    init_repo_with_files(&long_dir, &[(long_path, TARGET_FILE)]);
    let long_state = state_for(&long_dir, config);
    let long_window = projection(
        handle_review_preview(
            preview_request(serde_json::json!({ "path_patterns": ["src/"] })),
            &long_state,
        )
        .await,
    )
    .window;
    assert_eq!(long_window.matched_files, 1);
    assert_eq!(
        long_window.selected_files, 0,
        "the same content behind a longer path no longer fits the same budget"
    );
    assert_eq!(long_window.omitted_by_budget, 1);
    assert!(long_window.is_truncated());
}

/// The number of files is never a limit: a selection wider than the retired
/// 150-file batch cap is admitted in full while the token budget pays for it
/// (#c-1).
#[tokio::test]
async fn preview_admits_more_files_than_the_retired_batch_cap() {
    // The retired cap admitted at most 150 files per expansion, so a wider
    // selection proves that only the token budget decides the window.
    const MATCHED: usize = 151;
    let dir = TestDir::new();
    let paths = init_repo_with_uniform_files(&dir, MATCHED);
    let sample = paths.first().expect("the fixture creates the files");
    // Every fixture path has the same length, so one price describes them all.
    let per_file = per_file_tokens()
        + tauqe_core::prompt::context_element_overhead_tokens(sample, ContextAccess::ReadOnly);
    let mut config = test_config();
    config.context.files_token_budget = per_file.saturating_mul(MATCHED);
    let state = state_for(&dir, config);

    let result = projection(
        handle_review_preview(
            preview_request(serde_json::json!({ "path_patterns": ["src/"] })),
            &state,
        )
        .await,
    );

    let window = &result.window;
    assert_eq!(window.matched_files, MATCHED);
    assert_eq!(window.selected_files, MATCHED);
    assert_eq!(window.omitted_by_budget, 0);
    assert!(!window.is_truncated());
}
