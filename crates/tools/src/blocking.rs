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
//! failures mean opposite things.
//!
//! A closure that never ran left the work outstanding, so the caller may
//! complete it inline: the fallback performs the work once, exactly as a
//! synchronous caller would. A closure that ran and panicked already performed
//! the work and crashed, so the failure is deterministic: retrying it would
//! crash the calling thread too and spend the work a second time. Its panic
//! payload therefore resumes in the caller, so the failure surfaces once,
//! carrying the context it was raised with.
//!
//! Which of the two happened cannot be inferred from the join error of the
//! blocking task, because that error answers a different question. A blocking
//! closure cannot be cancelled: aborting its handle does not stop it, and a
//! runtime that shuts down while the closure already runs only stops waiting
//! for it. The join then reports a cancellation while the work continues on the
//! blocking pool, so a caller that read that report as "never ran" would start
//! a second copy of work that is still in flight.
//!
//! [`run_blocking_async`] therefore proves the answer instead of guessing it.
//! The closure owns the sending half of a channel and reports through it, so a
//! channel closed without a report can only mean the runtime dropped the closure
//! before it started, while a report always describes work that actually
//! happened. [`resolve_offload_failure`] draws that distinction for every
//! caller.

use std::any::Any;
use std::cell::Cell;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Mutex;

use tokio::runtime::{Handle, RuntimeFlavor};
use tokio::sync::oneshot;

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

/// What an offloaded closure reported before it ended.
enum OffloadOutcome<T> {
    /// The closure completed and produced its value.
    Completed(T),
    /// The closure ran and panicked; the payload is the panic it raised.
    Panicked(PanicPayload),
}

/// Runs `f` on the runtime's blocking thread pool and awaits its completion.
///
/// Unlike the synchronous [`run_blocking`], this async variant suspends the
/// current task while the closure runs on a dedicated blocking thread instead
/// of occupying the caller's thread. On a current-thread runtime that is the
/// difference between the whole executor stalling on a slow `git` invocation
/// and the executor staying free to poll every other task until the call
/// returns. It takes that path on either runtime flavor, so a multi-threaded
/// caller hands the work to the blocking pool too; only a call with no runtime
/// present delegates to [`run_blocking`], which runs the closure directly.
///
/// Returns `Ok(T)` once the offloaded closure completed, or an [`OffloadError`]
/// reporting why it produced no result: either the runtime dropped the closure
/// before it started, or the closure ran and panicked. Both cases are reported
/// by the closure itself, so the classification is proof rather than an
/// inference from the join error of the blocking task (see the module
/// documentation).
pub async fn run_blocking_async<T, F>(f: F) -> Result<T, OffloadError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    match Handle::try_current() {
        Ok(handle) => {
            let (sender, receiver) = oneshot::channel::<OffloadOutcome<T>>();
            // The closure owns the sending half, so the channel doubles as the
            // proof of whether the work started: a closure the runtime drops
            // before it runs closes the channel without a report, while every
            // closure that ran reports before it ends. The join handle carries
            // nothing the channel does not — a blocking task cannot be
            // cancelled through it — so it is detached here.
            //
            // The closure marks its thread as blocking before running `f`, so a
            // nested `run_blocking` call inside it runs inline instead of
            // attempting `block_in_place` on a blocking-pool thread (which
            // would panic). The guard is cleared when the scope ends, including
            // on panic, and the panic itself is caught so that a closure which
            // started always delivers its report.
            let _task = handle.spawn_blocking(move || {
                IN_BLOCKING.with(|flag| flag.set(true));
                let _reset = ResetBlockingFlag;
                let report = match catch_unwind(AssertUnwindSafe(f)) {
                    Ok(value) => OffloadOutcome::Completed(value),
                    Err(payload) => OffloadOutcome::Panicked(PanicPayload::new(payload)),
                };
                // A receiver that is already gone abandoned the result, which
                // is not a failure of the offloaded work.
                let _ = sender.send(report);
            });
            match receiver.await {
                Ok(OffloadOutcome::Completed(value)) => Ok(value),
                Ok(OffloadOutcome::Panicked(payload)) => Err(OffloadError::panicked(payload)),
                // Nothing was reported, so the runtime dropped the closure
                // before it started and the work is provably still outstanding.
                Err(_) => Err(OffloadError::never_ran()),
            }
        }
        Err(_) => Ok(run_blocking(f)),
    }
}

/// Why an offloaded blocking task produced no result.
///
/// The classification stays private to the offload: a closure reports its own
/// outcome, and a missing report is evidence the runtime never started it, so
/// nothing outside this module can fabricate either verdict. Callers read the
/// verdict through [`resolve_offload_failure`], which acts on it, and
/// [`log_join_error`], which records it.
#[derive(Debug)]
pub struct OffloadError {
    failure: OffloadFailure,
}

/// The classified failure of one offload.
#[derive(Debug)]
enum OffloadFailure {
    /// The runtime dropped the closure before it started, so the work never ran.
    NeverRan,
    /// The closure ran and panicked; the payload is the panic it raised.
    Panicked(PanicPayload),
}

/// The payload a panicked offload resumes with.
///
/// The [`Mutex`] keeps the error `Send + Sync`, which the join error it replaces
/// also was: `Box<dyn Any + Send>` alone is not `Sync`. The payload is taken out
/// exactly once, by the resume that consumes the error, so the lock is never
/// contended and never poisoned.
struct PanicPayload(Mutex<Box<dyn Any + Send + 'static>>);

impl std::fmt::Debug for PanicPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<panic payload>")
    }
}

