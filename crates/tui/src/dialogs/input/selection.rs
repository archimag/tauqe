use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
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
    st: MutexGuard<'_, AppState>,
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

    let (term_w, term_h) = crossterm::terminal::size().unwrap_or((80, 24));
    let area = crate::dialogs::render::selection_dialog_area(
        Rect::new(0, 0, term_w, term_h),
        dialog.items.len(),
    );
    let visible_height = crate::dialogs::render::selection_dialog_visible_height(area);

    if dialog.selector.handle_nav_key(key, visible_height) {
        st.selection_dialog = Some(dialog);
        return Ok(Some(InputResult::Continue));
    }

    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {}
        KeyCode::Enter => {
            if let Some(item) = dialog.items.get(dialog.selector.selected_index()).cloned() {
                activate_selection_item(st, item, server_writer).await?;
                return Ok(Some(InputResult::Continue));
            }
            st.selection_dialog = Some(dialog);
        }
        // Quick-select digits are derived from the row position and bounds-checked
        // against the item list, so the mapping stays correct as the entries evolve.
        KeyCode::Char(c @ '1'..='9') if dialog.kind == SelectionDialogKind::PrimaryModels => {
            if let Some(item) = dialog.items.get(c as usize - '1' as usize).cloned() {
                activate_selection_item(st, item, server_writer).await?;
                return Ok(Some(InputResult::Continue));
            }
            st.selection_dialog = Some(dialog);
        }
        _ => {
            st.selection_dialog = Some(dialog);
        }
    }
    Ok(Some(InputResult::Continue))
}
