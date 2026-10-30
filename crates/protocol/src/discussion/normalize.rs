//! Path normalization shared by the discussion wire payloads.

use serde::Deserialize;

use crate::context::ContextAccess;
use crate::discussion::{DirListRequest, DiscussionContextRequest};
use crate::edit::normalize_repo_path;

pub(crate) fn deserialize_null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    let opt = Option::<T>::deserialize(deserializer)?;
    Ok(opt.unwrap_or_default())
}

/// Normalizes a single targeted repository map request.
///
/// An empty path, `.`, or `./` names the whole repository and normalizes to an
/// empty string. Any other path is validated through [`normalize_repo_path`],
/// so a request can never address a location outside the repository; such a
/// path yields `None` and the request is dropped. This is the crate's single
/// shared normalizer for map requests, so every wire boundary applies the same
/// rules.
pub fn normalize_repo_map_path(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_matches('`').trim();
    if trimmed.is_empty() || trimmed == "." || trimmed == "./" {
        return Some(String::new());
    }
    normalize_repo_path(trimmed).ok()
}

/// Deserializes the `repo_maps` field of
/// [`DiscussionResponse`](crate::discussion::DiscussionResponse).
///
/// A `null` payload yields an empty list, and every entry is normalized through
/// [`normalize_repo_map_path`], so a directly decoded response can no longer
/// carry an unvalidated or escaping path.
pub(super) fn deserialize_repo_map_paths<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let paths = Option::<Vec<String>>::deserialize(deserializer)?;
    Ok(paths
        .unwrap_or_default()
        .iter()
        .filter_map(|path| normalize_repo_map_path(path))
        .collect())
}

/// Deserializes the `dir_lists` field of
/// [`DiscussionResponse`](crate::discussion::DiscussionResponse).
///
/// A `null` payload yields an empty list, and every entry is normalized through
/// [`DirListRequest::normalized`], so a directly decoded response can no longer
/// carry an unvalidated or escaping directory path.
pub(super) fn deserialize_dir_list_requests<'de, D>(
    deserializer: D,
) -> Result<Vec<DirListRequest>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let requests = Option::<Vec<DirListRequest>>::deserialize(deserializer)?;
    Ok(requests
        .unwrap_or_default()
        .into_iter()
        .filter_map(DirListRequest::normalized)
        .collect())
}

/// Deserializes the `context_requests` field of
/// [`DiscussionResponse`](crate::discussion::DiscussionResponse), normalizing
/// every path through [`normalize_repo_path`].
///
/// A `null` payload yields an empty list, and any request whose path cannot
/// identify a repository file is dropped, so a directly decoded response can no
/// longer bypass path validation.
pub(super) fn deserialize_context_requests<'de, D>(
    deserializer: D,
) -> Result<Vec<DiscussionContextRequest>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let requests = Option::<Vec<DiscussionContextRequest>>::deserialize(deserializer)?;
    Ok(requests
        .unwrap_or_default()
        .into_iter()
        .filter_map(DiscussionContextRequest::normalized)
        .collect())
}

