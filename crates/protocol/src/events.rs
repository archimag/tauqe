pub const MODEL_STARTED: &str = "model/started";
pub const MODEL_REASONING_DELTA: &str = "model/reasoningDelta";
pub const MODEL_TEXT_DELTA: &str = "model/textDelta";
pub const MODEL_CONNECTION_RETRYING: &str = "model/connectionRetrying";
pub const MODEL_STREAM_RESET: &str = "model/streamReset";
pub const MODEL_USAGE: &str = "model/usage";
pub const MODEL_USAGE_TOTALS: &str = "model/usageTotals";
pub const MODEL_RESULT: &str = "model/result";
pub const MODEL_FINISHED: &str = "model/finished";
pub const MODEL_CANCELLED: &str = "model/cancelled";
pub const MODEL_ERROR: &str = "model/error";
pub const MODEL_CHANGED: &str = "model/changed";
pub const MODEL_ESCALATED: &str = "model/escalated";

pub const CONTEXT_CHANGED: &str = "context/changed";
pub const CONFIG_CHANGED: &str = "config/changed";

pub const HISTORY_ENTRY_ADDED: &str = "history/entryAdded";
pub const HISTORY_COMPACTED: &str = "history/compacted";
pub const HISTORY_ENTRY_ROLLED_BACK: &str = "history/entryRolledBack";

pub const EDIT_STARTED: &str = "edit/started";
pub const EDIT_FILE_STARTED: &str = "edit/fileStarted";
pub const EDIT_HUNK: &str = "edit/hunk";
pub const EDIT_FILE_DONE: &str = "edit/fileDone";
pub const EDIT_FILE_RETRYING: &str = "edit/fileRetrying";
pub const EDIT_FINISHED: &str = "edit/finished";

pub const TURN_PHASE: &str = "turn/phase";
pub const TURN_REPOMAP: &str = "turn/repoMap";
pub const TOOLCHAIN_STARTED: &str = "toolchain/started";
pub const TOOLCHAIN_FINISHED: &str = "toolchain/finished";

pub const GIT_STATE_CHANGED: &str = "git/stateChanged";
pub const GIT_COMMIT_CREATED: &str = "git/commitCreated";
pub const GIT_SQUASH_COMPLETED: &str = "git/squashCompleted";

pub const REVIEW_STARTED: &str = "review/started";
pub const REVIEW_REASONING_DELTA: &str = "review/reasoningDelta";
pub const REVIEW_CONTENT_DELTA: &str = "review/contentDelta";
pub const REVIEW_FINISHED: &str = "review/finished";
pub const REVIEW_CANCELLED: &str = "review/cancelled";
pub const REVIEW_ERROR: &str = "review/error";
pub const REVIEW_UPDATED: &str = "review/updated";
pub const REVIEW_LIST_CHANGED: &str = "review/listChanged";
pub const REVIEW_GROUP_STARTED: &str = "review/groupStarted";
pub const REVIEW_GROUP_FINISHED: &str = "review/groupFinished";
pub const REVIEW_ANALYZE_STARTED: &str = "review/analyzeStarted";
pub const REVIEW_ANALYZE_FINISHED: &str = "review/analyzeFinished";
pub const REVIEW_BATCH_FINISHED: &str = "review/batchFinished";

pub const PLAN_UPDATED: &str = "plan/updated";
pub const PLAN_LIST_CHANGED: &str = "plan/listChanged";
pub const PLAN_BATCH_FINISHED: &str = "plan/batchFinished";

pub const DISCUSSION_FOCUS_CHANGED: &str = "discussion/focusChanged";
