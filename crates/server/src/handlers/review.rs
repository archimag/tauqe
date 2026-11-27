//! RPC handlers for review commands.

mod analysis;
mod batch;
mod groups;
mod preview;
mod refinement;
mod storage_ops;
mod synthesis;
// The deterministic projection of the audit window a launch reads.
mod window;

pub use analysis::handle_review_analyze;
pub use batch::handle_review_execute;
pub use preview::handle_review_preview;
pub use refinement::handle_review_refine_group;
pub use storage_ops::{
    handle_review_delete, handle_review_get, handle_review_list, handle_review_update_group_step,
    handle_review_update_item,
};
pub use synthesis::{handle_review_cancel, handle_review_start};
// The implementation host publishes a review session before it starts the model
// turn of a review scope, so the refreshed lifecycle reaches the client before
// the turn streams.
// The integration tests import the batch supervisor and the shared context
// helpers directly from their submodules, so no test-only re-exports are
// needed here.
#[cfg(test)]
mod tests;
