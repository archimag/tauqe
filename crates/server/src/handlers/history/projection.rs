//! Explicit projection of domain history into the stable RPC contract.
use tauqe_flow::api::types::{HistoryEntryStatus, HistoryFileOperation, HistoryItem, HistoryKind};

pub(crate) fn history_item_dto(item: HistoryItem) -> tauqe_protocol::UiHistoryItem {
    tauqe_protocol::UiHistoryItem {
        id: item.id,
        kind: match item.kind {
            HistoryKind::User => tauqe_protocol::UiHistoryKind::User,
            HistoryKind::Assistant => tauqe_protocol::UiHistoryKind::Assistant,
            HistoryKind::System => tauqe_protocol::UiHistoryKind::System,
        },
        text: item.text,
        summary: item.summary,
        commit_hash: item.commit_hash,
        files: item
            .files
            .into_iter()
            .map(|file| tauqe_protocol::HistoryFileChange {
                path: file.path,
                op: file.op.map(|op| match op {
                    HistoryFileOperation::Replace => tauqe_protocol::EditFileOp::Replace,
                    HistoryFileOperation::Create => tauqe_protocol::EditFileOp::Create,
                    HistoryFileOperation::Delete => tauqe_protocol::EditFileOp::Delete,
                    HistoryFileOperation::Move => tauqe_protocol::EditFileOp::Move,
                    HistoryFileOperation::MoveDir => tauqe_protocol::EditFileOp::MoveDir,
                    HistoryFileOperation::Overwrite => tauqe_protocol::EditFileOp::Overwrite,
                }),
                to: file.to,
            })
            .collect(),
        rounds: item
            .rounds
            .into_iter()
            .map(|round| tauqe_protocol::TurnRoundRecord {
                index: round.index,
                message: round.message,
                feedback: round.feedback,
            })
            .collect(),
        status: match item.status {
            HistoryEntryStatus::Active => tauqe_protocol::HistoryEntryStatus::Active,
            HistoryEntryStatus::Committed => tauqe_protocol::HistoryEntryStatus::Committed,
            HistoryEntryStatus::Interrupted => tauqe_protocol::HistoryEntryStatus::Interrupted,
            HistoryEntryStatus::RolledBack => tauqe_protocol::HistoryEntryStatus::RolledBack,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_core::history::{HistoryFileChange, TurnRoundRecord};

    #[test]
    fn history_projection_preserves_payload_and_optional_fields() {
        let item = HistoryItem {
            id: 42,
            kind: HistoryKind::Assistant,
            text: "Updated module".into(),
            summary: Some("Rename".into()),
            commit_hash: Some("abc123".into()),
            files: vec![
                HistoryFileChange::moved_dir("src/old", "src/new"),
                HistoryFileChange::plain("README.md"),
            ],
            rounds: vec![TurnRoundRecord {
                index: 1,
                message: "Investigating".into(),
                feedback: "Read source".into(),
            }],
            status: HistoryEntryStatus::Interrupted,
        };
        let wire = serde_json::to_value(history_item_dto(item)).unwrap();
        assert_eq!(
            wire,
            serde_json::json!({
                "id":42, "kind":"assistant", "text":"Updated module", "summary":"Rename", "commit_hash":"abc123",
                "files":[{"path":"src/old","op":"move_dir","to":"src/new"},{"path":"README.md"}],
                "rounds":[{"index":1,"message":"Investigating","feedback":"Read source"}], "status":"interrupted"
            })
        );
        let committed = HistoryItem {
            id: 1,
            kind: HistoryKind::User,
            text: "Prompt".into(),
            summary: None,
            commit_hash: None,
            files: Vec::new(),
            rounds: Vec::new(),
            status: HistoryEntryStatus::Committed,
        };
        assert_eq!(
            serde_json::to_value(history_item_dto(committed)).unwrap(),
            serde_json::json!({"id":1,"kind":"user","text":"Prompt"})
        );
    }
}
