use tokio::process::ChildStdin;
use tokio::sync::MutexGuard;

use crate::app::{AppState, SelectionItem};
use crate::rpc::{
    allocate_request_id, record_optimistic_rollback, send_request_with_id, OptimisticRollback,
};
use tauqe_protocol::{methods, ModelSelectParams, ModelSelection};

/// Applies a model choice optimistically and registers a rollback so that a failed
/// `model/select` restores the previously active selection.
async fn apply_model_selection(
    mut st: MutexGuard<'_, AppState>,
    selection: ModelSelection,
    server_writer: &mut ChildStdin,
) -> anyhow::Result<()> {
    st.selection_dialog = None;
    let prev_model = st.active_model.clone();
    let prev_selection = std::mem::replace(&mut st.model_choice.selection, selection.clone());
    if let Some(effective) = st.model_choice.resolve(&selection) {
        st.active_model = effective;
    }
    let req_id = allocate_request_id();
    record_optimistic_rollback(
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
    server_writer: &mut ChildStdin,
) -> anyhow::Result<()> {
    match item {
        SelectionItem::Role(role, _) => {
            apply_model_selection(st, ModelSelection::Role(role), server_writer).await
        }
        SelectionItem::Model(model) => {
            apply_model_selection(st, ModelSelection::Specific(model), server_writer).await
        }
        SelectionItem::Others => {
            if !st.open_model_catalog() {
                st.notify_warning("No other models are available");
            }
            Ok(())
        }
    }
}
