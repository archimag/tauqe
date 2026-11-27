//! Retained completion slots for work admitted before transaction shutdown.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use tokio::sync::Mutex as AsyncMutex;

use super::{start_blocking_operation, BlockingOperation, OffloadError};

type StopRequest = Box<dyn Fn() + Send + Sync + 'static>;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Completion {
    failed: bool,
    unconfirmed: bool,
}

trait Retained: Send + Sync {
    fn wait(&self) -> Pin<Box<dyn Future<Output = Completion> + Send + '_>>;
}

struct Slot<T> {
    state: AsyncMutex<SlotState<T>>,
    unconfirmed: std::sync::atomic::AtomicBool,
}

struct SlotState<T> {
    operation: Option<BlockingOperation<T>>,
    result: Option<Result<T, OffloadError>>,
    completion: Option<Completion>,
}

impl<T: Send + 'static> Slot<T> {
    async fn complete(&self) -> Completion {
        let mut state = self.state.lock().await;
        if state.completion.is_none() {
            let result = state
                .operation
                .as_mut()
                .expect("pending operation")
                .wait()
                .await;
            state.operation = None;
            state.completion = Some(Completion {
                failed: result.is_err(),
                unconfirmed: result.is_err(),
            });
            state.result = Some(result);
        }
        let mut completion = state.completion.expect("completed operation");
        completion.unconfirmed |= self.unconfirmed.load(std::sync::atomic::Ordering::Acquire);
        completion
    }
}

impl<T: Send + 'static> Retained for Slot<T> {
    fn wait(&self) -> Pin<Box<dyn Future<Output = Completion> + Send + '_>> {
        Box::pin(self.complete())
    }
}

struct Entry {
    slot: Arc<dyn Retained>,
    stop: Option<StopRequest>,
}

#[derive(Default)]
struct State {
    closed: bool,
    entries: Vec<Entry>,
}

/// Explicitly shared admission and completion owner. It does not schedule a
/// command queue: every registered closure starts at admission time.
#[derive(Clone, Default)]
pub struct StartedOperations {
    state: Arc<Mutex<State>>,
}

/// Facts known after all admitted closures have reported completion.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OperationSummary {
    pub total: usize,
    pub failed: usize,
    pub unconfirmed: usize,
}

/// The typed, once-consumable result of an admitted operation.
pub struct OperationHandle<T> {
    slot: Arc<Slot<T>>,
}

impl<T: Send + 'static> OperationHandle<T> {
    /// Wait cancellation does not discard the completion or rerun the work.
    pub async fn wait(&self) {
        self.slot.complete().await;
    }

    /// Take the result exactly once, including its original offload error.
    pub async fn take_result(&self) -> Option<Result<T, OffloadError>> {
        self.slot.complete().await;
        self.slot.state.lock().await.result.take()
    }

    /// Mark a process whose shutdown could not be confirmed separately from
    /// the offload's own failure to report.
    pub fn mark_unconfirmed(&self) {
        self.slot
            .unconfirmed
            .store(true, std::sync::atomic::Ordering::Release);
    }
}

impl StartedOperations {
    /// Start and retain work atomically with respect to closing admission.
    /// `stop` is a shutdown request, not a second invocation of the work.
    pub fn start<T, F>(
        &self,
        work: F,
        stop: Option<StopRequest>,
    ) -> Result<OperationHandle<T>, &'static str>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed {
            return Err("technical operation admission is closed");
        }
        let slot = Arc::new(Slot {
            state: AsyncMutex::new(SlotState {
                operation: Some(start_blocking_operation(work)),
                result: None,
                completion: None,
            }),
            unconfirmed: std::sync::atomic::AtomicBool::new(false),
        });
        state.entries.push(Entry {
            slot: slot.clone(),
            stop,
        });
        Ok(OperationHandle { slot })
    }

    /// Close admission before requesting shutdown of all active processes.
    /// Repeated calls never issue the same stop request twice.
    pub fn close(&self) {
        let stops = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.closed {
                return;
            }
            state.closed = true;
            state
                .entries
                .iter_mut()
                .filter_map(|entry| entry.stop.take())
                .collect::<Vec<_>>()
        };
        for stop in stops {
            stop();
        }
    }

    /// Confirm completion without consuming typed results. An interrupted
    /// wait leaves every slot in the owner for the next wait.
    pub async fn wait(&self) -> OperationSummary {
        let slots = {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state
                .entries
                .iter()
                .map(|entry| Arc::clone(&entry.slot))
                .collect::<Vec<_>>()
        };
        let mut summary = OperationSummary {
            total: slots.len(),
            ..OperationSummary::default()
        };
        for slot in slots {
            let completion = slot.wait().await;
            summary.failed += usize::from(completion.failed);
            summary.unconfirmed += usize::from(completion.unconfirmed);
        }
        summary
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    #[tokio::test]
    async fn lost_wait_retains_result_without_reexecution() {
        let owner = StartedOperations::default();
        let runs = Arc::new(AtomicUsize::new(0));
        let (release, held) = mpsc::channel();
        let count = runs.clone();
        let handle = owner
            .start(
                move || {
                    count.fetch_add(1, Ordering::SeqCst);
                    held.recv().unwrap();
                    42
                },
                None,
            )
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), owner.wait())
                .await
                .is_err()
        );
        release.send(()).unwrap();
        owner.close();
        assert_eq!(
            owner.wait().await,
            OperationSummary {
                total: 1,
                failed: 0,
                unconfirmed: 0
            }
        );
        assert_eq!(owner.wait().await, owner.wait().await);
        assert_eq!(handle.take_result().await.unwrap().unwrap(), 42);
        assert!(handle.take_result().await.is_none());
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn panic_and_unconfirmed_shutdown_remain_distinct() {
        let owner = StartedOperations::default();
        let panic = owner
            .start(|| -> () { panic!("retained panic") }, None)
            .unwrap();
        let uncertain = owner.start(|| 7, None).unwrap();
        uncertain.mark_unconfirmed();
        owner.close();
        assert_eq!(
            owner.wait().await,
            OperationSummary {
                total: 2,
                failed: 1,
                unconfirmed: 2
            }
        );
        assert!(panic
            .take_result()
            .await
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("panicked"));
        assert_eq!(uncertain.take_result().await.unwrap().unwrap(), 7);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_close_refuses_later_work_and_stops_once() {
        let owner = StartedOperations::default();
        let stopped = Arc::new(AtomicUsize::new(0));
        let counter = stopped.clone();
        let handle = owner
            .start(
                || 1,
                Some(Box::new(move || {
                    counter.fetch_add(1, Ordering::SeqCst);
                })),
            )
            .unwrap();
        let other = owner.clone();
        let closer = std::thread::spawn(move || other.close());
        owner.close();
        closer.join().unwrap();
        assert!(owner.start(|| 2, None).is_err());
        assert_eq!(stopped.load(Ordering::SeqCst), 1);
        assert_eq!(owner.wait().await.total, 1);
        assert_eq!(handle.take_result().await.unwrap().unwrap(), 1);
    }
}
