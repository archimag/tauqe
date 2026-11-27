//! Project settled REPL facts into application events.
use super::documents::DocumentChanges;
use crate::api::{ApplicationEvent, ApplicationObserver};
use crate::application::context::ContextSession;
use crate::application::edit_cycle::{EditCycle, EditTermination};
use crate::application::interactive_completion::REPL_EDIT_SUMMARY;
use crate::application::interactive_updates::{
    interactive_plan_updates, interactive_review_updates, repository_after_turn_commit,
};
use tauqe_core::edits::EditOutcome;
use tauqe_core::providers::StreamEvent;
use tauqe_core::stop::HardCancellation;
use tokio::sync::mpsc;

#[derive(Default)]
pub(super) struct StreamTermination {
    pub(super) cancelled: bool,
    pub(super) error: Option<String>,
}

pub(super) async fn relay_stream(
    observer: &dyn ApplicationObserver,
    id: &str,
    cycle: &EditCycle,
    mut events: mpsc::UnboundedReceiver<StreamEvent>,
    cancel: &HardCancellation,
) -> StreamTermination {
    let mut terminal = StreamTermination::default();
    while let Some(event) = events.recv().await {
        match event {
            StreamEvent::Cancelled => terminal.cancelled = true,
            StreamEvent::Error(error) => {
                terminal.error.get_or_insert(error);
            }
            event => {
                observer.ready(&event, cancel).await;
                cycle.observe(&event);
                observer.publish(id, ApplicationEvent::Stream(event));
            }
        }
    }
    terminal
}

pub(super) fn close_failed(cycle: &EditCycle, termination: EditTermination, reason: String) {
    cycle.close_outcome(
        termination,
        EditOutcome {
            error: Some(reason),
            ..EditOutcome::default()
        },
    );
}

pub(super) async fn publish_edit(
    context: &ContextSession,
    observer: &dyn ApplicationObserver,
    id: &str,
    cycle: &EditCycle,
    outcome: &EditOutcome,
) {
    if let Some(hash) = &outcome.commit_hash {
        observer.publish(
            id,
            ApplicationEvent::Commit {
                hash: hash.clone(),
                summary: REPL_EDIT_SUMMARY.to_owned(),
                changed_files: outcome.changed_files.clone(),
            },
        );
        if let Some(repository) =
            repository_after_turn_commit(context.repository_root().await).await
        {
            observer.publish(id, ApplicationEvent::Repository(repository));
        }
    }
    cycle.open();
    cycle.close_outcome(EditTermination::Answered, outcome.clone());
}

pub(super) async fn publish_documents(
    context: &ContextSession,
    observer: &dyn ApplicationObserver,
    id: &str,
    documents: &DocumentChanges,
) {
    let root = context.repository_root().await;
    if let Some(updates) = interactive_plan_updates(&root, documents).await {
        for plan in updates.saved {
            observer.publish(
                id,
                ApplicationEvent::PlanDocument {
                    plan,
                    is_save: true,
                },
            );
        }
        if let Some((plans, active_id)) = updates.listing {
            observer.publish(id, ApplicationEvent::PlanList { plans, active_id });
        }
    }
    for updates in interactive_review_updates(&root, documents).await {
        for _ in 0..updates.decisions_applied {
            observer.publish(
                id,
                ApplicationEvent::ReviewDocument(updates.session.clone()),
            );
        }
    }
}

pub(super) fn publish_interruption(
    observer: &dyn ApplicationObserver,
    id: &str,
    cancelled: bool,
    diagnostic: String,
) {
    tracing::warn!(operation_id = %id, cancelled, %diagnostic, "Model turn interrupted");
    let event = if cancelled {
        if !diagnostic.is_empty() {
            observer.publish(
                id,
                ApplicationEvent::Stream(StreamEvent::TextDelta(format!("\n\n{diagnostic}\n\n"))),
            );
        }
        StreamEvent::Cancelled
    } else {
        StreamEvent::Error(diagnostic)
    };
    observer.publish(id, ApplicationEvent::Stream(event));
}
