use serde::{Deserialize, Serialize};

/// Default history ceiling documented by the project archetype and manifest.
pub const DEFAULT_BUDGET_TOKENS: usize = 20_000;

fn default_budget_tokens() -> usize {
    DEFAULT_BUDGET_TOKENS
}

fn default_tail_turns() -> usize {
    crate::history::DEFAULT_TAIL_TURNS_COUNT
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryConfig {
    #[serde(default = "default_budget_tokens")]
    pub budget_tokens: usize,
    #[serde(default = "default_tail_turns")]
    pub tail_turns: usize,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            budget_tokens: default_budget_tokens(),
            tail_turns: default_tail_turns(),
        }
    }
}
