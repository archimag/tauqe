//! The external reference registry declared by `.tauqe/refs/refs.md`.
//!
//! A project keeps authoritative external material — upstream API notes,
//! protocol summaries, vendored `llm.txt` transcripts — outside the repository
//! and refers to it through one declarative manifest. The manifest is tracked by
//! Git and carries the only durable description of what the project treats as a
//! reference, while every document it names is copied into the isolated cache
//! (`.tauqe/cache/refs/`) that Git ignores. The snapshot the model reads is
//! therefore immutable for the whole task and never mixes with the working
//! tree.
//!
//! # Manifest grammar
//!
//! The manifest is a declared, line-oriented data format, not prose a reader
//! interprets. Exactly one entry occupies one line:
//!
//! ```text
//! - [title](name) = source
//! ```
//!
//! - `title` is the human-readable label the model sees in the showcase.
//! - `name` is a clean repository-relative path written with `/`: it is
//!   non-empty, never absolute, carries neither a `.` nor a `..` segment, and
//!   holds no backslash. It identifies the entry, so the cached copy is stored
//!   under exactly this path.
//! - `source` is either an `http://`/`https://` URL or an absolute local path.
//!
//! Whitespace around the delimiters is insignificant. Every line that does not
//! carry this shape — a heading, a paragraph, a note, a markdown link without a
//! source — is prose: it carries no registry meaning and is skipped silently, so
//! the manifest can document itself around its entries.
//!
//! A line that matches the shape but declares an unusable name or source is
//! never repaired silently: it is dropped and reported as a diagnostic while the
//! rest of the registry stays usable. A repeated name keeps its first
//! declaration and is reported the same way.

pub mod materialize;

use std::path::Path;

/// Repository-relative path of the registry manifest.
pub const REFS_MANIFEST: &str = ".tauqe/refs/refs.md";

/// Repository-relative directory holding the tracked registry material.
pub const REFS_DIR: &str = ".tauqe/refs";

/// Repository-relative directory holding the isolated, Git-ignored snapshot.
pub const REFS_CACHE_DIR: &str = ".tauqe/cache/refs";

/// Where the harness obtains the content of one declared reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefsSource {
    /// A remote document addressed by an `http://` or `https://` URL.
    Url(String),
    /// A local document named by an absolute path.
    LocalPath(String),
}

/// One reference declared by the manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefsEntry {
    /// Human-readable label rendered in the showcase the model reads.
    pub title: String,
    /// Clean repository-relative name that identifies the entry.
    pub name: String,
    /// Where the content of the entry comes from.
    pub source: RefsSource,
}

/// The registry as the harness reads it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefsRegistry {
    /// Declared entries, in the order the manifest lists them.
    pub entries: Vec<RefsEntry>,
    /// Rejections reported while reading, in the order they were found.
    pub diagnostics: Vec<String>,
}

/// Reads the registry manifest of `repo_root`.
///
/// The read is blocking file-system work, so it runs off the async executor
/// thread through [`crate::blocking::run_blocking`]. A repository without a
/// manifest simply declares no references, which is a legitimate state and
/// produces neither entries nor diagnostics. A manifest that exists but cannot
/// be read is reported as a diagnostic and recorded in the semantic journal,
/// so the degradation stays observable without aborting a turn.
pub fn load_registry(repo_root: &Path) -> RefsRegistry {
    crate::blocking::run_blocking(|| {
        let path = repo_root.join(REFS_MANIFEST);
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                let (entries, diagnostics) = parse_manifest(&content);
                RefsRegistry {
                    entries,
                    diagnostics,
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => RefsRegistry::default(),
            Err(error) => {
                tracing::warn!(
                    manifest = REFS_MANIFEST,
                    error = %error,
                    "the reference registry manifest could not be read"
                );
                RefsRegistry {
                    entries: Vec::new(),
                    diagnostics: vec![format!(
                        "the registry manifest `{REFS_MANIFEST}` could not be read: {error}"
                    )],
                }
            }
        }
    })
}

