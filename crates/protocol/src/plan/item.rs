use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PlanItemStatus {
    Discussion,
    #[default]
    Todo,
    InProgress,
    Done,
    Blocked,
    Cancelled,
}

impl PlanItemStatus {
    pub fn next(&self) -> Self {
        match self {
            Self::Discussion => Self::Todo,
            Self::Todo => Self::InProgress,
            Self::InProgress => Self::Done,
            Self::Done => Self::Blocked,
            Self::Blocked => Self::Cancelled,
            Self::Cancelled => Self::Discussion,
        }
    }
}

impl std::fmt::Display for PlanItemStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Discussion => write!(f, "DISCUSSION"),
            Self::Todo => write!(f, "TODO"),
            Self::InProgress => write!(f, "IN_PROGRESS"),
            Self::Done => write!(f, "DONE"),
            Self::Blocked => write!(f, "BLOCKED"),
            Self::Cancelled => write!(f, "CANCELLED"),
        }
    }
}

/// A node in the conceptual architectural tree ("What" / Requirements & Invariants).
///
/// Conceptual items represent design decisions, architectural invariants, and high-level
/// goals. They are formed and discussed in dialogue, and remain read-only during deep
/// implementation refinement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ConceptItem {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<ConceptItem>,
}

impl ConceptItem {
    pub fn is_leaf(&self) -> bool {
        self.children.is_empty()
    }
}

/// An actionable engineering step in the implementation blueprint ("How").
///
/// Implementation items are formed and refined by autonomous deep analysis.
/// Each step contains the exact paths of targeted files (`files`) that are
/// deterministically pre-loaded into context upon execution, as well as optional
/// references to conceptual requirements (`concept_refs`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PlanItem {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
    #[serde(default)]
    pub status: PlanItemStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub readonly_files: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub concept_refs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<PlanItem>,
}

impl PlanItem {
    pub fn is_leaf(&self) -> bool {
        self.children.is_empty()
    }
}
