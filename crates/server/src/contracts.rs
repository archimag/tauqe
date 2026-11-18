//! Exhaustive projections between owned domain documents and RPC DTOs.
//! Rust's orphan rule requires a server-owned trait for these foreign types.
pub(crate) trait Project: Sized {
    type Target;
    fn project(self) -> Self::Target;
}
use tauqe_core::models as domain;
use tauqe_protocol as wire;
impl Project for domain::ProviderKind {
    type Target = wire::ProviderKind;
    fn project(self) -> Self::Target {
        let value = self;
        match value {
            domain::ProviderKind::OpenRouter => wire::ProviderKind::OpenRouter,
        }
    }
}
impl Project for wire::ProviderKind {
    type Target = domain::ProviderKind;
    fn project(self) -> Self::Target {
        let value = self;
        match value {
            wire::ProviderKind::OpenRouter => domain::ProviderKind::OpenRouter,
        }
    }
}
impl Project for domain::ModelRole {
    type Target = wire::ModelRole;
    fn project(self) -> Self::Target {
        let value = self;
        match value {
            domain::ModelRole::Scribe => wire::ModelRole::Scribe,
            domain::ModelRole::Coder => wire::ModelRole::Coder,
            domain::ModelRole::Architect => wire::ModelRole::Architect,
            domain::ModelRole::Gatekeeper => wire::ModelRole::Gatekeeper,
            domain::ModelRole::REPL => wire::ModelRole::REPL,
            domain::ModelRole::Reviewer => wire::ModelRole::Reviewer,
        }
    }
}
impl Project for wire::ModelRole {
    type Target = domain::ModelRole;
    fn project(self) -> Self::Target {
        let value = self;
        match value {
            wire::ModelRole::Scribe => domain::ModelRole::Scribe,
            wire::ModelRole::Coder => domain::ModelRole::Coder,
            wire::ModelRole::Architect => domain::ModelRole::Architect,
            wire::ModelRole::Gatekeeper => domain::ModelRole::Gatekeeper,
            wire::ModelRole::REPL => domain::ModelRole::REPL,
            wire::ModelRole::Reviewer => domain::ModelRole::Reviewer,
        }
    }
}
impl Project for domain::ReasoningEffort {
    type Target = wire::ReasoningEffort;
    fn project(self) -> Self::Target {
        let value = self;
        match value {
            domain::ReasoningEffort::Low => wire::ReasoningEffort::Low,
            domain::ReasoningEffort::Medium => wire::ReasoningEffort::Medium,
            domain::ReasoningEffort::High => wire::ReasoningEffort::High,
            domain::ReasoningEffort::XHigh => wire::ReasoningEffort::XHigh,
            domain::ReasoningEffort::Max => wire::ReasoningEffort::Max,
        }
    }
}
impl Project for wire::ReasoningEffort {
    type Target = domain::ReasoningEffort;
    fn project(self) -> Self::Target {
        let value = self;
        match value {
            wire::ReasoningEffort::Low => domain::ReasoningEffort::Low,
            wire::ReasoningEffort::Medium => domain::ReasoningEffort::Medium,
            wire::ReasoningEffort::High => domain::ReasoningEffort::High,
            wire::ReasoningEffort::XHigh => domain::ReasoningEffort::XHigh,
            wire::ReasoningEffort::Max => domain::ReasoningEffort::Max,
        }
    }
}
impl Project for domain::ModelRef {
    type Target = wire::ModelRef;
    fn project(self) -> Self::Target {
        let value = self;
        wire::ModelRef {
            provider: value.provider.project(),
            name: value.name,
        }
    }
}
impl Project for wire::ModelRef {
    type Target = domain::ModelRef;
    fn project(self) -> Self::Target {
        let value = self;
        domain::ModelRef {
            provider: value.provider.project(),
            name: value.name,
        }
    }
}
impl Project for domain::ModelCatalogEntry {
    type Target = wire::ModelCatalogEntry;
    fn project(self) -> Self::Target {
        let value = self;
        wire::ModelCatalogEntry {
            model: value.model.project(),
            alias: value.alias,
        }
    }
}
impl Project for wire::ModelCatalogEntry {
    type Target = domain::ModelCatalogEntry;
    fn project(self) -> Self::Target {
        let value = self;
        domain::ModelCatalogEntry {
            model: value.model.project(),
            alias: value.alias,
        }
    }
}
impl Project for domain::ModelEntry {
    type Target = wire::ModelEntry;
    fn project(self) -> Self::Target {
        let value = self;
        wire::ModelEntry {
            model: value.model.project(),
            effort: value.effort.map(Project::project),
            attempts: value.attempts,
            level_up: value.level_up,
        }
    }
}
impl Project for wire::ModelEntry {
    type Target = domain::ModelEntry;
    fn project(self) -> Self::Target {
        let value = self;
        domain::ModelEntry {
            model: value.model.project(),
            effort: value.effort.map(Project::project),
            attempts: value.attempts,
            level_up: value.level_up,
        }
    }
}
impl Project for domain::ArchitectTier {
    type Target = wire::ArchitectTier;
    fn project(self) -> Self::Target {
        let value = self;
        wire::ArchitectTier {
            model: value.model.project(),
            effort: value.effort.map(Project::project),
            attempts: value.attempts,
            level_up: value.level_up,
        }
    }
}
impl Project for wire::ArchitectTier {
    type Target = domain::ArchitectTier;
    fn project(self) -> Self::Target {
        let value = self;
        domain::ArchitectTier {
            model: value.model.project(),
            effort: value.effort.map(Project::project),
            attempts: value.attempts,
            level_up: value.level_up,
        }
    }
}
impl Project for domain::ModelRolesEffort {
    type Target = wire::ModelRolesEffort;
    fn project(self) -> Self::Target {
        let value = self;
        wire::ModelRolesEffort {
            scribe: value.scribe.map(Project::project),
            coder: value.coder.map(Project::project),
            architect: value.architect.map(Project::project),
            repl: value.repl.map(Project::project),
            reviewer: value.reviewer.map(Project::project),
        }
    }
}
impl Project for wire::ModelRolesEffort {
    type Target = domain::ModelRolesEffort;
    fn project(self) -> Self::Target {
        let value = self;
        domain::ModelRolesEffort {
            scribe: value.scribe.map(Project::project),
            coder: value.coder.map(Project::project),
            architect: value.architect.map(Project::project),
            repl: value.repl.map(Project::project),
            reviewer: value.reviewer.map(Project::project),
        }
    }
}
impl Project for domain::ModelRolesEntries {
    type Target = wire::ModelRolesEntries;
    fn project(self) -> Self::Target {
        let value = self;
        wire::ModelRolesEntries {
            scribe: value.scribe.into_iter().map(Project::project).collect(),
            coder: value.coder.into_iter().map(Project::project).collect(),
            architect: value.architect.into_iter().map(Project::project).collect(),
            repl: value.repl.into_iter().map(Project::project).collect(),
            reviewer: value.reviewer.into_iter().map(Project::project).collect(),
            catalog: value
                .catalog
                .map(|items| items.into_iter().map(Project::project).collect()),
        }
    }
}
impl Project for wire::ModelRolesEntries {
    type Target = domain::ModelRolesEntries;
    fn project(self) -> Self::Target {
        let value = self;
        domain::ModelRolesEntries {
            scribe: value.scribe.into_iter().map(Project::project).collect(),
            coder: value.coder.into_iter().map(Project::project).collect(),
            architect: value.architect.into_iter().map(Project::project).collect(),
            repl: value.repl.into_iter().map(Project::project).collect(),
            reviewer: value.reviewer.into_iter().map(Project::project).collect(),
            catalog: value
                .catalog
                .map(|items| items.into_iter().map(Project::project).collect()),
        }
    }
}
impl Project for domain::ModelRolesSummary {
    type Target = wire::ModelRolesSummary;
    fn project(self) -> Self::Target {
        let value = self;
        wire::ModelRolesSummary {
            scribe: value.scribe.project(),
            coder: value.coder.project(),
            architect: value.architect.project(),
            repl: value.repl.project(),
            reviewer: value.reviewer.project(),
            efforts: value.efforts.map(Project::project),
            entries: value.entries.map(Project::project),
        }
    }
}
impl Project for wire::ModelRolesSummary {
    type Target = domain::ModelRolesSummary;
    fn project(self) -> Self::Target {
        let value = self;
        domain::ModelRolesSummary {
            scribe: value.scribe.project(),
            coder: value.coder.project(),
            architect: value.architect.project(),
            repl: value.repl.project(),
            reviewer: value.reviewer.project(),
            efforts: value.efforts.map(Project::project),
            entries: value.entries.map(Project::project),
        }
    }
}
impl Project for domain::ModelSelection {
    type Target = wire::ModelSelection;
    fn project(self) -> Self::Target {
        let value = self;
        match value {
            domain::ModelSelection::Role(role) => wire::ModelSelection::Role(role.project()),
            domain::ModelSelection::Specific(model) => {
                wire::ModelSelection::Specific(model.project())
            }
            domain::ModelSelection::Tier { model, effort } => wire::ModelSelection::Tier {
                model: model.project(),
                effort: effort.map(Project::project),
            },
        }
    }
}
impl Project for wire::ModelSelection {
    type Target = domain::ModelSelection;
    fn project(self) -> Self::Target {
        let value = self;
        match value {
            wire::ModelSelection::Role(role) => domain::ModelSelection::Role(role.project()),
            wire::ModelSelection::Specific(model) => {
                domain::ModelSelection::Specific(model.project())
            }
            wire::ModelSelection::Tier { model, effort } => domain::ModelSelection::Tier {
                model: model.project(),
                effort: effort.map(Project::project),
            },
        }
    }
}
use tauqe_core::{plan as domain_plan, review as domain_review};

