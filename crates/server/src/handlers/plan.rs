//! Plan RPC handlers facade.
//!
//! The plan subsystem is decomposed into focused submodules: shared helpers
//! (`common`), the CRUD RPC handlers (`storage_ops`), the architect
//! horizon-synthesis engine (`engine`), single-step execution (`step`), and
//! batch orchestration (`batch`). This facade re-exports exactly the entry
//! points other handlers and the review subsystem depend on, keeping the
//! public surface auditable and stable.

mod batch;
mod common;
mod engine;
mod horizon;
mod step;
mod storage_ops;

#[cfg(test)]
mod tests;

pub use batch::{
    handle_plan_execute, handle_plan_execute_pipeline, handle_plan_refine, handle_plan_replan,
};
pub(crate) use common::find_duplicate_item_ids;
pub(crate) use step::preload_step_files;
pub use storage_ops::{
    handle_plan_clear_horizon, handle_plan_delete, handle_plan_get, handle_plan_list,
    handle_plan_save, handle_plan_set_active, handle_plan_update_item,
};
