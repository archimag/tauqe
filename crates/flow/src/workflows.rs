//! Flow-owned scenarios and reusable execution workflows.
//!
//! Plan and review entry points pass session resources and observers, not the
//! application facade. The scenarios own admission, continuation, and durable
//! outcomes; shared workflows own only the mechanics of the work they run.
//!
//! A TurnState spans one user task; its budget and accepted transcript survive
//! technical and document corrections. run_round admits exactly one request
//! and returns its outcome. It neither retries nor executes decoded operations.
//! The executor is borrowed separately and owns technical request preparation,
//! including coherent materials and full-input admission.
//! TurnWorkflow owns technical continuation and settles its transaction before
//! returning document commands to the interactive scenario. It reuses core's
//! research, staging, application, verification and recovery mechanisms.
//!
//! REPL and isolated Coder attempts for plan and review share the technical
//! turn, not its interpretation.
//! Leaf attempts and structured-request supervision are reusable workflow
//! mechanics; plan synthesis, review audit and post-analysis retain separate
//! persistence and publication decisions. The interactive REPL still uses
//! application session services for history, context and document writes.

pub(crate) mod architect;
pub(crate) mod plan;
pub(crate) mod repl;
pub(crate) mod review;
pub(crate) mod round;
mod state;
pub(crate) mod step;
pub(crate) mod structured;
pub(crate) mod turn;
