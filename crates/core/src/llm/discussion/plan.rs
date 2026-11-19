//! Typed plan changes authored by a model or supplied to storage.
use super::deserialize_null_as_default;
use crate::plan_item::{ConceptItem, PlanItem};
use serde::{Deserialize, Serialize};

/// Action to perform on a project plan as part of discussion structured output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DiscussionPlanAction {
    #[default]
    Update,
    Save,
}

impl std::fmt::Display for DiscussionPlanAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Update => write!(f, "update"),
            Self::Save => write!(f, "save"),
        }
    }
}

/// Structured plan modification payload produced in discussion mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DiscussionPlanUpdate {
    #[serde(default)]
    pub action: DiscussionPlanAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) description: Option<String>,
    #[serde(
        default,
        deserialize_with = "deserialize_null_as_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub(crate) concept: Vec<ConceptItem>,
    #[serde(
        default,
        deserialize_with = "deserialize_null_as_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub(crate) items: Vec<PlanItem>,
}
