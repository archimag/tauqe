//! Dispatch classification for RPC methods.
//!
//! The stdin read loop must stay free while a request runs. A `model/cancel`
//! and a closing stdin are observed only by a loop that keeps draining stdin, so
//! a handler awaited inline holds cancellation and shutdown for as long as it
//! runs, while a handler spawned off the loop leaves both reachable.
//!
//! Enumerating the long-running methods by hand inverts the failure mode of that
//! decision. Such a list has to grow with every new handler, and a forgotten
//! entry silently parks the loop behind a provider stream or a subprocess chain.
//! This module therefore classifies by default: `dispatch_class` answers
//! `DispatchClass::Offload` for every method outside the curated `INLINE_METHODS`
//! table, so a handler wired into the dispatcher without a reviewed
//! classification costs one task spawn instead of a stalled read loop.
//!
//! Inline membership carries a proof obligation. A method belongs in
//! `INLINE_METHODS` only when its handler claims no operation slot across
//! subprocess, provider-stream, or `fsync`-competing work, and performs no
//! multi-probe filesystem walk. The model turns, the batch supervisors, the Git
//! history rewrites, and the repository lifecycle probes are all classified by
//! the default for exactly that reason, and the guard tests pin the inline set so
//! a reclassification has to be reviewed instead of drifting in unnoticed.

use tauqe_protocol::methods;

/// How the stdin read loop runs one RPC method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchClass {
    /// Spawn the handler off the read loop, so the loop keeps draining stdin and
    /// observes `model/cancel` and EOF while the call runs.
    Offload,
    /// Await the handler inline, which keeps the client's response order without
    /// a task spawn.
    Inline,
}

/// Methods whose handlers are reviewed as short enough to run inline.
///
/// Every entry satisfies the proof obligation stated in the module
/// documentation. The table is a curated allowlist, not the classification
/// itself: a method that is absent runs offloaded.
const INLINE_METHODS: &[&str] = &[
    // Repository reads.
    methods::REPOSITORY_GET_STATE,
    methods::REPOSITORY_LIST_FILES,
    methods::GIT_GET_DIFF,
    // Context curation.
    methods::CONTEXT_GET,
    methods::CONTEXT_ADD,
    methods::CONTEXT_ADD_PATTERN,
    methods::CONTEXT_REMOVE,
    methods::CONTEXT_SET_ACCESS,
    methods::CONTEXT_CLEAR,
    // Configuration and model selection.
    methods::CONFIG_GET,
    methods::CONFIG_SET,
    methods::MODEL_CANCEL,
    methods::MODEL_SELECT,
    methods::MODEL_CURRENT,
    methods::MODEL_USAGE_RESET,
    methods::MODEL_CLEAR_HISTORY,
    // Discussion focus.
    methods::DISCUSSION_SET_FOCUS,
    methods::DISCUSSION_GET_FOCUS,
    methods::DISCUSSION_CLEAR_FOCUS,
    // Plan records.
    methods::PLAN_LIST,
    methods::PLAN_GET,
    methods::PLAN_SAVE,
    methods::PLAN_UPDATE_ITEM,
    methods::PLAN_DELETE,
    methods::PLAN_SET_ACTIVE,
    methods::PLAN_CLEAR_HORIZON,
    // Review records.
    methods::REVIEW_CANCEL,
    methods::REVIEW_LIST,
    methods::REVIEW_GET,
    methods::REVIEW_UPDATE_ITEM,
    methods::REVIEW_DELETE,
];

