//! Caller budgets for core's task-exit confirmation.

pub use tauqe_core::execution::task::{
    abort_and_reap_with_deadline, await_with_deadline, Awaited, TaskExit,
};

pub const OPERATION_UNWIND_GRACE: std::time::Duration = std::time::Duration::from_secs(30);
pub const TASK_EXIT_DEADLINE: std::time::Duration = OPERATION_UNWIND_GRACE;
