//! Runtime-aware execution of synchronous, blocking work.
//!
//! The helpers live in `tauqe-tools` (`tauqe_tools::blocking`), beside the
//! substrate that drives blocking filesystem, Git, and toolchain work from
//! inside async tasks. The re-exports below keep
//! `crate::blocking::{run_blocking, run_blocking_async, log_join_error}`
//! resolvable for the cognitive layers while the transition lasts.
//!
//! The test-only log capture stays here: it observes the records emitted on the
//! installing thread, and every failure arm it pins runs inside this crate.

pub use tauqe_tools::blocking::{log_join_error, run_blocking, run_blocking_async};

/// Per-thread log capture for unit tests asserting semantic log records.
///
/// The capture subscriber is installed once for the whole test binary, as the
/// global default, and every record it receives is routed to the buffer of the
/// thread that emitted it. One global installation is what makes the capture
/// reliable: a callsite caches the answer it computed the first time it was
/// reached, so a record emitted while no subscriber was installed would stay
/// invisible for the rest of the process, while a subscriber that accepts
/// `warn` and `error` records rebuilds that cache before any test asserts on
/// one.
///
/// Captures stay thread-scoped: `install` publishes a buffer for the calling
/// thread only, so tests running in parallel never observe each other's
/// records.
#[cfg(test)]
pub(crate) mod log_capture {
    use std::cell::RefCell;
    use std::sync::{Arc, Mutex, OnceLock};

    /// Routes every captured record into the buffer of the emitting thread.
    struct SharedWriter;

    thread_local! {
        /// Buffer of the capture the current thread installed, if any.
        static CAPTURE: RefCell<Option<Arc<Mutex<Vec<u8>>>>> = const { RefCell::new(None) };
    }

    impl std::io::Write for SharedWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            CAPTURE.with(|capture| {
                if let Some(buffer) = capture.borrow().as_ref() {
                    let mut buffer = match buffer.lock() {
                        Ok(captured) => captured,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    buffer.extend_from_slice(buf);
                }
            });
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedWriter {
        type Writer = SharedWriter;

        fn make_writer(&'a self) -> Self::Writer {
            SharedWriter
        }
    }

    /// Detaches the capture of the installing thread when it goes out of scope.
    pub(crate) struct CaptureGuard;

    impl Drop for CaptureGuard {
        fn drop(&mut self) {
            CAPTURE.with(|capture| {
                *capture.borrow_mut() = None;
            });
        }
    }

    /// Installs the capture subscriber and starts a capture on the calling thread.
    pub(crate) fn install() -> (Arc<Mutex<Vec<u8>>>, CaptureGuard) {
        static SUBSCRIBER: OnceLock<()> = OnceLock::new();
        SUBSCRIBER.get_or_init(|| {
            let subscriber = tracing_subscriber::fmt()
                .with_max_level(tracing::Level::WARN)
                .with_ansi(false)
                .with_writer(SharedWriter)
                .finish();
            if let Err(error) = tracing::subscriber::set_global_default(subscriber) {
                // Another sink already owns the process, so no record can reach
                // the capture. The assertion that needed the record reports the
                // consequence; the degradation itself is named here.
                tracing::warn!(%error, "log capture cannot observe the journal");
            }
        });

        let buffer = Arc::new(Mutex::new(Vec::new()));
        CAPTURE.with(|capture| {
            *capture.borrow_mut() = Some(Arc::clone(&buffer));
        });
        (buffer, CaptureGuard)
    }

    /// Returns the captured records as text.
    pub(crate) fn contents(buffer: &Arc<Mutex<Vec<u8>>>) -> String {
        let bytes = match buffer.lock() {
            Ok(captured) => captured.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::{log_join_error, run_blocking_async};

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