/// Parses the manifest `content` into its entries and diagnostics.
///
/// Entries keep the order the manifest declares them in. Prose is skipped
/// silently, while an unusable name, an unusable source, and a repeated name
/// each drop exactly one line and add exactly one diagnostic naming the line, so
/// a malformed entry never hides the entries around it.
pub fn parse_manifest(content: &str) -> (Vec<RefsEntry>, Vec<String>) {
    let mut entries: Vec<RefsEntry> = Vec::new();
    let mut diagnostics: Vec<String> = Vec::new();

    for (index, line) in content.lines().enumerate() {
        let line_number = index + 1;
        let Some((title, name, source)) = split_entry(line) else {
            continue;
        };
        let name = name.trim();
        let source = source.trim();

        if !is_clean_registry_name(name) {
            diagnostics.push(format!(
                "line {line_number}: the entry name `{name}` is not a clean \
                 repository-relative path"
            ));
            continue;
        }
        let Some(source) = parse_source(source) else {
            diagnostics.push(format!(
                "line {line_number}: the entry `{name}` declares the source `{source}`, \
                 which is neither an http(s) URL nor an absolute local path"
            ));
            continue;
        };
        if entries.iter().any(|entry| entry.name == name) {
            diagnostics.push(format!(
                "line {line_number}: the name `{name}` is already declared"
            ));
            continue;
        }

        entries.push(RefsEntry {
            title: title.trim().to_string(),
            name: name.to_string(),
            source,
        });
    }

    (entries, diagnostics)
}

/// Reports whether `path` names the registry directory or anything beneath it.
///
/// The comparison runs over whole path components, so a sibling whose name
/// merely starts with the same prefix (`.tauqe/refs-archive/notes.md`) is not a
/// registry path, while the manifest and every declared name are. `path`
/// arrives in the normalized repository-relative form produced by
/// `ContextManager::normalize_path`, which carries no `.` or `..` component.
pub fn is_refs_registry_path(path: &str) -> bool {
    Path::new(path).starts_with(REFS_DIR)
}

/// Reports whether `path` names the isolated cache or anything beneath it.
///
/// The tree is Git-ignored and holds the downloaded snapshot, so a cached
/// document is never confused with the tracked registry it was declared in.
pub fn is_refs_cache_path(path: &str) -> bool {
    Path::new(path).starts_with(REFS_CACHE_DIR)
}

/// Returns the registry name `path` stores, when it stores one.
///
/// The manifest itself declares the entries rather than being one, so it has no
/// name. A path outside the registry directory, the directory itself, and a path
/// that does not reduce to a clean relative name all yield `None`.
pub fn registry_name(path: &str) -> Option<String> {
    if Path::new(path) == Path::new(REFS_MANIFEST) {
        return None;
    }
    let relative = Path::new(path).strip_prefix(REFS_DIR).ok()?;
    let name = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    if is_clean_registry_name(&name) {
        Some(name)
    } else {
        None
    }
}

/// Maps a registry name to the path its snapshot is stored under.
///
/// `name` is expected to come from a validated [`RefsEntry`], so the result
/// always stays inside the isolated cache directory.
pub fn cache_path_for(name: &str) -> String {
    format!("{REFS_CACHE_DIR}/{name}")
}

/// Splits one manifest line into its title, name, and source.
///
/// Returns `None` for every line that does not carry the declared entry shape,
/// which is exactly how prose surrounding the entries stays without registry
/// meaning. The slices are returned untrimmed; the caller decides what
/// whitespace is insignificant around them.
fn split_entry(line: &str) -> Option<(&str, &str, &str)> {
    let rest = line.trim().strip_prefix('-')?;
    let rest = rest.trim_start().strip_prefix('[')?;
    let title_end = rest.find(']')?;
    let title = &rest[..title_end];
    let rest = rest[title_end + 1..].trim_start().strip_prefix('(')?;
    let name_end = rest.find(')')?;
    let name = &rest[..name_end];
    let rest = rest[name_end + 1..].trim_start().strip_prefix('=')?;
    Some((title, name, rest))
}

/// Classifies a declared source as a remote URL or an absolute local path.
///
/// The two accepted forms cover the whole contract: a document is either
/// fetched over the network or copied from a location outside the repository.
/// Anything else — a repository-relative path, a malformed address, an empty
/// value — is rejected instead of being guessed at.
fn parse_source(source: &str) -> Option<RefsSource> {
    if is_http_url(source) {
        return Some(RefsSource::Url(source.to_string()));
    }
    if !source.contains('\0') && Path::new(source).is_absolute() {
        return Some(RefsSource::LocalPath(source.to_string()));
    }
    None
}

