//! Modal dialogs colocated by concern: rendering under [`render`] and input
//! handling under [`input`].
//!
//! Dialog submodules resolve layout helpers through the canonical `crate::ui`
//! paths so that no re-export is duplicated across module boundaries.

pub mod input;
pub mod render;
