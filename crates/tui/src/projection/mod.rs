//! Pure client projections over RPC DTOs. No engine, persistence, or transport I/O.
mod implementation;
mod item;
mod node;
mod render;
mod review;
pub use implementation::{PlanImplementationProjection, PlanScopeEvaluation};
pub use item::PlanItemProjection;
#[cfg(test)]
pub(crate) use item::PlanStatusProjection;
#[cfg(test)]
pub(crate) use node::PlanProjection;
pub use node::PlanValidationError;
pub use render::PlanRendering;
pub use review::ReviewProjection;
#[cfg(test)]
pub(crate) use review::{ReviewGroupProjection, ReviewStatusProjection};
#[cfg(test)]
mod plan_tests;
#[cfg(test)]
mod review_tests;
