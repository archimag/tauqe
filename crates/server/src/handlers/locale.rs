//! RPC handler for on-demand locale synthesis.
//!
//! English is the sole embedded source of truth for interface text (c-1); a
//! translated catalog is synthesized on demand by a language model through the
//! `locale/generate` method (c-2). The client never contacts a provider
//! directly (c-4): it sends the English catalog together with its semantic
//! annotations and receives a validated catalog back. Because a model cannot be
//! trusted to measure physical terminal columns, every translated string is
//! deterministically validated and sanitized (c-3) before the result leaves the
//! daemon.

use std::sync::Arc;

use tauqe_core::providers::{create_provider, ChatMessage, LlmProvider, StreamEvent};
use tauqe_protocol::{errors, LocaleGenerateParams, ModelRole, ProviderKind, Request, Response};

use crate::state::AppState;
use crate::usage::UsageGuard;

/// Handles `locale/generate`: validates the request, runs a single structured
/// provider call, and returns a sanitized catalog.
pub async fn handle_locale_generate(req: Request, state: &Arc<AppState>) -> Response {
    let params: LocaleGenerateParams = match req.params.and_then(|p| serde_json::from_value(p).ok())
    {
        Some(params) => params,
        None => {
            return Response::err(
                req.id,
                errors::INVALID_PARAMS,
                "Missing or invalid locale generate params",
            );
        }
    };

    if params.language.trim().is_empty() {
        return Response::err(
            req.id,
            errors::INVALID_PARAMS,
            "Target language cannot be empty",
        );
    }
    if params.items.is_empty() {
        return Response::err(
            req.id,
            errors::INVALID_PARAMS,
            "No catalog items to translate",
        );
    }

    // Resolve the Scribe model and provider up front: locale synthesis is a
    // lightweight structured text translation task owned by the Scribe role.
    let (config, model) = {
        let guard = state.config.lock().await;
        let model = guard.config.resolve_role(ModelRole::Scribe);
        (guard.config.clone(), model)
    };
    let provider = match resolve_locale_provider(state, &config, model.provider).await {
        Ok(provider) => provider,
        Err(err) => return Response::err(req.id, errors::NO_API_KEY, err),
    };

    let operation = match state.try_begin_operation() {
        Ok(guard) => Arc::new(guard),
        Err(()) => {
            return Response::err(
                req.id,
                errors::OPERATION_IN_PROGRESS,
                "Another operation is already running",
            );
        }
    };

    // Open the shared usage ledger: this call reports both the tokens the
    // provider returned and the duration the harness measured, so every
    // operation kind feeds one consistent snapshot.
    let op_id = format!("op-{}", crate::state::next_operation_id());
    let usage_guard = state.begin_usage().await;

    // Locale synthesis never produces code changes, so it is a Structured
    // Output operation (see `crates/core/Conventions.md`, section 1.2): the
    // model answers with the enforced JSON schema instead of free-form text.
    let messages = vec![
        ChatMessage::system(tauqe_core::locale::build_locale_system_prompt()),
        ChatMessage::user(tauqe_core::locale::build_locale_user_prompt(&params)),
    ];
    let response_format = tauqe_core::locale::locale_response_format();

    let cancel_rx = operation.cancel_receiver();
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let model_name = model.name.clone();
    let stream_cancel = cancel_rx.clone();
    let stream_task = tokio::spawn(async move {
        provider
            .stream_chat(
                &model_name,
                messages,
                Some(response_format),
                tx,
                stream_cancel,
            )
            .await
    });
    // The streamed provider task is reachable only through the operation slot:
    // a hard cancel and the shutdown drain abort the call through this handle,
    // so a provider that ignores the cooperative signal cannot outlive the
    // request it belongs to.
    operation.set_abort_handle(stream_task.abort_handle());

    let mut raw_text = String::new();
    let mut stream_error = None;
    let mut cancelled = false;
    while let Some(event) = rx.recv().await {
        match event {
            StreamEvent::TextDelta(delta) => raw_text.push_str(&delta),
            // Usage is folded into the in-flight window as it arrives, so a
            // cancelled or failed call is charged for whatever it already
            // spent instead of losing the report when the loop breaks early.
            StreamEvent::Usage(reported) => usage_guard.observe(&reported).await,
            StreamEvent::Cancelled => {
                cancelled = true;
                break;
            }
            StreamEvent::Error(message) => {
                stream_error = Some(message);
                break;
            }
            _ => {}
        }
    }

    // The event loop has exited, so the slot must stop pointing at the streamed
    // task: the call is about to be awaited locally, and a hard cancel must not
    // abort a task that has already finished.
    operation.clear_abort_handle();

    // A hard cancel is delivered as a signal rather than as a stream event, so
    // the borrowed signal is consulted as well: an operation aborted through
    // its handle yields a join error, which must surface as a cancellation
    // instead of a model failure.
    if *cancel_rx.borrow() || cancelled {
        stream_task.abort();
        let _ = stream_task.await;
        close_locale_usage(state, usage_guard, &op_id).await;
        return Response::err(req.id, errors::CANCELLED, "Locale generation was cancelled");
    }
    if let Some(message) = stream_error {
        close_locale_usage(state, usage_guard, &op_id).await;
        return Response::err(req.id, errors::MODEL_ERROR, message);
    }
    match stream_task.await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            close_locale_usage(state, usage_guard, &op_id).await;
            return Response::err(req.id, errors::MODEL_ERROR, err.to_string());
        }
        Err(join_err) => {
            close_locale_usage(state, usage_guard, &op_id).await;
            return Response::err(
                req.id,
                errors::MODEL_ERROR,
                format!("Locale generation stream task failed: {}", join_err),
            );
        }
    }

    // The terminal flush folds the reported usage and the measured duration of
    // this one-shot call into the session totals, then mirrors the authoritative
    // numbers onto the legacy per-operation channel. The legacy fields are
    // projected from that snapshot, never from a separately maintained counter,
    // so the aggregate and legacy views can never disagree.
    close_locale_usage(state, usage_guard, &op_id).await;

    // Deterministic geometry validation and graceful English fallback live in
    // `core`: the daemon never trusts the model's own width arithmetic, so an
    // over-budget string is shortened or reverted to its English term and a
    // missing key transparently degrades to English (c-5).
    let result = tauqe_core::locale::validate_and_sanitize_locale(
        &params,
        &tauqe_core::locale::parse_locale_output(&raw_text),
    );
    Response::ok_typed(req.id, &result)
}

