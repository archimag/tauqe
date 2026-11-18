//! Conversions between engine edits and technical staging inputs.

use super::{FailedFileEdit, StagedEditsState};
use crate::edits as engine;
use tauqe_tools::edits::operation as tools;

#[derive(Debug)]
pub struct StageEditsResult {
    pub staged_state: StagedEditsState,
    pub successful_paths: Vec<String>,
    pub successful_edits: Vec<engine::EditOperation>,
    pub failed_files: Vec<FailedFileEdit>,
}
impl StageEditsResult {
    pub fn is_success(&self) -> bool {
        self.failed_files.is_empty()
    }
}
impl From<tauqe_tools::edits::StageEditsResult> for StageEditsResult {
    fn from(result: tauqe_tools::edits::StageEditsResult) -> Self {
        Self {
            staged_state: result.staged_state,
            successful_paths: result.successful_paths,
            successful_edits: result
                .successful_edits
                .into_iter()
                .map(engine_operation)
                .collect(),
            failed_files: result.failed_files,
        }
    }
}

pub(super) fn tool_operation(operation: engine::EditOperation) -> tools::EditOperation {
    match operation {
        engine::EditOperation::Replace(op) => tools::EditOperation::Replace(tools::EditReplace {
            path: op.path,
            old_text: op.old_text,
            new_text: op.new_text,
        }),
        engine::EditOperation::Create(op) => tools::EditOperation::Create(tools::EditCreate {
            path: op.path,
            content: op.content,
        }),
        engine::EditOperation::Delete(op) => {
            tools::EditOperation::Delete(tools::EditDelete { path: op.path })
        }
        engine::EditOperation::Move(op) => tools::EditOperation::Move(tools::EditMove {
            from: op.from,
            to: op.to,
        }),
        engine::EditOperation::MoveDir(op) => tools::EditOperation::MoveDir(tools::EditMoveDir {
            from: op.from,
            to: op.to,
        }),
        engine::EditOperation::Overwrite(op) => {
            tools::EditOperation::Overwrite(tools::EditOverwrite {
                path: op.path,
                content: op.content,
            })
        }
        engine::EditOperation::LineAnchor(op) => {
            tools::EditOperation::LineAnchor(tools::EditLineAnchor {
                path: op.path,
                start: op.start,
                end: op.end,
                content: op.content,
            })
        }
    }
}

pub(super) fn engine_operation(operation: tools::EditOperation) -> engine::EditOperation {
    match operation {
        tools::EditOperation::Replace(op) => engine::EditOperation::Replace(engine::EditReplace {
            path: op.path,
            old_text: op.old_text,
            new_text: op.new_text,
        }),
        tools::EditOperation::Create(op) => engine::EditOperation::Create(engine::EditCreate {
            path: op.path,
            content: op.content,
        }),
        tools::EditOperation::Delete(op) => {
            engine::EditOperation::Delete(engine::EditDelete { path: op.path })
        }
        tools::EditOperation::Move(op) => engine::EditOperation::Move(engine::EditMove {
            from: op.from,
            to: op.to,
        }),
        tools::EditOperation::MoveDir(op) => engine::EditOperation::MoveDir(engine::EditMoveDir {
            from: op.from,
            to: op.to,
        }),
        tools::EditOperation::Overwrite(op) => {
            engine::EditOperation::Overwrite(engine::EditOverwrite {
                path: op.path,
                content: op.content,
            })
        }
        tools::EditOperation::LineAnchor(op) => {
            engine::EditOperation::LineAnchor(engine::EditLineAnchor {
                path: op.path,
                start: op.start,
                end: op.end,
                content: op.content,
            })
        }
    }
}
