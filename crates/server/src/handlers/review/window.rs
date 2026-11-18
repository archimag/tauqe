//! RPC projections for core-owned audit windows.

use tauqe_core::review::window::ReviewWindow;
use tauqe_protocol::review::ReviewPreviewWindow;
pub(super) fn window_dto(window: &ReviewWindow) -> ReviewPreviewWindow {
    ReviewPreviewWindow {
        context_files: window.context_files,
        context_tokens: window.context_tokens,
        matched_files: window.matched_files,
        selected_files: window.selected_files,
        selected_tokens: window.selected_tokens,
        omitted_files: window.omitted_by_budget,
        omitted_by_budget: window.omitted_by_budget,
        unreadable_files: window.unreadable_files,
        files_token_budget: window.files_token_budget,
    }
}
