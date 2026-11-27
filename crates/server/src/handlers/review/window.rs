//! RPC projections for core-owned audit windows.

use tauqe_flow::api::types::ReviewWindow;
use tauqe_protocol::review::ReviewPreviewWindow;

pub(super) fn window_dto(window: &ReviewWindow) -> ReviewPreviewWindow {
    ReviewPreviewWindow {
        context_files: window.context_files,
        context_tokens: window.context_tokens,
        matched_files: window.matched_files,
        selected_files: window.selected_files,
        selected_tokens: window.selected_tokens,
        // Keep the retired keys for older clients without advertising a
        // separate file budget that no longer controls expansion.
        omitted_files: 0,
        omitted_by_budget: 0,
        unreadable_files: window.unreadable_files,
        files_token_budget: 0,
    }
}
