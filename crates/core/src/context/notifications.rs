use super::{ContextManager, ContextState};
use std::sync::Arc;

/// Receives a completed context snapshot without rereading any file contents.
#[derive(Clone)]
pub struct ContextListener(Arc<dyn Fn(ContextState) + Send + Sync>);

impl ContextListener {
    pub fn new(callback: impl Fn(ContextState) + Send + Sync + 'static) -> Self {
        Self(Arc::new(callback))
    }

    fn notify(&self, state: ContextState) {
        (self.0)(state);
    }
}

impl ContextManager {
    /// Attaching a listener does not produce an event; the initial snapshot is
    /// available separately through `get_state`.
    pub fn set_listener(&mut self, listener: Option<ContextListener>) {
        self.listener = listener;
    }

    /// Commit the entire outer operation, even if it returned an error after
    /// leaving a partial change. Nested operations cannot publish incomplete
    /// pinned or merged states.
    pub(super) fn mutate<T>(&mut self, operation: impl FnOnce(&mut Self) -> T) -> T {
        let outermost = self.mutation_depth == 0;
        let revision = self.revision;
        self.mutation_depth += 1;
        let result = operation(self);
        self.mutation_depth -= 1;
        if outermost && self.revision != revision {
            self.revision = revision + 1;
            if let Some(listener) = &self.listener {
                listener.notify(self.get_state());
            }
        }
        result
    }
}
