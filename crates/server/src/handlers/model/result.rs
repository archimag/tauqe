//! Explicit projection of engine turn results onto the TUI wire contract.
use crate::contracts::Project;
pub(crate) fn result_dto(
    result: tauqe_flow::api::types::ModelResult,
) -> tauqe_protocol::ModelResult {
    match result {
        tauqe_flow::api::types::ModelResult::Answer { text } => {
            tauqe_protocol::ModelResult::Answer { text }
        }
        tauqe_flow::api::types::ModelResult::Edit {
            summary,
            edits,
            proposal,
            outcome,
        } => tauqe_protocol::ModelResult::Edit {
            summary,
            edits: edits.into_iter().map(Project::project).collect(),
            proposal: proposal.map(Project::project),
            outcome: outcome.project(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_core::llm::ModelResult;
    use tauqe_protocol::{EditOutcome, ModelResultProposal, VerificationVerdict};

    #[test]
    fn answer_keeps_its_wire_shape() {
        let result = result_dto(ModelResult::Answer {
            text: "answer".into(),
        });
        assert_eq!(
            serde_json::to_value(result).unwrap(),
            serde_json::json!({"kind":"answer","text":"answer"})
        );
    }

    #[test]
    fn edit_preserves_proposal_and_flat_outcome() {
        let proposal: ModelResultProposal = serde_json::from_value(serde_json::json!({
            "message":"Applied", "tauqe_doc_requests":["git"],
            "context_requests":[{"path":"src/lib.rs","access":"read_only"}],
            "context_drops":["src/old.rs"], "scratchpad":"notes"
        }))
        .unwrap();
        let outcome = EditOutcome {
            applied: true,
            error: None,
            changed_files: vec!["src/lib.rs".into()],
            commit_hash: Some("abc123".into()),
            verification: Some(VerificationVerdict::Passed),
        };
        let expected = tauqe_protocol::ModelResult::Edit {
            summary: "Change".into(),
            edits: Vec::new(),
            proposal: Some(proposal.clone()),
            outcome: outcome.clone(),
        };
        let actual = result_dto(ModelResult::Edit {
            summary: "Change".into(),
            edits: Vec::new(),
            proposal: Some(proposal.project()),
            outcome: outcome.project(),
        });
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
    }
}
