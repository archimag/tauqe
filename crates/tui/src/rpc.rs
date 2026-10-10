pub mod events;
pub mod handshake;
pub mod responses;
pub mod transport;

pub use events::{fail_safe_reject_edits, handle_event};
pub use handshake::initialize_connection;
pub use responses::{handle_response, messages};
pub use transport::{
    allocate_request_id, find_server_binary, record_optimistic_rollback, record_pending_request,
    send_request, send_request_with_id, spawn_message_reader, start_server,
    take_optimistic_rollback, take_pending_request, OptimisticRollback,
};

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::ModelRef;
    use crate::ui::develop::{DevelopView, StreamingFileEdit};

    #[test]
    fn test_optimistic_rollback_storage() {
        let req_id = 9999;
        record_optimistic_rollback(
            req_id,
            OptimisticRollback::ActiveModel {
                prev_model: ModelRef::openrouter("test/prev"),
                prev_selection: tauqe_protocol::ModelSelection::default(),
            },
        );
        let rollback = take_optimistic_rollback(req_id);
        assert!(matches!(rollback, Some(OptimisticRollback::ActiveModel { .. })));
        assert!(take_optimistic_rollback(req_id).is_none());
    }

    #[test]
    fn test_onboarding_messages_are_english() {
        assert_eq!(
            messages::stub_credentials_created("credentials.toml"),
            "Stub credentials file created at 'credentials.toml'. Edit it and select 'Check again'."
        );
        assert_eq!(messages::CONFIG_CREATED, "tauqe.toml created successfully.");
        assert_eq!(messages::API_KEY_SAVED, "OpenRouter API key saved successfully.");
        assert_eq!(messages::CONFIG_RELOADED_OK, "Configuration reloaded. All checks passed!");
        assert!(messages::API_KEY_NOT_FOUND.starts_with("API key still not found"));
        assert_eq!(messages::GIT_INIT_OK, "Git repository initialized successfully.");
    }

    #[test]
    fn test_fail_safe_reject_edits_clears_retrying_files() {
        let mut model = DevelopView {
            edits_active: true,
            files: vec![
                StreamingFileEdit {
                    path: "a.rs".to_string(),
                    op_type: "replace".to_string(),
                    status: "retrying".to_string(),
                    error: None,
                    hunks: Vec::new(),
                    expanded: false,
                    retry_info: Some("1/3 retrying".to_string()),
                },
                StreamingFileEdit {
                    path: "b.rs".to_string(),
                    op_type: "replace".to_string(),
                    status: "running".to_string(),
                    error: None,
                    hunks: Vec::new(),
                    expanded: false,
                    retry_info: None,
                },
                StreamingFileEdit {
                    path: "c.rs".to_string(),
                    op_type: "replace".to_string(),
                    status: "ok".to_string(),
                    error: None,
                    hunks: Vec::new(),
                    expanded: false,
                    retry_info: None,
                },
            ],
            ..Default::default()
        };

        fail_safe_reject_edits(&mut model, "Operation cancelled");

        assert_eq!(model.files[0].status, "error");
        assert_eq!(model.files[0].retry_info, None);
        assert_eq!(model.files[0].error.as_deref(), Some("Operation cancelled"));

        assert_eq!(model.files[1].status, "error");
        assert_eq!(model.files[1].error.as_deref(), Some("Operation cancelled"));

        assert_eq!(model.files[2].status, "ok");
        assert_eq!(model.edit_final_applied, Some(false));
    }
}
