//! Provider dialogue identity and cache invalidation policy.

use std::sync::Mutex;
use tauqe_core::models::ModelRef;

pub(crate) struct DialogueSession {
    id: Mutex<String>,
}

impl Default for DialogueSession {
    fn default() -> Self {
        Self {
            id: Mutex::new(uuid::Uuid::now_v7().to_string()),
        }
    }
}

impl DialogueSession {
    pub(crate) fn id(&self) -> String {
        self.id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn reset(&self) -> String {
        let fresh = uuid::Uuid::now_v7().to_string();
        let previous = {
            let mut id = self
                .id
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::replace(&mut *id, fresh.clone())
        };
        tracing::info!(previous_session = %previous, session_id = %fresh,
            "Rotated the dialogue session identifier");
        fresh
    }

    pub(crate) fn rotate_on_model_change(&self, old: &ModelRef, new: &ModelRef) -> bool {
        if old == new {
            return false;
        }
        let fresh = self.reset();
        tracing::info!(previous_model = %old, model = %new, session_id = %fresh,
            "Rotated the dialogue session identifier after a model change");
        true
    }
}
