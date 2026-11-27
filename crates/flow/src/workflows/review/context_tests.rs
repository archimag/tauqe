#![cfg(test)]

use super::*;
use crate::api::{Application, ApplicationEvent};
use std::sync::Mutex;
use tauqe_core::config::AppConfig;
use tauqe_core::context::{ContextAccess, ContextLayer, ContextListener, ContextState};

#[derive(Default)]
struct Observer(Mutex<Vec<ContextState>>);

impl ApplicationObserver for Observer {
    fn publish(&self, _: &str, event: ApplicationEvent) {
        if let ApplicationEvent::Context(state) = event {
            self.0.lock().unwrap().push(state);
        }
    }
}

fn application(root: &Path) -> Application {
    let (history, _) = mpsc::unbounded_channel();
    Application::for_repository(AppConfig::default(), root.into(), history)
}

#[tokio::test]
async fn review_preloads_are_auto_and_restore_manual_access_after_cleanup() {
    let root = tempfile::tempdir().unwrap();
    for path in ["user.rs", "new.rs"] {
        std::fs::write(root.path().join(path), "source").unwrap();
    }
    let app = application(root.path());
    app.test_mutate_manual_context(|manual| {
        manual.add_file("user.rs", ContextAccess::ReadOnly, ContextLayer::User)
    })
    .await
    .unwrap()
    .unwrap();
    let operation = app.try_begin_operation().unwrap();
    let observer = Arc::new(Observer::default());
    let session = app.context_session();
    assert!(session.activate_manual_publication(&operation, observer.clone()));
    let mut detached = session.context_for_turn().await;
    let listener = session
        .activate_research_publication(&operation, "preload".into(), observer.clone(), &detached)
        .unwrap();
    detached.set_listener(Some(listener));
    let (detached, missing) = sequence::prepare_context(
        detached,
        root.path(),
        vec!["user.rs".into(), "new.rs".into()],
        vec!["missing.rs".into()],
    )
    .await
    .unwrap();
    assert_eq!(missing, ["missing.rs"]);
    let visible = session.context_state().await;
    assert!(visible
        .items
        .iter()
        .any(|item| item.path == "new.rs" && item.layer == ContextLayer::Auto));
    assert!(visible
        .items
        .iter()
        .any(|item| item.path == "user.rs" && item.access == ContextAccess::Editable));
    assert_eq!(observer.0.lock().unwrap().last(), Some(&visible));
    assert_eq!(
        session.context_for_turn().await.get_state().items[0].access,
        ContextAccess::ReadOnly
    );
    session.merge_turn_context(detached).await.unwrap();
    session.close_research_publication(operation.id());
    assert!(session.activate_manual_publication(&operation, observer.clone()));
    session.clear_transient_context().await.unwrap();
    let restored = session.context_state().await;
    assert_eq!(restored.items.len(), 1);
    assert_eq!(restored.items[0].layer, ContextLayer::User);
    assert_eq!(restored.items[0].access, ContextAccess::ReadOnly);
    session.close_manual_publication(&operation);
}

#[tokio::test]
async fn stale_review_member_source_cannot_publish_over_next_member() {
    let root = tempfile::tempdir().unwrap();
    for path in ["first.rs", "second.rs"] {
        std::fs::write(root.path().join(path), "source").unwrap();
    }
    let app = application(root.path());
    let session = app.context_session();
    let operation = app.try_begin_operation().unwrap();
    let observer = Arc::new(Observer::default());
    assert!(session.activate_manual_publication(&operation, observer.clone()));
    let mut first = session.context_for_turn().await;
    let old: ContextListener = session
        .activate_research_publication(&operation, "first".into(), observer.clone(), &first)
        .unwrap();
    first.set_listener(Some(old.clone()));
    first
        .add_auto_file("first.rs", ContextAccess::ReadOnly)
        .unwrap();
    session.close_research_publication(operation.id());
    assert!(session.activate_manual_publication(&operation, observer.clone()));
    let mut second = session.context_for_turn().await;
    second.set_listener(Some(
        session
            .activate_research_publication(&operation, "second".into(), observer.clone(), &second)
            .unwrap(),
    ));
    second
        .add_auto_file("second.rs", ContextAccess::ReadOnly)
        .unwrap();
    let snapshot = session.context_state().await;
    let count = observer.0.lock().unwrap().len();
    first
        .add_auto_file("second.rs", ContextAccess::Editable)
        .unwrap();
    assert_eq!(session.context_state().await, snapshot);
    assert_eq!(observer.0.lock().unwrap().len(), count);
    session.close_research_publication(operation.id());
    session.close_manual_publication(&operation);
}
