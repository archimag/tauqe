//! `review/start` path-pattern selection.
//!
//! A pattern is a declaration of the audited file set, so a pattern that names
//! nothing is refused instead of silently widening or narrowing the audit, while
//! a blank pattern narrows nothing and keeps the loaded context.

use std::sync::Arc;

use super::{
    init_repo_with_file, init_repo_with_files, init_repo_with_uniform_files, test_config,
    test_state_with_provider, TestDir,
};

use super::super::{handle_review_preview, handle_review_start};

use crate::state::AppState;
use crate::transport::OutChannel;

use tauqe_core::providers::mock::{MockLlmProvider, MockResponse};
use tauqe_core::providers::LlmProvider;
use tauqe_protocol::{errors, events, Request};

/// Builds a `review/start` request carrying the given payload.
fn start_request(params: serde_json::Value) -> Request {
    Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::REVIEW_START.to_string(),
        params: Some(params),
    }
}

#[tokio::test]
async fn start_rejects_a_pattern_with_no_repository_match() {
    let dir = TestDir::new();
    init_repo_with_file(&dir, "src/target.rs");
    let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text("{}")]));
    let state = test_state_with_provider(&dir, test_config(), provider).await;

    let res = handle_review_start(
        start_request(serde_json::json!({
            "user_prompt": "Audit the target module",
            "path_patterns": ["*.missing"]
        })),
        &state,
    )
    .await;

    // A pattern that selects nothing names no repository file: launching anyway
    // would review a different file set than the one the developer declared.
    assert_eq!(
        res.error.expect("the pattern names nothing").code,
        errors::INVALID_PARAMS
    );
    assert!(!state.is_operation_active());
}

#[tokio::test]
async fn start_reports_an_empty_context() {
    let dir = TestDir::new();
    let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text("{}")]));
    let state = test_state_with_provider(&dir, test_config(), provider).await;

    let res = handle_review_start(
        start_request(serde_json::json!({ "user_prompt": "Audit the locking discipline" })),
        &state,
    )
    .await;

    assert_eq!(
        res.error.expect("nothing to review").code,
        errors::EMPTY_CONTEXT
    );
}

#[tokio::test]
async fn start_treats_a_blank_pattern_as_no_pattern() {
    let dir = TestDir::new();
    let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text("{}")]));
    let state = test_state_with_provider(&dir, test_config(), provider).await;

    let res = handle_review_start(
        start_request(serde_json::json!({ "path_patterns": ["   "] })),
        &state,
    )
    .await;

    // A blank line narrows nothing, so the audit keeps the loaded context and
    // reports the empty window exactly as an absent pattern does.
    assert_eq!(
        res.error.expect("nothing to review").code,
        errors::EMPTY_CONTEXT
    );
}

/// Builds a `review/preview` request carrying the given payload.
fn preview_request(params: serde_json::Value) -> Request {
    Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::REVIEW_PREVIEW.to_string(),
        params: Some(params),
    }
}

/// Decodes the window a `review/preview` response projected.
fn projected_window(
    response: tauqe_protocol::Response,
) -> tauqe_protocol::review::ReviewPreviewWindow {
    assert!(
        response.error.is_none(),
        "preview failed: {:?}",
        response.error
    );
    serde_json::from_value::<tauqe_protocol::review::ReviewPreviewResult>(
        response.result.expect("preview result"),
    )
    .expect("the projection decodes")
    .window
}

/// Builds the daemon state a launch-parity test runs against, keeping the
/// published event stream so the started event can be compared with the
/// projection that preceded it.
async fn state_with_events(
    dir: &TestDir,
    config: tauqe_core::config::AppConfig,
    provider: Arc<dyn LlmProvider>,
) -> (Arc<AppState>, tokio::sync::mpsc::UnboundedReceiver<String>) {
    let (out, events) = OutChannel::new();
    let (history_sender, _history_rx) = tokio::sync::mpsc::unbounded_channel();
    let state = AppState::new(config, dir.path().to_path_buf(), history_sender, out);
    *state.provider_override.lock().await = Some(provider);
    (state, events)
}

/// Drains the published event stream until the first event of `method`.
async fn drain_until(
    events: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
    method: &str,
) -> Option<serde_json::Value> {
    while let Ok(Some(line)) =
        tokio::time::timeout(std::time::Duration::from_secs(5), events.recv()).await
    {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if value.get("method").and_then(|name| name.as_str()) == Some(method) {
            return value.get("params").cloned();
        }
    }
    None
}

