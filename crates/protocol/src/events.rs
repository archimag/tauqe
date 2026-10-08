pub const MODEL_STARTED: &str = "model/started";
pub const MODEL_REASONING_DELTA: &str = "model/reasoningDelta";
pub const MODEL_TEXT_DELTA: &str = "model/textDelta";
pub const MODEL_USAGE: &str = "model/usage";
pub const MODEL_RESULT: &str = "model/result";
pub const MODEL_FINISHED: &str = "model/finished";
pub const MODEL_CANCELLED: &str = "model/cancelled";
pub const MODEL_ERROR: &str = "model/error";

pub const CONTEXT_CHANGED: &str = "context/changed";

pub const HISTORY_ENTRY_ADDED: &str = "history/entryAdded";

pub const EDIT_STARTED: &str = "edit/started";
pub const EDIT_FILE_STARTED: &str = "edit/fileStarted";
pub const EDIT_HUNK: &str = "edit/hunk";
pub const EDIT_FILE_DONE: &str = "edit/fileDone";
pub const EDIT_FILE_RETRYING: &str = "edit/fileRetrying";
pub const EDIT_FINISHED: &str = "edit/finished";

pub const TURN_PHASE: &str = "turn/phase";
pub const TOOLCHAIN_STARTED: &str = "toolchain/started";
pub const TOOLCHAIN_FINISHED: &str = "toolchain/finished";

pub const GIT_STATE_CHANGED: &str = "git/stateChanged";
pub const GIT_COMMIT_CREATED: &str = "git/commitCreated";
pub const GIT_UNDO_COMPLETED: &str = "git/undoCompleted";
pub const GIT_SQUASH_COMPLETED: &str = "git/squashCompleted";

pub const CONFIG_CHANGED: &str = "config/changed";

pub const REVIEW_STARTED: &str = "review/started";
pub const REVIEW_REASONING_DELTA: &str = "review/reasoningDelta";
pub const REVIEW_CONTENT_DELTA: &str = "review/contentDelta";
pub const REVIEW_FINISHED: &str = "review/finished";
pub const REVIEW_CANCELLED: &str = "review/cancelled";
pub const REVIEW_ERROR: &str = "review/error";
pub const REVIEW_STATE_CHANGED: &str = "review/stateChanged";

pub const PLAN_UPDATED: &str = "plan/updated";
pub const PLAN_LIST_CHANGED: &str = "plan/listChanged";