/// Reports whether `source` is an `http://` or `https://` URL with a host part.
///
/// The scheme is compared case-insensitively, and the remainder must stay free
/// of whitespace and control characters, so a line that appended a trailing
/// remark to a URL is rejected instead of being stored as a broken address.
fn is_http_url(source: &str) -> bool {
    let Some((scheme, rest)) = source.split_once("://") else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return false;
    }
    !rest.is_empty() && !rest.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// Reports whether `name` is a clean repository-relative registry name.
///
/// The manifest is portable, so a name is written with `/` and validated on its
/// raw segments: an empty name, an empty segment (which covers an absolute name,
/// a doubled separator, and a trailing separator), a `.` or `..` segment, and a
/// backslash are all rejected. The platform path then answers the remaining
/// question, whether the result could still name an absolute location, so one
/// manifest behaves identically on every host.
fn is_clean_registry_name(name: &str) -> bool {
    if name.is_empty() || name.contains('\\') || name.contains('\0') {
        return false;
    }
    let portable = name
        .split('/')
        .all(|segment| !segment.is_empty() && segment != "." && segment != "..");
    portable && !Path::new(name).is_absolute()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_parse_manifest_reads_declared_entries() {
        let content = "\
# External references

Upstream material the harness copies into the isolated cache.

- [Upstream API notes](vendor/api.md) = https://example.com/llm.txt
- [Protocol summary](vendor/protocol.md) = /srv/docs/protocol.md

Read the paragraph above before adding an entry.
";
        let (entries, diagnostics) = parse_manifest(content);
        assert!(
            diagnostics.is_empty(),
            "unexpected diagnostics: {diagnostics:?}"
        );
        assert_eq!(entries.len(), 2);

        assert_eq!(entries[0].title, "Upstream API notes");
        assert_eq!(entries[0].name, "vendor/api.md");
        assert_eq!(
            entries[0].source,
            RefsSource::Url("https://example.com/llm.txt".to_string())
        );

        assert_eq!(entries[1].title, "Protocol summary");
        assert_eq!(entries[1].name, "vendor/protocol.md");
        assert_eq!(
            entries[1].source,
            RefsSource::LocalPath("/srv/docs/protocol.md".to_string())
        );
    }

    #[test]
    fn test_prose_carries_no_registry_meaning() {
        let content = "\
# References

A note about the registry.

- a bullet that is not an entry
- [No source](no-source.md)
- [Broken](broken.md = https://example.com/x
* [Other bullet](other.md) = https://example.com/y
plain text mentioning [a link](vendor/api.md) = https://example.com/z
";
        let (entries, diagnostics) = parse_manifest(content);
        assert!(entries.is_empty(), "unexpected entries: {entries:?}");
        assert!(
            diagnostics.is_empty(),
            "unexpected diagnostics: {diagnostics:?}"
        );
    }

    #[test]
    fn test_a_repeated_name_keeps_its_first_declaration() {
        let content = "\
- [First](vendor/api.md) = https://example.com/first.txt
- [Second](vendor/api.md) = https://example.com/second.txt
";
        let (entries, diagnostics) = parse_manifest(content);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].title, "First");
        assert_eq!(
            entries[0].source,
            RefsSource::Url("https://example.com/first.txt".to_string())
        );
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].contains("vendor/api.md"));
        assert!(diagnostics[0].contains("already declared"));
    }

    #[test]
    fn test_entries_with_an_unusable_name_or_source_are_reported_and_dropped() {
        let content = concat!(
            "- [Absolute](/etc/passwd) = https://example.com/x.txt\n",
            "- [Parent](vendor/../secret.md) = https://example.com/x.txt\n",
            "- [Dot](./vendor/api.md) = https://example.com/x.txt\n",
            "- [Backslash](vendor\\api.md) = https://example.com/x.txt\n",
            "- [Empty]() = https://example.com/x.txt\n",
            "- [Relative source](vendor/api.md) = docs/local.md\n",
            "- [Blank source](vendor/blank.md) =\n",
            "- [Kept](vendor/kept.md) = https://example.com/kept.txt\n",
        );
        let (entries, diagnostics) = parse_manifest(content);

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "vendor/kept.md");
        assert_eq!(
            diagnostics.len(),
            7,
            "unexpected diagnostics: {diagnostics:?}"
        );

        assert!(diagnostics[0].contains("/etc/passwd"));
        assert!(diagnostics[1].contains("vendor/../secret.md"));
        assert!(diagnostics[2].contains("./vendor/api.md"));
        assert!(diagnostics[3].contains("vendor\\api.md"));
        assert!(diagnostics[5].contains("docs/local.md"));
        assert!(diagnostics[6].contains("vendor/blank.md"));
    }

    #[test]
    fn test_entries_roundtrip_the_trailing_remark_rejection() {
        // A trailing remark turns a URL into an address nobody can fetch, so the
        // line is rejected instead of being stored as a broken source.
        let content = "- [Trailing](vendor/api.md) = https://example.com/llm.txt see notes\n";
        let (entries, diagnostics) = parse_manifest(content);
        assert!(entries.is_empty());
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].contains("vendor/api.md"));
    }

    #[test]
    fn test_the_registry_and_the_cache_are_disjoint_isolated_trees() {
        assert!(is_refs_registry_path(REFS_DIR));
        assert!(is_refs_registry_path(REFS_MANIFEST));
        assert!(is_refs_registry_path(".tauqe/refs/vendor/api.md"));
        assert!(!is_refs_registry_path(".tauqe/refs-archive/notes.md"));
        assert!(!is_refs_registry_path(".tauqe/refs.rs"));
        assert!(!is_refs_registry_path(".tauqe/cache/refs/vendor/api.md"));
        assert!(!is_refs_registry_path(""));

        assert!(is_refs_cache_path(REFS_CACHE_DIR));
        assert!(is_refs_cache_path(".tauqe/cache/refs/vendor/api.md"));
        assert!(!is_refs_cache_path(
            ".tauqe/cache/refs-archive/vendor/api.md"
        ));
        assert!(!is_refs_cache_path(REFS_MANIFEST));
        assert!(!is_refs_cache_path(REFS_DIR));
        assert!(!is_refs_cache_path(""));
    }

    #[test]
    fn test_a_registry_path_maps_to_its_declared_name() {
        assert_eq!(
            registry_name(".tauqe/refs/vendor/api.md").as_deref(),
            Some("vendor/api.md")
        );
        assert_eq!(
            registry_name(".tauqe/refs/api.md").as_deref(),
            Some("api.md")
        );
        assert_eq!(registry_name(REFS_MANIFEST), None);
        assert_eq!(registry_name(REFS_DIR), None);
        assert_eq!(registry_name(".tauqe/refs/../outside.md"), None);
        assert_eq!(registry_name("docs/notes.md"), None);
    }

    #[test]
    fn test_a_name_maps_into_the_isolated_cache() {
        assert_eq!(
            cache_path_for("vendor/api.md"),
            ".tauqe/cache/refs/vendor/api.md"
        );
        assert!(is_refs_cache_path(&cache_path_for("vendor/api.md")));
        assert!(!is_refs_registry_path(&cache_path_for("vendor/api.md")));
    }

    #[test]
    fn test_the_registry_is_read_from_the_declared_manifest_path() {
        let dir = tempdir().unwrap();
        let manifest = dir.path().join(REFS_MANIFEST);
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        std::fs::write(
            &manifest,
            "- [API](vendor/api.md) = https://example.com/llm.txt\n",
        )
        .unwrap();

        let registry = load_registry(dir.path());
        assert!(
            registry.diagnostics.is_empty(),
            "unexpected diagnostics: {:?}",
            registry.diagnostics
        );
        assert_eq!(registry.entries.len(), 1);
        assert_eq!(registry.entries[0].name, "vendor/api.md");
    }

    #[test]
    fn test_a_repository_without_a_manifest_declares_no_references() {
        let dir = tempdir().unwrap();
        let registry = load_registry(dir.path());
        assert!(registry.entries.is_empty());
        assert!(registry.diagnostics.is_empty());
    }

    #[test]
    fn test_an_unreadable_manifest_becomes_a_diagnostic() {
        let dir = tempdir().unwrap();
        let manifest = dir.path().join(REFS_MANIFEST);
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        std::fs::write(&manifest, [0xff, 0xfe, 0x00]).unwrap();

        let registry = load_registry(dir.path());
        assert!(registry.entries.is_empty());
        assert_eq!(registry.diagnostics.len(), 1);
        assert!(registry.diagnostics[0].contains(REFS_MANIFEST));
    }
}
