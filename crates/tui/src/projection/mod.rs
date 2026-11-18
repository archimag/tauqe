//! Pure client projections over RPC DTOs. No engine, persistence, or transport I/O.
mod implementation;
mod item;
mod node;
mod render;
mod review;
pub use implementation::{compute_items_status, PlanImplementationProjection, PlanScopeEvaluation};
pub use item::{PlanItemProjection, PlanStatusProjection};
pub use node::{PlanProjection, PlanValidationError};
pub use render::PlanRendering;
pub use review::{ReviewGroupProjection, ReviewProjection, ReviewStatusProjection};
#[cfg(test)]
mod plan_tests;
#[cfg(test)]
mod review_tests;
