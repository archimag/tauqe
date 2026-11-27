//! Monotone stop requests for one operation.

use std::future::pending;

use tokio::sync::watch;

/// The strongest stop request observed for an operation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd)]
pub enum StopState {
    #[default]
    Running,
    Soft,
    Hard,
}

/// The sole writer of an operation's stop state.
#[derive(Clone, Debug)]
pub struct StopSource {
    sender: watch::Sender<StopState>,
}

impl Default for StopSource {
    fn default() -> Self {
        Self::new()
    }
}

impl StopSource {
    pub fn new() -> Self {
        let (sender, _) = watch::channel(StopState::Running);
        Self { sender }
    }

    pub fn token(&self) -> StopToken {
        StopToken {
            receiver: self.sender.subscribe(),
        }
    }

    pub fn request_soft(&self) {
        self.request(StopState::Soft);
    }

    pub fn request_hard(&self) {
        self.request(StopState::Hard);
    }

    fn request(&self, requested: StopState) {
        // The mutation lock serializes competing writers, even if no observer
        // is currently subscribed. No request can downgrade an earlier one.
        self.sender.send_modify(|state| {
            if *state < requested {
                *state = requested;
            }
        });
    }
}

/// Full, read-only stop state for scheduling boundaries.
#[derive(Clone, Debug)]
pub struct StopToken {
    receiver: watch::Receiver<StopState>,
}

impl StopToken {
    pub fn state(&self) -> StopState {
        *self.receiver.borrow()
    }

    pub fn hard_cancellation(&self) -> HardCancellation {
        HardCancellation {
            receiver: Some(self.receiver.clone()),
        }
    }
}

/// Read-only view that reacts only to Hard, not to Soft.
#[derive(Clone, Debug)]
pub struct HardCancellation {
    receiver: Option<watch::Receiver<StopState>>,
}

impl HardCancellation {
    /// A view for work that has no operation stop signal.
    pub fn never() -> Self {
        Self { receiver: None }
    }

    pub fn is_cancelled(&self) -> bool {
        self.receiver
            .as_ref()
            .is_some_and(|receiver| *receiver.borrow() == StopState::Hard)
    }

    /// Wait for Hard, including when it was requested before the first poll.
    /// Closing the source without Hard leaves this wait pending.
    pub async fn cancelled(&self) {
        match &self.receiver {
            Some(receiver) => {
                let mut receiver = receiver.clone();
                loop {
                    if *receiver.borrow_and_update() == StopState::Hard {
                        return;
                    }
                    if receiver.changed().await.is_err() {
                        pending().await
                    }
                }
            }
            None => pending().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Arc, time::Duration};
    use tokio::time::timeout;

    async fn stays_pending(cancellation: &HardCancellation) {
        assert!(timeout(Duration::from_millis(20), cancellation.cancelled())
            .await
            .is_err());
    }

    #[test]
    fn transitions_are_monotone_and_repeated_requests_are_idempotent() {
        let source = StopSource::new();
        let token = source.token();
        assert_eq!(token.state(), StopState::Running);
        source.request_soft();
        source.request_soft();
        assert_eq!(token.state(), StopState::Soft);
        source.request_hard();
        source.request_hard();
        source.request_soft();
        assert_eq!(token.state(), StopState::Hard);

        let direct = StopSource::new();
        direct.request_hard();
        assert_eq!(direct.token().state(), StopState::Hard);
        assert_eq!(StopSource::new().token().state(), StopState::Running);
    }

    #[test]
    fn concurrent_writers_cannot_downgrade_hard() {
        let source = Arc::new(StopSource::new());
        let token = source.token();
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let threads: Vec<_> = [false, true]
            .into_iter()
            .map(|hard| {
                let source = Arc::clone(&source);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    for _ in 0..1000 {
                        if hard {
                            source.request_hard();
                        } else {
                            source.request_soft();
                        }
                    }
                })
            })
            .collect();
        barrier.wait();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(token.state(), StopState::Hard);
    }

    #[tokio::test]
    async fn late_observers_and_hard_before_first_poll() {
        let source = StopSource::new();
        source.request_soft();
        assert_eq!(source.token().state(), StopState::Soft);
        let cancellation = source.token().hard_cancellation();
        let wait = cancellation.cancelled();
        source.request_hard();
        timeout(Duration::from_secs(1), wait).await.unwrap();
        assert!(cancellation.is_cancelled());
        assert!(source.token().hard_cancellation().is_cancelled());
        cancellation.cancelled().await;
    }

    #[tokio::test]
    async fn soft_does_not_complete_hard_wait_but_hard_wakes_all_observers() {
        let source = StopSource::new();
        let first = source.token().hard_cancellation();
        let second = first.clone();
        let first_wait = first.cancelled();
        tokio::pin!(first_wait);
        assert!(timeout(Duration::from_millis(20), &mut first_wait)
            .await
            .is_err());
        source.request_soft();
        assert!(!first.is_cancelled());
        assert!(timeout(Duration::from_millis(20), &mut first_wait)
            .await
            .is_err());
        stays_pending(&second).await;
        source.request_hard();
        timeout(Duration::from_secs(1), &mut first_wait)
            .await
            .unwrap();
        timeout(Duration::from_secs(1), second.cancelled())
            .await
            .unwrap();
        first.cancelled().await;
    }

    #[tokio::test]
    async fn closed_sources_preserve_state_without_spinning() {
        for state in [StopState::Running, StopState::Soft, StopState::Hard] {
            let source = StopSource::new();
            let token = source.token();
            match state {
                StopState::Running => {}
                StopState::Soft => source.request_soft(),
                StopState::Hard => source.request_hard(),
            }
            let cancellation = token.hard_cancellation();
            drop(source);
            assert_eq!(token.state(), state);
            if state == StopState::Hard {
                cancellation.cancelled().await;
            } else {
                stays_pending(&cancellation).await;
                stays_pending(&cancellation).await;
            }
        }
    }

    #[tokio::test]
    async fn never_view_stays_pending() {
        let never = HardCancellation::never();
        assert!(!never.is_cancelled());
        stays_pending(&never).await;
    }
}