impl Project for domain_plan::PlanItemStatus {
    type Target = wire::PlanItemStatus;
    fn project(self) -> Self::Target {
        let value = self;
        match value {
            domain_plan::PlanItemStatus::Discussion => wire::PlanItemStatus::Discussion,
            domain_plan::PlanItemStatus::Todo => wire::PlanItemStatus::Todo,
            domain_plan::PlanItemStatus::InProgress => wire::PlanItemStatus::InProgress,
            domain_plan::PlanItemStatus::Done => wire::PlanItemStatus::Done,
            domain_plan::PlanItemStatus::Blocked => wire::PlanItemStatus::Blocked,
            domain_plan::PlanItemStatus::Cancelled => wire::PlanItemStatus::Cancelled,
        }
    }
}
impl Project for wire::PlanItemStatus {
    type Target = domain_plan::PlanItemStatus;
    fn project(self) -> Self::Target {
        let value = self;
        match value {
            wire::PlanItemStatus::Discussion => domain_plan::PlanItemStatus::Discussion,
            wire::PlanItemStatus::Todo => domain_plan::PlanItemStatus::Todo,
            wire::PlanItemStatus::InProgress => domain_plan::PlanItemStatus::InProgress,
            wire::PlanItemStatus::Done => domain_plan::PlanItemStatus::Done,
            wire::PlanItemStatus::Blocked => domain_plan::PlanItemStatus::Blocked,
            wire::PlanItemStatus::Cancelled => domain_plan::PlanItemStatus::Cancelled,
        }
    }
}
impl Project for domain_plan::PlanRejectionKind {
    type Target = wire::PlanRejectionKind;
    fn project(self) -> Self::Target {
        let value = self;
        match value {
            domain_plan::PlanRejectionKind::PayloadDefect => wire::PlanRejectionKind::PayloadDefect,
            domain_plan::PlanRejectionKind::ModeForbidden => wire::PlanRejectionKind::ModeForbidden,
            domain_plan::PlanRejectionKind::Abandoned => wire::PlanRejectionKind::Abandoned,
        }
    }
}
impl Project for wire::PlanRejectionKind {
    type Target = domain_plan::PlanRejectionKind;
    fn project(self) -> Self::Target {
        let value = self;
        match value {
            wire::PlanRejectionKind::PayloadDefect => domain_plan::PlanRejectionKind::PayloadDefect,
            wire::PlanRejectionKind::ModeForbidden => domain_plan::PlanRejectionKind::ModeForbidden,
            wire::PlanRejectionKind::Abandoned => domain_plan::PlanRejectionKind::Abandoned,
        }
    }
}
impl Project for domain_review::IssueType {
    type Target = wire::IssueType;
    fn project(self) -> Self::Target {
        let value = self;
        match value {
            domain_review::IssueType::Bug => wire::IssueType::Bug,
            domain_review::IssueType::Security => wire::IssueType::Security,
            domain_review::IssueType::Architecture => wire::IssueType::Architecture,
            domain_review::IssueType::Performance => wire::IssueType::Performance,
            domain_review::IssueType::Quality => wire::IssueType::Quality,
        }
    }
}
impl Project for wire::IssueType {
    type Target = domain_review::IssueType;
    fn project(self) -> Self::Target {
        let value = self;
        match value {
            wire::IssueType::Bug => domain_review::IssueType::Bug,
            wire::IssueType::Security => domain_review::IssueType::Security,
            wire::IssueType::Architecture => domain_review::IssueType::Architecture,
            wire::IssueType::Performance => domain_review::IssueType::Performance,
            wire::IssueType::Quality => domain_review::IssueType::Quality,
        }
    }
}
impl Project for domain_review::IssueStatus {
    type Target = wire::IssueStatus;
    fn project(self) -> Self::Target {
        let value = self;
        match value {
            domain_review::IssueStatus::Discussion => wire::IssueStatus::Discussion,
            domain_review::IssueStatus::Todo => wire::IssueStatus::Todo,
            domain_review::IssueStatus::InProgress => wire::IssueStatus::InProgress,
            domain_review::IssueStatus::Fixed => wire::IssueStatus::Fixed,
            domain_review::IssueStatus::Skipped => wire::IssueStatus::Skipped,
            domain_review::IssueStatus::Rejected => wire::IssueStatus::Rejected,
        }
    }
}
impl Project for wire::IssueStatus {
    type Target = domain_review::IssueStatus;
    fn project(self) -> Self::Target {
        let value = self;
        match value {
            wire::IssueStatus::Discussion => domain_review::IssueStatus::Discussion,
            wire::IssueStatus::Todo => domain_review::IssueStatus::Todo,
            wire::IssueStatus::InProgress => domain_review::IssueStatus::InProgress,
            wire::IssueStatus::Fixed => domain_review::IssueStatus::Fixed,
            wire::IssueStatus::Skipped => domain_review::IssueStatus::Skipped,
            wire::IssueStatus::Rejected => domain_review::IssueStatus::Rejected,
        }
    }
}
impl Project for domain_review::ReviewSeverity {
    type Target = wire::ReviewSeverity;
    fn project(self) -> Self::Target {
        let value = self;
        match value {
            domain_review::ReviewSeverity::Critical => wire::ReviewSeverity::Critical,
            domain_review::ReviewSeverity::Warning => wire::ReviewSeverity::Warning,
            domain_review::ReviewSeverity::Suggestion => wire::ReviewSeverity::Suggestion,
            domain_review::ReviewSeverity::Info => wire::ReviewSeverity::Info,
        }
    }
}
impl Project for wire::ReviewSeverity {
    type Target = domain_review::ReviewSeverity;
    fn project(self) -> Self::Target {
        let value = self;
        match value {
            wire::ReviewSeverity::Critical => domain_review::ReviewSeverity::Critical,
            wire::ReviewSeverity::Warning => domain_review::ReviewSeverity::Warning,
            wire::ReviewSeverity::Suggestion => domain_review::ReviewSeverity::Suggestion,
            wire::ReviewSeverity::Info => domain_review::ReviewSeverity::Info,
        }
    }
}
impl Project for domain_review::ReviewGroupKind {
    type Target = wire::ReviewGroupKind;
    fn project(self) -> Self::Target {
        let value = self;
        match value {
            domain_review::ReviewGroupKind::Simple => wire::ReviewGroupKind::Simple,
            domain_review::ReviewGroupKind::Architectural => wire::ReviewGroupKind::Architectural,
        }
    }
}
impl Project for wire::ReviewGroupKind {
    type Target = domain_review::ReviewGroupKind;
    fn project(self) -> Self::Target {
        let value = self;
        match value {
            wire::ReviewGroupKind::Simple => domain_review::ReviewGroupKind::Simple,
            wire::ReviewGroupKind::Architectural => domain_review::ReviewGroupKind::Architectural,
        }
    }
}
impl Project for domain_review::PostAnalysisMode {
    type Target = wire::PostAnalysisMode;
    fn project(self) -> Self::Target {
        let value = self;
        match value {
            domain_review::PostAnalysisMode::Automatic => wire::PostAnalysisMode::Automatic,
            domain_review::PostAnalysisMode::Manual => wire::PostAnalysisMode::Manual,
            domain_review::PostAnalysisMode::None => wire::PostAnalysisMode::None,
        }
    }
}
impl Project for wire::PostAnalysisMode {
    type Target = domain_review::PostAnalysisMode;
    fn project(self) -> Self::Target {
        let value = self;
        match value {
            wire::PostAnalysisMode::Automatic => domain_review::PostAnalysisMode::Automatic,
            wire::PostAnalysisMode::Manual => domain_review::PostAnalysisMode::Manual,
            wire::PostAnalysisMode::None => domain_review::PostAnalysisMode::None,
        }
    }
}
impl Project for domain_plan::Plan {
    type Target = wire::Plan;
    fn project(self) -> Self::Target {
        let value = self;
        wire::Plan {
            id: value.id,
            title: value.title,
            description: value.description,
            created_at: value.created_at,
            updated_at: value.updated_at,
            concept: value.concept.into_iter().map(Project::project).collect(),
            implementation: value.implementation.project(),
        }
    }
}
impl Project for wire::Plan {
    type Target = domain_plan::Plan;
    fn project(self) -> Self::Target {
        let value = self;
        domain_plan::Plan {
            id: value.id,
            title: value.title,
            description: value.description,
            created_at: value.created_at,
            updated_at: value.updated_at,
            concept: value.concept.into_iter().map(Project::project).collect(),
            implementation: value.implementation.project(),
        }
    }
}
impl Project for domain_plan::PlanSummary {
    type Target = wire::PlanSummary;
    fn project(self) -> Self::Target {
        let value = self;
        wire::PlanSummary {
            id: value.id,
            title: value.title,
            description: value.description,
            created_at: value.created_at,
            updated_at: value.updated_at,
            total_items: value.total_items,
            completed_items: value.completed_items,
            status: value.status.project(),
        }
    }
}
impl Project for wire::PlanSummary {
    type Target = domain_plan::PlanSummary;
    fn project(self) -> Self::Target {
        let value = self;
        domain_plan::PlanSummary {
            id: value.id,
            title: value.title,
            description: value.description,
            created_at: value.created_at,
            updated_at: value.updated_at,
            total_items: value.total_items,
            completed_items: value.completed_items,
            status: value.status.project(),
        }
    }
}
impl Project for domain_plan::PlanItem {
    type Target = wire::PlanItem;
    fn project(self) -> Self::Target {
        let value = self;
        wire::PlanItem {
            id: value.id,
            title: value.title,
            details: value.details,
            status: value.status.project(),
            blocked_reason: value.blocked_reason,
            commit: value.commit,
            files: value.files,
            readonly_files: value.readonly_files,
            outer_docs: value.outer_docs,
            concept_refs: value.concept_refs,
            depends_on: value.depends_on,
            children: value.children.into_iter().map(Project::project).collect(),
        }
    }
}
impl Project for wire::PlanItem {
    type Target = domain_plan::PlanItem;
    fn project(self) -> Self::Target {
        let value = self;
        domain_plan::PlanItem {
            id: value.id,
            title: value.title,
            details: value.details,
            status: value.status.project(),
            blocked_reason: value.blocked_reason,
            commit: value.commit,
            files: value.files,
            readonly_files: value.readonly_files,
            outer_docs: value.outer_docs,
            concept_refs: value.concept_refs,
            depends_on: value.depends_on,
            children: value.children.into_iter().map(Project::project).collect(),
        }
    }
}
impl Project for domain_plan::ConceptItem {
    type Target = wire::ConceptItem;
    fn project(self) -> Self::Target {
        let value = self;
        wire::ConceptItem {
            id: value.id,
            title: value.title,
            details: value.details,
            children: value.children.into_iter().map(Project::project).collect(),
        }
    }
}
impl Project for wire::ConceptItem {
    type Target = domain_plan::ConceptItem;
    fn project(self) -> Self::Target {
        let value = self;
        domain_plan::ConceptItem {
            id: value.id,
            title: value.title,
            details: value.details,
            children: value.children.into_iter().map(Project::project).collect(),
        }
    }
}
impl Project for domain_plan::PlanImplementation {
    type Target = wire::PlanImplementation;
    fn project(self) -> Self::Target {
        let value = self;
        wire::PlanImplementation {
            items: value.items.into_iter().map(Project::project).collect(),
            architect: value.architect.map(Project::project),
            last_replanned_step: value.last_replanned_step,
        }
    }
}
impl Project for wire::PlanImplementation {
    type Target = domain_plan::PlanImplementation;
    fn project(self) -> Self::Target {
        let value = self;
        domain_plan::PlanImplementation {
            items: value.items.into_iter().map(Project::project).collect(),
            architect: value.architect.map(Project::project),
            last_replanned_step: value.last_replanned_step,
        }
    }
}
impl Project for domain_plan::PlanRejection {
    type Target = wire::PlanRejection;
    fn project(self) -> Self::Target {
        let value = self;
        wire::PlanRejection {
            kind: value.kind.project(),
            message: value.message,
        }
    }
}
impl Project for wire::PlanRejection {
    type Target = domain_plan::PlanRejection;
    fn project(self) -> Self::Target {
        let value = self;
        domain_plan::PlanRejection {
            kind: value.kind.project(),
            message: value.message,
        }
    }
}
impl Project for domain_review::ReviewIssue {
    type Target = wire::ReviewIssue;
    fn project(self) -> Self::Target {
        let value = self;
        wire::ReviewIssue {
            id: value.id,
            title: value.title,
            uuid: value.uuid,
            group: value.group,
            model: value.model.map(Project::project),
            issue_type: value.issue_type.project(),
            status: value.status.project(),
            description: value.description,
            recommendations: value.recommendations,
            files: value.files,
            readonly_files: value.readonly_files,
            commit: value.commit,
            note: value.note,
        }
    }
}
impl Project for wire::ReviewIssue {
    type Target = domain_review::ReviewIssue;
    fn project(self) -> Self::Target {
        let value = self;
        domain_review::ReviewIssue {
            id: value.id,
            title: value.title,
            uuid: value.uuid,
            group: value.group,
            model: value.model.map(Project::project),
            issue_type: value.issue_type.project(),
            status: value.status.project(),
            description: value.description,
            recommendations: value.recommendations,
            files: value.files,
            readonly_files: value.readonly_files,
            commit: value.commit,
            note: value.note,
        }
    }
}
impl Project for domain_review::ReviewItem {
    type Target = wire::ReviewItem;
    fn project(self) -> Self::Target {
        let value = self;
        wire::ReviewItem {
            id: value.id,
            title: value.title,
            uuid: value.uuid,
            group: value.group,
            model: value.model.map(Project::project),
            issue_type: value.issue_type.project(),
            severity: value.severity.project(),
            status: value.status.project(),
            recommendations: value.recommendations,
            files: value.files,
            readonly_files: value.readonly_files,
            commit: value.commit,
            resolution_notes: value.resolution_notes,
            body: value.body,
        }
    }
}
impl Project for wire::ReviewItem {
    type Target = domain_review::ReviewItem;
    fn project(self) -> Self::Target {
        let value = self;
        domain_review::ReviewItem {
            id: value.id,
            title: value.title,
            uuid: value.uuid,
            group: value.group,
            model: value.model.map(Project::project),
            issue_type: value.issue_type.project(),
            severity: value.severity.project(),
            status: value.status.project(),
            recommendations: value.recommendations,
            files: value.files,
            readonly_files: value.readonly_files,
            commit: value.commit,
            resolution_notes: value.resolution_notes,
            body: value.body,
        }
    }
}
impl Project for domain_review::ReviewGroup {
    type Target = wire::ReviewGroup;
    fn project(self) -> Self::Target {
        let value = self;
        wire::ReviewGroup {
            id: value.id,
            title: value.title,
            paths: value.paths,
            prompt: value.prompt,
            description: value.description,
            blocked_reason: value.blocked_reason,
            resolution_notes: value.resolution_notes,
            kind: value.kind.project(),
            implementation: value.implementation.project(),
        }
    }
}
impl Project for wire::ReviewGroup {
    type Target = domain_review::ReviewGroup;
    fn project(self) -> Self::Target {
        let value = self;
        domain_review::ReviewGroup {
            id: value.id,
            title: value.title,
            paths: value.paths,
            prompt: value.prompt,
            description: value.description,
            blocked_reason: value.blocked_reason,
            resolution_notes: value.resolution_notes,
            kind: value.kind.project(),
            implementation: value.implementation.project(),
        }
    }
}
impl Project for domain_review::ArchitecturalIssueGroup {
    type Target = wire::ArchitecturalIssueGroup;
    fn project(self) -> Self::Target {
        let value = self;
        wire::ArchitecturalIssueGroup {
            id: value.id,
            title: value.title,
            description: value.description,
            issue_ids: value.issue_ids,
            implementation: value.implementation.project(),
        }
    }
}
impl Project for wire::ArchitecturalIssueGroup {
    type Target = domain_review::ArchitecturalIssueGroup;
    fn project(self) -> Self::Target {
        let value = self;
        domain_review::ArchitecturalIssueGroup {
            id: value.id,
            title: value.title,
            description: value.description,
            issue_ids: value.issue_ids,
            implementation: value.implementation.project(),
        }
    }
}
impl Project for domain_review::ReviewPostAnalysis {
    type Target = wire::ReviewPostAnalysis;
    fn project(self) -> Self::Target {
        let value = self;
        wire::ReviewPostAnalysis {
            created_at: value.created_at,
            summary: value.summary,
            independent_issues: value.independent_issues,
            architectural_groups: value
                .architectural_groups
                .into_iter()
                .map(Project::project)
                .collect(),
        }
    }
}
impl Project for wire::ReviewPostAnalysis {
    type Target = domain_review::ReviewPostAnalysis;
    fn project(self) -> Self::Target {
        let value = self;
        domain_review::ReviewPostAnalysis {
            created_at: value.created_at,
            summary: value.summary,
            independent_issues: value.independent_issues,
            architectural_groups: value
                .architectural_groups
                .into_iter()
                .map(Project::project)
                .collect(),
        }
    }
}
impl Project for domain_review::ReviewLaunchConfig {
    type Target = wire::ReviewLaunchConfig;
    fn project(self) -> Self::Target {
        let value = self;
        wire::ReviewLaunchConfig {
            preset: value.preset,
            mode: value.mode.project(),
            groups: value.groups.into_iter().map(Project::project).collect(),
        }
    }
}
impl Project for wire::ReviewLaunchConfig {
    type Target = domain_review::ReviewLaunchConfig;
    fn project(self) -> Self::Target {
        let value = self;
        domain_review::ReviewLaunchConfig {
            preset: value.preset,
            mode: value.mode.project(),
            groups: value.groups.into_iter().map(Project::project).collect(),
        }
    }
}
impl Project for domain_review::ReviewSession {
    type Target = wire::ReviewSession;
    fn project(self) -> Self::Target {
        let value = self;
        wire::ReviewSession {
            id: value.id,
            title: value.title,
            created_at: value.created_at,
            model: value.model.project(),
            description: value.description,
            user_prompt: value.user_prompt,
            target_files: value.target_files,
            items: value.items.into_iter().map(Project::project).collect(),
            raw_markdown: value.raw_markdown,
            groups: value.groups.into_iter().map(Project::project).collect(),
            post_analysis: value.post_analysis.map(Project::project),
            launch_config: value.launch_config.map(Project::project),
        }
    }
}
impl Project for wire::ReviewSession {
    type Target = domain_review::ReviewSession;
    fn project(self) -> Self::Target {
        let value = self;
        domain_review::ReviewSession {
            id: value.id,
            title: value.title,
            created_at: value.created_at,
            model: value.model.project(),
            description: value.description,
            user_prompt: value.user_prompt,
            target_files: value.target_files,
            items: value.items.into_iter().map(Project::project).collect(),
            raw_markdown: value.raw_markdown,
            groups: value.groups.into_iter().map(Project::project).collect(),
            post_analysis: value.post_analysis.map(Project::project),
            launch_config: value.launch_config.map(Project::project),
        }
    }
}
impl Project for domain_review::ReviewSessionTarget {
    type Target = wire::ReviewSessionTarget;
    fn project(self) -> Self::Target {
        let value = self;
        wire::ReviewSessionTarget {
            review_id: value.review_id,
        }
    }
}
impl Project for wire::ReviewSessionTarget {
    type Target = domain_review::ReviewSessionTarget;
    fn project(self) -> Self::Target {
        let value = self;
        domain_review::ReviewSessionTarget {
            review_id: value.review_id,
        }
    }
}
impl Project for domain_review::ReviewFindingTarget {
    type Target = wire::ReviewFindingTarget;
    fn project(self) -> Self::Target {
        let value = self;
        wire::ReviewFindingTarget {
            review_id: value.review_id,
            item_id: value.item_id,
        }
    }
}
impl Project for wire::ReviewFindingTarget {
    type Target = domain_review::ReviewFindingTarget;
    fn project(self) -> Self::Target {
        let value = self;
        domain_review::ReviewFindingTarget {
            review_id: value.review_id,
            item_id: value.item_id,
        }
    }
}
impl Project for domain_review::ReviewFindingsTarget {
    type Target = wire::ReviewFindingsTarget;
    fn project(self) -> Self::Target {
        let value = self;
        wire::ReviewFindingsTarget {
            review_id: value.review_id,
            item_ids: value.item_ids,
        }
    }
}
impl Project for wire::ReviewFindingsTarget {
    type Target = domain_review::ReviewFindingsTarget;
    fn project(self) -> Self::Target {
        let value = self;
        domain_review::ReviewFindingsTarget {
            review_id: value.review_id,
            item_ids: value.item_ids,
        }
    }
}
impl Project for domain_review::ReviewGroupTarget {
    type Target = wire::ReviewGroupTarget;
    fn project(self) -> Self::Target {
        let value = self;
        wire::ReviewGroupTarget {
            review_id: value.review_id,
            group_id: value.group_id,
        }
    }
}
impl Project for wire::ReviewGroupTarget {
    type Target = domain_review::ReviewGroupTarget;
    fn project(self) -> Self::Target {
        let value = self;
        domain_review::ReviewGroupTarget {
            review_id: value.review_id,
            group_id: value.group_id,
        }
    }
}
impl Project for domain_review::ReviewExecuteTarget {
    type Target = wire::ReviewExecuteTarget;
    fn project(self) -> Self::Target {
        let value = self;
        match value {
            domain_review::ReviewExecuteTarget::AllSessions => {
                wire::ReviewExecuteTarget::AllSessions
            }
            domain_review::ReviewExecuteTarget::Session(target) => {
                wire::ReviewExecuteTarget::Session(target.project())
            }
            domain_review::ReviewExecuteTarget::Finding(target) => {
                wire::ReviewExecuteTarget::Finding(target.project())
            }
            domain_review::ReviewExecuteTarget::Findings(target) => {
                wire::ReviewExecuteTarget::Findings(target.project())
            }
            domain_review::ReviewExecuteTarget::Group(target) => {
                wire::ReviewExecuteTarget::Group(target.project())
            }
        }
    }
}
impl Project for wire::ReviewExecuteTarget {
    type Target = domain_review::ReviewExecuteTarget;
    fn project(self) -> Self::Target {
        let value = self;
        match value {
            wire::ReviewExecuteTarget::AllSessions => {
                domain_review::ReviewExecuteTarget::AllSessions
            }
            wire::ReviewExecuteTarget::Session(target) => {
                domain_review::ReviewExecuteTarget::Session(target.project())
            }
            wire::ReviewExecuteTarget::Finding(target) => {
                domain_review::ReviewExecuteTarget::Finding(target.project())
            }
            wire::ReviewExecuteTarget::Findings(target) => {
                domain_review::ReviewExecuteTarget::Findings(target.project())
            }
            wire::ReviewExecuteTarget::Group(target) => {
                domain_review::ReviewExecuteTarget::Group(target.project())
            }
        }
    }
}
impl Project for domain_plan::PlanSynthesisResult {
    type Target = wire::PlanRefineResult;
    fn project(self) -> Self::Target {
        let value = self;
        match value {
            domain_plan::PlanSynthesisResult::Success { plan, explanation } => {
                wire::PlanRefineResult::Success {
                    plan: plan.project(),
                    explanation,
                }
            }
            domain_plan::PlanSynthesisResult::Blocked { reason } => {
                wire::PlanRefineResult::Blocked { reason }
            }
        }
    }
}

