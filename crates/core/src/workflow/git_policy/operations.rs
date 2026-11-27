//! Git operation completions retained independently of their waiting futures.

use crate::blocking::{OperationHandle, StartedOperations};
use anyhow::{bail, Context, Result};
use std::sync::Arc;
use tokio::sync::OnceCell;

#[derive(Default, Clone)]
pub(super) enum GitOperation<T> {
    #[default]
    Idle,
    Running {
        handle: Arc<OperationHandle<Result<T>>>,
        completion: Arc<OnceCell<Result<T, String>>>,
    },
}

impl<T: Clone + Send + 'static> GitOperation<T> {
    pub(super) fn start(
        &mut self,
        owner: &StartedOperations,
        work: impl FnOnce() -> Result<T> + Send + 'static,
    ) -> Result<()> {
        if matches!(self, Self::Idle) {
            *self = Self::Running {
                handle: Arc::new(owner.start(work, None).map_err(anyhow::Error::msg)?),
                completion: Arc::new(OnceCell::new()),
            };
        }
        Ok(())
    }

    pub(super) async fn result(&self) -> Result<T> {
        match self {
            Self::Running { handle, completion } => {
                let result = completion
                    .get_or_init(|| async {
                        match handle
                            .take_result()
                            .await
                            .context("Git operation completion was already consumed")
                        {
                            Ok(Ok(result)) => result.map_err(|error| format!("{error:#}")),
                            Ok(Err(error)) => Err(error.to_string()),
                            Err(error) => Err(error.to_string()),
                        }
                    })
                    .await;
                match result {
                    Ok(value) => Ok(value.clone()),
                    Err(error) => bail!("{error}"),
                }
            }
            Self::Idle => bail!("Git operation was not started"),
        }
    }

    pub(super) fn started(&self) -> bool {
        !matches!(self, Self::Idle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn lost_wait_keeps_one_completion_and_one_invocation() {
        let owner = StartedOperations::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let (release, held) = std::sync::mpsc::channel();
        let count = calls.clone();
        let mut operation = GitOperation::Idle;
        operation
            .start(&owner, move || {
                count.fetch_add(1, Ordering::SeqCst);
                held.recv().unwrap();
                Ok(42)
            })
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), operation.result())
                .await
                .is_err()
        );
        operation.start(&owner, || Ok(0)).unwrap();
        release.send(()).unwrap();
        assert_eq!(operation.result().await.unwrap(), 42);
        assert_eq!(operation.clone().result().await.unwrap(), 42);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
