//! State whose lifetime belongs to one user task, not to a provider request.

use tauqe_core::context::ContextManager;
use tauqe_core::history::HistorySnapshot;
use tauqe_core::llm::CapabilityTree;
use tauqe_core::workflow::research::journal::TurnResearchJournal;

pub(crate) struct TurnState {
    pub context: ContextManager,
    pub capabilities: CapabilityTree,
    pub history: Option<HistorySnapshot>,
    pub journal: TurnResearchJournal,
    pub(super) created_paths: Vec<String>,
    pub(super) budget: TurnBudget,
}

impl TurnState {
    pub fn new(
        context: ContextManager,
        capabilities: CapabilityTree,
        history: Option<HistorySnapshot>,
        max_rounds: Option<usize>,
    ) -> Self {
        Self {
            context,
            capabilities,
            history,
            journal: TurnResearchJournal::default(),
            created_paths: Vec::new(),
            budget: TurnBudget {
                spent: 0,
                max_rounds,
            },
        }
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

    pub fn charge(&mut self, before: usize, after: usize) {
        self.spent += after.saturating_sub(before);
    }

    pub fn final_directive(&self) -> Option<&'static str> {
        self.is_last().then_some("<response_directive>\nThis is your final allowed round for this turn: the turn round budget is reaching its limit. Conclude the task using a terminal branch of the active response schema. Do not issue further discovery requests.\n</response_directive>")
    }
}
