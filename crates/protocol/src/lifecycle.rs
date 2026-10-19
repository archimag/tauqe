//! Process lifecycle contract shared by the daemon and the clients that spawn
//! it.
//!
//! A client opens the daemon's stdin, and closing that pipe is what asks the
//! daemon to stop. Stopping is not instantaneous: the daemon cancels the
//! active operation and rolls its Git transaction back before it exits. Both
//! sides therefore need the same timeline, and a second copy of it would let
//! the client kill the process inside the cleanup the drain exists to protect.

use std::time::Duration;

/// Seconds the daemon gives in-flight work to unwind before it aborts it.
const DRAIN_BUDGET_SECS: u64 = 10;

/// Seconds of headroom the client grants on top of the daemon's own deadline.
const CLIENT_GRACE_SECS: u64 = 2;

/// Time the daemon gives in-flight work to unwind after it begins a graceful
/// shutdown.
///
/// The drain re-signals cancellation on every pass and waits for the single
/// operation slot to be released: a running turn holds that slot for its whole
/// duration and rolls its Git transaction back before releasing it. Anything
/// still running when the budget expires is force-aborted, and startup crash
/// recovery covers whatever residue remains.
pub const SHUTDOWN_DRAIN_BUDGET: Duration = Duration::from_secs(DRAIN_BUDGET_SECS);

/// Time a client waits for the daemon to exit before it kills the process.
///
/// The drain budget is the daemon's own deadline, not the moment it is gone:
/// after the last check it still aborts the remaining tasks, joins them, and
/// returns from `main`. Killing it inside that window would cut exactly the
/// cleanup the drain protects, so the client waits for the budget plus a small
/// grace period. A daemon that already exited resolves the wait immediately,
/// so a normal quit never pays this budget.
pub const CLIENT_SHUTDOWN_WAIT: Duration =
    Duration::from_secs(DRAIN_BUDGET_SECS + CLIENT_GRACE_SECS);

#[cfg(test)]
mod tests {
    use super::*;

    /// The client must outlast the daemon's own deadline, otherwise every slow
    /// drain turns into a kill in the middle of a Git transaction.
    #[test]
    fn client_wait_outlasts_the_drain_budget() {
        assert!(
            CLIENT_SHUTDOWN_WAIT > SHUTDOWN_DRAIN_BUDGET,
            "the client would kill the daemon before its drain completes"
        );
    }

    /// The grace period is headroom for the final cleanup, not a second drain,
    /// so it must stay small enough that a wedged daemon cannot delay a quit.
    #[test]
    fn client_grace_stays_small() {
        // The invariant is pure arithmetic over two private constants, so it is
        // checked while the crate compiles rather than once per test run: a
        // nonsensical grace period can never be built in the first place.
        const {
            assert!(CLIENT_GRACE_SECS > 0);
            assert!(
                CLIENT_GRACE_SECS <= DRAIN_BUDGET_SECS,
                "the grace period must not dominate the drain budget"
            );
        }
    }

    /// Both budgets are finite and non-zero, so neither side waits forever.
    #[test]
    fn shutdown_budgets_are_positive() {
        assert!(SHUTDOWN_DRAIN_BUDGET > Duration::ZERO);
        assert!(CLIENT_SHUTDOWN_WAIT > Duration::ZERO);
    }
}
