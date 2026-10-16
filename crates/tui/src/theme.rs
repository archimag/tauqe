//! Semantic design system for the terminal interface.
//!
//! The subsystem owns the interface color vocabulary: semantic design tokens
//! (see `palette`) expressed in 24-bit sRGB, the projection engine (see
//! `color`) that degrades them onto whatever color depth the terminal supports,
//! and the built-in calibrated presets (see `presets`) users select by name.
//!
//! The design system is fully integrated: the render pipeline, modal dialogs,
//! and interactive selectors resolve their styling through the semantic palette
//! and the projection engine.

pub mod color;
pub mod engine;
pub mod palette;
pub mod presets;
pub mod selector;