pub(super) fn deserialize_context_access<'de, D>(deserializer: D) -> Result<ContextAccess, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt = Option::<ContextAccess>::deserialize(deserializer)?;
    Ok(opt.unwrap_or(ContextAccess::ReadOnly))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ContextAccess;
    use crate::discussion::{DirListRequest, DiscussionResponse};

    #[test]
    fn test_discussion_response_normalizes_context_request_paths() {
        let raw = r#"{
            "message": "Need files",
            "context_requests": [
                {"path": "./src/a.rs", "access": "read_only"},
                {"path": "../escape.rs", "access": "read_only"},
                {"path": "crates/core/src/lib.rs", "access": "editable"}
            ]
        }"#;
        let resp: DiscussionResponse = serde_json::from_str(raw).unwrap();
        // The `./` prefix is normalized and the traversal entry is dropped.
        assert_eq!(resp.context_requests.len(), 2);
        assert_eq!(resp.context_requests[0].path, "src/a.rs");
        assert_eq!(resp.context_requests[0].access, ContextAccess::ReadOnly);
        assert_eq!(resp.context_requests[1].path, "crates/core/src/lib.rs");
        assert_eq!(resp.context_requests[1].access, ContextAccess::Editable);

        // A `null` payload still yields an empty list.
        let null_resp: DiscussionResponse =
            serde_json::from_str(r#"{"message":"x","context_requests":null}"#).unwrap();
        assert!(null_resp.context_requests.is_empty());
    }

    #[test]
    fn test_discussion_response_repo_dirs_and_repo_maps() {
        let raw = r#"{
            "message": "Map the subsystem first",
            "repo_dirs": true,
            "repo_maps": [".", "./crates/core/", "../escape.rs", "crates/protocol/"]
        }"#;
        let resp: DiscussionResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(resp.repo_dirs, Some(true));
        // `.` names the whole repository, `./` prefixes collapse, and the
        // traversal entry is dropped instead of escaping the repository.
        assert_eq!(
            resp.repo_maps,
            vec![
                String::new(),
                "crates/core".to_string(),
                "crates/protocol".to_string()
            ]
        );

        // Both fields stay absent by default and survive a serde round-trip.
        let minimal: DiscussionResponse = serde_json::from_str(r#"{"message":"x"}"#).unwrap();
        assert!(minimal.repo_dirs.is_none());
        assert!(minimal.repo_maps.is_empty());
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"repo_dirs\":true"));
        assert!(json.contains("\"repo_maps\":[\"\",\"crates/core\",\"crates/protocol\"]"));
        assert_eq!(
            serde_json::from_str::<DiscussionResponse>(&json).unwrap(),
            resp
        );

        // The builder applies the same normalization as the decoder.
        let built = DiscussionResponse::new("x")
            .with_repo_dirs(false)
            .with_repo_maps(vec![
                "./crates/core/".to_string(),
                "../escape.rs".to_string(),
            ]);
        assert_eq!(built.repo_dirs, Some(false));
        assert_eq!(built.repo_maps, vec!["crates/core".to_string()]);

        assert_eq!(normalize_repo_map_path(""), Some(String::new()));
        assert_eq!(normalize_repo_map_path("  .  "), Some(String::new()));
        assert_eq!(
            normalize_repo_map_path("`crates/core`"),
            Some("crates/core".to_string())
        );
        assert_eq!(normalize_repo_map_path("/etc"), None);
        assert_eq!(normalize_repo_map_path("a/../b"), None);
    }

    #[test]
    fn test_discussion_dir_list_requests_normalize_paths() {
        let raw = r#"{
            "message": "List the subsystem",
            "dir_lists": [
                {"path": "./crates/core/", "recursive": false},
                {"path": ".", "recursive": true},
                {"path": "../escape", "recursive": true}
            ]
        }"#;
        let resp: DiscussionResponse = serde_json::from_str(raw).unwrap();
        // `./` collapses, `.` names the repository root, and the traversal
        // entry is dropped instead of escaping the repository.
        assert_eq!(resp.dir_lists.len(), 2);
        assert_eq!(resp.dir_lists[0].path, "crates/core");
        assert!(!resp.dir_lists[0].recursive);
        assert_eq!(resp.dir_lists[1].path, "");
        assert!(resp.dir_lists[1].recursive);

        // The builder applies the same normalization as the decoder.
        let built = DiscussionResponse::new("x").with_dir_lists(vec![
            DirListRequest::new("./crates/protocol/", true),
            DirListRequest::new("../escape", false),
        ]);
        assert_eq!(
            built.dir_lists,
            vec![DirListRequest::new("crates/protocol", true)]
        );

        // `recursive` defaults to false on the wire and an empty listing stays
        // absent from the encoded payload.
        let shallow: DirListRequest = serde_json::from_str(r#"{"path":"src"}"#).unwrap();
        assert_eq!(shallow, DirListRequest::new("src", false));
        let json = serde_json::to_string(&DiscussionResponse::new("x")).unwrap();
        assert!(!json.contains("dir_lists"));
    }
}
