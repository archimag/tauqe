//! RPC adaptation for headless locale generation.
use crate::state::AppState;
use std::sync::Arc;
use tauqe_flow::api::LocaleGenerationError;
use tauqe_protocol::{errors, LocaleGenerateParams, Request, Response};

pub async fn handle_locale_generate(req: Request, state: &Arc<AppState>) -> Response {
    let params: LocaleGenerateParams = match req.params.and_then(|p| serde_json::from_value(p).ok())
    {
        Some(p) => p,
        None => {
            return Response::err(
                req.id,
                errors::INVALID_PARAMS,
                "Missing or invalid locale generate params",
            )
        }
    };
    let input = tauqe_flow::api::types::LocaleGenerateParams {
        language: params.language,
        items: params
            .items
            .into_iter()
            .map(|item| tauqe_flow::api::types::LocaleCatalogItem {
                key: item.key,
                english: item.english,
                context: item.context,
                max_width: item.max_width,
            })
            .collect(),
    };
    let observer = Arc::new(crate::publication::Publisher {
        out: state.out.clone(),
    });
    match state
        .application
        .generate_locale(input, observer, crate::reap::task_exit_deadline())
        .await
    {
        Ok(result) => Response::ok_typed(
            req.id,
            &tauqe_protocol::LocaleGenerateResult {
                language: result.language,
                translations: result.translations,
            },
        ),
        Err(e) => locale_error(req.id, e),
    }
}
fn locale_error(id: tauqe_protocol::RequestId, error: LocaleGenerationError) -> Response {
    let code = match &error {
        LocaleGenerationError::Busy => errors::OPERATION_IN_PROGRESS,
        LocaleGenerationError::Invalid(_) => errors::INVALID_PARAMS,
        LocaleGenerationError::Provider(_) => errors::NO_API_KEY,
        LocaleGenerationError::Model(_) => errors::MODEL_ERROR,
        LocaleGenerationError::Cancelled(_) => errors::CANCELLED,
    };
    Response::err(id, code, error.to_string())
}
#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tauqe_core::providers::mock::{MockLlmProvider, MockResponse};
    use tauqe_core::providers::ResponseFormat;
    use tauqe_core::providers::{LlmProvider, StreamEvent};
    use tauqe_protocol::{events, LocaleCatalogItem, LocaleGenerateResult, RequestId};

    /// Hard budget for a fixture whose subject is a task that reaches no
    /// suspension point.
    ///
    /// The stretch ends at this budget on its own, so a flag that is never set
    /// cannot pin a worker thread until process exit.
    const SYNCHRONOUS_STRETCH_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

    /// Releases a fixture's synchronization flag on stack unwinding.
    ///
    /// An imperative release placed after the assertions never runs when one of
    /// them fails, which leaves the parked task spinning and hangs the runtime
    /// at its own shutdown.
    struct StretchRelease(Arc<AtomicBool>);

    impl Drop for StretchRelease {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

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
        state.application.test_set_provider(provider).await;
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
        state.application.test_set_provider(provider).await;
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
        let snapshot = state.usage_snapshot().await;
        assert_eq!(snapshot.operations, 1);
        assert!(snapshot.cur.is_none());
        assert!(!state.is_operation_active(), "the slot is released");
    }

    /// Provider double that reports usage and then terminates the stream with an
    /// explicit cancellation, so the cancelled terminal path is exercised
    /// instead of the ordinary success path.
    struct CancellingProvider {
        usage: tauqe_core::providers::ModelUsageInfo,
    }

    #[async_trait]
    impl LlmProvider for CancellingProvider {
        fn kind(&self) -> tauqe_core::models::ProviderKind {
            tauqe_core::models::ProviderKind::OpenRouter
        }

        async fn stream(
            &self,
            _request: tauqe_core::providers::LlmRequest<'_>,
            tx: tokio::sync::mpsc::Sender<StreamEvent>,
            _cancel: tauqe_core::stop::HardCancellation,
        ) -> tauqe_core::providers::ProviderResult {
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
        let usage = tauqe_core::providers::ModelUsageInfo {
            prompt_tokens: 20,
            completion_tokens: 8,
            total_tokens: 28,
            cost: Some(0.4),
            ..tauqe_core::providers::ModelUsageInfo::default()
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

        let snapshot = state.usage_snapshot().await;
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
        let usage = tauqe_core::providers::ModelUsageInfo {
            prompt_tokens: 5,
            completion_tokens: 1,
            total_tokens: 6,
            cost: Some(0.1),
            ..tauqe_core::providers::ModelUsageInfo::default()
        };
        let provider: Arc<dyn LlmProvider> = Arc::new(CancellingProvider { usage });
        let (state, mut rx) = state_with_provider_on_wire(provider).await;

        let catalog = params(vec![item("tab.review", "Review", None)]);
        let response = handle_locale_generate(request(&catalog), &state).await;
        assert_eq!(
            response.error.expect("cancellation must surface").code,
            errors::CANCELLED
        );

        let snapshot = state.usage_snapshot().await;
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
        fn kind(&self) -> tauqe_core::models::ProviderKind {
            tauqe_core::models::ProviderKind::OpenRouter
        }

        async fn stream(
            &self,
            _request: tauqe_core::providers::LlmRequest<'_>,
            _tx: tokio::sync::mpsc::Sender<StreamEvent>,
            _cancel: tauqe_core::stop::HardCancellation,
        ) -> tauqe_core::providers::ProviderResult {
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
    /// Provider double that reports a terminal error and then parks forever
    /// without observing the cancellation signal, modeling a call that fails
    /// while its own task is still running.
    struct FailingThenWedgedProvider {
        stopped: Arc<AtomicBool>,
    }

    #[async_trait]
    impl LlmProvider for FailingThenWedgedProvider {
        fn kind(&self) -> tauqe_core::models::ProviderKind {
            tauqe_core::models::ProviderKind::OpenRouter
        }

        async fn stream(
            &self,
            _request: tauqe_core::providers::LlmRequest<'_>,
            tx: tokio::sync::mpsc::Sender<StreamEvent>,
            _cancel: tauqe_core::stop::HardCancellation,
        ) -> tauqe_core::providers::ProviderResult {
            // The marker and the sender stay alive across the park: the future
            // only ever completes by being aborted, exactly like a provider call
            // that fails and then hangs instead of returning.
            let _marker = ProviderStopMarker(Arc::clone(&self.stopped));
            let _ = tx.send(StreamEvent::Error("boom".to_string())).await;
            std::future::pending::<()>().await;
            Ok(())
        }
    }

    /// A provider that reports a terminal error and then keeps running must not
    /// leave its task behind: the error path terminates the stream task the way
    /// the cancellation path does, so no provider call outlives the response
    /// after the operation slot has been released.
    #[tokio::test]
    async fn terminates_the_stream_task_when_the_provider_reports_an_error() {
        let stopped = Arc::new(AtomicBool::new(false));
        let provider: Arc<dyn LlmProvider> = Arc::new(FailingThenWedgedProvider {
            stopped: Arc::clone(&stopped),
        });
        let state = state_with_provider(provider).await;

        let catalog = params(vec![item("tab.review", "Review", None)]);
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            handle_locale_generate(request(&catalog), &state),
        )
        .await
        .expect("the handler must answer the provider error");

        let err = response.error.expect("the stream error must surface");
        assert_eq!(err.code, errors::MODEL_ERROR);
        assert!(err.message.contains("boom"));
        assert!(
            stopped.load(Ordering::SeqCst),
            "the provider task must be terminated before the handler returns"
        );
        assert!(
            !state.is_operation_active(),
            "the operation slot is released"
        );
    }
    /// Provider double that delivers its terminal stream events and then closes the
    /// channel before parking the task inside a non-yielding stretch, modeling a
    /// call whose event loop already ended while its own task keeps running.
    struct WedgedAfterTerminalProvider {
        /// Set by the test to let the parked task reach a suspension point.
        released: Arc<AtomicBool>,
        /// Set once the parked task reaches its own end.
        exited: Arc<AtomicBool>,
    }

    #[async_trait]
    impl LlmProvider for WedgedAfterTerminalProvider {
        fn kind(&self) -> tauqe_core::models::ProviderKind {
            tauqe_core::models::ProviderKind::OpenRouter
        }

        async fn stream(
            &self,
            _request: tauqe_core::providers::LlmRequest<'_>,
            tx: tokio::sync::mpsc::Sender<StreamEvent>,
            _cancel: tauqe_core::stop::HardCancellation,
        ) -> tauqe_core::providers::ProviderResult {
            // The payload and the channel close first, so the handler's event loop
            // ends and reaches its completed-stream await while this task is still
            // running. The abort such an await would otherwise rely on lands only at
            // a suspension point, and the stretch below reaches none.
            let _ = tx
                .send(StreamEvent::TextDelta("{\"translations\": []}".to_string()))
                .await;
            drop(tx);
            // The spin is bounded by its own budget, so a flag that is never
            // released cannot pin a worker thread past it. The test still holds
            // a drop guard that sets the flag on unwinding.
            let stretch_deadline = std::time::Instant::now() + SYNCHRONOUS_STRETCH_BUDGET;
            while !self.released.load(Ordering::SeqCst)
                && std::time::Instant::now() < stretch_deadline
            {
                std::thread::yield_now();
            }
            self.exited.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    /// Closing the provider's event channel without Done is not a clean completion:
    /// even after the provider returns, the locale must not fall back to English.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn refuses_a_stream_that_closes_without_done() {
        // The parked task occupies a worker of its own, so the runtime keeps enough
        // threads for the handler and for the bounded confirmation to run.
        let deadline = std::time::Duration::from_millis(50);

        let released = Arc::new(AtomicBool::new(false));
        // Held for the whole test: a failed assertion unwinds through this guard
        // instead of leaving the parked task spinning forever.
        let _stretch_guard = StretchRelease(Arc::clone(&released));
        let provider = Arc::new(WedgedAfterTerminalProvider {
            released: Arc::clone(&released),
            exited: Arc::new(AtomicBool::new(false)),
        });
        let exited = Arc::clone(&provider.exited);
        let provider_handle: Arc<dyn LlmProvider> = provider;
        let state = state_with_provider(provider_handle).await;

        let catalog = params(vec![item("tab.review", "Review", None)]);
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            crate::reap::with_task_exit_deadline(
                deadline,
                handle_locale_generate(request(&catalog), &state),
            ),
        )
        .await
        .expect("the request must resolve once the task-exit deadline expires");

        let err = response
            .error
            .expect("a stream without Done must not fall back to English");
        assert_eq!(err.code, errors::MODEL_ERROR);
        assert!(
            err.message.contains("closed without Done"),
            "the response must report incomplete output: {}",
            err.message
        );
        assert!(
            !state.is_operation_active(),
            "the operation slot is released"
        );
        assert!(
            state.usage_snapshot().await.cur.is_none(),
            "the incomplete stream still closes its usage window"
        );
        assert!(
            exited.load(Ordering::SeqCst),
            "the provider must finish before its missing Done is classified"
        );

        // The handle was released deliberately and no second kill is attempted: the
        // abandoned task ends at the first suspension point its stretch reaches.
        released.store(true, Ordering::SeqCst);
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !exited.load(Ordering::SeqCst) {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the abandoned task must end once its stretch ends");
    }
}
