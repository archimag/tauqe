use crossterm::event::{KeyCode, KeyEvent};
use tokio::sync::MutexGuard;

use crate::app::AppState;
use crate::input::InputResult;
use crate::rpc::{allocate_request_id, send_request_with_id, OptimisticRollback, RpcConnection};
use tauqe_protocol::{methods, ModelEntry, ModelSelectParams, ModelSelection};

/// Applies a model choice optimistically and registers a rollback so that a failed
/// `model/select` restores the previously active selection.
async fn apply_model_selection(
    mut st: MutexGuard<'_, AppState>,
    selection: ModelSelection,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<()> {
    st.selection_dialog = None;
    let prev_model = st.active_model.clone();
    let prev_selection = std::mem::replace(&mut st.model_choice.selection, selection.clone());
    if let Some(effective) = st.model_choice.resolve(&selection) {
        st.active_model = effective;
    }
    let req_id = allocate_request_id();
    server_writer.record_optimistic_rollback(
        req_id,
        OptimisticRollback::ActiveModel {
            prev_model,
            prev_selection,
        },
    );
    drop(st);
    let params = ModelSelectParams { selection };
    send_request_with_id(
        server_writer,
        req_id,
        methods::MODEL_SELECT,
        serde_json::to_value(params)?,
    )
    .await
}

/// Applies the chosen primary rung, shared by the keyboard and mouse paths.
///
/// The picker presents strictly the configured primary tiers, so the single
/// activation path sends the whole rung — the model together with the reasoning
/// depth it was configured with — and the label on screen is exactly the value
/// that reaches the resolver (c-1, c-3). Collapsing it to `Specific` would drop
/// the effort and make the second entry of the same model resolve like the first.
pub(crate) async fn activate_selection_item(
    st: MutexGuard<'_, AppState>,
    entry: ModelEntry,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<()> {
    apply_model_selection(
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
    mut st: MutexGuard<'_, AppState>,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<Option<InputResult>> {
    let mut dialog = match st.selection_dialog.take() {
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
        st.selection_dialog = Some(dialog);
        return Ok(Some(InputResult::Continue));
    }

    // `Enter` and `Space` activate the highlighted rung. Both share one path
    // with the mouse click, so the picker cannot activate a row two ways.
    if matches!(key.code, KeyCode::Enter | KeyCode::Char(' ')) {
        if let Some(entry) = dialog.selector.selected_entry().cloned() {
            activate_selection_item(st, entry, server_writer).await?;
            return Ok(Some(InputResult::Continue));
        }
    }

    match key.code {
        // `Esc` and `q` dismiss the dialog; any other key keeps it open.
        KeyCode::Esc | KeyCode::Char('q') => {}
        _ => st.selection_dialog = Some(dialog),
    }
    Ok(Some(InputResult::Continue))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use tauqe_protocol::{ModelEntry, ModelRef, ReasoningEffort};
    use tokio::sync::Mutex;

    use crate::app::{test_state, SelectionDialogState};
    use crate::rpc::RpcConnection;

    use super::{activate_selection_item, handle_selection_dialog_key};

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
        st.selection_dialog = Some(SelectionDialogState::new(vec![entry.clone()], 0));

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let guard = state.lock().await;
        activate_selection_item(guard, entry, &mut conn)
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
        st.selection_dialog = Some(SelectionDialogState::new(same_model_rungs(), 1));

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let guard = state.lock().await;
        let handled = handle_selection_dialog_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            guard,
            &mut conn,
        )
        .await
        .expect("activation succeeds");
        assert!(handled.is_some(), "the picker consumes the key");
        assert!(
            state.lock().await.selection_dialog.is_none(),
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
        st.selection_dialog = Some(SelectionDialogState::new(same_model_rungs(), 0));

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        {
            let guard = state.lock().await;
            handle_selection_dialog_key(
                KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE),
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
            .selection_dialog
            .as_ref()
            .map(|dialog| dialog.selector.selected_index());
        assert_eq!(highlighted, Some(2));

        {
            let guard = state.lock().await;
            handle_selection_dialog_key(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
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
        st.selection_dialog = Some(SelectionDialogState::new(same_model_rungs(), 0));

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        // The pointer path highlights the resolved row, then activates the
        // highlighted entry; mirroring that order keeps the test honest about
        // which step supplies the model.
        let entry = {
            let mut guard = state.lock().await;
            let dialog = guard.selection_dialog.as_mut().expect("picker stays open");
            let index = dialog.selector.visible_start(3) + 2;
            dialog.selector.select_index(index);
            dialog
                .selector
                .selected_entry()
                .cloned()
                .expect("the row resolves to a rung")
        };

        let guard = state.lock().await;
        activate_selection_item(guard, entry, &mut conn)
            .await
            .expect("activation writes the request");

        let written = String::from_utf8_lossy(conn.written()).to_string();
        assert!(written.contains("\"effort\":\"high\""), "got: {written}");
    }
}