impl Project for tauqe_core::workflow::WorkflowKind {
    type Target = wire::WorkflowKind;
    fn project(self) -> Self::Target {
        match self {
            tauqe_core::workflow::WorkflowKind::Git => wire::WorkflowKind::Git,
            tauqe_core::workflow::WorkflowKind::Naive => wire::WorkflowKind::Naive,
        }
    }
}

impl Project for wire::WorkflowKind {
    type Target = tauqe_core::workflow::WorkflowKind;
    fn project(self) -> Self::Target {
        match self {
            wire::WorkflowKind::Git => tauqe_core::workflow::WorkflowKind::Git,
            wire::WorkflowKind::Naive => tauqe_core::workflow::WorkflowKind::Naive,
        }
    }
}

impl Project for tauqe_core::workflow::EditProtocolKind {
    type Target = wire::EditProtocolKind;
    fn project(self) -> Self::Target {
        match self {
            tauqe_core::workflow::EditProtocolKind::Structured => {
                wire::EditProtocolKind::Structured
            }
        }
    }
}

impl Project for wire::EditProtocolKind {
    type Target = tauqe_core::workflow::EditProtocolKind;
    fn project(self) -> Self::Target {
        match self {
            wire::EditProtocolKind::Structured => {
                tauqe_core::workflow::EditProtocolKind::Structured
            }
        }
    }
}

