mod model;
pub use model::{
    ConceptItem, Plan, PlanImplementation, PlanItem, PlanItemStatus, PlanRejection,
    PlanRejectionKind, PlanScopeEvaluation, PlanSummary, PlanSynthesisResult, PlanValidationError,
};
pub mod matcher;
pub mod prompt;
pub mod storage;

pub(crate) use prompt::format_active_plan_context;
pub use prompt::{
    build_plan_horizon_system_prompt, build_plan_horizon_user_prompt, PlanRefinementOutput,
};
pub use storage::PlanStorage;

use std::collections::HashSet;

/// Returns the first duplicate or empty step ID found in a plan item tree, if any.
///
/// Architect and client payloads are not trusted: a repeated ID would make
/// `Plan::find_item` ambiguous and let one step silently shadow another, while an
/// empty ID cannot be addressed at all. Such a tree is rejected before persistence.
pub fn find_duplicate_item_ids(items: &[PlanItem]) -> Option<String> {
    fn walk(items: &[PlanItem], seen: &mut HashSet<String>) -> Option<String> {
        for item in items {
            let key = item.id.trim().to_ascii_lowercase();
            if key.is_empty() || !seen.insert(key) {
                return Some(item.id.clone());
            }
            if let Some(duplicate) = walk(&item.children, seen) {
                return Some(duplicate);
            }
        }
        None
    }
    walk(items, &mut HashSet::new())
}

pub use model::{DiscussionPlanAction, DiscussionPlanUpdate};
