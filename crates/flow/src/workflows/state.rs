//! State whose lifetime belongs to one user task, not to a provider request.

use std::sync::{Arc, Mutex};
use tauqe_core::context::ContextManager;
use tauqe_core::execution::transcript::TurnTranscript;
use tauqe_core::history::{HistorySnapshot, TurnRoundRecord};
use tauqe_core::llm::CapabilityTree;
use tauqe_core::workflow::research::journal::TurnResearchJournal;

pub(crate) struct TurnState {
    pub context: ContextManager,
    pub capabilities: CapabilityTree,
    pub history: Option<HistorySnapshot>,
    pub journal: TurnResearchJournal,
    pub(super) transcript: Arc<Mutex<TurnTranscript>>,
    pub(super) budget: TurnBudget,
    pub(super) max_scratchpad_chars: usize,
}

impl TurnState {
    pub fn new(
        context: ContextManager,
        capabilities: CapabilityTree,
        history: Option<HistorySnapshot>,
        max_rounds: Option<usize>,
        max_scratchpad_chars: usize,
    ) -> Self {
        Self {
            context,
            capabilities,
            history,
            journal: TurnResearchJournal::default(),
            transcript: Arc::new(Mutex::new(TurnTranscript::default())),
            budget: TurnBudget {
                spent: 0,
                max_rounds,
            },
            max_scratchpad_chars,
        }
    }

    /// A supervisor may retain this reader across task destruction. It exposes
    /// only accepted, sanitized rounds, not context, scratchpad or mutable state.
    pub fn history_reader(&self) -> impl FnOnce() -> Vec<TurnRoundRecord> + Send + 'static {
        TurnTranscript::history_reader(&self.transcript)
    }
}

pub(super) struct TurnBudget {
    pub spent: usize,
    pub max_rounds: Option<usize>,
}

impl TurnBudget {
    pub fn exhausted(&self) -> bool {
        self.max_rounds.is_some_and(|limit| self.spent >= limit)
    }

    pub fn is_last(&self) -> bool {
        self.max_rounds == self.spent.checked_add(1)
    }
}