impl Project for tauqe_core::workflow::turn::TurnPhase {
    type Target = wire::TurnPhase;
    fn project(self) -> Self::Target {
        match self {
            tauqe_core::workflow::turn::TurnPhase::Research => wire::TurnPhase::Research,
            tauqe_core::workflow::turn::TurnPhase::Proposal => wire::TurnPhase::Proposal,
            tauqe_core::workflow::turn::TurnPhase::Staging => wire::TurnPhase::Staging,
            tauqe_core::workflow::turn::TurnPhase::Verification => wire::TurnPhase::Verification,
            tauqe_core::workflow::turn::TurnPhase::Healing => wire::TurnPhase::Healing,
        }
    }
}

impl Project for wire::TurnPhase {
    type Target = tauqe_core::workflow::turn::TurnPhase;
    fn project(self) -> Self::Target {
        match self {
            wire::TurnPhase::Research => tauqe_core::workflow::turn::TurnPhase::Research,
            wire::TurnPhase::Proposal => tauqe_core::workflow::turn::TurnPhase::Proposal,
            wire::TurnPhase::Staging => tauqe_core::workflow::turn::TurnPhase::Staging,
            wire::TurnPhase::Verification => tauqe_core::workflow::turn::TurnPhase::Verification,
            wire::TurnPhase::Healing => tauqe_core::workflow::turn::TurnPhase::Healing,
        }
    }
}

