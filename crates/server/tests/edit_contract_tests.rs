//! Compatibility of owned edit/model payloads and their RPC projections.
#[path = "../src/contracts.rs"]
mod contracts;
use contracts::Project;
use serde_json::json;
use tauqe_core::{edits as domain, llm};
use tauqe_protocol as wire;

#[test]
fn every_operation_keeps_its_payload_and_escaped_path_round_trip() {
    let operations = [
        json!({"type":"replace","path":" ./src/a.rs ","old_text":"old\n","new_text":"new\n"}),
        json!({"type":"create","path":"`src/new.rs`","content":"full\n"}),
        json!({"type":"delete","path":"src/gone.rs"}),
        json!({"type":"move","from":"src/a.rs","to":"src/b.rs"}),
        json!({"type":"move_dir","from":"src/old","to":"src/new"}),
        json!({"type":"overwrite","path":r"src\\literal.rs","content":"whole\n"}),
        json!({"type":"line_anchor","path":"src/a.rs","start":"K1","end":"K9","content":"range\n"}),
    ];
    for input in operations {
        let core: domain::EditOperation = serde_json::from_value(input.clone()).unwrap();
        let wire: wire::EditOperation = serde_json::from_value(input).unwrap();
        assert_eq!(
            serde_json::to_value(&core).unwrap(),
            serde_json::to_value(&wire).unwrap()
        );
        assert_eq!(core.clone().project(), wire);
        assert_eq!(wire.project(), core);
        let serialized = serde_json::to_string(&core).unwrap();
        assert_eq!(
            serde_json::from_str::<domain::EditOperation>(&serialized).unwrap(),
            core
        );
    }
    for path in [
        "",
        "../escape",
        "/absolute",
        "./C:/absolute",
        r"\\server\share",
        "nul\0byte",
    ] {
        let input = json!({"type":"delete","path":path});
        let core = serde_json::from_value::<domain::EditOperation>(input.clone())
            .unwrap_err()
            .to_string();
        let wire = serde_json::from_value::<wire::EditOperation>(input)
            .unwrap_err()
            .to_string();
        assert_eq!(core, wire);
    }
}

#[test]
fn raw_model_proposals_preserve_unknown_operations_signals_and_research_requests() {
    let inputs = [
        json!({}),
        json!({"changes":null,"context_requests":null,"context_drops":null,"doc_requests":null,"suggested_actions":null}),
        json!({
            "message":"Proposed", "scratchpad":"Reasoning",
            "changes":[{"op":" FROBNICATE ","path":"raw/path","to":"dest","old_text":"old","new_text":"new","start":"K1","end":"K2","content":"body"},
                       {"op":null,"path":null,"to":null,"old_text":null,"new_text":null,"start":null,"end":null,"content":null}],
            "context_requests":["editable:./src/a.rs", "read_only:../escape", {"path":"./src/b.rs","access":"read_only"}, "./src/c.rs"],
            "repo_searches":[{"path":"./","regex":"^fn "},{"path":"../escape","regex":"ignored"},{"path":"src","regex":"same"},{"path":"src","regex":"same"}],
            "tauqe_doc_requests":["git","UNKNOWN",null,"context","workflow","interface","overview","all"],
            "context_drops":["./src/a.rs","../escape","src/a.rs"],
            "outcome":{"status":" SUCCESS ","commit_message":"Commit","message":"Detail","reason":"Reason"},
            "blocked":{"is_blocked":true,"reason":"Precondition","details":"Details"},
            "abort":{"is_abort":true,"reason":"Host","recommendation":"Repair"},
            "suggested_actions":["first","second"]
        }),
    ];
    for input in inputs {
        let core: llm::ModelResultProposal = serde_json::from_value(input.clone()).unwrap();
        let wire: wire::ModelResultProposal = serde_json::from_value(input).unwrap();
        assert_eq!(
            serde_json::to_value(&core).unwrap(),
            serde_json::to_value(&wire).unwrap()
        );
        assert_eq!(core.clone().project(), wire);
        assert_eq!(wire.project(), core);
    }
    for entry in [
        json!(false),
        json!({"access":"read_only"}),
        json!({"path":"src/a.rs","access":"invalid"}),
    ] {
        let value = json!({"context_requests":[entry]});
        assert_eq!(
            serde_json::from_value::<llm::ModelResultProposal>(value.clone())
                .unwrap_err()
                .to_string(),
            serde_json::from_value::<wire::ModelResultProposal>(value)
                .unwrap_err()
                .to_string()
        );
    }
}

#[test]
fn edit_outcomes_keep_all_verdicts_and_optional_fields() {
    for verdict in [
        None,
        Some(domain::VerificationVerdict::Passed),
        Some(domain::VerificationVerdict::Failed),
        Some(domain::VerificationVerdict::NotConfigured),
        Some(domain::VerificationVerdict::Cancelled),
    ] {
        for applied in [false, true] {
            let outcome = domain::EditOutcome {
                applied,
                error: Some("diagnostic".into()),
                changed_files: vec!["b".into(), "a".into()],
                commit_hash: Some("abc123".into()),
                verification: verdict,
            };
            let projected = outcome.clone().project();
            assert_eq!(
                serde_json::to_value(&outcome).unwrap(),
                serde_json::to_value(&projected).unwrap()
            );
            assert_eq!(projected.project(), outcome);
        }
    }
    assert_eq!(
        serde_json::to_value(domain::EditOutcome::default().project()).unwrap(),
        json!({"applied":false,"changed_files":[]})
    );
}

#[test]
fn file_event_vocabulary_keeps_its_names() {
    for (op, name) in [
        (domain::EditFileOp::Replace, "replace"),
        (domain::EditFileOp::Create, "create"),
        (domain::EditFileOp::Delete, "delete"),
        (domain::EditFileOp::Move, "move"),
        (domain::EditFileOp::MoveDir, "move_dir"),
        (domain::EditFileOp::Overwrite, "overwrite"),
    ] {
        assert_eq!(serde_json::to_value(op.project()).unwrap(), name);
        assert_eq!(op.project().project(), op);
    }
    for (status, name) in [
        (domain::EditFileStatus::Ok, "ok"),
        (domain::EditFileStatus::Error, "error"),
    ] {
        assert_eq!(serde_json::to_value(status.project()).unwrap(), name);
        assert_eq!(status.project().project(), status);
    }
}

#[test]
fn discussion_targets_keep_their_coordinates() {
    use tauqe_core::discussion::DiscussionTarget;
    for target in [
        DiscussionTarget::Plan { id: "p".into() },
        DiscussionTarget::Review { id: "r".into() },
        DiscussionTarget::ReviewGroup {
            session_id: "r".into(),
            group_id: "g".into(),
        },
    ] {
        let wire = target.clone().project();
        assert_eq!(
            serde_json::to_value(&target).unwrap(),
            serde_json::to_value(&wire).unwrap()
        );
        assert_eq!(wire.project(), target);
    }
}
