//! Domain values exchanged at the application boundary.

pub use tauqe_core::config::AppConfig;
pub use tauqe_core::context::{ContextAccess, ContextLayer, ContextState};
pub use tauqe_core::discussion::DiscussionTarget;
pub use tauqe_core::edits::{
    EditCreate, EditDelete, EditFileOp, EditFileStatus, EditLineAnchor, EditMove, EditMoveDir,
    EditOperation, EditOutcome, EditOverwrite, EditReplace, StructuredChangeProposal,
    VerificationVerdict,
};
pub use tauqe_core::history::{
    HistoryEntryStatus, HistoryFileOperation, HistoryItem, HistoryKind, HistoryMutation,
};
pub use tauqe_core::llm::discussion::{DiscussionContextRequest, RepoSearchRequest};
pub use tauqe_core::llm::{
    DocTopic, ModelResult, ModelResultProposal, StructuredAbortProposal, StructuredBlockedProposal,
    StructuredOutcomeProposal,
};
pub use tauqe_core::locale::{LocaleCatalogItem, LocaleGenerateParams};
pub use tauqe_core::models::{
    ArchitectTier, ModelCatalogEntry, ModelEntry, ModelRef, ModelRole, ModelRolesEffort,
    ModelRolesEntries, ModelRolesSummary, ModelSelection, ProviderKind, ReasoningEffort,
};
pub use tauqe_core::plan::{
    ConceptItem, Plan, PlanImplementation, PlanItem, PlanItemStatus, PlanRejection,
    PlanRejectionKind, PlanSummary, PlanSynthesisResult,
};
pub use tauqe_core::providers::types::TurnRequestReason;
pub use tauqe_core::providers::{ChatGptSignInUi, ModelUsageInfo, StreamEvent};
pub use tauqe_core::review::window::ReviewWindow;
pub use tauqe_core::review::{
    ArchitecturalIssueGroup, IssueStatus, IssueType, PostAnalysisMode, ReviewExecuteTarget,
    ReviewFindingTarget, ReviewFindingsTarget, ReviewGroup, ReviewGroupKind, ReviewGroupTarget,
    ReviewIssue, ReviewItem, ReviewLaunchConfig, ReviewPostAnalysis, ReviewSession,
    ReviewSessionTarget, ReviewSeverity,
};
pub use tauqe_core::workflow::batch::BatchStop;
pub use tauqe_core::workflow::turn::{
    ModelTurnPlanContext, ReviewWorkPhase, TurnPhase, TurnWorkContext, TurnWorkItem, TurnWorkQueue,
};
pub use tauqe_core::workflow::{EditProtocolKind, WorkflowKind};
