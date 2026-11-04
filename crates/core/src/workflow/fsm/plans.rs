//! Settlement of the plan payload accumulated by a bounded turn.
//!
//! Every terminal path of a turn runs its payload through this phase exactly
//! once, before the outcome is packed. A qualifying result applies the payload
//! through plan storage and collects the status repairs its barrier performed,
//! while a disqualifying result abandons the payload with one typed rejection
//! whose message names why the mutation could not be applied.
//!
//! The qualification is declared by the phase that closes the turn rather than
//! derived from the packed result: a turn that breaches its context budget
//! reports the rejection through an answer-shaped result and still must not
//! apply a payload it never qualified. Keeping the policy in this module makes
//! the lifecycle independent of which phase closed the turn.

use std::path::Path;

use tauqe_protocol::{DiscussionPlanAction, DiscussionPlanUpdate, PlanRejection};

use crate::plan::storage::{PlanMutationMode, PlanStorage};

/// Result of settling the plan payload of one bounded turn.
#[derive(Debug, Clone, Default)]
pub(super) struct PlanSettlement {
    /// Whether an applied mutation created or fully replaced a plan.
    pub(super) plan_saved: bool,
    /// Mutations the storage engine refused.
    pub(super) plan_errors: Vec<PlanRejection>,
    /// Repairs the status barrier performed on an otherwise valid payload.
    pub(super) plan_warnings: Vec<String>,
}

/// Whether the turn reached a result that qualifies its plan payload.
#[derive(Debug, Clone)]
pub(super) enum TurnQualification {
    /// The turn produced a validated answer or an applied change set.
    Qualified,
    /// The turn closed without a qualifying result; the carried reason names why.
    Disqualified(String),
}

impl TurnQualification {
    /// Reports a turn that closed without a qualifying result.
    pub(super) fn disqualified(reason: impl Into<String>) -> Self {
        Self::Disqualified(reason.into())
    }
}

/// Runs the plan payload of the turn through its settlement phase.
pub(super) async fn settle_plans(
    repo_root: &Path,
    plans: &[DiscussionPlanUpdate],
    mode: PlanMutationMode,
    qualification: TurnQualification,
) -> PlanSettlement {
    match qualification {
        TurnQualification::Qualified => apply_turn_plans(repo_root, plans, mode).await,
        TurnQualification::Disqualified(reason) => abandon_turn_plans(plans, &reason),
    }
}

/// Abandons the payload of a turn that closed without a qualifying result.
///
/// The diagnostic is synthesized once for the whole payload: a turn that closed
/// without qualifying produced no applied mutation, so one rejection states the
/// outcome and names the reason instead of repeating itself per mutation.
fn abandon_turn_plans(plans: &[DiscussionPlanUpdate], reason: &str) -> PlanSettlement {
    if plans.is_empty() {
        return PlanSettlement::default();
    }
    tracing::warn!(
        mutations = plans.len(),
        reason = %reason,
        "plan payload abandoned: the turn closed without a qualifying result"
    );
    PlanSettlement {
        plan_saved: false,
        plan_errors: vec![PlanRejection::abandoned(format!(
            "Plan mutations were not applied: {reason}"
        ))],
        plan_warnings: Vec::new(),
    }
}

