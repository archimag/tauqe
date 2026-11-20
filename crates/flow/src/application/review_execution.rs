//! Contracts of a review member's step and sequence execution.

use std::path::Path;
use tauqe_core::control::outcome::SystemError;
use tauqe_core::plan::Plan;

/// Storage of the lifecycle of steps owned by a review entity.
pub trait StepPersistence: Send + Sync {
    fn load(&self, repo_root: &Path) -> Result<Plan, SystemError>;
    fn persist(&self, repo_root: &Path, plan: &Plan) -> Result<(), SystemError>;
}

/// Result of a completed review leaf.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanStepArtifact {
    pub step_id: String,
    pub commit: Option<String>,
    pub modified_paths: Vec<String>,
}

/// Whether all scheduled leaves completed or the sequence stopped between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanSequenceStop {
    Completed,
    Stopped,
}

/// Results of the leaves executed by a review member.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanSequenceArtifact {
    pub executed_steps: Vec<PlanStepArtifact>,
    pub stop: PlanSequenceStop,
}

impl PlanSequenceArtifact {
    pub fn is_success(&self) -> bool {
        self.stop == PlanSequenceStop::Completed
    }
}