impl Project for tauqe_core::workflow::turn::ReviewWorkPhase {
    type Target = wire::ReviewWorkPhase;
    fn project(self) -> Self::Target {
        match self {
            tauqe_core::workflow::turn::ReviewWorkPhase::Audit => wire::ReviewWorkPhase::Audit,
            tauqe_core::workflow::turn::ReviewWorkPhase::Decomposition => {
                wire::ReviewWorkPhase::Decomposition
            }
            tauqe_core::workflow::turn::ReviewWorkPhase::PostAnalysis => {
                wire::ReviewWorkPhase::PostAnalysis
            }
        }
    }
}

impl Project for wire::ReviewWorkPhase {
    type Target = tauqe_core::workflow::turn::ReviewWorkPhase;
    fn project(self) -> Self::Target {
        match self {
            wire::ReviewWorkPhase::Audit => tauqe_core::workflow::turn::ReviewWorkPhase::Audit,
            wire::ReviewWorkPhase::Decomposition => {
                tauqe_core::workflow::turn::ReviewWorkPhase::Decomposition
            }
            wire::ReviewWorkPhase::PostAnalysis => {
                tauqe_core::workflow::turn::ReviewWorkPhase::PostAnalysis
            }
        }
    }
}

impl Project for tauqe_core::workflow::turn::ModelTurnPlanContext {
    type Target = wire::ModelTurnPlanContext;
    fn project(self) -> Self::Target {
        wire::ModelTurnPlanContext {
            plan_id: self.plan_id,
            step_id: self.step_id,
            batch_step_ids: self.batch_step_ids,
        }
    }
}

impl Project for wire::ModelTurnPlanContext {
    type Target = tauqe_core::workflow::turn::ModelTurnPlanContext;
    fn project(self) -> Self::Target {
        tauqe_core::workflow::turn::ModelTurnPlanContext {
            plan_id: self.plan_id,
            step_id: self.step_id,
            batch_step_ids: self.batch_step_ids,
        }
    }
}