/// Applies the turn's plan mutations and reports every repair and refusal.
///
/// The whole batch runs under the storage lock of the plan engine and off the
/// async executor thread, so the read-modify-write cycle of each mutation never
/// interleaves with another task and never stalls the turn.
async fn apply_turn_plans(
    repo_root: &Path,
    plans: &[DiscussionPlanUpdate],
    mode: PlanMutationMode,
) -> PlanSettlement {
    if plans.is_empty() {
        return PlanSettlement::default();
    }
    let root = repo_root.to_path_buf();
    let plans_vec = plans.to_vec();
    match crate::blocking::run_blocking_async(move || {
        let mut settlement = PlanSettlement::default();
        for plan_update in &plans_vec {
            let outcome =
                PlanStorage::apply_discussion_plan_update_with_mode(&root, plan_update, mode);
            for rejection in &outcome.errors {
                tracing::warn!("Plan update rejected: {}", rejection.message);
            }
            settlement.plan_errors.extend(outcome.errors);
            for warning in &outcome.warnings {
                tracing::warn!("Plan mutation normalized: {}", warning);
            }
            settlement.plan_warnings.extend(outcome.warnings);
            if plan_update.action == DiscussionPlanAction::Save && !outcome.applied.is_empty() {
                settlement.plan_saved = true;
            }
            for plan in outcome.applied {
                let _ = PlanStorage::save_active_id(&root, Some(&plan.id));
            }
        }
        settlement
    })
    .await
    {
        Ok(settlement) => settlement,
        Err(err) => {
            // The offload itself failed, so every mutation of this turn was left
            // unapplied. The payload never reached plan storage, so the turn
            // abandons it and reports that as one typed rejection instead of
            // letting the plans disappear silently.
            tracing::error!("Failed to apply turn plans: {}", err);
            PlanSettlement {
                plan_saved: false,
                plan_errors: vec![PlanRejection::abandoned(format!(
                    "The turn's plan mutations could not be applied: {err}"
                ))],
                plan_warnings: Vec::new(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::{ConceptItem, PlanItem, PlanItemStatus, PlanRejectionKind};
    use tempfile::tempdir;

    /// Builds a `save` payload whose only step requests `status`.
    fn save_payload(id: &str, status: PlanItemStatus) -> DiscussionPlanUpdate {
        DiscussionPlanUpdate {
            action: DiscussionPlanAction::Save,
            id: Some(id.to_string()),
            title: Some(format!("Plan {id}")),
            description: None,
            concept: vec![ConceptItem {
                id: "c-1".to_string(),
                title: "Plan invariant".to_string(),
                details: None,
                children: Vec::new(),
            }],
            items: vec![PlanItem {
                id: "1".to_string(),
                title: "Step 1".to_string(),
                status,
                files: vec!["src/step.rs".to_string()],
                ..Default::default()
            }],
        }
    }

    /// A qualifying turn applies its payload and reports every repair.
    #[tokio::test]
    async fn a_qualified_turn_applies_its_payload_and_reports_the_repair() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let settlement = settle_plans(
            &root,
            &[save_payload("warning-plan", PlanItemStatus::InProgress)],
            PlanMutationMode::Discussion,
            TurnQualification::Qualified,
        )
        .await;

        assert!(settlement.plan_errors.is_empty());
        assert!(settlement.plan_saved);
        assert_eq!(settlement.plan_warnings.len(), 1);
        assert!(settlement.plan_warnings[0].contains("new step '1'"));

        let stored = PlanStorage::load_plan(&root, "warning-plan")
            .expect("load the plan")
            .expect("the qualified payload must be stored");
        assert_eq!(stored.find_item("1").unwrap().status, PlanItemStatus::Todo);
    }

    /// An `update` payload reports the repair without claiming a save.
    #[tokio::test]
    async fn an_update_payload_reports_its_repair_without_a_save() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let seeded = settle_plans(
            &root,
            &[save_payload("update-plan", PlanItemStatus::Todo)],
            PlanMutationMode::Discussion,
            TurnQualification::Qualified,
        )
        .await;
        assert!(seeded.plan_errors.is_empty());
        assert!(seeded.plan_saved);

        let update = DiscussionPlanUpdate {
            action: DiscussionPlanAction::Update,
            id: Some("update-plan".to_string()),
            title: None,
            description: None,
            concept: Vec::new(),
            items: vec![PlanItem {
                id: "1".to_string(),
                title: "Step 1 refined".to_string(),
                status: PlanItemStatus::InProgress,
                ..Default::default()
            }],
        };
        let settlement = settle_plans(
            &root,
            &[update],
            PlanMutationMode::Discussion,
            TurnQualification::Qualified,
        )
        .await;

        assert!(settlement.plan_errors.is_empty());
        assert!(!settlement.plan_saved);
        assert_eq!(settlement.plan_warnings.len(), 1);
        assert!(settlement.plan_warnings[0].contains("cannot be moved to IN_PROGRESS"));
    }

    /// A disqualifying turn abandons its payload with one typed diagnostic.
    #[tokio::test]
    async fn a_disqualified_turn_abandons_its_payload() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let settlement = settle_plans(
            &root,
            &[save_payload("abandoned-plan", PlanItemStatus::Todo)],
            PlanMutationMode::Discussion,
            TurnQualification::disqualified("the turn reported a semantic block"),
        )
        .await;

        assert!(settlement.plan_warnings.is_empty());
        assert!(!settlement.plan_saved);
        assert_eq!(settlement.plan_errors.len(), 1);
        let rejection = &settlement.plan_errors[0];
        assert_eq!(rejection.kind, PlanRejectionKind::Abandoned);
        assert!(
            rejection.message.contains("semantic block"),
            "{:?}",
            settlement.plan_errors
        );

        assert!(PlanStorage::load_plan(&root, "abandoned-plan")
            .expect("load the plan")
            .is_none());
    }

    /// An empty payload is no diagnosis at all.
    #[tokio::test]
    async fn an_empty_payload_produces_no_diagnostic() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let settlement = settle_plans(
            &root,
            &[],
            PlanMutationMode::Discussion,
            TurnQualification::disqualified("the turn reported a semantic block"),
        )
        .await;

        assert!(settlement.plan_errors.is_empty());
        assert!(settlement.plan_warnings.is_empty());
        assert!(!settlement.plan_saved);
    }

    /// A mutation the turn regime forbids keeps the classification of storage.
    #[tokio::test]
    async fn a_forbidden_mutation_keeps_its_storage_category() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let settlement = settle_plans(
            &root,
            &[save_payload("isolated-plan", PlanItemStatus::Todo)],
            PlanMutationMode::IsolatedStep,
            TurnQualification::Qualified,
        )
        .await;

        assert!(!settlement.plan_saved);
        assert_eq!(settlement.plan_errors.len(), 1);
        let rejection = &settlement.plan_errors[0];
        assert_eq!(rejection.kind, PlanRejectionKind::ModeForbidden);
        assert!(PlanStorage::load_plan(&root, "isolated-plan")
            .expect("load the plan")
            .is_none());
    }
}