/// Classifies one RPC method for the stdin read loop.
///
/// Default-offload: only a method named in `INLINE_METHODS` runs inline, so an
/// unreviewed method can never be awaited by the loop that has to observe
/// cancels and EOF.
pub fn dispatch_class(method: &str) -> DispatchClass {
    if INLINE_METHODS.contains(&method) {
        DispatchClass::Inline
    } else {
        DispatchClass::Offload
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// A model-backed request resolves a provider, claims the operation slot,
    /// and streams a completion across `.await`s, so none of them is short
    /// enough to run inline. A streaming handler once ran inline by omission
    /// (`review/analyze`); the default classification makes that direction of
    /// drift impossible, and this test keeps the reviewed set explicit.
    #[test]
    fn model_backed_requests_are_offloaded() {
        for method in [
            methods::MODEL_ASK,
            methods::PLAN_REFINE,
            methods::PLAN_REPLAN,
            methods::PLAN_EXECUTE,
            methods::PLAN_EXECUTE_PIPELINE,
            methods::REVIEW_START,
            methods::REVIEW_EXECUTE,
            methods::REVIEW_ANALYZE,
            methods::LOCALE_GENERATE,
            methods::GIT_SQUASH_GENERATE_MESSAGE,
        ] {
            assert_eq!(
                dispatch_class(method),
                DispatchClass::Offload,
                "'{method}' must be offloaded"
            );
        }
    }

    /// A repository lifecycle request reloads configuration, re-anchors the
    /// context, and rebuilds the history manager from disk, `git/squashApply`
    /// rewrites history behind an `fsync` barrier, and a `history/get` page read
    /// competes with a bounded turn for the session journal. Awaiting any of
    /// them inline would hold the read loop for the whole call.
    #[test]
    fn long_running_repository_requests_are_offloaded() {
        for method in [
            methods::CLIENT_INITIALIZE,
            methods::REPOSITORY_INIT,
            methods::GIT_SQUASH_APPLY,
            methods::HISTORY_GET,
            // The review window projection lists the repository and reads every
            // matched file, so it is never short enough to run inline.
            methods::REVIEW_PREVIEW,
        ] {
            assert_eq!(
                dispatch_class(method),
                DispatchClass::Offload,
                "'{method}' must be offloaded"
            );
        }
    }

    /// The direct regression for the finding. `git/squashPreview` claims the
    /// operation slot across a commit listing, a diff-stat, and a cumulative
    /// file diff, and the repository probe family walks the same multi-probe
    /// filesystem and git surface that made `client/initialize` long-running.
    /// Inline they park the read loop for as long as their subprocesses run, so
    /// neither a cancel nor stdin EOF can reach it.
    #[test]
    fn slot_claiming_probe_handlers_are_offloaded() {
        for method in [
            methods::GIT_SQUASH_PREVIEW,
            methods::SYSTEM_STATUS,
            methods::CONFIG_RELOAD,
            methods::CONFIG_CREATE,
            methods::CREDENTIALS_SAVE,
            methods::CREDENTIALS_CREATE_STUB,
        ] {
            assert_eq!(
                dispatch_class(method),
                DispatchClass::Offload,
                "'{method}' must be offloaded"
            );
        }
    }

    /// An unreviewed method is offloaded, so the dangerous direction of drift —
    /// a handler wired into the dispatcher that nobody classified — costs one
    /// task spawn instead of a stalled read loop.
    #[test]
    fn unreviewed_methods_default_to_offload() {
        assert_eq!(dispatch_class("nonexistent/method"), DispatchClass::Offload);
    }

    /// Inline membership preserves the client's request order, so a short
    /// handler that a caller depends on being answered in sequence must not
    /// drift across the boundary.
    #[test]
    fn reviewed_short_handlers_stay_inline() {
        assert_eq!(dispatch_class(methods::CONFIG_GET), DispatchClass::Inline);
        assert_eq!(
            dispatch_class(methods::MODEL_CANCEL),
            DispatchClass::Inline,
            "a cancel has to be answered while a turn runs offloaded"
        );
    }

    /// A duplicate entry carries no information and hides which entry a
    /// reclassification actually changed.
    #[test]
    fn inline_table_has_no_duplicates() {
        let mut seen = HashSet::new();
        for method in INLINE_METHODS {
            assert!(seen.insert(*method), "duplicate inline method: '{method}'");
        }
    }

    /// Pins the exact inline set. Moving a method across the boundary changes
    /// whether the read loop can observe a cancel or EOF while it runs, so the
    /// move has to show up here as a reviewed change instead of silent drift.
    #[test]
    fn inline_table_is_pinned_to_the_reviewed_set() {
        let mut actual: Vec<&str> = INLINE_METHODS.to_vec();
        actual.sort_unstable();

        let mut expected = vec![
            methods::REPOSITORY_GET_STATE,
            methods::REPOSITORY_LIST_FILES,
            methods::GIT_GET_DIFF,
            methods::CONTEXT_GET,
            methods::CONTEXT_ADD,
            methods::CONTEXT_ADD_PATTERN,
            methods::CONTEXT_REMOVE,
            methods::CONTEXT_SET_ACCESS,
            methods::CONTEXT_CLEAR,
            methods::CONFIG_GET,
            methods::CONFIG_SET,
            methods::MODEL_CANCEL,
            methods::MODEL_SELECT,
            methods::MODEL_CURRENT,
            methods::MODEL_USAGE_RESET,
            methods::MODEL_CLEAR_HISTORY,
            methods::DISCUSSION_SET_FOCUS,
            methods::DISCUSSION_GET_FOCUS,
            methods::DISCUSSION_CLEAR_FOCUS,
            methods::PLAN_LIST,
            methods::PLAN_GET,
            methods::PLAN_SAVE,
            methods::PLAN_UPDATE_ITEM,
            methods::PLAN_DELETE,
            methods::PLAN_SET_ACTIVE,
            methods::PLAN_CLEAR_HORIZON,
            methods::REVIEW_CANCEL,
            methods::REVIEW_LIST,
            methods::REVIEW_GET,
            methods::REVIEW_UPDATE_ITEM,
            methods::REVIEW_DELETE,
        ];
        expected.sort_unstable();

        assert_eq!(actual, expected);
    }
}