impl Project for tauqe_core::workflow::turn::TurnWorkItem {
    type Target = wire::TurnWorkItem;
    fn project(self) -> Self::Target {
        wire::TurnWorkItem {
            id: self.id,
            title: self.title,
        }
    }
}

impl Project for wire::TurnWorkItem {
    type Target = tauqe_core::workflow::turn::TurnWorkItem;
    fn project(self) -> Self::Target {
        tauqe_core::workflow::turn::TurnWorkItem {
            id: self.id,
            title: self.title,
        }
    }
}

impl Project for tauqe_core::workflow::turn::TurnWorkQueue {
    type Target = wire::TurnWorkQueue;
    fn project(self) -> Self::Target {
        wire::TurnWorkQueue {
            items: self.items.into_iter().map(Project::project).collect(),
            active_index: self.active_index,
        }
    }
}

impl Project for wire::TurnWorkQueue {
    type Target = tauqe_core::workflow::turn::TurnWorkQueue;
    fn project(self) -> Self::Target {
        tauqe_core::workflow::turn::TurnWorkQueue {
            items: self.items.into_iter().map(Project::project).collect(),
            active_index: self.active_index,
        }
    }
}

impl Project for tauqe_core::workflow::turn::TurnWorkContext {
    type Target = wire::TurnWorkContext;
    fn project(self) -> Self::Target {
        match self {
            tauqe_core::workflow::turn::TurnWorkContext::Interactive => {
                wire::TurnWorkContext::Interactive
            }
            tauqe_core::workflow::turn::TurnWorkContext::PlanStep {
                plan_id,
                plan_title,
                queue,
            } => wire::TurnWorkContext::PlanStep {
                plan_id,
                plan_title,
                queue: queue.project(),
            },
            tauqe_core::workflow::turn::TurnWorkContext::ReviewGroupStep {
                review_id,
                review_title,
                group_id,
                group_title,
                queue,
            } => wire::TurnWorkContext::ReviewGroupStep {
                review_id,
                review_title,
                group_id,
                group_title,
                queue: queue.project(),
            },
            tauqe_core::workflow::turn::TurnWorkContext::ReviewStep {
                review_id,
                review_title,
                queue,
            } => wire::TurnWorkContext::ReviewStep {
                review_id,
                review_title,
                queue: queue.project(),
            },
            tauqe_core::workflow::turn::TurnWorkContext::ReviewPhase {
                review_id,
                review_title,
                phase,
                queue,
            } => wire::TurnWorkContext::ReviewPhase {
                review_id,
                review_title,
                phase: phase.project(),
                queue: queue.project(),
            },
        }
    }
}

impl Project for wire::TurnWorkContext {
    type Target = tauqe_core::workflow::turn::TurnWorkContext;
    fn project(self) -> Self::Target {
        match self {
            wire::TurnWorkContext::Interactive => {
                tauqe_core::workflow::turn::TurnWorkContext::Interactive
            }
            wire::TurnWorkContext::PlanStep {
                plan_id,
                plan_title,
                queue,
            } => tauqe_core::workflow::turn::TurnWorkContext::PlanStep {
                plan_id,
                plan_title,
                queue: queue.project(),
            },
            wire::TurnWorkContext::ReviewGroupStep {
                review_id,
                review_title,
                group_id,
                group_title,
                queue,
            } => tauqe_core::workflow::turn::TurnWorkContext::ReviewGroupStep {
                review_id,
                review_title,
                group_id,
                group_title,
                queue: queue.project(),
            },
            wire::TurnWorkContext::ReviewStep {
                review_id,
                review_title,
                queue,
            } => tauqe_core::workflow::turn::TurnWorkContext::ReviewStep {
                review_id,
                review_title,
                queue: queue.project(),
            },
            wire::TurnWorkContext::ReviewPhase {
                review_id,
                review_title,
                phase,
                queue,
            } => tauqe_core::workflow::turn::TurnWorkContext::ReviewPhase {
                review_id,
                review_title,
                phase: phase.project(),
                queue: queue.project(),
            },
        }
    }
}

impl Project for tauqe_core::edits::EditFileOp {
    type Target = wire::EditFileOp;
    fn project(self) -> Self::Target {
        match self {
            tauqe_core::edits::EditFileOp::Replace => wire::EditFileOp::Replace,
            tauqe_core::edits::EditFileOp::Create => wire::EditFileOp::Create,
            tauqe_core::edits::EditFileOp::Delete => wire::EditFileOp::Delete,
            tauqe_core::edits::EditFileOp::Move => wire::EditFileOp::Move,
            tauqe_core::edits::EditFileOp::MoveDir => wire::EditFileOp::MoveDir,
            tauqe_core::edits::EditFileOp::Overwrite => wire::EditFileOp::Overwrite,
        }
    }
}

impl Project for wire::EditFileOp {
    type Target = tauqe_core::edits::EditFileOp;
    fn project(self) -> Self::Target {
        match self {
            wire::EditFileOp::Replace => tauqe_core::edits::EditFileOp::Replace,
            wire::EditFileOp::Create => tauqe_core::edits::EditFileOp::Create,
            wire::EditFileOp::Delete => tauqe_core::edits::EditFileOp::Delete,
            wire::EditFileOp::Move => tauqe_core::edits::EditFileOp::Move,
            wire::EditFileOp::MoveDir => tauqe_core::edits::EditFileOp::MoveDir,
            wire::EditFileOp::Overwrite => tauqe_core::edits::EditFileOp::Overwrite,
        }
    }
}

impl Project for tauqe_core::edits::EditFileStatus {
    type Target = wire::EditFileStatus;
    fn project(self) -> Self::Target {
        match self {
            tauqe_core::edits::EditFileStatus::Ok => wire::EditFileStatus::Ok,
            tauqe_core::edits::EditFileStatus::Error => wire::EditFileStatus::Error,
        }
    }
}

impl Project for wire::EditFileStatus {
    type Target = tauqe_core::edits::EditFileStatus;
    fn project(self) -> Self::Target {
        match self {
            wire::EditFileStatus::Ok => tauqe_core::edits::EditFileStatus::Ok,
            wire::EditFileStatus::Error => tauqe_core::edits::EditFileStatus::Error,
        }
    }
}

impl Project for tauqe_core::edits::VerificationVerdict {
    type Target = wire::VerificationVerdict;
    fn project(self) -> Self::Target {
        match self {
            tauqe_core::edits::VerificationVerdict::Passed => wire::VerificationVerdict::Passed,
            tauqe_core::edits::VerificationVerdict::Failed => wire::VerificationVerdict::Failed,
            tauqe_core::edits::VerificationVerdict::NotConfigured => {
                wire::VerificationVerdict::NotConfigured
            }
            tauqe_core::edits::VerificationVerdict::Cancelled => {
                wire::VerificationVerdict::Cancelled
            }
        }
    }
}

impl Project for wire::VerificationVerdict {
    type Target = tauqe_core::edits::VerificationVerdict;
    fn project(self) -> Self::Target {
        match self {
            wire::VerificationVerdict::Passed => tauqe_core::edits::VerificationVerdict::Passed,
            wire::VerificationVerdict::Failed => tauqe_core::edits::VerificationVerdict::Failed,
            wire::VerificationVerdict::NotConfigured => {
                tauqe_core::edits::VerificationVerdict::NotConfigured
            }
            wire::VerificationVerdict::Cancelled => {
                tauqe_core::edits::VerificationVerdict::Cancelled
            }
        }
    }
}

impl Project for tauqe_core::context::ContextAccess {
    type Target = wire::ContextAccess;
    fn project(self) -> Self::Target {
        match self {
            tauqe_core::context::ContextAccess::ReadOnly => wire::ContextAccess::ReadOnly,
            tauqe_core::context::ContextAccess::Editable => wire::ContextAccess::Editable,
        }
    }
}

