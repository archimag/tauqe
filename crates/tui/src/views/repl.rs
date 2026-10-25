mod diff_cache;
pub mod input;
mod reasoning;
pub mod render;
mod state;

pub use self::diff_cache::DiffRenderCache;
pub use self::input::{handle_repl_key, REPL_COMMAND_MAP};
pub use self::reasoning::ReasoningState;
pub use self::render::{compute_model_lines, SPINNER_FRAMES};
pub use self::state::{
    EditOp, FileEditStatus, ModelCodeBlock, ReplFocus, ReplView, RetryingState, StreamingFileEdit,
    StreamingHunk, ToolchainState, TurnStatus,
};

use crate::clipboard::CopyResult;

impl ReplView {
    /// Copies the rendered code block `block_id` and flashes its header.
    ///
    /// The view owns the blocks it drew, so it resolves the action to the exact
    /// source it rendered, copies it, and arms the flash badge; the pointer
    /// dispatcher receives only the typed action and the reported outcome
    /// (c-1, c-2). `None` means the block no longer exists and nothing happens.
    pub fn copy_code_block(&mut self, block_id: usize) -> Option<(CopyResult, usize)> {
        let code = self
            .code_blocks
            .iter()
            .find(|block| block.id == block_id)
            .map(|block| block.code.clone())?;
        let lines = code.lines().count().max(1);
        let result = crate::clipboard::copy_to_clipboard(&code);
        if result.is_success() {
            self.copy_flash = Some((block_id, std::time::Instant::now()));
            self.update_markdown();
        }
        Some((result, lines))
    }
}
