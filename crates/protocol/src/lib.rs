use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: &str = "0.1.0";

pub mod methods {
    pub const CLIENT_INITIALIZE: &str = "client/initialize";
    pub const REPOSITORY_GET_STATE: &str = "repository/getState";
    pub const REPOSITORY_LIST_FILES: &str = "repository/listFiles";
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

    pub const CONFIG_GET: &str = "config/get";
    pub const CONFIG_SET: &str = "config/set";
}

pub mod events {
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

    pub const GIT_STATE_CHANGED: &str = "git/stateChanged";
    pub const GIT_COMMIT_CREATED: &str = "git/commitCreated";
    pub const GIT_UNDO_COMPLETED: &str = "git/undoCompleted";

    pub const CONFIG_CHANGED: &str = "config/changed";

    pub const TOOLCHAIN_STARTED: &str = "toolchain/started";
    pub const TOOLCHAIN_RESULT: &str = "toolchain/result";
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    Number(u64),
    String(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: RequestId,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: RequestId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ResponseError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseError {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Message {
    Request(Request),
    Response(Response),
    Event(Event),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeParams {
    pub protocol_version: String,
    pub client_name: String,
    pub client_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeResult {
    pub protocol_version: String,
    pub server_name: String,
    pub server_version: String,
    pub repository: Option<RepositoryState>,
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edit_protocol: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_workflows: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_edit_protocols: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_models: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigState {
    pub workflow: String,
    pub edit_protocol: String,
    #[serde(default)]
    pub model: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_workflows: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_edit_protocols: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_models: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConfigSetParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edit_protocol: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepositoryState {
    pub root: String,
    pub branch: String,
    pub head: String,
    pub dirty: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepositoryListFilesResult {
    pub files: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextAccess {
    ReadOnly,
    Editable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextItem {
    pub path: String,
    pub access: ContextAccess,
    pub size_bytes: u64,
    pub estimated_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ContextState {
    pub revision: u64,
    pub items: Vec<ContextItem>,
    pub total_estimated_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextAddParams {
    pub path: String,
    #[serde(default = "default_context_access")]
    pub access: ContextAccess,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextAddPatternParams {
    pub pattern: String,
    #[serde(default = "default_context_access")]
    pub access: ContextAccess,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextAddPatternResult {
    pub added_count: usize,
    pub added_tokens: u64,
    pub state: ContextState,
}

fn default_context_access() -> ContextAccess {
    ContextAccess::ReadOnly
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextRemoveParams {
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextSetAccessParams {
    pub path: String,
    pub access: ContextAccess,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextChangedEvent {
    pub state: ContextState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UiHistoryKind {
    User,
    Assistant,
    System,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiHistoryItem {
    pub id: u64,
    pub kind: UiHistoryKind,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HistoryGetParams {
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub before_id: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryGetResult {
    pub items: Vec<UiHistoryItem>,
    pub has_more: bool,
    pub total_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntryAddedEvent {
    pub item: UiHistoryItem,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelAskParams {
    pub prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelStartedEvent {
    pub operation_id: String,
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelDeltaEvent {
    pub operation_id: String,
    pub delta: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelUsageInfo {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    #[serde(default)]
    pub reasoning_tokens: Option<u32>,
    #[serde(default)]
    pub cached_tokens: Option<u32>,
    #[serde(default)]
    pub cost: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelUsageEvent {
    pub operation_id: String,
    pub usage: ModelUsageInfo,
    pub session_total_cost: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EditOperation {
    Replace {
        path: String,
        old_text: String,
        new_text: String,
    },
    Create {
        path: String,
        content: String,
    },
    Delete {
        path: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditProposal {
    pub summary: String,
    pub edits: Vec<EditOperation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructuredChangeProposal {
    #[serde(default = "default_change_op")]
    pub op: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

fn default_change_op() -> String {
    "replace".to_string()
}

impl StructuredChangeProposal {
    pub fn to_edit_operation(&self) -> EditOperation {
        match self.op.as_str() {
            "create" => EditOperation::Create {
                path: self.path.clone(),
                content: self.content.clone().unwrap_or_default(),
            },
            "delete" => EditOperation::Delete {
                path: self.path.clone(),
            },
            _ => EditOperation::Replace {
                path: self.path.clone(),
                old_text: self.old_text.clone().unwrap_or_default(),
                new_text: self
                    .new_text
                    .clone()
                    .or_else(|| self.content.clone())
                    .unwrap_or_default(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ModelResultProposal {
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub changes: Vec<StructuredChangeProposal>,
    #[serde(default)]
    pub context_requests: Vec<String>,
    #[serde(default)]
    pub suggested_actions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ModelResult {
    Answer {
        text: String,
    },
    Edit {
        summary: String,
        edits: Vec<EditOperation>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        proposal: Option<ModelResultProposal>,
        applied: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(default)]
        changed_files: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        commit_hash: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelResultEvent {
    pub operation_id: String,
    pub result: ModelResult,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<ModelUsageInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_total_cost: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelFinishedEvent {
    pub operation_id: String,
    pub full_text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelErrorEvent {
    pub operation_id: String,
    pub message: String,
}

// Structured Edit Events
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditStartedEvent {
    pub operation_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditFileStartedEvent {
    pub operation_id: String,
    pub path: String,
    pub op_type: String, // "replace", "create", "delete"
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditHunkEvent {
    pub operation_id: String,
    pub path: String,
    pub hunk_index: usize,
    pub old_text: String,
    pub new_text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditFileDoneEvent {
    pub operation_id: String,
    pub path: String,
    pub status: String, // "ok", "error"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub hunks_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditFileRetryingEvent {
    pub operation_id: String,
    pub path: String,
    pub attempt: usize,
    pub max_retries: usize,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditFinishedEvent {
    pub operation_id: String,
    pub applied: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub changed_files: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_hash: Option<String>,
}

// Toolchain-related Protocol Types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolchainStartedEvent {
    pub operation_id: String,
    pub command: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolchainResultEvent {
    pub operation_id: String,
    pub command: String,
    pub success: bool,
    pub output: String,
}

// Git-related Protocol Types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitCommitCreatedEvent {
    pub commit_hash: String,
    pub summary: String,
    pub changed_files: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitUndoResult {
    pub undone_commit: String,
    pub restored_checkpoint: bool,
    pub new_head: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GitDiffParams {
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitDiffResult {
    pub diff: String,
}

/// Matches a relative path against a glob pattern.
pub fn matches_glob_pattern(pattern: &str, path: &str) -> bool {
    let clean_pat = pattern.trim().trim_start_matches("./");
    let clean_path = path.trim().trim_start_matches("./");

    if clean_pat.is_empty() {
        return false;
    }

    if clean_pat.ends_with('/') {
        let dir = clean_pat.trim_end_matches('/');
        return clean_path.starts_with(&format!("{}/", dir));
    }

    if !clean_pat.contains('*') && !clean_pat.contains('?') {
        if clean_path == clean_pat || clean_path.starts_with(&format!("{}/", clean_pat)) {
            return true;
        }
    }

    if !clean_pat.contains('/') && clean_pat.starts_with("*.") {
        let ext = &clean_pat[1..];
        return clean_path.ends_with(ext);
    }

    let pat_parts: Vec<&str> = clean_pat.split('/').collect();
    let path_parts: Vec<&str> = clean_path.split('/').collect();

    match_segments(&pat_parts, &path_parts)
}

fn match_segments(pat: &[&str], path: &[&str]) -> bool {
    if pat.is_empty() {
        return path.is_empty();
    }

    if pat[0] == "**" {
        if match_segments(&pat[1..], path) {
            return true;
        }
        if !path.is_empty() && match_segments(pat, &path[1..]) {
            return true;
        }
        return false;
    }

    if path.is_empty() {
        return false;
    }

    if match_wildcard_string(pat[0], path[0]) {
        return match_segments(&pat[1..], &path[1..]);
    }

    false
}

fn match_wildcard_string(pattern: &str, s: &str) -> bool {
    let p_bytes = pattern.as_bytes();
    let s_bytes = s.as_bytes();
    let (mut p_idx, mut s_idx) = (0, 0);
    let mut star_idx = None;
    let mut match_idx = 0;

    while s_idx < s_bytes.len() {
        if p_idx < p_bytes.len() && (p_bytes[p_idx] == b'?' || p_bytes[p_idx] == s_bytes[s_idx]) {
            p_idx += 1;
            s_idx += 1;
        } else if p_idx < p_bytes.len() && p_bytes[p_idx] == b'*' {
            star_idx = Some(p_idx);
            match_idx = s_idx;
            p_idx += 1;
        } else if let Some(star) = star_idx {
            p_idx = star + 1;
            match_idx += 1;
            s_idx = match_idx;
        } else {
            return false;
        }
    }

    while p_idx < p_bytes.len() && p_bytes[p_idx] == b'*' {
        p_idx += 1;
    }

    p_idx == p_bytes.len()
}
