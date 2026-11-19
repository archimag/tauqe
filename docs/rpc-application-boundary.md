# RPC application scenarios

The method names are constants from `tauqe_protocol::methods`. An immediate response projects state or acknowledges a signal. Synchronous requests wait for the command to settle. Launch requests acknowledge registered work that continues in flow.

Exclusive scenarios claim the operation slot. Observe samples the slot for a bounded update. Open routes remain available during an operation. The server decodes parameters before entering the scenario.

| RPC method | Application scenario | Response | Admission |
| --- | --- | --- | --- |
| `CLIENT_INITIALIZE` | `initialize_client` | Synchronous | Exclusive |
| `REPOSITORY_GET_STATE` | `repository_state` | Synchronous | Open |
| `REPOSITORY_INIT` | `initialize_repository` | Synchronous | Exclusive |
| `REPOSITORY_LIST_FILES` | `repository_files` | Synchronous | Open |
| `GIT_GET_DIFF` | `repository_diff` | Synchronous | Open |
| `GIT_SQUASH_PREVIEW` | `squash_preview` | Synchronous | Exclusive |
| `GIT_SQUASH_GENERATE_MESSAGE` | `generate_squash_message` | Synchronous | Exclusive |
| `GIT_SQUASH_APPLY` | `apply_squash` | Synchronous | Exclusive |
| `CONTEXT_GET` | `context_state` | Immediate | Open |
| `CONTEXT_ADD` | `add_context_file` | Synchronous | Exclusive |
| `CONTEXT_ADD_PATTERN` | `add_context_pattern` | Synchronous | Exclusive |
| `CONTEXT_REMOVE` | `remove_context_file` | Synchronous | Exclusive |
| `CONTEXT_SET_ACCESS` | `set_context_access` | Synchronous | Exclusive |
| `CONTEXT_CLEAR` | `clear_manual_context` | Synchronous | Exclusive |
| `OUTER_DOCS_CACHE_LIST` | `outer_docs_cache` | Synchronous | Open |
| `OUTER_DOCS_CACHE_REMOVE` | `remove_cached_documents` | Synchronous | Exclusive |
| `HISTORY_GET` | `history_page` | Synchronous | Open |
| `MODEL_ASK` | `ask` | Launch | Exclusive |
| `MODEL_CANCEL` | `cancel` | Immediate | Open |
| `MODEL_CLEAR_HISTORY` | `clear_dialogue_history` | Synchronous | Exclusive |
| `MODEL_SELECT` | `change_model` | Synchronous | Exclusive |
| `MODEL_CURRENT` | `configuration_view` | Immediate | Open |
| `MODEL_USAGE_RESET` | `reset_project_usage` | Synchronous | Observe |
| `DISCUSSION_SET_FOCUS` | `set_focus` | Immediate | Open |
| `DISCUSSION_GET_FOCUS` | `discussion_focus` | Immediate | Open |
| `DISCUSSION_CLEAR_FOCUS` | `set_focus` with empty focus | Immediate | Open |
| `LOCALE_GENERATE` | `generate_locale` | Synchronous | Exclusive |
| `REVIEW_START` | `start_review` | Launch | Exclusive |
| `REVIEW_PREVIEW` | `preview_review` | Synchronous | Open |
| `REVIEW_CANCEL` | `cancel` | Immediate | Open |
| `REVIEW_LIST` | `list_reviews` | Synchronous | Open |
| `REVIEW_GET` | `get_review` | Synchronous | Open |
| `REVIEW_UPDATE_ITEM` | `update_review_item` | Synchronous | Exclusive |
| `REVIEW_DELETE` | `delete_review` | Synchronous | Exclusive |
| `REVIEW_EXECUTE` | `execute_review` | Launch | Exclusive |
| `REVIEW_ANALYZE` | `analyze_review` | Synchronous | Exclusive |
| `PLAN_LIST` | `list_plans` | Synchronous | Open |
| `PLAN_GET` | `get_plan` | Synchronous | Open |
| `PLAN_SAVE` | `save_plan` | Synchronous | Exclusive |
| `PLAN_UPDATE_ITEM` | `update_plan_item` | Synchronous | Exclusive |
| `PLAN_DELETE` | `delete_plan` | Synchronous | Exclusive |
| `PLAN_SET_ACTIVE` | `set_active_plan` | Synchronous | Exclusive |
| `PLAN_CLEAR_HORIZON` | `clear_plan_horizon` | Synchronous | Exclusive |
| `PLAN_EXECUTE` | `execute_plan` | Launch | Exclusive |
| `PLAN_EXECUTE_PIPELINE` | `execute_plan_pipeline` | Launch | Exclusive |
| `PLAN_CREATE_PIPELINE` | `synthesize_plan` with Create | Synchronous | Exclusive |
| `PLAN_REFINE` | `synthesize_plan` with Refine | Synchronous | Exclusive |
| `PLAN_REPLAN` | `synthesize_plan` with Replan | Synchronous | Exclusive |
| `CONFIG_GET` | `configuration_view` | Immediate | Open |
| `CONFIG_SET` | `configure` | Synchronous | Exclusive |
| `CONFIG_RELOAD` | `change_configuration_files` with Reload | Synchronous | Exclusive |
| `CONFIG_CREATE` | `change_configuration_files` with Create | Synchronous | Exclusive |
| `CREDENTIALS_SAVE` | `change_configuration_files` with SaveCredentials | Synchronous | Exclusive |
| `CHATGPT_SIGN_IN_CANCEL` | Dismiss the sign-in invitation on this connection | Immediate | Observe |
| `CHATGPT_SIGN_OUT` | `sign_out_chatgpt` | Synchronous | Exclusive |
| `CREDENTIALS_CREATE_STUB` | `change_configuration_files` with CreateCredentialsStub | Synchronous | Exclusive |
| `SYSTEM_STATUS` | `status` | Synchronous | Open |
