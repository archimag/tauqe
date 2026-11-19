//! Exhaustive projections between owned domain documents and RPC DTOs.
//! Rust's orphan rule requires a server-owned trait for these foreign types.
pub(crate) trait Project: Sized {
    type Target;
    fn project(self) -> Self::Target;
}
use tauqe_flow::api::types as domain;
use tauqe_protocol as wire;
impl Project for domain::ProviderKind {
    type Target = wire::ProviderKind;
    fn project(self) -> Self::Target {
        let value = self;
        match value {
            domain::ProviderKind::OpenRouter => wire::ProviderKind::OpenRouter,
            domain::ProviderKind::OpenAI => wire::ProviderKind::OpenAI,
        }
    }
}
impl Project for wire::ProviderKind {
    type Target = domain::ProviderKind;
    fn project(self) -> Self::Target {
        let value = self;
        match value {
            wire::ProviderKind::OpenRouter => domain::ProviderKind::OpenRouter,
            wire::ProviderKind::OpenAI => domain::ProviderKind::OpenAI,
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

impl Project for domain::PlanItemStatus {
    type Target = wire::PlanItemStatus;
    fn project(self) -> Self::Target {
        let value = self;
        match value {
            domain::PlanItemStatus::Discussion => wire::PlanItemStatus::Discussion,
            domain::PlanItemStatus::Todo => wire::PlanItemStatus::Todo,
            domain::PlanItemStatus::InProgress => wire::PlanItemStatus::InProgress,
            domain::PlanItemStatus::Done => wire::PlanItemStatus::Done,
            domain::PlanItemStatus::Blocked => wire::PlanItemStatus::Blocked,
            domain::PlanItemStatus::Cancelled => wire::PlanItemStatus::Cancelled,
        }
    }
}
impl Project for wire::PlanItemStatus {
    type Target = domain::PlanItemStatus;
    fn project(self) -> Self::Target {
        let value = self;
        match value {
            wire::PlanItemStatus::Discussion => domain::PlanItemStatus::Discussion,
            wire::PlanItemStatus::Todo => domain::PlanItemStatus::Todo,
            wire::PlanItemStatus::InProgress => domain::PlanItemStatus::InProgress,
            wire::PlanItemStatus::Done => domain::PlanItemStatus::Done,
            wire::PlanItemStatus::Blocked => domain::PlanItemStatus::Blocked,
            wire::PlanItemStatus::Cancelled => domain::PlanItemStatus::Cancelled,
        }
    }
}
impl Project for domain::PlanRejectionKind {
    type Target = wire::PlanRejectionKind;
    fn project(self) -> Self::Target {
        let value = self;
        match value {
            domain::PlanRejectionKind::PayloadDefect => wire::PlanRejectionKind::PayloadDefect,
            domain::PlanRejectionKind::ModeForbidden => wire::PlanRejectionKind::ModeForbidden,
            domain::PlanRejectionKind::Abandoned => wire::PlanRejectionKind::Abandoned,
        }
    }
}
impl Project for wire::PlanRejectionKind {
    type Target = domain::PlanRejectionKind;
    fn project(self) -> Self::Target {
        let value = self;
        match value {
            wire::PlanRejectionKind::PayloadDefect => domain::PlanRejectionKind::PayloadDefect,
            wire::PlanRejectionKind::ModeForbidden => domain::PlanRejectionKind::ModeForbidden,
            wire::PlanRejectionKind::Abandoned => domain::PlanRejectionKind::Abandoned,
        }
    }
}
impl Project for domain::IssueType {
    type Target = wire::IssueType;
    fn project(self) -> Self::Target {
        let value = self;
        match value {
            domain::IssueType::Bug => wire::IssueType::Bug,
            domain::IssueType::Security => wire::IssueType::Security,
            domain::IssueType::Architecture => wire::IssueType::Architecture,
            domain::IssueType::Performance => wire::IssueType::Performance,
            domain::IssueType::Quality => wire::IssueType::Quality,
        }
    }
}
impl Project for wire::IssueType {
    type Target = domain::IssueType;
    fn project(self) -> Self::Target {
        let value = self;
        match value {
            wire::IssueType::Bug => domain::IssueType::Bug,
            wire::IssueType::Security => domain::IssueType::Security,
            wire::IssueType::Architecture => domain::IssueType::Architecture,
            wire::IssueType::Performance => domain::IssueType::Performance,
            wire::IssueType::Quality => domain::IssueType::Quality,
        }
    }
}
impl Project for domain::IssueStatus {
    type Target = wire::IssueStatus;
    fn project(self) -> Self::Target {
        let value = self;
        match value {
            domain::IssueStatus::Discussion => wire::IssueStatus::Discussion,
            domain::IssueStatus::Todo => wire::IssueStatus::Todo,
            domain::IssueStatus::InProgress => wire::IssueStatus::InProgress,
            domain::IssueStatus::Fixed => wire::IssueStatus::Fixed,
            domain::IssueStatus::Skipped => wire::IssueStatus::Skipped,
            domain::IssueStatus::Rejected => wire::IssueStatus::Rejected,
        }
    }
}
impl Project for wire::IssueStatus {
    type Target = domain::IssueStatus;
    fn project(self) -> Self::Target {
        let value = self;
        match value {
            wire::IssueStatus::Discussion => domain::IssueStatus::Discussion,
            wire::IssueStatus::Todo => domain::IssueStatus::Todo,
            wire::IssueStatus::InProgress => domain::IssueStatus::InProgress,
            wire::IssueStatus::Fixed => domain::IssueStatus::Fixed,
            wire::IssueStatus::Skipped => domain::IssueStatus::Skipped,
            wire::IssueStatus::Rejected => domain::IssueStatus::Rejected,
        }
    }
}
impl Project for domain::ReviewSeverity {
    type Target = wire::ReviewSeverity;
    fn project(self) -> Self::Target {
        let value = self;
        match value {
            domain::ReviewSeverity::Critical => wire::ReviewSeverity::Critical,
            domain::ReviewSeverity::Warning => wire::ReviewSeverity::Warning,
            domain::ReviewSeverity::Suggestion => wire::ReviewSeverity::Suggestion,
            domain::ReviewSeverity::Info => wire::ReviewSeverity::Info,
        }
    }
}
impl Project for wire::ReviewSeverity {
    type Target = domain::ReviewSeverity;
    fn project(self) -> Self::Target {
        let value = self;
        match value {
            wire::ReviewSeverity::Critical => domain::ReviewSeverity::Critical,
            wire::ReviewSeverity::Warning => domain::ReviewSeverity::Warning,
            wire::ReviewSeverity::Suggestion => domain::ReviewSeverity::Suggestion,
            wire::ReviewSeverity::Info => domain::ReviewSeverity::Info,
        }
    }
}
impl Project for domain::ReviewGroupKind {
    type Target = wire::ReviewGroupKind;
    fn project(self) -> Self::Target {
        let value = self;
        match value {
            domain::ReviewGroupKind::Simple => wire::ReviewGroupKind::Simple,
            domain::ReviewGroupKind::Architectural => wire::ReviewGroupKind::Architectural,
        }
    }
}
impl Project for wire::ReviewGroupKind {
    type Target = domain::ReviewGroupKind;
    fn project(self) -> Self::Target {
        let value = self;
        match value {
            wire::ReviewGroupKind::Simple => domain::ReviewGroupKind::Simple,
            wire::ReviewGroupKind::Architectural => domain::ReviewGroupKind::Architectural,
        }
    }
}
impl Project for domain::PostAnalysisMode {
    type Target = wire::PostAnalysisMode;
    fn project(self) -> Self::Target {
        let value = self;
        match value {
            domain::PostAnalysisMode::Automatic => wire::PostAnalysisMode::Automatic,
            domain::PostAnalysisMode::Manual => wire::PostAnalysisMode::Manual,
            domain::PostAnalysisMode::None => wire::PostAnalysisMode::None,
        }
    }
}
impl Project for wire::PostAnalysisMode {
    type Target = domain::PostAnalysisMode;
    fn project(self) -> Self::Target {
        let value = self;
        match value {
            wire::PostAnalysisMode::Automatic => domain::PostAnalysisMode::Automatic,
            wire::PostAnalysisMode::Manual => domain::PostAnalysisMode::Manual,
            wire::PostAnalysisMode::None => domain::PostAnalysisMode::None,
        }
    }
}
impl Project for domain::Plan {
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
    type Target = domain::Plan;
    fn project(self) -> Self::Target {
        let value = self;
        domain::Plan {
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
impl Project for domain::PlanSummary {
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
    type Target = domain::PlanSummary;
    fn project(self) -> Self::Target {
        let value = self;
        domain::PlanSummary {
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
impl Project for domain::PlanItem {
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
    type Target = domain::PlanItem;
    fn project(self) -> Self::Target {
        let value = self;
        domain::PlanItem {
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
impl Project for domain::ConceptItem {
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
    type Target = domain::ConceptItem;
    fn project(self) -> Self::Target {
        let value = self;
        domain::ConceptItem {
            id: value.id,
            title: value.title,
            details: value.details,
            children: value.children.into_iter().map(Project::project).collect(),
        }
    }
}
impl Project for domain::PlanImplementation {
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
    type Target = domain::PlanImplementation;
    fn project(self) -> Self::Target {
        let value = self;
        domain::PlanImplementation {
            items: value.items.into_iter().map(Project::project).collect(),
            architect: value.architect.map(Project::project),
            last_replanned_step: value.last_replanned_step,
        }
    }
}
impl Project for domain::PlanRejection {
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
    type Target = domain::PlanRejection;
    fn project(self) -> Self::Target {
        let value = self;
        domain::PlanRejection {
            kind: value.kind.project(),
            message: value.message,
        }
    }
}
impl Project for domain::ReviewIssue {
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
    type Target = domain::ReviewIssue;
    fn project(self) -> Self::Target {
        let value = self;
        domain::ReviewIssue {
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
impl Project for domain::ReviewItem {
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
    type Target = domain::ReviewItem;
    fn project(self) -> Self::Target {
        let value = self;
        domain::ReviewItem {
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
impl Project for domain::ReviewGroup {
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
    type Target = domain::ReviewGroup;
    fn project(self) -> Self::Target {
        let value = self;
        domain::ReviewGroup {
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
impl Project for domain::ArchitecturalIssueGroup {
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
    type Target = domain::ArchitecturalIssueGroup;
    fn project(self) -> Self::Target {
        let value = self;
        domain::ArchitecturalIssueGroup {
            id: value.id,
            title: value.title,
            description: value.description,
            issue_ids: value.issue_ids,
            implementation: value.implementation.project(),
        }
    }
}
impl Project for domain::ReviewPostAnalysis {
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
    type Target = domain::ReviewPostAnalysis;
    fn project(self) -> Self::Target {
        let value = self;
        domain::ReviewPostAnalysis {
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
impl Project for domain::ReviewLaunchConfig {
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
    type Target = domain::ReviewLaunchConfig;
    fn project(self) -> Self::Target {
        let value = self;
        domain::ReviewLaunchConfig {
            preset: value.preset,
            mode: value.mode.project(),
            groups: value.groups.into_iter().map(Project::project).collect(),
        }
    }
}
impl Project for domain::ReviewSession {
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
    type Target = domain::ReviewSession;
    fn project(self) -> Self::Target {
        let value = self;
        domain::ReviewSession {
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
impl Project for domain::ReviewSessionTarget {
    type Target = wire::ReviewSessionTarget;
    fn project(self) -> Self::Target {
        let value = self;
        wire::ReviewSessionTarget {
            review_id: value.review_id,
        }
    }
}
impl Project for wire::ReviewSessionTarget {
    type Target = domain::ReviewSessionTarget;
    fn project(self) -> Self::Target {
        let value = self;
        domain::ReviewSessionTarget {
            review_id: value.review_id,
        }
    }
}
impl Project for domain::ReviewFindingTarget {
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
    type Target = domain::ReviewFindingTarget;
    fn project(self) -> Self::Target {
        let value = self;
        domain::ReviewFindingTarget {
            review_id: value.review_id,
            item_id: value.item_id,
        }
    }
}
impl Project for domain::ReviewFindingsTarget {
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
    type Target = domain::ReviewFindingsTarget;
    fn project(self) -> Self::Target {
        let value = self;
        domain::ReviewFindingsTarget {
            review_id: value.review_id,
            item_ids: value.item_ids,
        }
    }
}
impl Project for domain::ReviewGroupTarget {
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
    type Target = domain::ReviewGroupTarget;
    fn project(self) -> Self::Target {
        let value = self;
        domain::ReviewGroupTarget {
            review_id: value.review_id,
            group_id: value.group_id,
        }
    }
}
impl Project for domain::ReviewExecuteTarget {
    type Target = wire::ReviewExecuteTarget;
    fn project(self) -> Self::Target {
        let value = self;
        match value {
            domain::ReviewExecuteTarget::AllSessions => wire::ReviewExecuteTarget::AllSessions,
            domain::ReviewExecuteTarget::Session(target) => {
                wire::ReviewExecuteTarget::Session(target.project())
            }
            domain::ReviewExecuteTarget::Finding(target) => {
                wire::ReviewExecuteTarget::Finding(target.project())
            }
            domain::ReviewExecuteTarget::Findings(target) => {
                wire::ReviewExecuteTarget::Findings(target.project())
            }
            domain::ReviewExecuteTarget::Group(target) => {
                wire::ReviewExecuteTarget::Group(target.project())
            }
        }
    }
}
impl Project for wire::ReviewExecuteTarget {
    type Target = domain::ReviewExecuteTarget;
    fn project(self) -> Self::Target {
        let value = self;
        match value {
            wire::ReviewExecuteTarget::AllSessions => domain::ReviewExecuteTarget::AllSessions,
            wire::ReviewExecuteTarget::Session(target) => {
                domain::ReviewExecuteTarget::Session(target.project())
            }
            wire::ReviewExecuteTarget::Finding(target) => {
                domain::ReviewExecuteTarget::Finding(target.project())
            }
            wire::ReviewExecuteTarget::Findings(target) => {
                domain::ReviewExecuteTarget::Findings(target.project())
            }
            wire::ReviewExecuteTarget::Group(target) => {
                domain::ReviewExecuteTarget::Group(target.project())
            }
        }
    }
}
impl Project for domain::PlanSynthesisResult {
    type Target = wire::PlanRefineResult;
    fn project(self) -> Self::Target {
        let value = self;
        match value {
            domain::PlanSynthesisResult::Success { plan, explanation } => {
                wire::PlanRefineResult::Success {
                    plan: Box::new(plan.project()),
                    explanation,
                }
            }
            domain::PlanSynthesisResult::Blocked { reason } => {
                wire::PlanRefineResult::Blocked { reason }
            }
        }
    }
}

impl Project for domain::WorkflowKind {
    type Target = wire::WorkflowKind;
    fn project(self) -> Self::Target {
        match self {
            domain::WorkflowKind::Git => wire::WorkflowKind::Git,
            domain::WorkflowKind::Naive => wire::WorkflowKind::Naive,
        }
    }
}

impl Project for wire::WorkflowKind {
    type Target = domain::WorkflowKind;
    fn project(self) -> Self::Target {
        match self {
            wire::WorkflowKind::Git => domain::WorkflowKind::Git,
            wire::WorkflowKind::Naive => domain::WorkflowKind::Naive,
        }
    }
}

impl Project for domain::EditProtocolKind {
    type Target = wire::EditProtocolKind;
    fn project(self) -> Self::Target {
        match self {
            domain::EditProtocolKind::Structured => wire::EditProtocolKind::Structured,
        }
    }
}

impl Project for wire::EditProtocolKind {
    type Target = domain::EditProtocolKind;
    fn project(self) -> Self::Target {
        match self {
            wire::EditProtocolKind::Structured => domain::EditProtocolKind::Structured,
        }
    }
}

impl Project for domain::TurnPhase {
    type Target = wire::TurnPhase;
    fn project(self) -> Self::Target {
        match self {
            domain::TurnPhase::Research => wire::TurnPhase::Research,
            domain::TurnPhase::Proposal => wire::TurnPhase::Proposal,
            domain::TurnPhase::Staging => wire::TurnPhase::Staging,
            domain::TurnPhase::Verification => wire::TurnPhase::Verification,
            domain::TurnPhase::Healing => wire::TurnPhase::Healing,
        }
    }
}

impl Project for wire::TurnPhase {
    type Target = domain::TurnPhase;
    fn project(self) -> Self::Target {
        match self {
            wire::TurnPhase::Research => domain::TurnPhase::Research,
            wire::TurnPhase::Proposal => domain::TurnPhase::Proposal,
            wire::TurnPhase::Staging => domain::TurnPhase::Staging,
            wire::TurnPhase::Verification => domain::TurnPhase::Verification,
            wire::TurnPhase::Healing => domain::TurnPhase::Healing,
        }
    }
}

impl Project for domain::ReviewWorkPhase {
    type Target = wire::ReviewWorkPhase;
    fn project(self) -> Self::Target {
        match self {
            domain::ReviewWorkPhase::Audit => wire::ReviewWorkPhase::Audit,
            domain::ReviewWorkPhase::Decomposition => wire::ReviewWorkPhase::Decomposition,
            domain::ReviewWorkPhase::PostAnalysis => wire::ReviewWorkPhase::PostAnalysis,
        }
    }
}

impl Project for wire::ReviewWorkPhase {
    type Target = domain::ReviewWorkPhase;
    fn project(self) -> Self::Target {
        match self {
            wire::ReviewWorkPhase::Audit => domain::ReviewWorkPhase::Audit,
            wire::ReviewWorkPhase::Decomposition => domain::ReviewWorkPhase::Decomposition,
            wire::ReviewWorkPhase::PostAnalysis => domain::ReviewWorkPhase::PostAnalysis,
        }
    }
}

impl Project for domain::ModelTurnPlanContext {
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
    type Target = domain::ModelTurnPlanContext;
    fn project(self) -> Self::Target {
        domain::ModelTurnPlanContext {
            plan_id: self.plan_id,
            step_id: self.step_id,
            batch_step_ids: self.batch_step_ids,
        }
    }
}

impl Project for domain::TurnWorkItem {
    type Target = wire::TurnWorkItem;
    fn project(self) -> Self::Target {
        wire::TurnWorkItem {
            id: self.id,
            title: self.title,
        }
    }
}

impl Project for wire::TurnWorkItem {
    type Target = domain::TurnWorkItem;
    fn project(self) -> Self::Target {
        domain::TurnWorkItem {
            id: self.id,
            title: self.title,
        }
    }
}

impl Project for domain::TurnWorkQueue {
    type Target = wire::TurnWorkQueue;
    fn project(self) -> Self::Target {
        wire::TurnWorkQueue {
            items: self.items.into_iter().map(Project::project).collect(),
            active_index: self.active_index,
        }
    }
}

impl Project for wire::TurnWorkQueue {
    type Target = domain::TurnWorkQueue;
    fn project(self) -> Self::Target {
        domain::TurnWorkQueue {
            items: self.items.into_iter().map(Project::project).collect(),
            active_index: self.active_index,
        }
    }
}

impl Project for domain::TurnWorkContext {
    type Target = wire::TurnWorkContext;
    fn project(self) -> Self::Target {
        match self {
            domain::TurnWorkContext::Interactive => wire::TurnWorkContext::Interactive,
            domain::TurnWorkContext::PlanStep {
                plan_id,
                plan_title,
                queue,
            } => wire::TurnWorkContext::PlanStep {
                plan_id,
                plan_title,
                queue: queue.project(),
            },
            domain::TurnWorkContext::ReviewGroupStep {
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
            domain::TurnWorkContext::ReviewStep {
                review_id,
                review_title,
                queue,
            } => wire::TurnWorkContext::ReviewStep {
                review_id,
                review_title,
                queue: queue.project(),
            },
            domain::TurnWorkContext::ReviewPhase {
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
    type Target = domain::TurnWorkContext;
    fn project(self) -> Self::Target {
        match self {
            wire::TurnWorkContext::Interactive => domain::TurnWorkContext::Interactive,
            wire::TurnWorkContext::PlanStep {
                plan_id,
                plan_title,
                queue,
            } => domain::TurnWorkContext::PlanStep {
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
            } => domain::TurnWorkContext::ReviewGroupStep {
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
            } => domain::TurnWorkContext::ReviewStep {
                review_id,
                review_title,
                queue: queue.project(),
            },
            wire::TurnWorkContext::ReviewPhase {
                review_id,
                review_title,
                phase,
                queue,
            } => domain::TurnWorkContext::ReviewPhase {
                review_id,
                review_title,
                phase: phase.project(),
                queue: queue.project(),
            },
        }
    }
}

impl Project for domain::EditFileOp {
    type Target = wire::EditFileOp;
    fn project(self) -> Self::Target {
        match self {
            domain::EditFileOp::Replace => wire::EditFileOp::Replace,
            domain::EditFileOp::Create => wire::EditFileOp::Create,
            domain::EditFileOp::Delete => wire::EditFileOp::Delete,
            domain::EditFileOp::Move => wire::EditFileOp::Move,
            domain::EditFileOp::MoveDir => wire::EditFileOp::MoveDir,
            domain::EditFileOp::Overwrite => wire::EditFileOp::Overwrite,
        }
    }
}

impl Project for wire::EditFileOp {
    type Target = domain::EditFileOp;
    fn project(self) -> Self::Target {
        match self {
            wire::EditFileOp::Replace => domain::EditFileOp::Replace,
            wire::EditFileOp::Create => domain::EditFileOp::Create,
            wire::EditFileOp::Delete => domain::EditFileOp::Delete,
            wire::EditFileOp::Move => domain::EditFileOp::Move,
            wire::EditFileOp::MoveDir => domain::EditFileOp::MoveDir,
            wire::EditFileOp::Overwrite => domain::EditFileOp::Overwrite,
        }
    }
}

impl Project for domain::EditFileStatus {
    type Target = wire::EditFileStatus;
    fn project(self) -> Self::Target {
        match self {
            domain::EditFileStatus::Ok => wire::EditFileStatus::Ok,
            domain::EditFileStatus::Error => wire::EditFileStatus::Error,
        }
    }
}

impl Project for wire::EditFileStatus {
    type Target = domain::EditFileStatus;
    fn project(self) -> Self::Target {
        match self {
            wire::EditFileStatus::Ok => domain::EditFileStatus::Ok,
            wire::EditFileStatus::Error => domain::EditFileStatus::Error,
        }
    }
}

impl Project for domain::VerificationVerdict {
    type Target = wire::VerificationVerdict;
    fn project(self) -> Self::Target {
        match self {
            domain::VerificationVerdict::Passed => wire::VerificationVerdict::Passed,
            domain::VerificationVerdict::Failed => wire::VerificationVerdict::Failed,
            domain::VerificationVerdict::NotConfigured => wire::VerificationVerdict::NotConfigured,
            domain::VerificationVerdict::Cancelled => wire::VerificationVerdict::Cancelled,
        }
    }
}

impl Project for wire::VerificationVerdict {
    type Target = domain::VerificationVerdict;
    fn project(self) -> Self::Target {
        match self {
            wire::VerificationVerdict::Passed => domain::VerificationVerdict::Passed,
            wire::VerificationVerdict::Failed => domain::VerificationVerdict::Failed,
            wire::VerificationVerdict::NotConfigured => domain::VerificationVerdict::NotConfigured,
            wire::VerificationVerdict::Cancelled => domain::VerificationVerdict::Cancelled,
        }
    }
}

impl Project for domain::ContextAccess {
    type Target = wire::ContextAccess;
    fn project(self) -> Self::Target {
        match self {
            domain::ContextAccess::ReadOnly => wire::ContextAccess::ReadOnly,
            domain::ContextAccess::Editable => wire::ContextAccess::Editable,
        }
    }
}

impl Project for wire::ContextAccess {
    type Target = domain::ContextAccess;
    fn project(self) -> Self::Target {
        match self {
            wire::ContextAccess::ReadOnly => domain::ContextAccess::ReadOnly,
            wire::ContextAccess::Editable => domain::ContextAccess::Editable,
        }
    }
}

impl Project for domain::DocTopic {
    type Target = wire::DocTopic;
    fn project(self) -> Self::Target {
        match self {
            domain::DocTopic::Overview => wire::DocTopic::Overview,
            domain::DocTopic::Context => wire::DocTopic::Context,
            domain::DocTopic::Workflow => wire::DocTopic::Workflow,
            domain::DocTopic::Git => wire::DocTopic::Git,
            domain::DocTopic::Interface => wire::DocTopic::Interface,
            domain::DocTopic::Review => wire::DocTopic::Review,
            domain::DocTopic::Plan => wire::DocTopic::Plan,
            domain::DocTopic::Config => wire::DocTopic::Config,
            domain::DocTopic::All => wire::DocTopic::All,
        }
    }
}

impl Project for wire::DocTopic {
    type Target = domain::DocTopic;
    fn project(self) -> Self::Target {
        match self {
            wire::DocTopic::Overview => domain::DocTopic::Overview,
            wire::DocTopic::Context => domain::DocTopic::Context,
            wire::DocTopic::Workflow => domain::DocTopic::Workflow,
            wire::DocTopic::Git => domain::DocTopic::Git,
            wire::DocTopic::Interface => domain::DocTopic::Interface,
            wire::DocTopic::Review => domain::DocTopic::Review,
            wire::DocTopic::Plan => domain::DocTopic::Plan,
            wire::DocTopic::Config => domain::DocTopic::Config,
            wire::DocTopic::All => domain::DocTopic::All,
        }
    }
}

impl Project for domain::EditReplace {
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
    type Target = domain::EditReplace;
    fn project(self) -> Self::Target {
        domain::EditReplace {
            path: self.path,
            old_text: self.old_text,
            new_text: self.new_text,
        }
    }
}

impl Project for domain::EditCreate {
    type Target = wire::EditCreate;
    fn project(self) -> Self::Target {
        wire::EditCreate {
            path: self.path,
            content: self.content,
        }
    }
}

impl Project for wire::EditCreate {
    type Target = domain::EditCreate;
    fn project(self) -> Self::Target {
        domain::EditCreate {
            path: self.path,
            content: self.content,
        }
    }
}

impl Project for domain::EditDelete {
    type Target = wire::EditDelete;
    fn project(self) -> Self::Target {
        wire::EditDelete { path: self.path }
    }
}

impl Project for wire::EditDelete {
    type Target = domain::EditDelete;
    fn project(self) -> Self::Target {
        domain::EditDelete { path: self.path }
    }
}

impl Project for domain::EditMove {
    type Target = wire::EditMove;
    fn project(self) -> Self::Target {
        wire::EditMove {
            from: self.from,
            to: self.to,
        }
    }
}

impl Project for wire::EditMove {
    type Target = domain::EditMove;
    fn project(self) -> Self::Target {
        domain::EditMove {
            from: self.from,
            to: self.to,
        }
    }
}

impl Project for domain::EditMoveDir {
    type Target = wire::EditMoveDir;
    fn project(self) -> Self::Target {
        wire::EditMoveDir {
            from: self.from,
            to: self.to,
        }
    }
}

impl Project for wire::EditMoveDir {
    type Target = domain::EditMoveDir;
    fn project(self) -> Self::Target {
        domain::EditMoveDir {
            from: self.from,
            to: self.to,
        }
    }
}

impl Project for domain::EditOverwrite {
    type Target = wire::EditOverwrite;
    fn project(self) -> Self::Target {
        wire::EditOverwrite {
            path: self.path,
            content: self.content,
        }
    }
}

impl Project for wire::EditOverwrite {
    type Target = domain::EditOverwrite;
    fn project(self) -> Self::Target {
        domain::EditOverwrite {
            path: self.path,
            content: self.content,
        }
    }
}

impl Project for domain::EditLineAnchor {
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
    type Target = domain::EditLineAnchor;
    fn project(self) -> Self::Target {
        domain::EditLineAnchor {
            path: self.path,
            start: self.start,
            end: self.end,
            content: self.content,
        }
    }
}

impl Project for domain::EditOperation {
    type Target = wire::EditOperation;
    fn project(self) -> Self::Target {
        match self {
            domain::EditOperation::Replace(value) => wire::EditOperation::Replace(value.project()),
            domain::EditOperation::Create(value) => wire::EditOperation::Create(value.project()),
            domain::EditOperation::Delete(value) => wire::EditOperation::Delete(value.project()),
            domain::EditOperation::Move(value) => wire::EditOperation::Move(value.project()),
            domain::EditOperation::MoveDir(value) => wire::EditOperation::MoveDir(value.project()),
            domain::EditOperation::Overwrite(value) => {
                wire::EditOperation::Overwrite(value.project())
            }
            domain::EditOperation::LineAnchor(value) => {
                wire::EditOperation::LineAnchor(value.project())
            }
        }
    }
}

impl Project for wire::EditOperation {
    type Target = domain::EditOperation;
    fn project(self) -> Self::Target {
        match self {
            wire::EditOperation::Replace(value) => domain::EditOperation::Replace(value.project()),
            wire::EditOperation::Create(value) => domain::EditOperation::Create(value.project()),
            wire::EditOperation::Delete(value) => domain::EditOperation::Delete(value.project()),
            wire::EditOperation::Move(value) => domain::EditOperation::Move(value.project()),
            wire::EditOperation::MoveDir(value) => domain::EditOperation::MoveDir(value.project()),
            wire::EditOperation::Overwrite(value) => {
                domain::EditOperation::Overwrite(value.project())
            }
            wire::EditOperation::LineAnchor(value) => {
                domain::EditOperation::LineAnchor(value.project())
            }
        }
    }
}

impl Project for domain::EditOutcome {
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
    type Target = domain::EditOutcome;
    fn project(self) -> Self::Target {
        domain::EditOutcome {
            applied: self.applied,
            error: self.error,
            changed_files: self.changed_files,
            commit_hash: self.commit_hash,
            verification: self.verification.map(Project::project),
        }
    }
}

impl Project for domain::StructuredChangeProposal {
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
    type Target = domain::StructuredChangeProposal;
    fn project(self) -> Self::Target {
        domain::StructuredChangeProposal {
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

impl Project for domain::StructuredOutcomeProposal {
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
    type Target = domain::StructuredOutcomeProposal;
    fn project(self) -> Self::Target {
        domain::StructuredOutcomeProposal {
            status: self.status,
            commit_message: self.commit_message,
            message: self.message,
            reason: self.reason,
        }
    }
}

impl Project for domain::StructuredBlockedProposal {
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
    type Target = domain::StructuredBlockedProposal;
    fn project(self) -> Self::Target {
        domain::StructuredBlockedProposal {
            is_blocked: self.is_blocked,
            reason: self.reason,
            details: self.details,
        }
    }
}

impl Project for domain::StructuredAbortProposal {
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
    type Target = domain::StructuredAbortProposal;
    fn project(self) -> Self::Target {
        domain::StructuredAbortProposal {
            is_abort: self.is_abort,
            reason: self.reason,
            recommendation: self.recommendation,
        }
    }
}

impl Project for domain::RepoSearchRequest {
    type Target = wire::RepoSearchRequest;
    fn project(self) -> Self::Target {
        wire::RepoSearchRequest {
            path: self.path,
            regex: self.regex,
        }
    }
}

impl Project for wire::RepoSearchRequest {
    type Target = domain::RepoSearchRequest;
    fn project(self) -> Self::Target {
        domain::RepoSearchRequest {
            path: self.path,
            regex: self.regex,
        }
    }
}

impl Project for domain::DiscussionContextRequest {
    type Target = wire::DiscussionContextRequest;
    fn project(self) -> Self::Target {
        wire::DiscussionContextRequest {
            path: self.path,
            access: self.access.project(),
        }
    }
}

impl Project for wire::DiscussionContextRequest {
    type Target = domain::DiscussionContextRequest;
    fn project(self) -> Self::Target {
        domain::DiscussionContextRequest {
            path: self.path,
            access: self.access.project(),
        }
    }
}

impl Project for domain::ModelResultProposal {
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
    type Target = domain::ModelResultProposal;
    fn project(self) -> Self::Target {
        domain::ModelResultProposal {
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

impl Project for domain::DiscussionTarget {
    type Target = wire::DiscussionTarget;
    fn project(self) -> Self::Target {
        match self {
            domain::DiscussionTarget::Plan { id } => wire::DiscussionTarget::Plan { id },
            domain::DiscussionTarget::Review { id } => wire::DiscussionTarget::Review { id },
            domain::DiscussionTarget::ReviewGroup {
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
    type Target = domain::DiscussionTarget;
    fn project(self) -> Self::Target {
        match self {
            wire::DiscussionTarget::Plan { id } => domain::DiscussionTarget::Plan { id },
            wire::DiscussionTarget::Review { id } => domain::DiscussionTarget::Review { id },
            wire::DiscussionTarget::ReviewGroup {
                session_id,
                group_id,
            } => domain::DiscussionTarget::ReviewGroup {
                session_id,
                group_id,
            },
        }
    }
}
