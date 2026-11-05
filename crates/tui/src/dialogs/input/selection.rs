use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent};
use tokio::sync::{Mutex, MutexGuard};

use crate::app::AppState;
use crate::input::InputResult;
use crate::rpc::{
    allocate_request_id, send_request_with_rollback, OptimisticRollback, RpcConnection,
};
use tauqe_protocol::{methods, ModelEntry, ModelSelectParams, ModelSelection};

/// Applies a model choice optimistically, sends the selection, and reverts the
/// header inside this function when the request never reaches the transport.
///
/// The rollback travels with the request only once the write succeeds; when the
/// write fails the same value is applied inline, because the daemon never saw the
/// request and no response will ever undo the mutation. The failure is reported
/// once, at the site that owns the revert, mirroring `start_turn` (c-1, c-3).
async fn apply_model_selection(
    state: &Arc<Mutex<AppState>>,
    mut st: MutexGuard<'_, AppState>,
    selection: ModelSelection,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<()> {
    st.modals.selection_dialog = None;
    let prev_model = st.active_model.clone();
    let prev_selection = std::mem::replace(&mut st.model_choice.selection, selection.clone());
    if let Some(effective) = st.model_choice.resolve(&selection) {
        st.active_model = effective;
    }
    let rollback = OptimisticRollback::ActiveModel {
        prev_model,
        prev_selection,
    };
    let req_id = allocate_request_id();
    let params = serde_json::to_value(ModelSelectParams { selection })?;
    drop(st);

    let result = send_request_with_rollback(
        server_writer,
        req_id,
        methods::MODEL_SELECT,
        params,
        &rollback,
    )
    .await;
    if let Err(error) = result {
        // The request never reached the daemon, so no response will ever undo the
        // optimistic mutation: revert it here and report the failure once, at the
        // site that owns the revert, mirroring `start_turn` (c-1, c-3).
        let mut st = state.lock().await;
        crate::rpc::responses::apply_optimistic_rollback(&mut st, rollback);
        let message = crate::i18n::active_t("notify.command_error");
        st.notify_error(message.replacen("{}", &error.to_string(), 1));
    }
    Ok(())
}

/// Applies the chosen primary rung, shared by the keyboard and mouse paths.
///
/// The picker presents strictly the configured primary tiers, so the single
/// activation path sends the whole rung — the model together with the reasoning
/// depth it was configured with — and the label on screen is exactly the value
/// that reaches the resolver (c-1, c-3). Collapsing it to `Specific` would drop
/// the effort and make the second entry of the same model resolve like the first.
/// A rung whose request never reaches the transport is reverted inside the shared
/// helper, so the header never keeps a model the server did not accept.
pub(crate) async fn activate_selection_item(
    state: &Arc<Mutex<AppState>>,
    st: MutexGuard<'_, AppState>,
    entry: ModelEntry,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<()> {
    apply_model_selection(
        state,
        st,
        ModelSelection::Tier {
            model: entry.model,
            effort: entry.effort,
        },
        server_writer,
    )
    .await
}

/// Handles keyboard input for the model picker.
///
/// Navigation, paging, and the digit quick-select are delegated to the shared
/// [`crate::components::ModelSelector`] over the exact window the renderer
/// draws, so the picker behaves like every role dialog and its scroll arithmetic
/// exists in one place (c-1). This module only resolves activation (`Enter`,
/// `Space`) and dismissal (`Esc`, `q`), so it never intercepts edited text.
pub(super) async fn handle_selection_dialog_key(
    key: KeyEvent,
    state: &Arc<Mutex<AppState>>,
    mut st: MutexGuard<'_, AppState>,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<Option<InputResult>> {
    let mut dialog = match st.modals.selection_dialog.take() {
        Some(dialog) => dialog,
        None => return Ok(None),
    };

    // Reuse the geometry of the last rendered frame instead of querying the OS
    // terminal size mid-event: it avoids an unnecessary syscall and keeps the
    // scroll viewport aligned with what is actually on screen (and with the
    // frame area customized by tests).
    let area =
        crate::dialogs::render::selection_dialog_area(st.last_frame_area, dialog.selector.len());
    let visible_height = crate::dialogs::render::selection_dialog_visible_height(area);

    if dialog.selector.handle_list_key(key, visible_height) {
        st.modals.selection_dialog = Some(dialog);
        return Ok(Some(InputResult::Continue));
    }

    // `Enter` and `Space` activate the highlighted rung. Both share one path
    // with the mouse click, so the picker cannot activate a row two ways.
    if matches!(key.code, KeyCode::Enter | KeyCode::Char(' ')) {
        if let Some(entry) = dialog.selector.selected_entry().cloned() {
            activate_selection_item(state, st, entry, server_writer).await?;
            return Ok(Some(InputResult::Continue));
        }
    }

    match key.code {
        // `Esc` and `q` dismiss the dialog; any other key keeps it open.
        KeyCode::Esc | KeyCode::Char('q') => {}
        _ => st.modals.selection_dialog = Some(dialog),
    }
    Ok(Some(InputResult::Continue))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use tauqe_protocol::{ModelEntry, ModelRef, ModelRole, ModelSelection, ReasoningEffort};
    use tokio::sync::Mutex;

    use super::{activate_selection_item, handle_selection_dialog_key};
    use crate::app::{test_state, NotificationLevel, SelectionDialogState};
    use crate::rpc::{allocate_request_id, RpcConnection};

    /// Three rungs of one model at distinct reasoning depths, so a test can
    /// prove which coordinate of the picker reached the wire (c-3).
    fn same_model_rungs() -> Vec<ModelEntry> {
        let model = ModelRef::openrouter("openai/gpt-4o");
        vec![
            ModelEntry::new(model.clone(), Some(ReasoningEffort::Low)),
            ModelEntry::new(model.clone(), Some(ReasoningEffort::Medium)),
            ModelEntry::new(model, Some(ReasoningEffort::High)),
        ]
    }

    /// Activating a configured primary rung sends the whole tier, so the
    /// reasoning depth shown on the row reaches the resolver instead of being
    /// dropped in favour of the first entry of the same model (c-3).
    #[tokio::test]
    async fn primary_entry_activation_sends_tier_with_effort() {
        let mut st = test_state();

        let entry = ModelEntry::new(
            ModelRef::openrouter("openai/gpt-4o"),
            Some(ReasoningEffort::High),
        );
        st.modals.selection_dialog = Some(SelectionDialogState::new(vec![entry.clone()], 0));

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let guard = state.lock().await;
        activate_selection_item(&state, guard, entry, &mut conn)
            .await
            .expect("activation writes the model selection request");

        let written = String::from_utf8_lossy(conn.written()).to_string();
        assert!(written.contains("model/select"), "got: {written}");
        assert!(written.contains("\"effort\":\"high\""), "got: {written}");
    }

    /// `Enter` dispatches the highlighted rung exactly as it is drawn, so the
    /// label on screen and the value on the wire never disagree when the same
    /// model is configured at several depths (c-3).
    #[tokio::test]
    async fn enter_activates_the_highlighted_rung_of_a_repeated_model() {
        let mut st = test_state();
        // The picker reopens on the rung the developer picked last, which is the
        // index the highlight resolution seeds.
        st.modals.selection_dialog = Some(SelectionDialogState::new(same_model_rungs(), 1));

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let guard = state.lock().await;
        let handled = handle_selection_dialog_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &state,
            guard,
            &mut conn,
        )
        .await
        .expect("activation succeeds");
        assert!(handled.is_some(), "the picker consumes the key");
        assert!(
            state.lock().await.modals.selection_dialog.is_none(),
            "a dispatched selection closes the picker"
        );

        let written = String::from_utf8_lossy(conn.written()).to_string();
        assert!(written.contains("model/select"), "got: {written}");
        assert!(written.contains("\"effort\":\"medium\""), "got: {written}");
    }

    /// A digit only steers the highlight; the `Enter` that follows dispatches the
    /// rung it steered to, so positional quick-select never highlights one rung
    /// and runs another (c-1, c-3).
    #[tokio::test]
    async fn digit_quick_select_steers_the_following_activation() {
        let mut st = test_state();
        st.modals.selection_dialog = Some(SelectionDialogState::new(same_model_rungs(), 0));

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        {
            let guard = state.lock().await;
            handle_selection_dialog_key(
                KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE),
                &state,
                guard,
                &mut conn,
            )
            .await
            .expect("the digit is handled");
        }

        assert!(
            conn.written().is_empty(),
            "steering the highlight spends no selection"
        );
        let highlighted = state
            .lock()
            .await
            .modals
            .selection_dialog
            .as_ref()
            .map(|dialog| dialog.selector.selected_index());
        assert_eq!(highlighted, Some(2));

        {
            let guard = state.lock().await;
            handle_selection_dialog_key(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                &state,
                guard,
                &mut conn,
            )
            .await
            .expect("activation succeeds");
        }

        let written = String::from_utf8_lossy(conn.written()).to_string();
        assert!(written.contains("\"effort\":\"high\""), "got: {written}");
    }

    /// The click route resolves a visible row through the selector's own window
    /// and dispatches that rung, so the pointer and the keyboard cannot end up on
    /// different models (c-1, c-3).
    #[tokio::test]
    async fn clicked_row_dispatches_the_rung_the_window_resolves() {
        let mut st = test_state();
        st.modals.selection_dialog = Some(SelectionDialogState::new(same_model_rungs(), 0));

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        // The pointer path highlights the resolved row, then activates the
        // highlighted entry; mirroring that order keeps the test honest about
        // which step supplies the model.
        let entry = {
            let mut guard = state.lock().await;
            let dialog = guard
                .modals
                .selection_dialog
                .as_mut()
                .expect("picker stays open");
            let index = dialog.selector.visible_start(3) + 2;
            dialog.selector.select_index(index);
            dialog
                .selector
                .selected_entry()
                .cloned()
                .expect("the row resolves to a rung")
        };

        let guard = state.lock().await;
        activate_selection_item(&state, guard, entry, &mut conn)
            .await
            .expect("activation writes the request");

        let written = String::from_utf8_lossy(conn.written()).to_string();
        assert!(written.contains("\"effort\":\"high\""), "got: {written}");
    }
    /// Async sink whose every write fails, standing in for a server pipe that
    /// closed before the selection reached it.
    struct FailingWriter;

    impl tokio::io::AsyncWrite for FailingWriter {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "server pipe closed",
            )))
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// A selection whose request never reaches the transport reverts the header
    /// to the previously active model, so the interface keeps no model the server
    /// never accepted, and it leaves no tracker entry whose id will never receive
    /// a response (c-3).
    #[tokio::test]
    async fn failed_model_selection_reverts_the_optimistic_update() {
        let mut st = test_state();
        let previous_model = ModelRef::openrouter("openai/gpt-4o-mini");
        st.active_model = previous_model.clone();
        st.model_choice.selection = ModelSelection::Role(ModelRole::Coder);
        st.modals.selection_dialog = Some(SelectionDialogState::new(same_model_rungs(), 2));

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(FailingWriter);

        // The request id is allocated inside the activation, so the tracked
        // window is bracketed by two client-side allocations (c-3).
        let window_start = allocate_request_id();
        {
            let guard = state.lock().await;
            let entry = guard
                .modals
                .selection_dialog
                .as_ref()
                .and_then(|dialog| dialog.selector.selected_entry())
                .cloned()
                .expect("the seeded highlight resolves to a rung");
            activate_selection_item(&state, guard, entry, &mut conn)
                .await
                .expect("a failed write is reported in the interface, not as a dispatch error");
        }
        let window_end = allocate_request_id();

        assert!(conn.written().is_empty(), "the transport accepted no bytes");
        for id in window_start..window_end {
            assert!(
                conn.take_optimistic_rollback(id).is_none(),
                "request {id} leaked a rollback entry"
            );
        }

        let guard = state.lock().await;
        assert_eq!(guard.active_model, previous_model);
        assert!(matches!(
            guard.model_choice.selection,
            ModelSelection::Role(ModelRole::Coder)
        ));
        let notification = guard
            .active_notification()
            .expect("the failed write explains itself");
        assert_eq!(notification.level, NotificationLevel::Error);
    }
    /// Presses one physical key through the interface gateway, so a regression
    /// test exercises the layout normalization the real event loop applies
    /// before any modal handler runs (c-single-choke-point).
    async fn press(
        state: &Arc<Mutex<crate::app::AppState>>,
        conn: &mut RpcConnection,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) {
        crate::input::handle_terminal_event(
            crossterm::event::Event::Key(KeyEvent::new(code, modifiers)),
            state,
            conn,
        )
        .await
        .expect("in-memory dispatch never fails");
    }

    /// Modifiers of a plain character press.
    const PLAIN: KeyModifiers = KeyModifiers::NONE;

    /// Under the JCUKEN layout the picker keeps the row vocabulary it declares:
    /// the arrows step the highlight unchanged by the language, the digits keep
    /// their positional meaning, and `й` (the physical `q`) dismisses the dialog
    /// instead of leaving it open on the raw Cyrillic code point
    /// (c-modal-dispatch-parity).
    ///
    /// The rows answer to the arrows rather than to the bare `n` / `p` letters in
    /// either layout, so the projection is proven where this dialog actually
    /// reads a letter: its dismissal.
    #[tokio::test]
    async fn selection_dialog_navigates_and_dismisses_from_the_jcuken_layout() {
        let mut st = test_state();
        st.tui_config.input.layout = crate::config::LayoutPreset::RuJcuken;
        st.last_frame_area = ratatui::layout::Rect::new(0, 0, 120, 40);
        st.modals.selection_dialog = Some(SelectionDialogState::new(same_model_rungs(), 0));

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        for (code, expected) in [(KeyCode::Down, 1usize), (KeyCode::Up, 0)] {
            press(&state, &mut conn, code, PLAIN).await;
            let guard = state.lock().await;
            let dialog = guard
                .modals
                .selection_dialog
                .as_ref()
                .expect("the picker stays open while it navigates");
            assert_eq!(
                dialog.selector.selected_index(),
                expected,
                "{code:?} must move the highlight"
            );
        }

        // The digits keep their positional meaning: `3` and `1` name the third
        // and the first rung of the same model at distinct reasoning depths.
        for (character, expected) in [('3', 2usize), ('1', 0)] {
            press(&state, &mut conn, KeyCode::Char(character), PLAIN).await;
            let guard = state.lock().await;
            let dialog = guard
                .modals
                .selection_dialog
                .as_ref()
                .expect("the picker stays open while a digit steers it");
            assert_eq!(
                dialog.selector.selected_index(),
                expected,
                "{character:?} must select the row it numbers"
            );
        }

        press(&state, &mut conn, KeyCode::Char('й'), PLAIN).await;
        assert!(
            state.lock().await.modals.selection_dialog.is_none(),
            "`й` sits on the physical `q` position and must dismiss the picker"
        );
        assert!(
            conn.written().is_empty(),
            "browsing the picker never talks to the server"
        );
    }
}
