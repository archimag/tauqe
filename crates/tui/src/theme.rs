//! Semantic design system for the terminal interface.
//!
//! The subsystem owns the interface color vocabulary: semantic design tokens
//! (see `palette`) expressed in 24-bit sRGB, the projection engine (see
//! `color`) that degrades them onto whatever color depth the terminal supports,
//! and the built-in calibrated presets (see `presets`) users select by name.
//!
//! The module is registered now so the design system compiles and is tested as
//! a unit; the render pipeline adopts it in a later plan step, which is why its
//! API is temporarily exempt from the dead-code lint.

#![allow(dead_code)]

pub mod color;
pub mod engine;
pub mod palette;
pub mod presets;
pub mod selector;
