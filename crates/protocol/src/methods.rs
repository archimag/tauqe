pub const CLIENT_INITIALIZE: &str = "client/initialize";
pub const REPOSITORY_GET_STATE: &str = "repository/getState";
pub const REPOSITORY_LIST_FILES: &str = "repository/listFiles";
pub const REPOSITORY_INIT: &str = "repository/init";
pub const MODEL_ASK: &str = "model/ask";
pub const MODEL_CANCEL: &str = "model/cancel";
pub const MODEL_CLEAR_HISTORY: &str = "model/clearHistory";
pub const MODEL_SELECT: &str = "model/select";
pub const MODEL_CURRENT: &str = "model/current";
pub const MODEL_USAGE_RESET: &str = "model/usageReset";

pub const CONTEXT_GET: &str = "context/get";
pub const CONTEXT_ADD: &str = "context/add";
pub const CONTEXT_ADD_PATTERN: &str = "context/addPattern";
pub const CONTEXT_REMOVE: &str = "context/remove";
pub const CONTEXT_SET_ACCESS: &str = "context/setAccess";
pub const CONTEXT_CLEAR: &str = "context/clear";

pub const HISTORY_GET: &str = "history/get";

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
pub const REVIEW_PREVIEW: &str = "review/preview";
pub const REVIEW_CANCEL: &str = "review/cancel";
pub const REVIEW_LIST: &str = "review/list";
pub const REVIEW_GET: &str = "review/get";
pub const REVIEW_UPDATE_ITEM: &str = "review/updateItem";
pub const REVIEW_DELETE: &str = "review/delete";
pub const REVIEW_EXECUTE: &str = "review/execute";
pub const REVIEW_ANALYZE: &str = "review/analyze";

pub const PLAN_LIST: &str = "plan/list";
pub const PLAN_GET: &str = "plan/get";
pub const PLAN_SAVE: &str = "plan/save";
pub const PLAN_UPDATE_ITEM: &str = "plan/updateItem";
pub const PLAN_DELETE: &str = "plan/delete";
pub const PLAN_SET_ACTIVE: &str = "plan/setActive";
pub const PLAN_CLEAR_HORIZON: &str = "plan/clearHorizon";
pub const PLAN_EXECUTE: &str = "plan/execute";
pub const PLAN_EXECUTE_PIPELINE: &str = "plan/executePipeline";
pub const PLAN_CREATE_PIPELINE: &str = "plan/createPipeline";
pub const PLAN_REFINE: &str = "plan/refine";
pub const PLAN_REPLAN: &str = "plan/replan";

pub const DISCUSSION_SET_FOCUS: &str = "discussion/setFocus";
pub const DISCUSSION_GET_FOCUS: &str = "discussion/getFocus";
pub const DISCUSSION_CLEAR_FOCUS: &str = "discussion/clearFocus";

pub const LOCALE_GENERATE: &str = "locale/generate";

pub const OUTER_DOCS_CACHE_LIST: &str = "outerdocs/cacheList";
pub const OUTER_DOCS_CACHE_REMOVE: &str = "outerdocs/cacheRemove";

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Derives the wire name of every method constant declared in this module's
    /// own source, so a newly declared method is checked without being listed
    /// here: each name must be unique and must follow the
    /// `<lowercase-namespace>/<camelCaseVerb>` convention.
    fn declared_methods() -> Vec<&'static str> {
        let source = include_str!("methods.rs");
        // The test module is cut away first, so the fixtures below can never be
        // mistaken for a declaration. The index comes from a matched ASCII
        // marker, so the slice always lands on a character boundary.
        let declarations = match source.find("#[cfg(test)]") {
            Some(index) => &source[..index],
            None => source,
        };
        declarations.lines().filter_map(wire_name_of).collect()
    }

    /// Returns the wire name declared by one `pub const NAME: &str = "...";`
    /// line, or `None` for every other line.
    fn wire_name_of(line: &str) -> Option<&str> {
        let declaration = line.trim_start().strip_prefix("pub const ")?;
        let (_, literal) = declaration.split_once(": &str = \"")?;
        let (value, _) = literal.split_once('"')?;
        Some(value)
    }

    #[test]
    fn test_method_names_are_unique_and_namespaced_camel_case() {
        let methods = declared_methods();

        // A derivation that silently matched nothing would make every assertion
        // below vacuous, so it is anchored on the first and the last constant
        // this module declares.
        for anchor in [CLIENT_INITIALIZE, OUTER_DOCS_CACHE_REMOVE] {
            assert!(
                methods.contains(&anchor),
                "derivation missed the declaration of '{anchor}'"
            );
        }

        let mut seen = HashSet::new();
        for method in methods {
            assert!(
                seen.insert(method),
                "duplicate method wire name: '{method}'"
            );

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
