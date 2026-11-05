use serde::{Deserialize, Serialize};

/// One materialized external document stored in the cache.
///
/// `path` is the logical document path — the declared identifier followed by
/// the document-relative path — so the disk prefix that stores the cache stays
/// inside the engine and never reaches the wire contract or the model context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OuterDocsCacheEntry {
    pub path: String,
    pub size_bytes: u64,
    pub estimated_tokens: u64,
}

/// Deterministically ordered inventory of the cache contents.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OuterDocsCacheListResult {
    #[serde(default)]
    pub entries: Vec<OuterDocsCacheEntry>,
}

/// Logical path of the cached file or directory subtree to remove.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OuterDocsCacheRemoveParams {
    /// Logical path carrying no disk prefix of the cache.
    pub path: String,
}

/// What one cache removal actually dropped from disk.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OuterDocsCacheRemoveResult {
    #[serde(default)]
    pub removed_files: usize,
    #[serde(default)]
    pub removed_bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_list_result_round_trips_through_json() {
        let result = OuterDocsCacheListResult {
            entries: vec![OuterDocsCacheEntry {
                path: "openrouter/chat-completion".to_string(),
                size_bytes: 4096,
                estimated_tokens: 1024,
            }],
        };

        let json = serde_json::to_string(&result).unwrap();
        assert_eq!(
            serde_json::from_str::<OuterDocsCacheListResult>(&json).unwrap(),
            result
        );
    }

    #[test]
    fn test_cache_remove_params_decode_logical_path() {
        // The wire payload names the document exactly as the model and the
        // interface address it: no cache prefix, no filesystem path.
        let params: OuterDocsCacheRemoveParams =
            serde_json::from_str(r#"{"path":"openrouter/stt"}"#).unwrap();
        assert_eq!(params.path, "openrouter/stt");
    }

    #[test]
    fn test_cache_remove_result_defaults_missing_counters() {
        let result: OuterDocsCacheRemoveResult = serde_json::from_str("{}").unwrap();
        assert_eq!(result.removed_files, 0);
        assert_eq!(result.removed_bytes, 0);
    }
}
