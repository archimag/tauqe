use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RepositoryInitParams {
    #[serde(default)]
    pub initial_commit: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepositoryInitResult {
    pub repository: RepositoryState,
}

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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GitSquashPreviewParams {
    #[serde(default)]
    pub base_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitSquashCommitItem {
    pub hash: String,
    pub author: String,
    pub date: String,
    pub subject: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitSquashFileDiff {
    pub path: String,
    pub diff: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitSquashPreviewResult {
    pub base_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_base: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_base: Option<String>,
    pub commits: Vec<GitSquashCommitItem>,
    pub diff_stat: String,
    pub files: Vec<GitSquashFileDiff>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested_message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitSquashGenerateMessageParams {
    pub base_ref: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitSquashGenerateMessageResult {
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitSquashApplyParams {
    pub base_ref: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitSquashApplyResult {
    pub squashed_commit: String,
    pub message: String,
    pub base_ref: String,
}