impl Project for wire::ContextAccess {
    type Target = tauqe_core::context::ContextAccess;
    fn project(self) -> Self::Target {
        match self {
            wire::ContextAccess::ReadOnly => tauqe_core::context::ContextAccess::ReadOnly,
            wire::ContextAccess::Editable => tauqe_core::context::ContextAccess::Editable,
        }
    }
}

impl Project for tauqe_core::llm::DocTopic {
    type Target = wire::DocTopic;
    fn project(self) -> Self::Target {
        match self {
            tauqe_core::llm::DocTopic::Overview => wire::DocTopic::Overview,
            tauqe_core::llm::DocTopic::Context => wire::DocTopic::Context,
            tauqe_core::llm::DocTopic::Workflow => wire::DocTopic::Workflow,
            tauqe_core::llm::DocTopic::Git => wire::DocTopic::Git,
            tauqe_core::llm::DocTopic::Interface => wire::DocTopic::Interface,
            tauqe_core::llm::DocTopic::Review => wire::DocTopic::Review,
            tauqe_core::llm::DocTopic::Plan => wire::DocTopic::Plan,
            tauqe_core::llm::DocTopic::Config => wire::DocTopic::Config,
            tauqe_core::llm::DocTopic::All => wire::DocTopic::All,
        }
    }
}

impl Project for wire::DocTopic {
    type Target = tauqe_core::llm::DocTopic;
    fn project(self) -> Self::Target {
        match self {
            wire::DocTopic::Overview => tauqe_core::llm::DocTopic::Overview,
            wire::DocTopic::Context => tauqe_core::llm::DocTopic::Context,
            wire::DocTopic::Workflow => tauqe_core::llm::DocTopic::Workflow,
            wire::DocTopic::Git => tauqe_core::llm::DocTopic::Git,
            wire::DocTopic::Interface => tauqe_core::llm::DocTopic::Interface,
            wire::DocTopic::Review => tauqe_core::llm::DocTopic::Review,
            wire::DocTopic::Plan => tauqe_core::llm::DocTopic::Plan,
            wire::DocTopic::Config => tauqe_core::llm::DocTopic::Config,
            wire::DocTopic::All => tauqe_core::llm::DocTopic::All,
        }
    }
}

impl Project for tauqe_core::edits::EditReplace {
    type Target = wire::EditReplace;
    fn project(self) -> Self::Target {
        wire::EditReplace {
            path: self.path,
            old_text: self.old_text,
            new_text: self.new_text,
        }
    }
}

impl Project for wire::EditReplace {
    type Target = tauqe_core::edits::EditReplace;
    fn project(self) -> Self::Target {
        tauqe_core::edits::EditReplace {
            path: self.path,
            old_text: self.old_text,
            new_text: self.new_text,
        }
    }
}

impl Project for tauqe_core::edits::EditCreate {
    type Target = wire::EditCreate;
    fn project(self) -> Self::Target {
        wire::EditCreate {
            path: self.path,
            content: self.content,
        }
    }
}

impl Project for wire::EditCreate {
    type Target = tauqe_core::edits::EditCreate;
    fn project(self) -> Self::Target {
        tauqe_core::edits::EditCreate {
            path: self.path,
            content: self.content,
        }
    }
}

impl Project for tauqe_core::edits::EditDelete {
    type Target = wire::EditDelete;
    fn project(self) -> Self::Target {
        wire::EditDelete { path: self.path }
    }
}

impl Project for wire::EditDelete {
    type Target = tauqe_core::edits::EditDelete;
    fn project(self) -> Self::Target {
        tauqe_core::edits::EditDelete { path: self.path }
    }
}

impl Project for tauqe_core::edits::EditMove {
    type Target = wire::EditMove;
    fn project(self) -> Self::Target {
        wire::EditMove {
            from: self.from,
            to: self.to,
        }
    }
}

impl Project for wire::EditMove {
    type Target = tauqe_core::edits::EditMove;
    fn project(self) -> Self::Target {
        tauqe_core::edits::EditMove {
            from: self.from,
            to: self.to,
        }
    }
}

impl Project for tauqe_core::edits::EditMoveDir {
    type Target = wire::EditMoveDir;
    fn project(self) -> Self::Target {
        wire::EditMoveDir {
            from: self.from,
            to: self.to,
        }
    }
}

impl Project for wire::EditMoveDir {
    type Target = tauqe_core::edits::EditMoveDir;
    fn project(self) -> Self::Target {
        tauqe_core::edits::EditMoveDir {
            from: self.from,
            to: self.to,
        }
    }
}

impl Project for tauqe_core::edits::EditOverwrite {
    type Target = wire::EditOverwrite;
    fn project(self) -> Self::Target {
        wire::EditOverwrite {
            path: self.path,
            content: self.content,
        }
    }
}

impl Project for wire::EditOverwrite {
    type Target = tauqe_core::edits::EditOverwrite;
    fn project(self) -> Self::Target {
        tauqe_core::edits::EditOverwrite {
            path: self.path,
            content: self.content,
        }
    }
}

impl Project for tauqe_core::edits::EditLineAnchor {
    type Target = wire::EditLineAnchor;
    fn project(self) -> Self::Target {
        wire::EditLineAnchor {
            path: self.path,
            start: self.start,
            end: self.end,
            content: self.content,
        }
    }
}

impl Project for wire::EditLineAnchor {
    type Target = tauqe_core::edits::EditLineAnchor;
    fn project(self) -> Self::Target {
        tauqe_core::edits::EditLineAnchor {
            path: self.path,
            start: self.start,
            end: self.end,
            content: self.content,
        }
    }
}

impl Project for tauqe_core::edits::EditOperation {
    type Target = wire::EditOperation;
    fn project(self) -> Self::Target {
        match self {
            tauqe_core::edits::EditOperation::Replace(value) => {
                wire::EditOperation::Replace(value.project())
            }
            tauqe_core::edits::EditOperation::Create(value) => {
                wire::EditOperation::Create(value.project())
            }
            tauqe_core::edits::EditOperation::Delete(value) => {
                wire::EditOperation::Delete(value.project())
            }
            tauqe_core::edits::EditOperation::Move(value) => {
                wire::EditOperation::Move(value.project())
            }
            tauqe_core::edits::EditOperation::MoveDir(value) => {
                wire::EditOperation::MoveDir(value.project())
            }
            tauqe_core::edits::EditOperation::Overwrite(value) => {
                wire::EditOperation::Overwrite(value.project())
            }
            tauqe_core::edits::EditOperation::LineAnchor(value) => {
                wire::EditOperation::LineAnchor(value.project())
            }
        }
    }
}

impl Project for wire::EditOperation {
    type Target = tauqe_core::edits::EditOperation;
    fn project(self) -> Self::Target {
        match self {
            wire::EditOperation::Replace(value) => {
                tauqe_core::edits::EditOperation::Replace(value.project())
            }
            wire::EditOperation::Create(value) => {
                tauqe_core::edits::EditOperation::Create(value.project())
            }
            wire::EditOperation::Delete(value) => {
                tauqe_core::edits::EditOperation::Delete(value.project())
            }
            wire::EditOperation::Move(value) => {
                tauqe_core::edits::EditOperation::Move(value.project())
            }
            wire::EditOperation::MoveDir(value) => {
                tauqe_core::edits::EditOperation::MoveDir(value.project())
            }
            wire::EditOperation::Overwrite(value) => {
                tauqe_core::edits::EditOperation::Overwrite(value.project())
            }
            wire::EditOperation::LineAnchor(value) => {
                tauqe_core::edits::EditOperation::LineAnchor(value.project())
            }
        }
    }
}

