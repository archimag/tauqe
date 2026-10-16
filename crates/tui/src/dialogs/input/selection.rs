use crossterm::event::{KeyCode, KeyEvent};
use tokio::sync::MutexGuard;

use crate::app::{AppState, SelectionDialogKind, SelectionItem};
use crate::input::InputResult;
use crate::rpc::{allocate_request_id, send_request_with_id, OptimisticRollback, RpcConnection};
use tauqe_protocol::{methods, ModelSelectParams, ModelSelection};

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

/// Applies the chosen picker row (shared by keyboard and mouse): a role or a model is
/// selected, `Others...` opens the full model catalog.
pub(crate) async fn activate_selection_item(
    mut st: MutexGuard<'_, AppState>,
    item: SelectionItem,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<()> {
    match item {
        SelectionItem::Primary(entry) => {
            apply_model_selection(st, ModelSelection::Specific(entry.model), server_writer).await
        }
        SelectionItem::Model(model) => {
            apply_model_selection(st, ModelSelection::Specific(model), server_writer).await
        }
        // Swap the primary picker for the full catalog, keeping the configured
        // primary models reachable while restoring access to every available
        // provider model.
        SelectionItem::Catalog => {
            st.open_model_catalog();
            Ok(())
        }
    }
}

/// Handles keyboard input for the model selection dialog. Navigation is delegated
/// to the shared [`crate::components::ListSelector`]; this module only resolves
/// activation (`Enter`, quick-select digits) and dismissal.
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
    let area = crate::dialogs::render::selection_dialog_area(
        st.last_frame_area,
        dialog.items.len(),
    );
    let visible_height = crate::dialogs::render::selection_dialog_visible_height(area);

    if dialog.selector.handle_nav_key(key, visible_height) {
        st.selection_dialog = Some(dialog);
        return Ok(Some(InputResult::Continue));
    }

    // `Enter` activates the highlighted row; a `1..9` digit quick-selects the
    // row printed with that number (Primary picker only). Both resolve to a row
    // index and share one activation path, and the digit equals the `[N]`
    // number produced by the shared `numbered_entry_label`, so the rendered
    // rows and the quick-select keys cannot drift apart.
    let target_index = match key.code {
        KeyCode::Char(c @ '1'..='9') if dialog.kind == SelectionDialogKind::PrimaryModels => {
            Some(c as usize - '1' as usize)
        }
        KeyCode::Enter | KeyCode::Char(' ') => Some(dialog.selector.selected_index()),
        _ => None,
    };

    if let Some(index) = target_index {
        if let Some(item) = dialog.items.get(index).cloned() {
            activate_selection_item(st, item, server_writer).await?;
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

    use tauqe_protocol::ModelRef;
    use tokio::sync::Mutex;

    use crate::app::{test_state, SelectionDialogKind, SelectionDialogState, SelectionItem};
    use crate::rpc::RpcConnection;

    use super::activate_selection_item;

    #[tokio::test]
    async fn catalog_pseudo_entry_opens_full_model_catalog() {
        let mut st = test_state();
        st.available_models = vec![ModelRef::openrouter("catalog-model")];
        st.selection_dialog = Some(SelectionDialogState::new(
            SelectionDialogKind::PrimaryModels,
            vec![SelectionItem::Catalog],
            0,
        ));

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let guard = state.lock().await;
        activate_selection_item(guard, SelectionItem::Catalog, &mut conn)
            .await
            .expect("catalog activation never fails");

        let guard = state.lock().await;
        let dialog = guard
            .selection_dialog
            .as_ref()
            .expect("the catalog dialog must replace the primary picker");
        assert_eq!(dialog.kind, SelectionDialogKind::ModelCatalog);
        assert_eq!(
            dialog.items,
            vec![SelectionItem::Model(ModelRef::openrouter("catalog-model"))]
        );
    }
}
