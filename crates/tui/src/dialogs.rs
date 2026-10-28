//! Modal dialogs colocated by concern: rendering under [`render`] and input
//! Modal dialogs colocated by concern: rendering under [`render`], input
//! handling under [`input`], and the pure path-pattern preview under
//! [`pattern`].
//!
//! Dialog submodules resolve layout helpers through the canonical `crate::ui`
//! paths so that no re-export is duplicated across module boundaries.

pub mod input;
pub mod pattern;
pub mod render;