impl Project for tauqe_core::edits::EditOutcome {
    type Target = wire::EditOutcome;
    fn project(self) -> Self::Target {
        wire::EditOutcome {
            applied: self.applied,
            error: self.error,
            changed_files: self.changed_files,
            commit_hash: self.commit_hash,
            verification: self.verification.map(Project::project),
        }
    }
}

impl Project for wire::EditOutcome {
    type Target = tauqe_core::edits::EditOutcome;
    fn project(self) -> Self::Target {
        tauqe_core::edits::EditOutcome {
            applied: self.applied,
            error: self.error,
            changed_files: self.changed_files,
            commit_hash: self.commit_hash,
            verification: self.verification.map(Project::project),
        }
    }
}

impl Project for tauqe_core::edits::StructuredChangeProposal {
    type Target = wire::StructuredChangeProposal;
    fn project(self) -> Self::Target {
        wire::StructuredChangeProposal {
            op: self.op,
            path: self.path,
            to: self.to,
            old_text: self.old_text,
            new_text: self.new_text,
            start: self.start,
            end: self.end,
            content: self.content,
        }
    }
}

impl Project for wire::StructuredChangeProposal {
    type Target = tauqe_core::edits::StructuredChangeProposal;
    fn project(self) -> Self::Target {
        tauqe_core::edits::StructuredChangeProposal {
            op: self.op,
            path: self.path,
            to: self.to,
            old_text: self.old_text,
            new_text: self.new_text,
            start: self.start,
            end: self.end,
            content: self.content,
        }
    }
}

impl Project for tauqe_core::llm::StructuredOutcomeProposal {
    type Target = wire::StructuredOutcomeProposal;
    fn project(self) -> Self::Target {
        wire::StructuredOutcomeProposal {
            status: self.status,
            commit_message: self.commit_message,
            message: self.message,
            reason: self.reason,
        }
    }
}

impl Project for wire::StructuredOutcomeProposal {
    type Target = tauqe_core::llm::StructuredOutcomeProposal;
    fn project(self) -> Self::Target {
        tauqe_core::llm::StructuredOutcomeProposal {
            status: self.status,
            commit_message: self.commit_message,
            message: self.message,
            reason: self.reason,
        }
    }
}

impl Project for tauqe_core::llm::StructuredBlockedProposal {
    type Target = wire::StructuredBlockedProposal;
    fn project(self) -> Self::Target {
        wire::StructuredBlockedProposal {
            is_blocked: self.is_blocked,
            reason: self.reason,
            details: self.details,
        }
    }
}

impl Project for wire::StructuredBlockedProposal {
    type Target = tauqe_core::llm::StructuredBlockedProposal;
    fn project(self) -> Self::Target {
        tauqe_core::llm::StructuredBlockedProposal {
            is_blocked: self.is_blocked,
            reason: self.reason,
            details: self.details,
        }
    }
}

impl Project for tauqe_core::llm::StructuredAbortProposal {
    type Target = wire::StructuredAbortProposal;
    fn project(self) -> Self::Target {
        wire::StructuredAbortProposal {
            is_abort: self.is_abort,
            reason: self.reason,
            recommendation: self.recommendation,
        }
    }
}

impl Project for wire::StructuredAbortProposal {
    type Target = tauqe_core::llm::StructuredAbortProposal;
    fn project(self) -> Self::Target {
        tauqe_core::llm::StructuredAbortProposal {
            is_abort: self.is_abort,
            reason: self.reason,
            recommendation: self.recommendation,
        }
    }
}

impl Project for tauqe_core::llm::discussion::RepoSearchRequest {
    type Target = wire::RepoSearchRequest;
    fn project(self) -> Self::Target {
        wire::RepoSearchRequest {
            path: self.path,
            regex: self.regex,
        }
    }
}

impl Project for wire::RepoSearchRequest {
    type Target = tauqe_core::llm::discussion::RepoSearchRequest;
    fn project(self) -> Self::Target {
        tauqe_core::llm::discussion::RepoSearchRequest {
            path: self.path,
            regex: self.regex,
        }
    }
}

impl Project for tauqe_core::llm::discussion::DiscussionContextRequest {
    type Target = wire::DiscussionContextRequest;
    fn project(self) -> Self::Target {
        wire::DiscussionContextRequest {
            path: self.path,
            access: self.access.project(),
        }
    }
}

impl Project for wire::DiscussionContextRequest {
    type Target = tauqe_core::llm::discussion::DiscussionContextRequest;
    fn project(self) -> Self::Target {
        tauqe_core::llm::discussion::DiscussionContextRequest {
            path: self.path,
            access: self.access.project(),
        }
    }
}

impl Project for tauqe_core::llm::ModelResultProposal {
    type Target = wire::ModelResultProposal;
    fn project(self) -> Self::Target {
        wire::ModelResultProposal {
            message: self.message,
            scratchpad: self.scratchpad,
            context_drops: self.context_drops,
            suggested_actions: self.suggested_actions,
            changes: self.changes.into_iter().map(Project::project).collect(),
            context_requests: self
                .context_requests
                .into_iter()
                .map(Project::project)
                .collect(),
            repo_searches: self
                .repo_searches
                .into_iter()
                .map(Project::project)
                .collect(),
            doc_requests: self
                .doc_requests
                .into_iter()
                .map(Project::project)
                .collect(),
            outcome: self.outcome.map(Project::project),
            blocked: self.blocked.map(Project::project),
            abort: self.abort.map(Project::project),
        }
    }
}

impl Project for wire::ModelResultProposal {
    type Target = tauqe_core::llm::ModelResultProposal;
    fn project(self) -> Self::Target {
        tauqe_core::llm::ModelResultProposal {
            message: self.message,
            scratchpad: self.scratchpad,
            context_drops: self.context_drops,
            suggested_actions: self.suggested_actions,
            changes: self.changes.into_iter().map(Project::project).collect(),
            context_requests: self
                .context_requests
                .into_iter()
                .map(Project::project)
                .collect(),
            repo_searches: self
                .repo_searches
                .into_iter()
                .map(Project::project)
                .collect(),
            doc_requests: self
                .doc_requests
                .into_iter()
                .map(Project::project)
                .collect(),
            outcome: self.outcome.map(Project::project),
            blocked: self.blocked.map(Project::project),
            abort: self.abort.map(Project::project),
        }
    }
}

impl Project for tauqe_core::discussion::DiscussionTarget {
    type Target = wire::DiscussionTarget;
    fn project(self) -> Self::Target {
        match self {
            tauqe_core::discussion::DiscussionTarget::Plan { id } => {
                wire::DiscussionTarget::Plan { id }
            }
            tauqe_core::discussion::DiscussionTarget::Review { id } => {
                wire::DiscussionTarget::Review { id }
            }
            tauqe_core::discussion::DiscussionTarget::ReviewGroup {
                session_id,
                group_id,
            } => wire::DiscussionTarget::ReviewGroup {
                session_id,
                group_id,
            },
        }
    }
}

impl Project for wire::DiscussionTarget {
    type Target = tauqe_core::discussion::DiscussionTarget;
    fn project(self) -> Self::Target {
        match self {
            wire::DiscussionTarget::Plan { id } => {
                tauqe_core::discussion::DiscussionTarget::Plan { id }
            }
            wire::DiscussionTarget::Review { id } => {
                tauqe_core::discussion::DiscussionTarget::Review { id }
            }
            wire::DiscussionTarget::ReviewGroup {
                session_id,
                group_id,
            } => tauqe_core::discussion::DiscussionTarget::ReviewGroup {
                session_id,
                group_id,
            },
        }
    }
}
