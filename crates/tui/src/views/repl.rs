mod diff_cache;
pub mod input;
mod reasoning;
pub mod render;
mod state;

pub use self::diff_cache::DiffRenderCache;
pub(crate) use self::diff_cache::RenderedDiff;
pub use self::input::{handle_repl_key, REPL_COMMAND_MAP};
pub use self::reasoning::ReasoningState;
pub use self::render::{compute_model_lines, SPINNER_FRAMES};
pub use self::state::{
    EditOp, FileEditStatus, ModelCodeBlock, ReplFocus, ReplView, RetryingState, StreamingFileEdit,
    StreamingHunk, ToolchainState, TurnStatus,
};

impl ReplView {
    /// Resolves the rendered code block `block_id` to the exact source it drew.
    ///
    /// The view owns the blocks it rendered, so the pointer dispatcher asks it
    /// what to copy instead of re-deriving a block from the action (c-1, c-2).
    /// `None` means the block no longer exists and nothing happens.
    pub fn code_block_source(&self, block_id: usize) -> Option<String> {
        self.code_blocks
            .iter()
            .find(|block| block.id == block_id)
            .map(|block| block.code.clone())
    }

    /// Arms the flash badge of a block whose copy has already succeeded.
    ///
    /// The clipboard round trip is awaited outside the state guard, so the view
    /// only records the outcome here and never performs the write itself (c-1).
    pub fn arm_code_copy_flash(&mut self, block_id: usize) {
        self.copy_flash = Some((block_id, std::time::Instant::now()));
        self.update_markdown();
    }
}
