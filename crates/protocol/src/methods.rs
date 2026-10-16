pub const CLIENT_INITIALIZE: &str = "client/initialize";
pub const REPOSITORY_GET_STATE: &str = "repository/getState";
pub const REPOSITORY_LIST_FILES: &str = "repository/listFiles";
pub const REPOSITORY_INIT: &str = "repository/init";
pub const MODEL_ASK: &str = "model/ask";
pub const MODEL_CANCEL: &str = "model/cancel";
pub const MODEL_CLEAR_HISTORY: &str = "model/clearHistory";
pub const MODEL_SELECT: &str = "model/select";
pub const MODEL_CURRENT: &str = "model/current";

pub const CONTEXT_GET: &str = "context/get";
pub const CONTEXT_ADD: &str = "context/add";
pub const CONTEXT_ADD_PATTERN: &str = "context/addPattern";
pub const CONTEXT_REMOVE: &str = "context/remove";
pub const CONTEXT_SET_ACCESS: &str = "context/setAccess";
pub const CONTEXT_CLEAR: &str = "context/clear";

pub const HISTORY_GET: &str = "history/get";

pub const GIT_UNDO: &str = "git/undo";
pub const GIT_GET_DIFF: &str = "git/getDiff";
pub const GIT_SQUASH_PREVIEW: &str = "git/squashPreview";
pub const GIT_SQUASH_GENERATE_MESSAGE: &str = "git/squashGenerateMessage";
pub const GIT_SQUASH_APPLY: &str = "git/squashApply";

pub const CONFIG_GET: &str = "config/get";
pub const CONFIG_SET: &str = "config/set";
pub const CONFIG_RELOAD: &str = "config/reload";
pub const CONFIG_CREATE: &str = "config/create";

pub const CREDENTIALS_SAVE: &str = "credentials/save";
pub const CREDENTIALS_CREATE_STUB: &str = "credentials/createStub";

pub const SYSTEM_STATUS: &str = "system/status";

pub const REVIEW_START: &str = "review/start";
pub const REVIEW_CANCEL: &str = "review/cancel";
pub const REVIEW_LIST: &str = "review/list";
pub const REVIEW_GET: &str = "review/get";
pub const REVIEW_UPDATE_ITEM: &str = "review/updateItem";
pub const REVIEW_DELETE: &str = "review/delete";
pub const REVIEW_EXECUTE: &str = "review/execute";
pub const REVIEW_REFINE_ITEM: &str = "review/refineItem";

pub const PLAN_LIST: &str = "plan/list";
pub const PLAN_GET: &str = "plan/get";
pub const PLAN_SAVE: &str = "plan/save";
pub const PLAN_UPDATE_ITEM: &str = "plan/updateItem";
pub const PLAN_DELETE: &str = "plan/delete";
pub const PLAN_SET_ACTIVE: &str = "plan/setActive";
pub const PLAN_EXECUTE: &str = "plan/execute";
pub const PLAN_REFINE: &str = "plan/refine";
pub const PLAN_REPLAN: &str = "plan/replan";

pub const DISCUSSION_SET_FOCUS: &str = "discussion/setFocus";
pub const DISCUSSION_GET_FOCUS: &str = "discussion/getFocus";
pub const DISCUSSION_CLEAR_FOCUS: &str = "discussion/clearFocus";

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Enumerates every method constant so the contract test can pin the wire
    /// names: each must be unique and must follow the
    /// `<lowercase-namespace>/<camelCaseVerb>` convention.
    fn all_methods() -> Vec<&'static str> {
        vec![
            CLIENT_INITIALIZE,
            REPOSITORY_GET_STATE,
            REPOSITORY_LIST_FILES,
            REPOSITORY_INIT,
            MODEL_ASK,
            MODEL_CANCEL,
            MODEL_CLEAR_HISTORY,
            MODEL_SELECT,
            MODEL_CURRENT,
            CONTEXT_GET,
            CONTEXT_ADD,
            CONTEXT_ADD_PATTERN,
            CONTEXT_REMOVE,
            CONTEXT_SET_ACCESS,
            CONTEXT_CLEAR,
            HISTORY_GET,
            GIT_UNDO,
            GIT_GET_DIFF,
            GIT_SQUASH_PREVIEW,
            GIT_SQUASH_GENERATE_MESSAGE,
            GIT_SQUASH_APPLY,
            CONFIG_GET,
            CONFIG_SET,
            CONFIG_RELOAD,
            CONFIG_CREATE,
            CREDENTIALS_SAVE,
            CREDENTIALS_CREATE_STUB,
            SYSTEM_STATUS,
            REVIEW_START,
            REVIEW_CANCEL,
            REVIEW_LIST,
            REVIEW_GET,
            REVIEW_UPDATE_ITEM,
            REVIEW_DELETE,
            REVIEW_EXECUTE,
            REVIEW_REFINE_ITEM,
            PLAN_LIST,
            PLAN_GET,
            PLAN_SAVE,
            PLAN_UPDATE_ITEM,
            PLAN_DELETE,
            PLAN_SET_ACTIVE,
            PLAN_EXECUTE,
            PLAN_REFINE,
            PLAN_REPLAN,
            DISCUSSION_SET_FOCUS,
            DISCUSSION_GET_FOCUS,
            DISCUSSION_CLEAR_FOCUS,
        ]
    }

    #[test]
    fn test_method_names_are_unique_and_namespaced_camel_case() {
        let mut seen = HashSet::new();
        for method in all_methods() {
            assert!(seen.insert(method), "duplicate method wire name: '{method}'");

            let (namespace, verb) = method
                .split_once('/')
                .unwrap_or_else(|| panic!("method '{method}' must be '<namespace>/<verb>'"));

            assert!(
                !namespace.is_empty() && namespace.chars().all(|c| c.is_ascii_lowercase()),
                "method '{method}' namespace must be non-empty lowercase"
            );
            assert!(
                verb.chars().next().is_some_and(|c| c.is_ascii_lowercase()),
                "method '{method}' verb must start with a lowercase letter"
            );
            assert!(
                !verb.contains('_'),
                "method '{method}' verb must be camelCase, not snake_case"
            );
        }
    }
}
