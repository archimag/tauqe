//! Path normalization shared by the discussion wire payloads.
//!
//! Every wire boundary applies the same rules: a request whose path cannot
//! identify a location inside the repository is dropped instead of being
//! forwarded to the engine. `context_requests`, `context_drops`, `repo_maps`,
//! `dir_lists`, and `repo_searches` therefore all decode through the shared
//! normalizers of this module rather than through per-field logic of their own.

use serde::Deserialize;

use crate::context::ContextAccess;
use crate::discussion::{DirListRequest, DiscussionContextRequest, RepoSearchRequest};
use crate::edit::normalize_repo_path;
use crate::edit::proposal::decode_repo_path;

pub(crate) fn deserialize_null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    let opt = Option::<T>::deserialize(deserializer)?;
    Ok(opt.unwrap_or_default())
}

/// Normalizes a single repository-path request of the model-output boundary.
///
/// One rule serves every research list alike: the targeted repository map
/// requests, the directory listings, and the repository searches. The
/// whole-repository root is derived from the decoded segment structure instead
/// of being recognized by spelling: the shared core strips the surrounding
/// markup, decodes the separator escapes, refuses an absolute spelling, and
/// collapses every dot and empty segment, so a spelling that leaves no
/// surviving segment names the whole repository and normalizes to an empty
/// string. Any other path is validated through the same core, so a request
/// can never address a location outside the repository; such a path yields
/// `None` and the request is dropped. This is the crate's single shared
/// normalizer for these paths, so every wire boundary applies the same rules.
pub fn normalize_repo_map_path(raw: &str) -> Option<String> {
    normalize_repo_map_path_checked(raw).ok()
}

/// Normalizes a repository-path request, reporting the rejection diagnostic.
///
/// The decision is derived from the decoded segment set rather than from a list
/// of recognized spellings: the shared `decode_repo_path` core strips the
/// surrounding markup, decodes the separator escapes, refuses an absolute
/// spelling, and collapses every dot and empty segment. A spelling that leaves
/// no surviving segment names the whole repository and yields the empty string,
/// so `.`, `./`, `./.`, `.//`, and their single-backslash forms all reach the
/// same root. A leading separator never names it: a spelling that decodes to a
/// leading forward slash, or to the literal backslash byte the separator
/// doubling escape places at position zero, is absolute and stays a refusal.
///
/// A boundary that cannot drop a malformed entry decodes through this sibling
/// instead of the dropping [`normalize_repo_map_path`]: a typed command field
/// has no representation for "absent", so a rejected path must surface as a
/// decode failure. Both apply the very same rules and differ only in how they
/// report a rejection.
fn normalize_repo_map_path_checked(raw: &str) -> Result<String, String> {
    // The shared core already collapsed the segments, so an empty result is the
    // whole-repository root this boundary reports and every other result is the
    // normalized repository path.
    decode_repo_path(raw)
}

/// Deserializes one repository-map-style path at the wire boundary.
///
/// The decoded path is normalized and validated through
/// [`normalize_repo_map_path_checked`], so a field deserialized with this
/// adapter can never address a location outside the repository. An escaping
/// path is reported as a decode failure naming it instead of reaching the
/// engine, because a typed command field cannot silently drop itself. This is a
/// first line of defence only: the engine still confines every path against the
/// repository root.
pub(crate) fn deserialize_repo_map_path<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    normalize_repo_map_path_checked(&raw).map_err(serde::de::Error::custom)
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

