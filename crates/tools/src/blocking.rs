//! Runtime-aware execution of synchronous, blocking work.
//!
//! The substrate is synchronous by design: the Git layer shells out through
//! `std::process::Command`, and the repository map, the context layer, and the
//! edit layer read and write files with `std::fs`. The autonomous turn drives
//! these routines from inside async tasks, so a slow `git status` or a burst of
//! file writes would run directly on a Tokio worker thread and starve the pool,
//! stalling every other task on the runtime, including UI RPC handling and
//! provider streaming.
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
//!
//! An offload can also fail instead of returning its result, and its two
//! failures mean opposite things. A cancelled blocking task never ran the work
//! — the runtime shut down first — so the work is still outstanding and a
//! synchronous fallback completes it. A panicked blocking task ran the work and
//! crashed, so the failure is deterministic: retrying it would crash the
//! calling thread too and spend the work a second time.
//! [`resolve_offload_failure`] draws that distinction for every caller.

use std::cell::Cell;

use tokio::runtime::{Handle, RuntimeFlavor};
use tokio::task::JoinError;

thread_local! {
    /// Whether the current thread is already running inside [`run_blocking`].
    static IN_BLOCKING: Cell<bool> = const { Cell::new(false) };
}

/// Runs `f`, moving it off the async worker pool when the caller is on a
/// multi-threaded Tokio runtime.
pub fn run_blocking<T>(f: impl FnOnce() -> T) -> T {
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
///
/// Returns `Ok(T)` on successful completion, or `Err(JoinError)` if the offloaded
/// task panicked or was cancelled during runtime shutdown.
pub async fn run_blocking_async<T, F>(f: F) -> Result<T, JoinError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    match Handle::try_current() {
        Ok(handle) => {
            // Mark the offloaded thread as blocking before running `f`, so a
            // nested `run_blocking` call inside the closure runs inline instead
            // of attempting `block_in_place` on a blocking-pool thread (which
            // would panic). The guard is cleared when the scope ends, including
            // on panic, so the thread is never left marked.
            handle
                .spawn_blocking(move || {
                    IN_BLOCKING.with(|flag| flag.set(true));
                    let _reset = ResetBlockingFlag;
                    f()
                })
                .await
        }
        Err(_) => Ok(run_blocking(f)),
    }
}

/// Records a failed [`run_blocking_async`] result in the semantic journal.
///
/// A panicked or cancelled offloaded task degrades into a fallback value at
/// several call sites; without this record the degradation would be
/// indistinguishable from a legitimately empty result. Every call site that
/// downgrades a failed offload to a fallback must route the `JoinError`
/// through here first, so the failure point leaves an observable trace.
/// [`resolve_offload_failure`] pairs that record with the degradation itself.
pub fn log_join_error(error: &JoinError, operation: &str) {
    tracing::warn!(
        operation = operation,
        error = %error,
        panicked = error.is_panic(),
        "offloaded blocking task failed"
    );
}

/// Completes the work a failed offload left undone, or resumes the panic that
/// ended it.
///
/// The two offload failures are not interchangeable. A cancelled blocking task
/// was never run — the runtime shut down first — so the work is still
/// outstanding and `inline` performs it on the calling thread, which is exactly
/// the synchronous path the caller would have taken without the offload. A
/// panicked blocking task, in contrast, ran the work and crashed: the panic is
/// deterministic, so retrying it inline would crash the calling thread too,
/// spend the work a second time, and replace the original payload with a fresh
/// one raised where nothing remembers the real failure. The panic is therefore
/// recorded and its payload resumed here, so the failure surfaces once, carrying
/// the context it was raised with.
pub fn resolve_offload_failure<T>(
    error: JoinError,
    operation: &str,
    inline: impl FnOnce() -> T,
) -> T {
    log_join_error(&error, operation);
    if error.is_panic() {
        std::panic::resume_unwind(error.into_panic());
    }
    inline()
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
    use super::{resolve_offload_failure, run_blocking, run_blocking_async};

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
        assert_eq!(run_blocking_async(|| 19).await.unwrap(), 19);
    }

    #[tokio::test]
    async fn async_offloads_off_the_current_thread_executor() {
        // On a current-thread runtime the closure must leave the single
        // executor thread, otherwise a blocking call would stall it.
        let executor_thread = std::thread::current().id();
        let worker_thread = run_blocking_async(|| std::thread::current().id())
            .await
            .unwrap();
        assert_ne!(executor_thread, worker_thread);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn async_runs_on_a_multi_thread_runtime() {
        let executor_thread = std::thread::current().id();
        let worker_thread = run_blocking_async(|| std::thread::current().id())
            .await
            .unwrap();
        assert_ne!(executor_thread, worker_thread);
        assert_eq!(run_blocking_async(|| 23).await.unwrap(), 23);
    }

    #[tokio::test]
    async fn async_captures_panic_as_join_error() {
        let result = run_blocking_async(|| {
            panic!("intentional panic for test");
        })
        .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().is_panic());
    }

    #[tokio::test]
    async fn cancelled_offload_runs_the_inline_fallback() {
        let handle = tokio::spawn(std::future::pending::<()>());
        handle.abort();
        let error = handle
            .await
            .expect_err("an aborted task reports a cancelled offload");
        assert!(error.is_cancelled());
        let mut inline_ran = false;
        let value = resolve_offload_failure(error, "test", || {
            inline_ran = true;
            29
        });
        assert_eq!(value, 29);
        assert!(inline_ran, "a cancelled offload is completed inline");
    }

    #[tokio::test]
    async fn panicked_offload_resumes_the_payload_without_running_inline() {
        let error = tokio::task::spawn_blocking(|| panic!("offloaded work crashed"))
            .await
            .expect_err("a panicking task reports a panicked offload");
        assert!(error.is_panic());
        let mut inline_ran = false;
        let resumed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            resolve_offload_failure(error, "test", || {
                inline_ran = true;
                31
            })
        }));
        let payload = resumed.expect_err("a panicked offload resumes its payload");
        assert_eq!(
            payload.downcast_ref::<&str>().copied(),
            Some("offloaded work crashed")
        );
        assert!(!inline_ran, "a panicked offload is never retried inline");
    }
}
