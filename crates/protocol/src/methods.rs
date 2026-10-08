pub const CLIENT_INITIALIZE: &str = "client/initialize";
pub const REPOSITORY_GET_STATE: &str = "repository/getState";
pub const REPOSITORY_LIST_FILES: &str = "repository/listFiles";
pub const REPOSITORY_INIT: &str = "repository/init";
pub const MODEL_ASK: &str = "model/ask";
pub const MODEL_CANCEL: &str = "model/cancel";
pub const MODEL_CLEAR_HISTORY: &str = "model/clearHistory";

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
pub const CREDENTIALS_CREATE_STUB: &str = "credentials/create_stub";

pub const SYSTEM_STATUS: &str = "system/status";

pub const REVIEW_START: &str = "review/start";
pub const REVIEW_CANCEL: &str = "review/cancel";
pub const REVIEW_GET: &str = "review/get";
pub const REVIEW_UPDATE_ITEM: &str = "review/updateItem";
pub const REVIEW_CLEAR: &str = "review/clear";
