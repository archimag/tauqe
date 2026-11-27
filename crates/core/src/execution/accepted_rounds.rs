//! Recovery records for accepted rounds, separate from the prompt conversation.

use crate::history::TurnRoundRecord;
use std::sync::{Arc, Mutex};

/// Identifies one accepted record within its turn, including across reseeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcceptedRoundId(u32);

impl AcceptedRoundId {
    pub fn identifies(self, record: &TurnRoundRecord) -> bool {
        self.0 == record.index
    }
}

/// Only sanitized history records are shared with a recovery reader.
#[derive(Debug, Clone, Default)]
pub(crate) struct AcceptedRounds(Arc<Mutex<Vec<TurnRoundRecord>>>);

impl AcceptedRounds {
    pub(crate) fn reader(&self) -> impl FnOnce() -> Vec<TurnRoundRecord> + Send + 'static {
        let records = Arc::clone(&self.0);
        move || {
            records
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone()
        }
    }

    pub(crate) fn append(&self, index: u32, message: &str) -> AcceptedRoundId {
        let record = crate::history::entry::round_record(index, message, "");
        let id = AcceptedRoundId(record.index);
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(record);
        id
    }

    pub(crate) fn feedback(&self, feedback: &str) {
        if let Some(record) = self
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .last_mut()
        {
            // The full feedback stays in the prompt; history keeps an excerpt.
            record.feedback = feedback.trim().chars().take(240).collect();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepted_rounds_are_clean_ordered_and_readable_without_the_conversation() {
        let journal = AcceptedRounds::default();
        let reader = journal.reader();
        let first = journal.append(1, "  first <scratchpad>secret</scratchpad>  ");
        journal.feedback(&format!("  {}  ", "界".repeat(250)));
        let second = journal.append(3, "");
        drop(journal);
        let rounds = reader();
        assert_eq!(rounds.len(), 2);
        assert!(first.identifies(&rounds[0]));
        assert!(second.identifies(&rounds[1]));
        assert!(!first.identifies(&rounds[1]));
        assert_eq!(rounds[0].message, "first");
        assert_eq!(rounds[0].feedback, "界".repeat(240));
        assert!(rounds[1].message.is_empty());
        assert!(rounds[1].feedback.is_empty());
        assert!(rounds.iter().all(|round| !round.message.contains("secret")));
    }
}
