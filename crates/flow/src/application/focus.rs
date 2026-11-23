//! Discussion focus shared by the scenarios of one session.

use super::events::{ApplicationEvent, ApplicationObserver};
use tauqe_core::discussion::{DiscussionFocus, DiscussionTarget};
use tokio::sync::Mutex;

#[derive(Default)]
pub(crate) struct FocusSession(Mutex<DiscussionFocus>);

/// Result and change notification of one atomic focus mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocusUpdate {
    pub(crate) focus: DiscussionFocus,
    pub(crate) changed: bool,
}

impl FocusSession {
    pub(crate) async fn current(&self) -> DiscussionFocus {
        self.0.lock().await.clone()
    }

    pub(crate) async fn set(&self, focus: DiscussionFocus) -> FocusUpdate {
        let mut current = self.0.lock().await;
        let changed = *current != focus;
        *current = focus.clone();
        FocusUpdate { focus, changed }
    }

    pub(crate) async fn clear(&self) -> FocusUpdate {
        self.set(DiscussionFocus::None).await
    }

    /// Compare and clear under one lock so deletion cannot clear another target.
    pub(crate) async fn release(&self, anchor: &DiscussionTarget) -> FocusUpdate {
        let mut current = self.0.lock().await;
        let changed =
            matches!(&*current, DiscussionFocus::Focused { target, .. } if target == anchor);
        if changed {
            *current = DiscussionFocus::None;
        }
        FocusUpdate {
            focus: current.clone(),
            changed,
        }
    }
}

impl FocusUpdate {
    /// Publishes this transition at the point selected by its owning scenario.
    pub(crate) fn publish(
        self,
        operation_id: &str,
        observer: &dyn ApplicationObserver,
    ) -> DiscussionFocus {
        if self.changed {
            observer.publish(operation_id, ApplicationEvent::Focus(self.focus.clone()));
        }
        self.focus
    }
}
