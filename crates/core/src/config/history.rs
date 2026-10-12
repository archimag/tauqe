use serde::{Deserialize, Serialize};

fn default_history_budget_tokens() -> u64 {
    crate::history::DEFAULT_HISTORY_BUDGET_TOKENS
}

fn default_tail_turns() -> usize {
    crate::history::DEFAULT_TAIL_TURNS_COUNT
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryConfig {
    #[serde(default = "default_history_budget_tokens")]
    pub budget_tokens: u64,
    #[serde(default = "default_tail_turns")]
    pub tail_turns: usize,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            budget_tokens: default_history_budget_tokens(),
            tail_turns: default_tail_turns(),
        }
    }
}
