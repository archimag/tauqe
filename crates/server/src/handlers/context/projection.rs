//! Context RPC adaptation. Core snapshots have no transport serialization.
use crate::contracts::Project;
use tauqe_core::context::{ContextAccess, ContextLayer, ContextState};

pub(crate) fn context_access_domain(access: tauqe_protocol::ContextAccess) -> ContextAccess {
    access.project()
}
pub(crate) fn context_layer_domain(layer: tauqe_protocol::ContextLayer) -> ContextLayer {
    match layer {
        tauqe_protocol::ContextLayer::Pinned => ContextLayer::Pinned,
        tauqe_protocol::ContextLayer::OuterDocs => ContextLayer::OuterDocs,
        tauqe_protocol::ContextLayer::User => ContextLayer::User,
        tauqe_protocol::ContextLayer::Auto => ContextLayer::Auto,
    }
}
pub(crate) fn context_state_dto(state: &ContextState) -> tauqe_protocol::ContextState {
    tauqe_protocol::ContextState {
        revision: state.revision,
        total_estimated_tokens: state.total_estimated_tokens,
        items: state
            .items
            .iter()
            .map(|item| tauqe_protocol::ContextItem {
                path: item.path.clone(),
                access: item.access.project(),
                size_bytes: item.size_bytes,
                estimated_tokens: item.estimated_tokens,
                layer: match item.layer {
                    ContextLayer::Pinned => tauqe_protocol::ContextLayer::Pinned,
                    ContextLayer::OuterDocs => tauqe_protocol::ContextLayer::OuterDocs,
                    ContextLayer::User => tauqe_protocol::ContextLayer::User,
                    ContextLayer::Auto => tauqe_protocol::ContextLayer::Auto,
                },
                insertion_seq: item.insertion_seq,
            })
            .collect(),
    }
}

/// Publishes one authoritative context snapshot through the wire DTO.
pub(crate) fn publish_context(out: &crate::transport::OutChannel, state: &ContextState) {
    out.send_event(&tauqe_protocol::Event {
        method: tauqe_protocol::events::CONTEXT_CHANGED.into(),
        params: serde_json::to_value(tauqe_protocol::ContextChangedEvent {
            state: context_state_dto(state),
        })
        .ok(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_core::context::ContextItem;

    #[test]
    fn context_event_keeps_revision_layer_order_and_arrival_watermarks() {
        let state = ContextState {
            revision: 13,
            items: [
                ContextLayer::Pinned,
                ContextLayer::OuterDocs,
                ContextLayer::User,
                ContextLayer::Auto,
            ]
            .into_iter()
            .enumerate()
            .map(|(index, layer)| ContextItem {
                path: format!("{index}.rs"),
                access: if index == 2 {
                    ContextAccess::Editable
                } else {
                    ContextAccess::ReadOnly
                },
                size_bytes: 20,
                estimated_tokens: 5,
                layer,
                insertion_seq: index as u64,
            })
            .collect(),
            total_estimated_tokens: 20,
        };
        let (out, mut receiver) = crate::transport::OutChannel::new();
        publish_context(&out, &state);
        let event: serde_json::Value = serde_json::from_str(&receiver.try_recv().unwrap()).unwrap();
        assert_eq!(
            event,
            serde_json::json!({"method":"context/changed", "params":{"state":{
                "revision":13, "total_estimated_tokens":20, "items":[
                    {"path":"0.rs","access":"read_only","size_bytes":20,"estimated_tokens":5,"layer":"pinned","insertion_seq":0},
                    {"path":"1.rs","access":"read_only","size_bytes":20,"estimated_tokens":5,"layer":"outer_docs","insertion_seq":1},
                    {"path":"2.rs","access":"editable","size_bytes":20,"estimated_tokens":5,"layer":"user","insertion_seq":2},
                    {"path":"3.rs","access":"read_only","size_bytes":20,"estimated_tokens":5,"layer":"auto","insertion_seq":3}
                ]
            }}})
        );
        assert_eq!(state.revision, 13);
        assert_eq!(state.items[1].layer, ContextLayer::OuterDocs);
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn legacy_request_aliases_and_defaults_are_resolved_before_core_admission() {
        let request: tauqe_protocol::ContextAddParams =
            serde_json::from_value(serde_json::json!({"path":"ref.md","layer":"refs"})).unwrap();
        assert_eq!(
            context_access_domain(request.access),
            ContextAccess::ReadOnly
        );
        assert_eq!(
            request.layer.map(context_layer_domain),
            Some(ContextLayer::OuterDocs)
        );
        let request: tauqe_protocol::ContextAddParams =
            serde_json::from_value(serde_json::json!({"path":"file.rs","access":"editable"}))
                .unwrap();
        assert_eq!(
            context_access_domain(request.access),
            ContextAccess::Editable
        );
        assert_eq!(request.layer.map(context_layer_domain), None);
    }
}
