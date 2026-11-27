//! Session-wide observable context, independent of the computational lock.

use super::operation::{OperationGuard, OperationRegistry, PublicationHold};
use crate::api::{ApplicationEvent, ApplicationObserver};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tauqe_core::context::{ContextListener, ContextState};

#[derive(Clone)]
pub(super) struct ContextPublication {
    projection: Arc<Mutex<Projection>>,
    delivery: Arc<Mutex<()>>,
    #[allow(dead_code)] // Used when operation-bound sources are connected by scenarios.
    registry: Arc<OperationRegistry>,
}

struct Delivery {
    observer: Arc<dyn ApplicationObserver>,
    stream_id: String,
    state: ContextState,
}

struct Source {
    #[allow(dead_code)]
    operation: u64,
    #[allow(dead_code)]
    generation: u64,
    stream_id: String,
    observer: Arc<dyn ApplicationObserver>,
    _hold: PublicationHold,
}

struct Projection {
    shared: ContextState,
    visible: ContextState,
    showing_shared: bool,
    #[allow(dead_code)]
    generation: u64,
    source: Option<Source>,
    pending: VecDeque<Delivery>,
    draining: bool,
}

impl ContextPublication {
    /// Bind manual mutations to the shared manager's Core listener.
    pub(super) fn activate_shared(
        &self,
        operation: &OperationGuard,
        observer: Arc<dyn ApplicationObserver>,
    ) -> bool {
        let Some(hold) = operation.retain_publication(&self.registry) else {
            return false;
        };
        let mut projection = self
            .projection
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if projection.source.is_some() {
            return false;
        }
        projection.generation += 1;
        let generation = projection.generation;
        projection.source = Some(Source {
            operation: operation.id(),
            generation,
            stream_id: String::new(),
            observer,
            _hold: hold,
        });
        projection.showing_shared = true;
        true
    }

    pub(super) fn new(mut initial: ContextState, registry: Arc<OperationRegistry>) -> Self {
        initial.revision = 1;
        Self {
            projection: Arc::new(Mutex::new(Projection {
                shared: initial.clone(),
                visible: initial,
                showing_shared: true,
                generation: 0,
                source: None,
                pending: VecDeque::new(),
                draining: false,
            })),
            delivery: Arc::new(Mutex::new(())),
            registry,
        }
    }

    pub(super) fn listener(&self) -> ContextListener {
        let projection = Arc::downgrade(&self.projection);
        let delivery = Arc::downgrade(&self.delivery);
        ContextListener::new(move |state| {
            if let (Some(projection), Some(delivery)) = (projection.upgrade(), delivery.upgrade()) {
                Self::update_shared(&projection, &delivery, state);
            }
        })
    }

    pub(super) fn state(&self) -> ContextState {
        self.projection
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .visible
            .clone()
    }

    fn update_shared(
        ledger: &Arc<Mutex<Projection>>,
        delivery: &Arc<Mutex<()>>,
        mut state: ContextState,
    ) {
        let mut projection = ledger.lock().unwrap_or_else(|error| error.into_inner());
        state.revision = projection.shared.revision;
        projection.shared = state.clone();
        if projection.showing_shared {
            if replace_visible(&mut projection.visible, state) {
                projection.enqueue_visible();
            }
            projection.shared.revision = projection.visible.revision;
        }
        drop(projection);
        Self::drain(ledger, delivery);
    }

    /// Rebinding the same operation revokes its old callback before accepting the new snapshot.
    pub(super) fn activate(
        &self,
        operation: &OperationGuard,
        stream_id: String,
        observer: Arc<dyn ApplicationObserver>,
        state: ContextState,
    ) -> Option<ContextListener> {
        let hold = operation.retain_publication(&self.registry)?;
        let mut projection = self
            .projection
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if projection
            .source
            .as_ref()
            .is_some_and(|source| source.operation != operation.id())
        {
            return None;
        }
        projection.generation += 1;
        let generation = projection.generation;
        let operation_id = operation.id();
        projection.source = Some(Source {
            operation: operation_id,
            generation,
            stream_id,
            observer,
            _hold: hold,
        });
        projection.showing_shared = false;
        if replace_visible(&mut projection.visible, state) {
            projection.enqueue_visible();
        }
        drop(projection);
        Self::drain(&self.projection, &self.delivery);
        let ledger = Arc::downgrade(&self.projection);
        let delivery = Arc::downgrade(&self.delivery);
        Some(ContextListener::new(move |state| {
            if let (Some(ledger), Some(delivery)) = (ledger.upgrade(), delivery.upgrade()) {
                let mut projection = ledger.lock().unwrap_or_else(|error| error.into_inner());
                if projection.source.as_ref().is_some_and(|source| {
                    source.operation == operation_id && source.generation == generation
                }) && replace_visible(&mut projection.visible, state)
                {
                    projection.enqueue_visible();
                }
                drop(projection);
                Self::drain(&ledger, &delivery);
            }
        }))
    }

