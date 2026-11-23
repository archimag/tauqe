//! Supervised writes that outlive the task awaiting their results.

use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tauqe_core::discussion::DiscussionTarget;
use tauqe_core::execution::{CancellationToken, ExecutionError};
use tauqe_core::plan::storage::{PlanApplicationOutcome, PlanMutationMode, PlanStorage};
use tauqe_core::plan::DiscussionPlanUpdate;
use tauqe_core::review::storage::{ReviewDecisionOutcome, ReviewStorage};
use tauqe_core::review::ReviewUpdateProposal;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

pub(super) enum DocumentWrite {
    Plans {
        updates: Vec<DiscussionPlanUpdate>,
        mode: PlanMutationMode,
    },
    Review {
        focus: DiscussionTarget,
        decision: ReviewUpdateProposal,
    },
}

pub(crate) enum DocumentOutcome {
    Plans(Vec<PlanApplicationOutcome>),
    Review {
        item_id: u32,
        session_id: Option<String>,
        result: Result<ReviewDecisionOutcome, String>,
    },
    OffloadFailed(String),
}

type Receipt = Arc<DocumentOutcome>;
type Request = (DocumentWrite, oneshot::Sender<Receipt>);

pub(super) struct DocumentExecutor {
    requests: mpsc::UnboundedSender<Request>,
    cancel: CancellationToken,
}

#[derive(Default)]
pub(crate) struct DocumentSettlement {
    pub(crate) receipts: Vec<Receipt>,
    pub(crate) join_error: Option<tokio::task::JoinError>,
}

pub(crate) struct DocumentWorker {
    task: JoinHandle<()>,
    // Keep receipts outside the worker future so a join failure cannot lose them.
    receipts: Arc<Mutex<Vec<Receipt>>>,
}

impl DocumentWorker {
    pub(crate) async fn finish(self) -> DocumentSettlement {
        let join_error = self.task.await.err();
        let receipts =
            std::mem::take(&mut *self.receipts.lock().unwrap_or_else(|e| e.into_inner()));
        DocumentSettlement {
            receipts,
            join_error,
        }
    }
}

impl DocumentExecutor {
    pub(super) fn start(
        root: PathBuf,
        cancel: CancellationToken,
        #[cfg(test)] gate: Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
    ) -> (Self, DocumentWorker) {
        #[cfg(test)]
        if let Some(gate) = gate {
            let mut gate = Some(gate);
            return Self::spawn(root, cancel, move || {
                let gate = gate.take();
                async move {
                    if let Some((admitted, release)) = gate {
                        admitted.notify_one();
                        release.notified().await;
                    }
                }
            });
        }
        Self::spawn(root, cancel, || std::future::ready(()))
    }

    pub(super) fn spawn<F: Future<Output = ()> + Send + 'static>(
        root: PathBuf,
        cancel: CancellationToken,
        mut before_write: impl FnMut() -> F + Send + 'static,
    ) -> (Self, DocumentWorker) {
        let (requests, mut inbox) = mpsc::unbounded_channel::<Request>();
        let receipts = Arc::new(Mutex::new(Vec::new()));
        let completed = receipts.clone();
        let task = tokio::spawn(async move {
            while let Some((write, reply)) = inbox.recv().await {
                before_write().await;
                let root = root.clone();
                let outcome = tauqe_core::blocking::run_blocking_async(move || match write {
                    DocumentWrite::Plans { updates, mode } => DocumentOutcome::Plans(
                        PlanStorage::apply_discussion_plan_updates_with_mode(&root, &updates, mode),
                    ),
                    DocumentWrite::Review { focus, decision } => {
                        let result = ReviewStorage::apply_decision(&root, &focus, &decision)
                            .map_err(|error| format!("{error:#}"));
                        DocumentOutcome::Review {
                            item_id: decision.item_id,
                            session_id: focus.review_session_id().map(str::to_owned),
                            result,
                        }
                    }
                })
                .await
                .unwrap_or_else(|error| DocumentOutcome::OffloadFailed(error.to_string()));
                let receipt = Arc::new(outcome);
                completed
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(receipt.clone());
                let _ = reply.send(receipt);
            }
        });
        (Self { requests, cancel }, DocumentWorker { task, receipts })
    }

    pub(super) async fn write(&self, write: DocumentWrite) -> Result<Receipt, ExecutionError> {
        let (reply, receipt) = oneshot::channel();
        let stopped =
            || ExecutionError::Other("The document worker stopped without a result".into());
        {
            // Order admission with the cancellation signal. Once sent, the
            // request belongs to the worker even if this future is dropped.
            let receiver = self.cancel.receiver();
            let cancelled = receiver.borrow();
            if *cancelled {
                return Err(ExecutionError::Cancelled);
            }
            self.requests.send((write, reply)).map_err(|_| stopped())?;
        }
        receipt.await.map_err(|_| stopped())
    }
}
