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
pub fn log_join_error(error: &JoinError, operation: &str) {
    tracing::warn!(
        operation = operation,
        error = %error,
        panicked = error.is_panic(),
        "offloaded blocking task failed"
    );
}

/// Clears the thread-local blocking flag when the guarded scope ends, including
/// during unwinding, so a failed call never leaves the thread marked as blocking.
struct ResetBlockingFlag;

impl Drop for ResetBlockingFlag {
    fn drop(&mut self) {
        IN_BLOCKING.with(|flag| flag.set(false));
    }
}

/// Thread-local log capture for unit tests asserting semantic log records.
///
/// `install` returns the shared capture buffer together with the default
/// subscriber guard, which keeps the capture active for its scope. The
/// subscriber is thread-local: it observes records emitted on the installing
/// thread, which is where every failure arm this helper pins runs — after the
/// offloaded task has been awaited.
#[cfg(test)]
pub(crate) mod log_capture {
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct SharedWriter {
        buffer: Arc<Mutex<Vec<u8>>>,
    }

    impl std::io::Write for SharedWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.buffer
                .lock()
                .expect("log capture lock")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedWriter {
        type Writer = SharedWriter;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Installs a thread-local subscriber capturing `warn` and `error` records.
    pub(crate) fn install() -> (Arc<Mutex<Vec<u8>>>, tracing::subscriber::DefaultGuard) {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .with_writer(SharedWriter {
                buffer: Arc::clone(&buffer),
            })
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        (buffer, guard)
    }

    /// Returns the captured records as text.
    pub(crate) fn contents(buffer: &Arc<Mutex<Vec<u8>>>) -> String {
        let bytes = buffer.lock().expect("log capture lock").clone();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::{log_join_error, run_blocking, run_blocking_async};

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

    /// A failed offload must never degrade silently: the record emitted by
    /// `log_join_error` keeps the degradation observable in the journal.
    #[tokio::test]
    async fn log_join_error_records_the_failed_operation() {
        let (buffer, _guard) = crate::blocking::log_capture::install();

        let error = run_blocking_async(|| {
            panic!("intentional panic for test");
        })
        .await
        .expect_err("a panicking offloaded closure becomes a JoinError");
        log_join_error(&error, "test offload");

        let logged = crate::blocking::log_capture::contents(&buffer);
        assert!(
            logged.contains("offloaded blocking task failed"),
            "the degradation must be observable: {logged}"
        );
        assert!(logged.contains("test offload"));
        assert!(logged.contains("panicked"));
    }
}
