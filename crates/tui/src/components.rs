//! Reusable interface components.
//!
//! General-purpose widgets shared across every view live here. The module root
//! declares its submodules next to the `components/` directory (never a
//! `mod.rs`), following the repository module layout conventions.

pub mod confirm;
pub mod editor;
pub mod modal;
pub mod model_selector;
pub mod selector;

// The facade exposes component types only: geometry math, per-dialog parameter
// builders, and helper predicates stay behind their submodule paths, and the
// dialog renderers reach them through `dialogs::render` (Conventions §3.1).
pub use confirm::ConfirmDialog;
pub use editor::TextEditor;
pub use modal::ModalFrame;
pub use model_selector::ModelSelector;
pub use selector::ListSelector;