impl PanicPayload {
    /// Wraps the payload of a caught panic.
    fn new(payload: Box<dyn Any + Send + 'static>) -> Self {
        Self(Mutex::new(payload))
    }

    /// Resumes the panic on the calling thread with its original payload.
    ///
    /// Nothing locks the payload before this consumes it, so the lock cannot be
    /// poisoned; the recovery keeps the resume panic-free all the same.
    fn resume(self) -> ! {
        let payload = match self.0.into_inner() {
            Ok(payload) => payload,
            Err(poisoned) => poisoned.into_inner(),
        };
        std::panic::resume_unwind(payload)
    }
}

impl OffloadError {
    /// Builds the error of a closure the runtime dropped before it started.
    fn never_ran() -> Self {
        Self {
            failure: OffloadFailure::NeverRan,
        }
    }

    /// Builds the error of a closure that ran and panicked.
    fn panicked(payload: PanicPayload) -> Self {
        Self {
            failure: OffloadFailure::Panicked(payload),
        }
    }

    /// Reports whether the offloaded work ran and panicked.
    fn is_panic(&self) -> bool {
        matches!(self.failure, OffloadFailure::Panicked(_))
    }
}

impl std::fmt::Display for OffloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.failure {
            OffloadFailure::NeverRan => f.write_str("the offloaded blocking task never ran"),
            OffloadFailure::Panicked(_) => f.write_str("the offloaded blocking task panicked"),
        }
    }
}

impl std::error::Error for OffloadError {}

/// Records a failed [`run_blocking_async`] result in the semantic journal.
///
/// A failed offload degrades into a fallback value at several call sites;
/// without this record the degradation would be indistinguishable from a
/// legitimately absent result. Every call site that downgrades a failed offload
/// to a fallback must route the [`OffloadError`] through here first, so the
/// failure point leaves an observable trace. [`resolve_offload_failure`] pairs
/// that record with the degradation itself.
pub fn log_join_error(error: &OffloadError, operation: &str) {
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
/// The two failures are not interchangeable. A closure the runtime dropped
/// before it started never ran the work, so the work is still outstanding and
/// `inline` performs it on the calling thread — exactly the synchronous path the
/// caller would have taken without the offload. A closure that ran and panicked
/// performed the work and crashed: the panic is deterministic, so retrying it
/// inline would crash the calling thread too, spend the work a second time, and
/// replace the original payload with a fresh one raised where nothing remembers
/// the real failure. The panic is therefore recorded and its payload resumed
/// here, so the failure surfaces once, carrying the context it was raised with.
pub fn resolve_offload_failure<T>(
    error: OffloadError,
    operation: &str,
    inline: impl FnOnce() -> T,
) -> T {
    log_join_error(&error, operation);
    match error.failure {
        OffloadFailure::Panicked(payload) => payload.resume(),
        OffloadFailure::NeverRan => inline(),
    }
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::time::Duration;

    use super::{resolve_offload_failure, run_blocking, run_blocking_async, OffloadError};

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

    /// A closure that ran and panicked keeps its classification: the offload
    /// reports it as panicked, so no caller can mistake it for work that never
    /// started.
    #[tokio::test]
    async fn async_reports_a_panicking_closure_as_panicked() {
        let error = run_blocking_async(|| {
            panic!("intentional panic for test");
        })
        .await
        .expect_err("a panicking closure produces no result");
        assert!(error.is_panic());
        assert_eq!(error.to_string(), "the offloaded blocking task panicked");
    }

    /// Only a closure the runtime dropped before it started is completed
    /// inline, and that verdict is produced by the offload itself: it can never
    /// be inferred from a cancellation, because a blocking closure that already
    /// runs cannot be cancelled.
    #[test]
    fn a_closure_that_never_ran_is_completed_inline() {
        let mut inline_ran = false;
        let value = resolve_offload_failure(OffloadError::never_ran(), "test", || {
            inline_ran = true;
            29
        });
        assert_eq!(value, 29);
        assert!(inline_ran, "work that never ran is completed inline");
    }

    /// A panicked offload already ran the work, so it is never retried inline:
    /// its payload resumes and the work is not spent a second time.
    #[tokio::test]
    async fn panicked_offload_resumes_the_payload_without_running_inline() {
        let error = run_blocking_async(|| panic!("offloaded work crashed"))
            .await
            .expect_err("a panicking closure produces no result");
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

    /// A blocking closure cannot be cancelled: abandoning the task that awaits
    /// it leaves the closure running until it finishes, and the work is
    /// performed exactly once instead of being started again by a fallback.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_abandoned_await_leaves_the_started_closure_running_exactly_once() {
        let runs = Arc::new(AtomicUsize::new(0));
        let (started_tx, started_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let (finished_tx, finished_rx) = mpsc::channel::<()>();
        let counter = Arc::clone(&runs);
        let waiting = tokio::spawn(async move {
            run_blocking_async(move || {
                counter.fetch_add(1, Ordering::SeqCst);
                let _ = started_tx.send(());
                // Hold the blocking thread until the test releases it, so the
                // awaiting task is provably abandoned mid-flight.
                let _ = release_rx.recv();
                let _ = finished_tx.send(());
                37
            })
            .await
        });

        started_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the closure reports that it started");
        waiting.abort();
        let _ = waiting.await;
        let _ = release_tx.send(());
        finished_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the abandoned closure still finishes");
        assert_eq!(
            runs.load(Ordering::SeqCst),
            1,
            "the closure runs exactly once"
        );
    }
}
