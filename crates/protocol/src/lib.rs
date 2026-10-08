#![cfg_attr(
    not(test),
    warn(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

pub const PROTOCOL_VERSION: &str = "0.1.0";

pub mod config;
pub mod context;
pub mod edit;
pub mod errors;
pub mod events;
pub mod git;
pub mod glob;
pub mod history;
pub mod message;
pub mod methods;
pub mod model;
pub mod plan;
pub mod review;

pub use config::*;
pub use context::*;
pub use edit::*;
pub use errors::*;
pub use git::*;
pub use glob::*;
pub use history::*;
pub use message::*;
pub use model::*;
pub use plan::*;
pub use review::*;
