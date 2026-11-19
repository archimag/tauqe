//! Path normalization shared by the discussion wire payloads.
//!
//! Every wire boundary applies the same rules: a request whose path cannot
//! identify a location inside the repository is dropped instead of being
//! retained in a decoded proposal. Context paths and repository searches
//! decode through the shared
//! normalizers of this module rather than through per-field logic of their own.

use serde::Deserialize;

use crate::context::ContextAccess;
use crate::discussion::RepoSearchRequest;
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
pub(crate) fn normalize_repo_map_path(raw: &str) -> Option<String> {
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

/// Deserializes the `repo_searches` field of
/// [`ModelResultProposal`](crate::ModelResultProposal).
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
