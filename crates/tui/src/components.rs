//! Reusable interface components.
//!
//! General-purpose widgets shared across every view live here. The module root
//! declares its submodules next to the `components/` directory (never a
//! `mod.rs`), following the repository module layout conventions.

pub mod confirm;
pub mod editor;
pub mod modal;
pub mod selector;

pub use confirm::{
    confirm_modal_geometry, render_modal_confirm, ConfirmDialog, ConfirmModalGeometry,
    ConfirmModalParams,
};
pub use editor::{default_prompt_history_path, is_char_typing, TextEditor};
pub use modal::{render_modal_frame, ModalFrame, ModalFrameParams};
pub use selector::ListSelector;
