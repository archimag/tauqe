//! Compatibility shim for the unified text editor component.
//!
//! The editor implementation now lives in [`crate::components::editor`]. This
//! module keeps the historical `InputEditor` name and the prompt history path
//! helper available so existing call sites keep compiling while the views
//! migrate incrementally to the shared component.

pub use crate::components::editor::{default_prompt_history_path, TextEditor as InputEditor};