/// Closes the locale call's usage window exactly once.
///
/// Every terminal path — success, cancellation, and provider failure — reaches
/// this, so the tokens and the time the call already consumed are folded into
/// the session totals instead of leaving a window open for the next operation to
/// discard. The legacy per-operation usage channel is then mirrored from the
/// same authoritative snapshot.
async fn close_locale_usage(state: &Arc<AppState>, guard: UsageGuard, op_id: &str) {
    guard.flush().await;
    crate::handlers::model::emit_legacy_usage_event(state, op_id).await;
}

/// Resolves the LLM provider for a locale call.
///
/// Deterministic unit tests inject a scripted provider through the state's test
/// seam; production builds construct a real network provider from configuration.
async fn resolve_locale_provider(
    _state: &Arc<AppState>,
    config: &tauqe_core::config::AppConfig,
    kind: ProviderKind,
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

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tauqe_core::providers::mock::{MockLlmProvider, MockResponse};
    use tauqe_core::providers::ResponseFormat;
    use tauqe_protocol::{events, LocaleCatalogItem, LocaleGenerateResult, RequestId};

    fn item(key: &str, english: &str, max_width: Option<u16>) -> LocaleCatalogItem {
        LocaleCatalogItem {
            key: key.to_string(),
            english: english.to_string(),
            context: "semantic context".to_string(),
            max_width,
        }
    }

    fn params(items: Vec<LocaleCatalogItem>) -> LocaleGenerateParams {
        LocaleGenerateParams {
            language: "Russian".to_string(),
            items,
        }
    }

    fn request(params: &LocaleGenerateParams) -> Request {
        Request {
            id: RequestId::Number(1),
            method: tauqe_protocol::methods::LOCALE_GENERATE.to_string(),
            params: Some(serde_json::to_value(params).unwrap()),
        }
    }

    fn bare_state() -> Arc<AppState> {
        let (out, _rx) = crate::transport::OutChannel::new();
        let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
        AppState::new(
            tauqe_core::config::AppConfig::default(),
            std::path::PathBuf::from("/nonexistent"),
            hist_tx,
            out,
        )
    }

    async fn state_with_provider(provider: Arc<dyn LlmProvider>) -> Arc<AppState> {
        let state = bare_state();
        *state.provider_override.lock().await = Some(provider);
        state
    }

    /// Builds a state together with its wire channel, so a test can inspect the
    /// events the handler publishes.
    fn state_with_wire() -> (Arc<AppState>, tokio::sync::mpsc::UnboundedReceiver<String>) {
        let (out, rx) = crate::transport::OutChannel::new();
        let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(
            tauqe_core::config::AppConfig::default(),
            std::path::PathBuf::from("/nonexistent"),
            hist_tx,
            out,
        );
        (state, rx)
    }

    /// Builds a state whose provider is a double, keeping the wire channel open
    /// for assertion.
    async fn state_with_provider_on_wire(
        provider: Arc<dyn LlmProvider>,
    ) -> (Arc<AppState>, tokio::sync::mpsc::UnboundedReceiver<String>) {
        let (state, rx) = state_with_wire();
        *state.provider_override.lock().await = Some(provider);
        (state, rx)
    }

    /// Drains every line the handler already wrote to the wire.
    fn drain_wire(rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>) -> Vec<String> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    /// Counts the legacy per-operation usage events on the wire.
    ///
    /// The lines are parsed instead of substring-matched, because the legacy
    /// method name is a prefix of the aggregate one.
    fn count_legacy_usage_events(lines: &[String]) -> usize {
        lines
            .iter()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|event| {
                event.get("method").and_then(|method| method.as_str()) == Some(events::MODEL_USAGE)
            })
            .count()
    }

    fn parse_result(response: Response) -> LocaleGenerateResult {
        serde_json::from_value(response.result.expect("result")).expect("valid result")
    }

    #[tokio::test]
    async fn synthesizes_sanitizes_and_requests_structured_output() {
        let raw = serde_json::json!({
            "translations": [
                {"key": "modal.confirm", "translation": "Подтвердить"},
                {"key": "modal.ok", "translation": "Подтверждение"},
                {"key": "tab.review", "translation": "Обзор"}
            ]
        })
        .to_string();
        // Keep the concrete mock handle so the recorded request stays
        // inspectable; the trait object would hide `last_request`.
        let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(raw)]));
        // Clone through an explicit trait-object binding: `Arc::clone(&provider)`
        // resolves the clone to the concrete type and fails to coerce to
        // `Arc<dyn LlmProvider>` at the call site.
        let provider_handle: Arc<dyn LlmProvider> = provider.clone();
        let state = state_with_provider(provider_handle).await;

        let catalog = params(vec![
            item("modal.confirm", "Confirm", Some(10)),
            item("modal.ok", "OK", Some(2)),
            item("tab.review", "Review", None),
            item("missing.key", "Missing fallback", None),
        ]);

        let response = handle_locale_generate(request(&catalog), &state).await;
        assert!(
            response.error.is_none(),
            "unexpected error: {:?}",
            response.error
        );

        let result = parse_result(response);
        assert_eq!(result.language, "Russian");
        // An over-budget translation is deterministically shortened to fit.
        assert_eq!(result.translations["modal.confirm"], "Подтверди…");
        // Shortening would lose too much, so English is used verbatim.
        assert_eq!(result.translations["modal.ok"], "OK");
        // An unbounded entry keeps the full translation.
        assert_eq!(result.translations["tab.review"], "Обзор");
        // A missing key degrades transparently to English (c-5).
        assert_eq!(result.translations["missing.key"], "Missing fallback");

        // The locale call is a structured-output operation.
        let sent = provider.last_request().expect("recorded request");
        match sent.response_format.as_ref() {
            Some(ResponseFormat::JsonSchema { json_schema, .. }) => {
                assert_eq!(json_schema.name, "locale_translation");
            }
            other => panic!("expected the locale JSON schema, got {other:?}"),
        }
        assert_eq!(sent.messages.len(), 2);
        assert_eq!(sent.messages[0].role, "system");
        assert!(sent.messages[1].content.contains("Russian"));
    }

    #[tokio::test]
    async fn rejects_invalid_params() {
        let state = state_with_provider(Arc::new(MockLlmProvider::new())).await;

        let missing = Request {
            id: RequestId::Number(1),
            method: tauqe_protocol::methods::LOCALE_GENERATE.to_string(),
            params: None,
        };
        assert_eq!(
            handle_locale_generate(missing, &state)
                .await
                .error
                .unwrap()
                .code,
            errors::INVALID_PARAMS
        );

        let empty_language = LocaleGenerateParams {
            language: "   ".to_string(),
            items: vec![item("a", "A", None)],
        };
        assert_eq!(
            handle_locale_generate(request(&empty_language), &state)
                .await
                .error
                .unwrap()
                .code,
            errors::INVALID_PARAMS
        );

        let empty_items = params(Vec::new());
        assert_eq!(
            handle_locale_generate(request(&empty_items), &state)
                .await
                .error
                .unwrap()
                .code,
            errors::INVALID_PARAMS
        );
    }

    #[tokio::test]
    async fn fails_without_api_key_before_touching_the_operation_slot() {
        let state = bare_state();
        let catalog = params(vec![item("a", "A", None)]);
        let response = handle_locale_generate(request(&catalog), &state).await;
        assert_eq!(response.error.unwrap().code, errors::NO_API_KEY);
        // Provider resolution precedes registration: a missing key never leaks a
        // dangling operation slot.
        assert!(!state.is_operation_active());
    }

    #[tokio::test]
    async fn refuses_when_an_operation_is_already_active() {
        let state = state_with_provider(Arc::new(MockLlmProvider::scripted([MockResponse::text(
            "{}",
        )])))
        .await;
        let _guard = state.try_begin_operation().expect("register operation");

        let catalog = params(vec![item("a", "A", None)]);
        let response = handle_locale_generate(request(&catalog), &state).await;
        assert_eq!(response.error.unwrap().code, errors::OPERATION_IN_PROGRESS);
    }

    #[tokio::test]
    async fn surfaces_a_provider_stream_error() {
        let state =
            state_with_provider(Arc::new(MockLlmProvider::scripted([MockResponse::error(
                "boom",
            )])))
            .await;

        let catalog = params(vec![item("a", "A", None)]);
        let response = handle_locale_generate(request(&catalog), &state).await;
        let err = response.error.expect("provider error must surface");
        assert_eq!(err.code, errors::MODEL_ERROR);
        assert!(err.message.contains("boom"));

        // A failed call still closes its window exactly once, so the next
        // operation starts from a clean ledger instead of inheriting an
        // abandoned in-flight window.
        let snapshot = state.usage.lock().await.clone();
        assert_eq!(snapshot.operations, 1);
        assert!(snapshot.cur.is_none());
        assert!(!state.is_operation_active(), "the slot is released");
    }

    /// Provider double that reports usage and then terminates the stream with an
    /// explicit cancellation, so the cancelled terminal path is exercised
    /// instead of the ordinary success path.
    struct CancellingProvider {
        usage: tauqe_protocol::ModelUsageInfo,
    }

    #[async_trait]
    impl LlmProvider for CancellingProvider {
        fn kind(&self) -> tauqe_protocol::ProviderKind {
            tauqe_protocol::ProviderKind::OpenRouter
        }

        async fn stream_chat(
            &self,
            _model: &str,
            _messages: Vec<ChatMessage>,
            _response_format: Option<ResponseFormat>,
            tx: tokio::sync::mpsc::Sender<StreamEvent>,
            _cancel_rx: tokio::sync::watch::Receiver<bool>,
        ) -> anyhow::Result<()> {
            let _ = tx.send(StreamEvent::Usage(self.usage.clone())).await;
            let _ = tx.send(StreamEvent::Cancelled).await;
            Ok(())
        }
    }

    /// A successful translation is one accounted operation: its usage folds into
    /// the session totals exactly once and is mirrored onto the legacy channel
    /// from the same authoritative snapshot.
    #[tokio::test]
    async fn accounts_the_translation_and_mirrors_the_usage_event() {
        let raw = serde_json::json!({
            "translations": [{"key": "tab.review", "translation": "Обзор"}]
        })
        .to_string();
        let usage = tauqe_protocol::ModelUsageInfo {
            prompt_tokens: 20,
            completion_tokens: 8,
            total_tokens: 28,
            cost: Some(0.4),
            ..tauqe_protocol::ModelUsageInfo::default()
        };
        let provider: Arc<dyn LlmProvider> = Arc::new(MockLlmProvider::scripted([
            MockResponse::text(raw).with_usage(usage),
        ]));
        let (state, mut rx) = state_with_provider_on_wire(provider).await;

        let catalog = params(vec![item("tab.review", "Review", None)]);
        let response = handle_locale_generate(request(&catalog), &state).await;
        assert!(
            response.error.is_none(),
            "unexpected error: {:?}",
            response.error
        );
        let result = parse_result(response);
        assert_eq!(result.translations["tab.review"], "Обзор");

        let snapshot = state.usage.lock().await.clone();
        assert_eq!(snapshot.operations, 1, "one terminal flush per call");
        assert!(snapshot.cur.is_none(), "the flush closes the window");
        assert_eq!(snapshot.session.total_tokens, 28);
        assert!((snapshot.session.cost - 0.4).abs() < f64::EPSILON);

        let lines = drain_wire(&mut rx);
        assert_eq!(
            count_legacy_usage_events(&lines),
            1,
            "exactly one legacy usage event per call"
        );
        let reported = lines
            .iter()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find(|event| {
                event.get("method").and_then(|method| method.as_str()) == Some(events::MODEL_USAGE)
            })
            .expect("legacy usage event");
        assert_eq!(
            reported["params"]["usage"]["total_tokens"].as_u64(),
            Some(28)
        );
        assert_eq!(reported["params"]["session_total_cost"].as_f64(), Some(0.4));
        assert!(!state.is_operation_active());
    }

    /// A cancelled call is charged for what it already spent and leaves no open
    /// window behind.
    #[tokio::test]
    async fn cancellation_still_charges_the_operation_once() {
        let usage = tauqe_protocol::ModelUsageInfo {
            prompt_tokens: 5,
            completion_tokens: 1,
            total_tokens: 6,
            cost: Some(0.1),
            ..tauqe_protocol::ModelUsageInfo::default()
        };
        let provider: Arc<dyn LlmProvider> = Arc::new(CancellingProvider { usage });
        let (state, mut rx) = state_with_provider_on_wire(provider).await;

        let catalog = params(vec![item("tab.review", "Review", None)]);
        let response = handle_locale_generate(request(&catalog), &state).await;
        assert_eq!(
            response.error.expect("cancellation must surface").code,
            errors::CANCELLED
        );

        let snapshot = state.usage.lock().await.clone();
        assert_eq!(snapshot.operations, 1, "a cancelled call flushes once");
        assert!(snapshot.cur.is_none(), "cancellation closes the window");
        assert_eq!(snapshot.session.total_tokens, 6);
        assert!((snapshot.session.cost - 0.1).abs() < f64::EPSILON);
        assert!(
            !state.is_operation_active(),
            "the operation slot is released"
        );

        let lines = drain_wire(&mut rx);
        assert_eq!(
            count_legacy_usage_events(&lines),
            1,
            "a cancelled call reports its usage exactly once"
        );
    }
    /// Flips a flag when the spawned provider future is dropped, so a test can
    /// prove the wedged task really stopped instead of lingering detached.
    struct ProviderStopMarker(Arc<AtomicBool>);

    impl Drop for ProviderStopMarker {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    /// Provider double that parks forever without observing the cancellation
    /// signal, modeling a wedged call that only the abort handle registered in
    /// the operation slot can terminate.
    struct WedgedProvider {
        entered: tokio::sync::mpsc::UnboundedSender<()>,
        stopped: Arc<AtomicBool>,
    }

    impl WedgedProvider {
        fn new(stopped: Arc<AtomicBool>) -> (Self, tokio::sync::mpsc::UnboundedReceiver<()>) {
            let (entered, receiver) = tokio::sync::mpsc::unbounded_channel();
            (Self { entered, stopped }, receiver)
        }
    }

    #[async_trait]
    impl LlmProvider for WedgedProvider {
        fn kind(&self) -> tauqe_protocol::ProviderKind {
            tauqe_protocol::ProviderKind::OpenRouter
        }

        async fn stream_chat(
            &self,
            _model: &str,
            _messages: Vec<ChatMessage>,
            _response_format: Option<ResponseFormat>,
            _tx: tokio::sync::mpsc::Sender<StreamEvent>,
            _cancel_rx: tokio::sync::watch::Receiver<bool>,
        ) -> anyhow::Result<()> {
            // The marker and the sender stay alive across the park: the future
            // only ever completes by being aborted, exactly like a provider call
            // that hangs instead of reacting to the cancellation signal.
            let _marker = ProviderStopMarker(Arc::clone(&self.stopped));
            let _ = self.entered.send(());
            std::future::pending::<()>().await;
            Ok(())
        }
    }

    /// A provider call that never ends on its own and never observes the
    /// cancellation signal is terminated through the abort handle the handler
    /// registers in the operation slot: the drain aborts it, the call reports a
    /// cancellation, and the slot is released.
    #[tokio::test]
    async fn registers_the_stream_task_so_a_wedged_provider_is_aborted() {
        let stopped = Arc::new(AtomicBool::new(false));
        let (provider, mut entered) = WedgedProvider::new(Arc::clone(&stopped));
        let provider: Arc<dyn LlmProvider> = Arc::new(provider);
        let state = state_with_provider(provider).await;

        let catalog = params(vec![item("tab.review", "Review", None)]);
        let handler = {
            let state = Arc::clone(&state);
            tokio::spawn(async move { handle_locale_generate(request(&catalog), &state).await })
        };

        tokio::time::timeout(std::time::Duration::from_secs(10), entered.recv())
            .await
            .expect("the handler must reach the provider")
            .expect("the provider signals that it is parked");
        assert!(
            state.is_operation_active(),
            "the locale call owns the operation slot while its provider streams"
        );

        // The hard cancel is delivered as a signal, and the drain then aborts
        // whatever handle the slot holds. Without the registration the abort has
        // no target and the wedged call keeps the slot alive until it ends.
        state.cancel_active_operation();
        state
            .force_abort_active_operation(std::time::Duration::from_secs(10))
            .await;

        let response = tokio::time::timeout(std::time::Duration::from_secs(10), handler)
            .await
            .expect("the wedged provider must not outlive the abort")
            .expect("the handler task joins");
        assert_eq!(
            response
                .error
                .expect("an aborted call must not succeed")
                .code,
            errors::CANCELLED
        );
        assert!(
            stopped.load(Ordering::SeqCst),
            "the wedged provider task must really have stopped"
        );
        assert!(
            !state.is_operation_active(),
            "the operation slot is released after the abort"
        );
    }
}