/// Deserializes the `repo_searches` field of
/// [`DiscussionResponse`](crate::discussion::DiscussionResponse).
///
/// A `null` payload yields an empty list, and every entry is normalized through
/// [`RepoSearchRequest::normalized`], so a directly decoded response can no
/// longer carry an unvalidated or escaping search path.
pub(crate) fn deserialize_repo_search_requests<'de, D>(
    deserializer: D,
) -> Result<Vec<RepoSearchRequest>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let requests = Option::<Vec<RepoSearchRequest>>::deserialize(deserializer)?;
    Ok(requests
        .unwrap_or_default()
        .into_iter()
        .filter_map(RepoSearchRequest::normalized)
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

/// Deserializes a null-tolerant list of repository-relative file paths.
///
/// One rule serves every list of repository files that tolerates an explicit
/// `null`: a `null` payload yields an empty list, and every entry is normalized
/// and validated through [`normalize_repo_path`], so a list decoded with this
/// adapter can no longer carry an unvalidated or escaping path. An entry that
/// cannot identify a repository file (empty, `.`, absolute, a drive path, a
/// `..` traversal component, or a NUL byte) is dropped instead of being
/// forwarded, while the usable entries keep their order and their duplicates.
/// The `context_drops` field of the model-facing result proposal decodes through
/// this adapter.
pub(crate) fn deserialize_repo_path_list<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let paths = Option::<Vec<String>>::deserialize(deserializer)?;
    Ok(paths
        .unwrap_or_default()
        .into_iter()
        .filter_map(|path| normalize_repo_path(&path).ok())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ContextAccess;
    use crate::discussion::{DirListRequest, DiscussionResponse, RepoSearchRequest};

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
            "repo_maps": [".", "./.", "./crates/core/", "../escape.rs", "crates/protocol/"]
        }"#;
        let resp: DiscussionResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(resp.repo_dirs, Some(true));
        // `.` and `./.` both name the whole repository through the derived
        // segment rule, `./` prefixes collapse, and the traversal entry is
        // dropped instead of escaping the repository.
        assert_eq!(
            resp.repo_maps,
            vec![
                String::new(),
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
        assert!(json.contains("\"repo_maps\":[\"\",\"\",\"crates/core\",\"crates/protocol\"]"));
        assert_eq!(
            serde_json::from_str::<DiscussionResponse>(&json).unwrap(),
            resp
        );

        // The builder applies the same normalization as the decoder.
        let built = DiscussionResponse::new("x")
            .with_repo_dirs(false)
            .with_repo_maps(vec![
                "./.".to_string(),
                "./crates/core/".to_string(),
                "../escape.rs".to_string(),
            ]);
        assert_eq!(built.repo_dirs, Some(false));
        assert_eq!(built.repo_maps.len(), 2);
        assert!(built.repo_maps[0].is_empty());
        assert_eq!(built.repo_maps[1], "crates/core");

        assert_eq!(normalize_repo_map_path(""), Some(String::new()));
        assert_eq!(normalize_repo_map_path("."), Some(String::new()));
        assert_eq!(normalize_repo_map_path("./"), Some(String::new()));
        assert_eq!(normalize_repo_map_path("  .  "), Some(String::new()));
        assert_eq!(normalize_repo_map_path("./."), Some(String::new()));
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
                {"path": "./.", "recursive": false},
                {"path": "./crates/core/", "recursive": false},
                {"path": ".", "recursive": true},
                {"path": "../escape", "recursive": true}
            ]
        }"#;
        let resp: DiscussionResponse = serde_json::from_str(raw).unwrap();
        // A `./.` composition leaves no surviving segment and names the
        // repository root, `./` collapses, `.` names the root too, and the
        // traversal entry is dropped instead of escaping the repository.
        assert_eq!(resp.dir_lists.len(), 3);
        assert_eq!(resp.dir_lists[0].path, "");
        assert!(!resp.dir_lists[0].recursive);
        assert_eq!(resp.dir_lists[1].path, "crates/core");
        assert!(!resp.dir_lists[1].recursive);
        assert_eq!(resp.dir_lists[2].path, "");
        assert!(resp.dir_lists[2].recursive);

        // The builder applies the same normalization as the decoder.
        let built = DiscussionResponse::new("x").with_dir_lists(vec![
            DirListRequest::new("./.", false),
            DirListRequest::new("./crates/protocol/", true),
            DirListRequest::new("../escape", false),
        ]);
        let listed = &built.dir_lists;
        assert_eq!(listed.len(), 2);
        assert!(listed[0].path.is_empty());
        assert!(!listed[0].recursive);
        assert_eq!(listed[1].path, "crates/protocol");
        assert!(listed[1].recursive);

        // `recursive` defaults to false on the wire and an empty listing stays
        // absent from the encoded payload.
        let shallow: DirListRequest = serde_json::from_str(r#"{"path":"src"}"#).unwrap();
        assert_eq!(shallow, DirListRequest::new("src", false));
        let json = serde_json::to_string(&DiscussionResponse::new("x")).unwrap();
        assert!(!json.contains("dir_lists"));
    }

    #[test]
    fn test_discussion_repo_searches_normalize_paths() {
        let raw = r#"{
            "message": "Search the subsystem",
            "repo_searches": [
                {"path": "./.", "regex": "pub fn"},
                {"path": "./crates/core/", "regex": "fn run"},
                {"path": ".", "regex": "pub fn"},
                {"path": "../escape", "regex": "secret"},
                {"path": "/etc/passwd", "regex": "root"}
            ]
        }"#;
        let resp: DiscussionResponse = serde_json::from_str(raw).unwrap();
        // A `./.` composition leaves no surviving segment and names the
        // repository root, `./` collapses, `.` names the root too, and the
        // traversal and absolute entries are dropped instead of escaping the
        // repository.
        assert_eq!(resp.repo_searches.len(), 3);
        assert_eq!(resp.repo_searches[0].path, "");
        assert_eq!(resp.repo_searches[0].regex, "pub fn");
        assert_eq!(resp.repo_searches[1].path, "crates/core");
        assert_eq!(resp.repo_searches[1].regex, "fn run");
        assert_eq!(resp.repo_searches[2].path, "");
        assert_eq!(resp.repo_searches[2].regex, "pub fn");

        // A `null` payload still yields an empty list.
        let null_resp: DiscussionResponse =
            serde_json::from_str(r#"{"message":"x","repo_searches":null}"#).unwrap();
        assert!(null_resp.repo_searches.is_empty());

        // The builder applies the same normalization as the decoder.
        let built = DiscussionResponse::new("x").with_repo_searches(vec![
            RepoSearchRequest::new("./.", "pub fn"),
            RepoSearchRequest::new("./crates/core/", "fn run"),
            RepoSearchRequest::new("../escape", "secret"),
        ]);
        let searched = &built.repo_searches;
        assert_eq!(searched.len(), 2);
        assert!(searched[0].path.is_empty());
        assert_eq!(searched[0].regex, "pub fn");
        assert_eq!(searched[1].path, "crates/core");
        assert_eq!(searched[1].regex, "fn run");

        // An accepted request survives a full round-trip unchanged.
        let valid = RepoSearchRequest::new("crates/protocol", r"pub fn \w+");
        let encoded = serde_json::to_string(
            &DiscussionResponse::new("x").with_repo_searches(vec![valid.clone()]),
        )
        .unwrap();
        let decoded: DiscussionResponse = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.repo_searches, vec![valid]);
    }

    #[test]
    fn test_repo_path_list_adapter_normalizes_and_drops_unusable_entries() {
        #[derive(serde::Deserialize)]
        struct Payload {
            #[serde(default, deserialize_with = "deserialize_repo_path_list")]
            paths: Vec<String>,
        }

        let payload: Payload = serde_json::from_str(
            r#"{
                "paths": [
                    "./src/a.rs",
                    "src\\b.rs",
                    "crates/core/",
                    "`src/c.rs`",
                    "./src/a.rs",
                    "",
                    ".",
                    "/etc/passwd",
                    "../escape.rs",
                    "a/../b.rs"
                ]
            }"#,
        )
        .unwrap();
        // Benign spellings arrive clean and in order, a duplicate stays a
        // duplicate, and every entry that cannot identify a repository file is
        // dropped instead of being forwarded to the engine.
        assert_eq!(
            payload.paths,
            vec![
                "src/a.rs".to_string(),
                "src/b.rs".to_string(),
                "crates/core".to_string(),
                "src/c.rs".to_string(),
                "src/a.rs".to_string(),
            ]
        );

        // A JSON null decodes to an empty list and an absent field keeps the
        // empty default, exactly like the sibling null-tolerant adapters.
        let null: Payload = serde_json::from_str(r#"{"paths": null}"#).unwrap();
        assert!(null.paths.is_empty());
        let absent: Payload = serde_json::from_str("{}").unwrap();
        assert!(absent.paths.is_empty());

        // The list shape is still enforced, so a scalar stays a decode error.
        let scalar = serde_json::from_str::<Payload>(r#"{"paths": "src/a.rs"}"#);
        assert!(scalar.is_err());
    }

    #[test]
    fn test_normalize_repo_map_path_strips_surrounding_backticks() {
        // The whole-repository spellings keep naming the repository root when
        // a producer wraps them in markdown backticks.
        assert_eq!(normalize_repo_map_path("` . `"), Some(String::new()));
        assert_eq!(normalize_repo_map_path("`./`"), Some(String::new()));
        assert_eq!(normalize_repo_map_path(" `` "), Some(String::new()));
        assert_eq!(
            normalize_repo_map_path("`./crates/core/`"),
            Some("crates/core".to_string())
        );

        // An interior backtick stays untouched, and a wrapped escaping
        // spelling is dropped exactly like its bare form.
        assert_eq!(
            normalize_repo_map_path("src/`odd`/a.rs"),
            Some("src/`odd`/a.rs".to_string())
        );
        assert_eq!(normalize_repo_map_path("`../escape`"), None);
    }

    #[test]
    fn test_repo_map_root_is_derived_from_the_decoded_segments() {
        // A spelling that leaves no surviving segment names the repository root
        // whatever dot and separator composition produces it, so the sibling
        // that drops malformed entries keeps the request and the sibling that
        // reports a rejection accepts it.
        for raw in ["", ".", "./", "./.", "././", ".//", r".\."] {
            assert_eq!(normalize_repo_map_path(raw), Some(String::new()));
            assert_eq!(normalize_repo_map_path_checked(raw), Ok(String::new()));
        }

        // The single-backslash-separator composition reaches the same root, and
        // the surrounding markup never hides it.
        for raw in [r".\.\", " `./.` ", "`././`", "`.//`"] {
            assert_eq!(normalize_repo_map_path(raw), Some(String::new()));
            assert_eq!(normalize_repo_map_path_checked(raw), Ok(String::new()));
        }

        // A leading separator never names the repository root: a forward slash
        // is absolute, and so is the literal backslash byte the separator
        // doubling escape places at position zero. A UNC spelling, a lone
        // doubled backslash, and the file-system root stay refusals.
        for raw in ["/", "////", r"\\server\share", r"\\?\C:\x", r"\\"] {
            assert_eq!(normalize_repo_map_path(raw), None);
            assert!(normalize_repo_map_path_checked(raw).is_err());
        }

        // The rejection names the absolute spelling instead of reporting a
        // missing segment, so a refusal is never mistaken for an empty request.
        let rejection = normalize_repo_map_path_checked(r"\\server\share")
            .expect_err("an absolute spelling must be rejected");
        assert!(rejection.contains("repository-relative"));
    }
}
