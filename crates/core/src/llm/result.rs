//! Engine result of a model turn, independent of RPC serialization.

use crate::edits::EditOperation;
use crate::edits::EditOutcome;
use crate::llm::ModelResultProposal;

/// Completed model output consumed by engine workflows.
///
/// Payloads belong to core; the result itself has no wire tag or serde
/// implementation. The server explicitly projects it into RPC events.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelResult {
    Answer {
        text: String,
    },
    Edit {
        summary: String,
        edits: Vec<EditOperation>,
        proposal: Option<ModelResultProposal>,
        outcome: EditOutcome,
    },
}
