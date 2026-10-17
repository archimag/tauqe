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
use tauqe_protocol::{
    errors, events, Event, LocaleGenerateParams, ModelRole, ProviderKind, Request, Response,
};

use crate::state::AppState;

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
    let stream_task = tokio::spawn(async move {
        provider
            .stream_chat(&model_name, messages, Some(response_format), tx, cancel_rx)
            .await
    });

    let mut raw_text = String::new();
    let mut usage = None;
    let mut stream_error = None;
    let mut cancelled = false;
    while let Some(event) = rx.recv().await {
        match event {
            StreamEvent::TextDelta(delta) => raw_text.push_str(&delta),
            StreamEvent::Usage(reported) => usage = Some(reported),
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

    if cancelled {
        stream_task.abort();
        let _ = stream_task.await;
        return Response::err(req.id, errors::CANCELLED, "Locale generation was cancelled");
    }
    if let Some(message) = stream_error {
        return Response::err(req.id, errors::MODEL_ERROR, message);
    }
    match stream_task.await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => return Response::err(req.id, errors::MODEL_ERROR, err.to_string()),
        Err(join_err) => {
            return Response::err(
                req.id,
                errors::MODEL_ERROR,
                format!("Locale generation stream task failed: {}", join_err),
            );
        }
    }

    // Account for the provider cost of this one-shot call.
    let usage = usage.unwrap_or_default();
    let cost = usage.cost.unwrap_or(0.0);
    let total_cost = {
        let mut total = state.total_cost.lock().await;
        *total += cost;
        *total
    };
    state.out.send_event(&Event {
        method: events::MODEL_USAGE.to_string(),
        params: serde_json::to_value(tauqe_protocol::ModelUsageEvent {
            operation_id: format!("op-{}", crate::state::next_operation_id()),
            usage: usage.clone(),
            session_total_cost: total_cost,
            current_cost: Some(cost),
        })
        .ok(),
    });

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
    use crate::transport::OutChannel;
    use tauqe_core::providers::mock::{MockLlmProvider, MockResponse};
    use tauqe_core::providers::ResponseFormat;
    use tauqe_protocol::{LocaleCatalogItem, LocaleGenerateResult, RequestId};

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
        let (out, _rx) = OutChannel::new();
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
        let state =
            state_with_provider(Arc::new(MockLlmProvider::scripted([MockResponse::text("{}")])))
                .await;
        let _guard = state.try_begin_operation().expect("register operation");

        let catalog = params(vec![item("a", "A", None)]);
        let response = handle_locale_generate(request(&catalog), &state).await;
        assert_eq!(response.error.unwrap().code, errors::OPERATION_IN_PROGRESS);
    }

    #[tokio::test]
    async fn surfaces_a_provider_stream_error() {
        let state = state_with_provider(Arc::new(MockLlmProvider::scripted([
            MockResponse::error("boom"),
        ])))
        .await;

        let catalog = params(vec![item("a", "A", None)]);
        let response = handle_locale_generate(request(&catalog), &state).await;
        let err = response.error.expect("provider error must surface");
        assert_eq!(err.code, errors::MODEL_ERROR);
        assert!(err.message.contains("boom"));
    }
}