/// The projection is a pre-computation of the launch, not a parallel copy of
/// it: the window it reports is exactly the window `review/start` reads, down
/// to the omitted files and the token estimate (#c-1, #c-4).
#[tokio::test]
async fn preview_window_matches_the_window_the_launch_reads() {
    let dir = TestDir::new();
    init_repo_with_files(
        &dir,
        &[
            ("src/a.rs", "pub fn target() {}\n"),
            ("src/b.rs", "pub fn target() {}\n"),
            ("src/c.rs", "pub fn target() {}\n"),
        ],
    );
    let mut config = test_config();
    // The file token budget is the only knife: it pays for two of the three
    // matched files, so the projection and the launch must drop exactly one.
    let content_tokens =
        tauqe_core::prompt::estimate_tokens_from_bytes("pub fn target() {}\n".len() as u64)
            as usize;
    let wrapper = tauqe_core::prompt::context_element_overhead_tokens(
        "src/a.rs",
        tauqe_protocol::ContextAccess::ReadOnly,
    );
    config.context.files_token_budget = content_tokens.saturating_add(wrapper).saturating_mul(2);
    let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
        r#"{"summary":"The audited slice is sound.","findings":[]}"#,
    )]));
    let (state, mut events) = state_with_events(&dir, config, provider).await;

    let params = serde_json::json!({ "path_patterns": ["src/"] });
    let window =
        projected_window(handle_review_preview(preview_request(params.clone()), &state).await);
    assert!(
        !state.is_operation_active(),
        "a read RPC claims no operation slot"
    );
    assert_eq!(window.matched_files, 3);
    assert_eq!(window.selected_files, 2);
    assert_eq!(window.omitted_by_budget, 1);

    let res = handle_review_start(start_request(params), &state).await;
    assert!(
        res.error.is_none(),
        "the launch must start from the previewed window: {:?}",
        res.error
    );
    let started = drain_until(&mut events, events::REVIEW_STARTED)
        .await
        .expect("the launch announces its audit window");

    assert_eq!(
        started["files_count"].as_u64(),
        Some((window.context_files + window.selected_files) as u64),
        "the launch reads exactly the files the projection selected"
    );
    assert_eq!(
        started["omitted_files"].as_u64(),
        Some(window.omitted_files as u64),
        "the launch drops exactly what the projection reported"
    );
    assert_eq!(
        started["estimated_tokens"].as_u64(),
        Some((window.context_tokens + window.selected_tokens) as u64),
        "the launch spends exactly the tokens the projection estimated"
    );
}

/// The number of matched files is never a limit: a launch audits every file the
/// token budget pays for, even beyond the retired 150-file batch cap (#c-1).
#[tokio::test]
async fn a_launch_audits_every_paid_file_beyond_the_retired_batch_cap() {
    // The retired cap admitted at most 150 files per expansion, so a wider
    // selection proves that only the token budget decides the window.
    const MATCHED: usize = 151;
    let dir = TestDir::new();
    let paths = init_repo_with_uniform_files(&dir, MATCHED);
    let sample = paths.first().expect("the fixture creates the files");
    // Every fixture path has the same length, so one price describes them all.
    let content_tokens =
        tauqe_core::prompt::estimate_tokens_from_bytes("pub fn target() {}\n".len() as u64)
            as usize;
    let wrapper = tauqe_core::prompt::context_element_overhead_tokens(
        sample,
        tauqe_protocol::ContextAccess::ReadOnly,
    );
    let mut config = test_config();
    config.context.files_token_budget = content_tokens
        .saturating_add(wrapper)
        .saturating_mul(MATCHED);

    let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
        r#"{"summary":"The audited slice is sound.","findings":[]}"#,
    )]));
    let (state, mut events) = state_with_events(&dir, config, provider).await;

    let res = handle_review_start(
        start_request(serde_json::json!({ "path_patterns": ["src/"] })),
        &state,
    )
    .await;
    assert!(
        res.error.is_none(),
        "the launch must audit the whole paid selection: {:?}",
        res.error
    );

    let started = drain_until(&mut events, events::REVIEW_STARTED)
        .await
        .expect("the launch announces its audit window");
    assert_eq!(
        started["files_count"].as_u64(),
        Some(MATCHED as u64),
        "no count limits the launch: every paid file is audited"
    );
    assert_eq!(started["omitted_files"].as_u64(), Some(0));
}