    /// Revoke callbacks from the computation before recovery mutates its copy.
    pub(super) fn rebind_source(
        &self,
        operation_id: u64,
        state: ContextState,
    ) -> Option<ContextListener> {
        let mut projection = self
            .projection
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !projection
            .source
            .as_ref()
            .is_some_and(|source| source.operation == operation_id)
        {
            return None;
        }
        projection.generation += 1;
        let generation = projection.generation;
        if let Some(source) = projection.source.as_mut() {
            source.generation = generation;
        }
        if replace_visible(&mut projection.visible, state) {
            projection.enqueue_visible();
        }
        drop(projection);
        Self::drain(&self.projection, &self.delivery);
        let ledger = Arc::downgrade(&self.projection);
        let delivery = Arc::downgrade(&self.delivery);
        Some(ContextListener::new(move |state| {
            if let (Some(ledger), Some(delivery)) = (ledger.upgrade(), delivery.upgrade()) {
                let mut projection = ledger.lock().unwrap_or_else(|error| error.into_inner());
                if projection.source.as_ref().is_some_and(|source| {
                    source.operation == operation_id && source.generation == generation
                }) && replace_visible(&mut projection.visible, state)
                {
                    projection.enqueue_visible();
                }
                drop(projection);
                Self::drain(&ledger, &delivery);
            }
        }))
    }

    /// Drain all accepted snapshots before releasing the source's registration hold.
    pub(super) fn close(&self, operation: &OperationGuard) -> bool {
        if !operation.belongs_to(&self.registry) {
            return false;
        }
        self.close_source(operation.id())
    }

    /// The supervisor can close a source after the caller waiting on it exits.
    pub(super) fn close_source(&self, operation_id: u64) -> bool {
        // Wait for any current publisher first. No callback can publish past
        // this gate, and source revocation and its final delivery are atomic.
        let _gate = self
            .delivery
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut projection = self
            .projection
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !projection
            .source
            .as_ref()
            .is_some_and(|source| source.operation == operation_id)
        {
            return false;
        }
        projection.generation += 1;
        projection.showing_shared = true;
        let shared = projection.shared.clone();
        if replace_visible(&mut projection.visible, shared) {
            projection.enqueue_visible();
        }
        drop(projection);
        Self::deliver(&self.projection);
        self.projection
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .source = None;
        true
    }

    fn drain(ledger: &Arc<Mutex<Projection>>, delivery: &Arc<Mutex<()>>) {
        // Only one caller drains; other callbacks enqueue and return. Marking
        // the queue idle under the same lock as enqueue closes the empty-queue
        // race without blocking a reentrant observer callback.
        {
            let mut projection = ledger.lock().unwrap_or_else(|error| error.into_inner());
            if projection.draining || projection.pending.is_empty() {
                return;
            }
            projection.draining = true;
        }
        let _gate = delivery.lock().unwrap_or_else(|error| error.into_inner());
        Self::deliver(ledger);
    }

    fn deliver(ledger: &Arc<Mutex<Projection>>) {
        loop {
            let next = {
                let mut projection = ledger.lock().unwrap_or_else(|error| error.into_inner());
                match projection.pending.pop_front() {
                    Some(next) => next,
                    None => {
                        projection.draining = false;
                        break;
                    }
                }
            };
            next.observer
                .publish(&next.stream_id, ApplicationEvent::Context(next.state));
        }
    }

    /// Retain shared mutations while a detached computation is displayed.
    /// Activation of detached sources belongs to the workflow publication step.
    #[allow(dead_code)]
    pub(super) fn show_detached(&self, state: ContextState) {
        let mut projection = self
            .projection
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        projection.showing_shared = false;
        if replace_visible(&mut projection.visible, state) {
            projection.enqueue_visible();
        }
        drop(projection);
        Self::drain(&self.projection, &self.delivery);
    }

    #[allow(dead_code)]
    pub(super) fn show_shared(&self) {
        let mut projection = self
            .projection
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        projection.showing_shared = true;
        let shared = projection.shared.clone();
        if replace_visible(&mut projection.visible, shared) {
            projection.enqueue_visible();
        }
        projection.shared.revision = projection.visible.revision;
        drop(projection);
        Self::drain(&self.projection, &self.delivery);
    }
}

impl Projection {
    fn enqueue_visible(&mut self) {
        let Some(source) = &self.source else { return };
        self.pending.push_back(Delivery {
            observer: source.observer.clone(),
            stream_id: source.stream_id.clone(),
            state: self.visible.clone(),
        });
    }
}

fn replace_visible(current: &mut ContextState, mut next: ContextState) -> bool {
    next.revision = current.revision;
    let changed = next.items != current.items
        || next.total_estimated_tokens != current.total_estimated_tokens;
    if changed {
        next.revision += 1;
    }
    *current = next;
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_core::context::{ContextAccess, ContextManager};

    #[test]
    fn detached_display_retains_shared_updates_and_uses_one_revision() {
        let root = tempfile::tempdir().unwrap();
        for path in ["shared.rs", "detached.rs"] {
            std::fs::write(root.path().join(path), "source").unwrap();
        }
        let mut shared = ContextManager::new(root.path().into());
        let publication =
            ContextPublication::new(shared.get_state(), Arc::new(OperationRegistry::default()));
        shared.set_listener(Some(publication.listener()));
        let mut detached = shared.clone();
        detached
            .add_auto_file("detached.rs", ContextAccess::ReadOnly)
            .unwrap();
        publication.show_detached(detached.get_state());
        let detached_revision = publication.state().revision;
        shared
            .add_auto_file("shared.rs", ContextAccess::ReadOnly)
            .unwrap();
        assert_eq!(publication.state().revision, detached_revision);
        publication.show_shared();
        let restored = publication.state();
        assert!(restored.revision > detached_revision);
        assert_eq!(restored.items[0].path, "shared.rs");
        publication.show_shared();
        assert_eq!(publication.state(), restored);
    }
}
