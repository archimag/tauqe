//! Runtime-aware execution of synchronous, blocking work.
//!
//! The core engine is synchronous by design: the Git layer shells out through
//! `std::process::Command`, and the context and edit layers read and write
//! files with `std::fs`. The autonomous turn drives these routines from inside
//! async tasks, so a slow `git status` or a burst of file writes would run
//! directly on a Tokio worker thread and starve the pool, stalling every other
//! task on the runtime, including UI RPC handling and provider streaming.
//!
//! [`run_blocking`] routes such work through the runtime's blocking-aware path.
//! On a multi-threaded runtime it hands the worker's slot back to the executor
//! with [`tokio::task::block_in_place`], so the pool keeps its full capacity
//! while the call runs. On a current-thread runtime, on a blocking thread, or
//! outside any runtime it runs the closure directly, keeping tests and
//! synchronous callers panic-free. The helper is reentrant: a blocking call
//! nested inside another runs inline instead of stacking hand-offs.
//!
//! A synchronous caller cannot avoid occupying its own thread. On a
//! current-thread runtime that thread is the *only* executor thread, so
//! [`run_blocking`] stalls the whole runtime for the duration of the call.
//! Async callers that drive blocking work from inside a task must therefore
//! route it through [`run_blocking_async`], which suspends the task and runs
//! the closure on a dedicated blocking thread, leaving the executor free to
//! poll the rest of the runtime (streaming, cancellations, RPC handling).

use std::cell::Cell;

use tokio::runtime::{Handle, RuntimeFlavor};

thread_local! {
    /// Whether the current thread is already running inside [`run_blocking`].
    static IN_BLOCKING: Cell<bool> = const { Cell::new(false) };
}

/// Runs `f`, moving it off the async worker pool when the caller is on a
/// multi-threaded Tokio runtime.
pub(crate) fn run_blocking<T>(f: impl FnOnce() -> T) -> T {
    if IN_BLOCKING.with(Cell::get) {
        return f();
    }

    match Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| {
                IN_BLOCKING.with(|flag| flag.set(true));
                let _reset = ResetBlockingFlag;
                f()
            })
        }
        _ => f(),
    }
}

/// Runs `f` on the runtime's blocking thread pool and awaits its completion.
///
/// Unlike the synchronous [`run_blocking`], this async variant suspends the
/// current task while the closure runs on a dedicated blocking thread instead
/// of occupying the caller's thread. On a current-thread runtime that is the
/// difference between the whole executor stalling on a slow `git` invocation
/// and the executor staying free to poll every other task until the call
/// returns. On a multi-threaded runtime, or when no runtime is present, it
/// delegates to [`run_blocking`] so the `block_in_place` fast path and the
/// panic-free synchronous fallback are preserved.
pub(crate) async fn run_blocking_async<T, F>(f: F) -> T
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    match Handle::try_current() {
        Ok(handle) => match handle.spawn_blocking(f).await {
            Ok(value) => value,
            Err(join_error) => resume_join_panic(join_error),
        },
        Err(_) => run_blocking(f),
    }
}

/// Propagates the failure of an offloaded blocking task.
///
/// `spawn_blocking` only reports an error when the closure panicked or the
/// runtime shut down before it finished. There is no value to hand back in
/// either case, so the panic is resumed (or synthesized for a shutdown
/// cancellation) to keep the failure visible instead of fabricating a value.
fn resume_join_panic(join_error: tokio::task::JoinError) -> ! {
    if join_error.is_panic() {
        std::panic::resume_unwind(join_error.into_panic());
    }
    std::panic::resume_unwind(Box::new(
        "blocking task was cancelled before completing",
    ));
}

/// Clears the thread-local blocking flag when the guarded scope ends, including
/// during unwinding, so a failed call never leaves the thread marked as blocking.
struct ResetBlockingFlag;

impl Drop for ResetBlockingFlag {
    fn drop(&mut self) {
        IN_BLOCKING.with(|flag| flag.set(false));
    }
}

#[cfg(test)]
mod tests {
    use super::{run_blocking, run_blocking_async};

    #[test]
    fn runs_outside_a_runtime() {
        assert_eq!(run_blocking(|| 42), 42);
    }

    #[test]
    fn nested_calls_run_inline_without_a_runtime() {
        assert_eq!(run_blocking(|| run_blocking(|| 7)), 7);
    }

    #[tokio::test]
    async fn runs_on_a_current_thread_runtime() {
        assert_eq!(run_blocking(|| 11), 11);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn runs_on_a_multi_thread_runtime() {
        assert_eq!(run_blocking(|| 13), 13);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn nested_calls_run_inline_on_a_multi_thread_runtime() {
        assert_eq!(run_blocking(|| run_blocking(|| 17)), 17);
    }

    #[tokio::test]
    async fn async_runs_on_a_current_thread_runtime() {
        assert_eq!(run_blocking_async(|| 19).await, 19);
    }

    #[tokio::test]
    async fn async_offloads_off_the_current_thread_executor() {
        // On a current-thread runtime the closure must leave the single
        // executor thread, otherwise a blocking call would stall it.
        let executor_thread = std::thread::current().id();
        let worker_thread = run_blocking_async(|| std::thread::current().id()).await;
        assert_ne!(executor_thread, worker_thread);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn async_runs_on_a_multi_thread_runtime() {
        let executor_thread = std::thread::current().id();
        let worker_thread = run_blocking_async(|| std::thread::current().id()).await;
        assert_ne!(executor_thread, worker_thread);
        assert_eq!(run_blocking_async(|| 23).await, 23);
    }
}
