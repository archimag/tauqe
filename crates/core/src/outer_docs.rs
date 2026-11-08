//! External documentation declared by the project in `[[outer_docs]]`.
//!
//! A project keeps authoritative material — an upstream API reference, a
//! protocol summary, a published book — outside the repository and declares it
//! once in the configuration. Every declaration is materialized into the isolated
//! cache (`.tauqe/cache/outer_docs/`) that Git ignores, so the snapshot the model
//! reads stays stable for the whole task and never mixes with the working tree.
//!
//! # The base prefix jail
//!
//! One declaration names one root document and, through it, the directory that
//! holds it. A request for the declared `id` answers the root file exactly as it
//! is: the harness parses no index, synthesizes no catalog of parts, and rewrites
//! no text, so the model follows the relative links the document itself carries
//! and requests the parts it needs.
//!
//! A request for `{id}/{relative}` resolves below the directory of the root.
//! Nothing resolves outside it: a `..` segment, an absolute path, a backslash, an
//! empty segment, and a foreign scheme are refused before a byte moves, so one
//! declaration never reaches a file the project did not publish.
//!
//! A snapshot the cache already holds is answered without consulting the
//! developer's trust declaration, because it is local material an earlier request
//! delivered. That declaration decides only whether a source may be reached at
//! all: a local file below a declared root, or a public address of a declared
//! host. Authorizing the root therefore authorizes the whole subtree the jail
//! keeps inside it, and no second declaration is needed to follow a link.

pub mod materialize;
pub(crate) mod trust;

use std::collections::BTreeMap;
use std::path::Path;

use tauqe_protocol::{OuterDocsCacheEntry, OuterDocsCacheRemoveResult};
use tokio::sync::watch;

use crate::config::{OuterDocEntry, OuterDocSource};
use crate::outer_docs::trust::RefsTrust;

pub use materialize::MaterializeError;
pub use trust::{TrustRefusal, WorkspaceDeclaration};

/// Repository-relative directory holding the isolated documentation snapshot.
pub const OUTER_DOCS_CACHE_DIR: &str = ".tauqe/cache/outer_docs";

/// Repository-relative directory one declared root occupies inside the cache.
///
/// The directory is what makes the prefix jail expressible on disk: the root
/// document and every relative document it links to share one subtree, and no
/// document the declaration does not cover ever lands inside it.
pub fn document_dir(id: &str) -> String {
    format!("{OUTER_DOCS_CACHE_DIR}/{id}")
}

/// Repository-relative path the root document of `doc` is cached under.
///
/// The root keeps its own file name inside the directory of its declaration, so a
/// relative document lands beside it and a link the root text carries names the
/// very file the harness caches.
pub fn document_cache_path(doc: &OuterDoc) -> String {
    format!("{}/{}", document_dir(&doc.id), root_file_name(&doc.source))
}

/// Repository-relative path one relative document of `doc` is cached under.
pub fn relative_cache_path(doc: &OuterDoc, relative: &str) -> String {
    format!("{}/{}", document_dir(&doc.id), relative)
}

/// Reports whether `path` names the documentation cache or anything beneath it.
pub fn is_outer_docs_cache_path(path: &str) -> bool {
    Path::new(path).starts_with(OUTER_DOCS_CACHE_DIR)
}

/// Reduces a cache path to the logical document path it addresses.
///
/// The stored path of a document carries the cache directory the harness
/// derived from the declaration, while the wire contract addresses the document
/// by its logical path alone. A path outside the cache addresses no document
/// and answers `None`.
pub fn logical_path(path: &str) -> Option<&str> {
    let relative = path.strip_prefix(OUTER_DOCS_CACHE_DIR)?.strip_prefix('/')?;
    if relative.is_empty() {
        return None;
    }
    Some(relative)
}

/// Byte ceiling of one materialized snapshot.
///
/// A declared document is materialized before it competes for the single
/// `[context].token_budget` ceiling, so this ceiling bounds only what one transfer
/// may buffer: a source larger than it is refused with a diagnostic instead of
/// being fetched and rejected by the token budget afterwards.
pub const MAX_DOCUMENT_BYTES: u64 = 4 * 1024 * 1024;

/// Reports whether `path` is a clean repository-relative path written with `/`.
/// Reports whether `path` is a clean repository-relative path written with `/`.
///
/// The declaration is portable, so the rule is stated on the raw segments: an
/// empty path, an empty segment (which covers an absolute path, a doubled
/// separator, and a trailing separator), a `.` or `..` segment, a backslash, a
/// NUL, a `%`, and a `:` (which would smuggle a URL scheme or a drive letter)
/// are all rejected. The platform path then answers the remaining question,
/// whether the result could still name an absolute location.
///
/// The `%` is rejected because it is the escape character of the percent
/// decoding a remote host may apply to the address the path becomes. A segment
/// such as `%2e%2e` is not the literal `..` this jail inspects, yet a host that
/// decodes its path resolves it above the declared root. Refusing the escape
/// character keeps the bytes the jail validated and the bytes the host resolves
/// identical, so the rule is closed under decoding without the harness decoding
/// anything itself.
pub fn is_clean_relative_path(path: &str) -> bool {
    if path.is_empty()
        || path.contains('\\')
        || path.contains('\0')
        || path.contains('%')
        || path.contains(':')
    {
        return false;
    }
    let portable = path
        .split('/')
        .all(|segment| !segment.is_empty() && segment != "." && segment != "..");
    portable && !Path::new(path).is_absolute()
}

/// One document of the cache, as the cache inspection surface reports it.
///
/// The reported path is the logical path of the document — the declared
/// identifier and the relative path below it — so the disk prefix the
/// declaration compiles to never reaches the wire contract.
pub fn list_cache_entries(repo_root: &Path) -> Result<Vec<OuterDocsCacheEntry>, String> {
    let root = repo_root.join(OUTER_DOCS_CACHE_DIR);
    // Scanning the cache reads every directory of the snapshot tree, so the
    // walk runs off the async executor thread.
    crate::blocking::run_blocking(move || -> Result<Vec<OuterDocsCacheEntry>, String> {
        let mut entries = BTreeMap::new();
        collect_cache_entries(&root, &root, &mut entries)?;
        Ok(entries.into_values().collect())
    })
}

/// Collects the documents directly below `directory`, descending into every
/// subdirectory of it.
///
/// The map is keyed by logical path, so the inventory is deterministic without
/// a second sorting pass: one snapshot never appears twice and the traversal
/// order of the file system never reaches the caller.
fn collect_cache_entries(
    root: &Path,
    directory: &Path,
    entries: &mut BTreeMap<String, OuterDocsCacheEntry>,
) -> Result<(), String> {
    let read = match std::fs::read_dir(directory) {
        Ok(read) => read,
        // A cache that was never written holds no document, and an entry an
        // earlier removal already dropped is indistinguishable from one that
        // never existed.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(cache_unreadable(directory, &error)),
    };
    for entry in read {
        let entry = entry.map_err(|error| cache_unreadable(directory, &error))?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|error| {
            format!(
                "the external documentation cache entry `{}` could not be inspected: {error}",
                path.display()
            )
        })?;
        if file_type.is_dir() {
            collect_cache_entries(root, &path, entries)?;
        } else if file_type.is_file() {
            // Only a regular file is a document. A symbolic link is skipped
            // like a directory, so a snapshot the harness did not write never
            // enters the inventory and is never followed.
            let size_bytes = entry.metadata().map(|metadata| metadata.len()).unwrap_or(0);
            let relative = path.strip_prefix(root).unwrap_or(&path);
            let logical = relative
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/");
            entries.insert(
                logical.clone(),
                OuterDocsCacheEntry {
                    path: logical,
                    size_bytes,
                    estimated_tokens: crate::prompt::estimate_tokens_from_bytes(size_bytes),
                },
            );
        }
    }
    Ok(())
}

/// The diagnostic of a cache directory that could not be read.
fn cache_unreadable(directory: &Path, error: &std::io::Error) -> String {
    format!(
        "the external documentation cache `{}` could not be read: {error}",
        directory.display()
    )
}

/// Why one removal from the external documentation cache did not happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheRemoveError {
    /// The requested path is not a clean logical path of the cache.
    InvalidPath(String),
    /// The requested path names no cached document or directory.
    NotFound(String),
    /// The removal reached the file system and failed.
    Failed(String),
}

impl CacheRemoveError {
    /// One-line cause of the failure.
    pub fn reason(&self) -> &str {
        match self {
            Self::InvalidPath(reason) | Self::NotFound(reason) | Self::Failed(reason) => reason,
        }
    }
}

impl std::fmt::Display for CacheRemoveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.reason())
    }
}

/// Removes one document or one directory below the cache directory.
///
/// The logical path is validated before a byte moves: an empty request, a
/// parent directory, an absolute path, a backslash, and a foreign scheme
/// address no document the cache holds. Every file is unlinked atomically and
/// every directory the removal emptied is pruned afterwards, so a crash leaves
/// either the complete subtree or none of it, and the inventory never reports a
/// directory that no longer holds a document.
pub fn remove_cache_entry(
    repo_root: &Path,
    logical_path: &str,
) -> Result<OuterDocsCacheRemoveResult, CacheRemoveError> {
    let logical = logical_path.trim();
    if logical.is_empty() {
        return Err(CacheRemoveError::InvalidPath(
            "a cache removal names no document".to_string(),
        ));
    }
    if !is_clean_relative_path(logical) {
        return Err(CacheRemoveError::InvalidPath(format!(
            "`{logical}` is not a clean logical path of the external documentation cache"
        )));
    }
    let root = repo_root.join(OUTER_DOCS_CACHE_DIR);
    let target = root.join(logical);
    let logical = logical.to_string();
    // Unlinking every file of the subtree touches the disk, so the removal runs
    // off the async executor thread.
    crate::blocking::run_blocking(move || remove_cache_target(&root, &target, &logical))
}

/// Removes the file or the subtree `target` names and prunes what it emptied.
fn remove_cache_target(
    root: &Path,
    target: &Path,
    logical: &str,
) -> Result<OuterDocsCacheRemoveResult, CacheRemoveError> {
    let metadata = match std::fs::symlink_metadata(target) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(CacheRemoveError::NotFound(format!(
                "the external documentation cache holds no `{logical}`"
            )))
        }
        Err(error) => {
            return Err(CacheRemoveError::Failed(format!(
                "the cached document `{logical}` could not be inspected: {error}"
            )))
        }
    };

    let mut removal = CacheRemoval::default();
    if metadata.is_dir() {
        removal.directory(target)?;
        // The removal unlinked every file of the subtree, so what remains is
        // the directory skeleton: it is pruned deepest first until the target
        // directory itself collapses.
        prune_empty_directories(target);
    } else {
        removal.file(target)?;
    }

    // Pruning the ancestors runs last, after the whole subtree is gone, so a
    // directory that still holds a document survives. The cache directory
    // itself is never removed: it is the root every declaration resolves into.
    tauqe_tools::atomic_write::prune_empty_parents(root, target);
    Ok(removal.result())
}

/// Prunes `target` and every directory below it that a removal emptied.
///
/// The documents are already unlinked when this runs, so only the directory
/// skeleton is left behind. Directories are pruned deepest first, which lets a
/// whole subtree the removal emptied collapse up to `target` itself.
///
/// A pruning step that fails — because a concurrent writer put a document back,
/// or because the filesystem refuses the removal — ends the walk of that
/// subtree instead of being reported: the removal already took the documents,
/// and a surviving empty directory never invalidates it.
fn prune_empty_directories(target: &Path) {
    let Ok(read) = std::fs::read_dir(target) else {
        return;
    };
    for entry in read.flatten() {
        if matches!(entry.file_type(), Ok(kind) if kind.is_dir()) {
            prune_empty_directories(&entry.path());
        }
    }
    let _ = std::fs::remove_dir(target);
}

/// What one removal took out of the cache.
#[derive(Default)]
struct CacheRemoval {
    removed_files: usize,
    removed_bytes: u64,
}

impl CacheRemoval {
    /// Removes one cached document.
    fn file(&mut self, path: &Path) -> Result<(), CacheRemoveError> {
        let size = std::fs::symlink_metadata(path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        tauqe_tools::atomic_write::remove_atomic(path).map_err(|error| {
            CacheRemoveError::Failed(format!(
                "the cached document `{}` could not be removed: {error:#}",
                path.display()
            ))
        })?;
        self.removed_files += 1;
        self.removed_bytes = self.removed_bytes.saturating_add(size);
        Ok(())
    }

    /// Removes every document below `directory`.
    fn directory(&mut self, directory: &Path) -> Result<(), CacheRemoveError> {
        let read = std::fs::read_dir(directory).map_err(|error| {
            CacheRemoveError::Failed(format!(
                "the cached directory `{}` could not be read: {error}",
                directory.display()
            ))
        })?;
        for entry in read {
            let entry = entry.map_err(|error| {
                CacheRemoveError::Failed(format!(
                    "the cached directory `{}` could not be read: {error}",
                    directory.display()
                ))
            })?;
            let path = entry.path();
            let file_type = entry.file_type().map_err(|error| {
                CacheRemoveError::Failed(format!(
                    "the cached entry `{}` could not be inspected: {error}",
                    path.display()
                ))
            })?;
            if file_type.is_dir() {
                self.directory(&path)?;
            } else {
                self.file(&path)?;
            }
        }
        Ok(())
    }

    /// The report describing what was removed.
    fn result(self) -> OuterDocsCacheRemoveResult {
        OuterDocsCacheRemoveResult {
            removed_files: self.removed_files,
            removed_bytes: self.removed_bytes,
        }
    }
}

/// The resident documents that the removal of `removed` addresses.
///
/// A resident path is stored as the repository-relative path of its snapshot,
/// which carries the cache directory the harness derives from the declaration,
/// while the removal surface addresses logical paths. A removal of a directory
/// therefore covers every document below it, and the resident layer is
/// reconciled with exactly the rule the removal applied to the disk.
pub fn affected_documents(removed: &str, resident: &[String]) -> Vec<String> {
    let logical = removed.trim();
    if logical.is_empty() {
        return Vec::new();
    }
    resident
        .iter()
        .filter(|path| {
            let document = logical_path(path.as_str()).unwrap_or(path.as_str());
            document == logical
                || document
                    .strip_prefix(logical)
                    .is_some_and(|rest| rest.starts_with('/'))
        })
        .cloned()
        .collect()
}

/// One declared document resolved for runtime use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OuterDoc {
    /// Mount point of the root document inside the outer documentation tree.
    pub id: String,
    /// Human-readable label rendered in the catalog the model reads.
    pub title: String,
    /// Optional note describing what the document covers.
    pub description: Option<String>,
    /// Where the content of the root document comes from.
    pub source: OuterDocSource,
}

impl OuterDoc {
    /// Resolves one declared entry, reporting why it is unusable.
    pub fn from_entry(entry: &OuterDocEntry) -> Result<Self, String> {
        let id = entry.id.trim();
        if !is_clean_relative_path(id) {
            return Err(format!(
                "the outer document id `{id}` is not a clean repository-relative path"
            ));
        }
        let title = entry.title.trim();
        if title.is_empty() {
            return Err(format!("the outer document `{id}` declares an empty title"));
        }
        Ok(Self {
            id: id.to_string(),
            title: title.to_string(),
            description: entry
                .description
                .as_deref()
                .map(str::trim)
                .filter(|description| !description.is_empty())
                .map(str::to_string),
            source: entry.source()?,
        })
    }
}

/// Returns the file name the root document of `source` is cached under.
///
/// A source that names no usable file name still needs a deterministic cache
/// path, so the fallback keeps the layout total instead of failing a request the
/// trust declaration could still authorize.
fn root_file_name(source: &OuterDocSource) -> String {
    let name = match source {
        OuterDocSource::Local(path) => path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned()),
        OuterDocSource::Remote(url) => reqwest::Url::parse(url).ok().and_then(|parsed| {
            let mut segments = parsed.path_segments()?;
            segments
                .rfind(|segment| !segment.is_empty())
                .map(str::to_string)
        }),
    };
    match name {
        Some(name) if !name.is_empty() => name,
        _ => "index.md".to_string(),
    }
}

/// The external documentation a project declares.
#[derive(Debug, Clone, Default)]
pub struct OuterDocsRegistry {
    docs: Vec<OuterDoc>,
    diagnostics: Vec<String>,
}

/// What one documented request addresses.
enum DeclaredTarget<'a> {
    /// The root document of a declaration.
    Root(&'a OuterDoc),
    /// A document resolved below the directory of a declaration.
    Relative(&'a OuterDoc, String),
}

impl OuterDocsRegistry {
    /// Builds the registry from the declared entries.
    ///
    /// An entry that cannot be resolved is reported as a diagnostic and left
    /// out, so one broken declaration never hides the documents around it. The
    /// configuration validator rejects such an entry before a turn starts, so
    /// this degradation is a second line of defense.
    pub fn new(entries: &[OuterDocEntry]) -> Self {
        let mut docs: Vec<OuterDoc> = Vec::with_capacity(entries.len());
        let mut diagnostics: Vec<String> = Vec::new();
        for entry in entries {
            match OuterDoc::from_entry(entry) {
                Ok(doc) => {
                    if docs.iter().any(|declared| declared.id == doc.id) {
                        diagnostics.push(format!(
                            "the outer document id `{}` is declared twice",
                            doc.id
                        ));
                        continue;
                    }
                    docs.push(doc);
                }
                Err(reason) => diagnostics.push(reason),
            }
        }
        Self { docs, diagnostics }
    }

    /// Reports whether the project declares no external documentation.
    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }

    /// The declared documents, in declaration order.
    pub fn docs(&self) -> &[OuterDoc] {
        &self.docs
    }

    /// Declarations that could not be resolved, in declaration order.
    pub fn diagnostics(&self) -> &[String] {
        &self.diagnostics
    }

    /// Finds a declared root document by its id.
    pub fn document(&self, id: &str) -> Option<&OuterDoc> {
        self.docs.iter().find(|doc| doc.id == id)
    }

    /// Materializes the declared document one research request addresses.
    ///
    /// A declared document has no file on disk until the harness materializes
    /// it, so a research round resolves the requests that address the
    /// documentation cache before the dispatcher looks for their files. The
    /// answers stay aligned with the requests: `Ok(Some(cache_path))` names the
    /// snapshot that now exists, `Ok(None)` reports that the path names no
    /// declared document and leaves it an ordinary request, and a refusal travels
    /// as [`MaterializeError`] with its classification intact.
    pub async fn materialize_request(
        &self,
        repo_root: &Path,
        path: &str,
        max_bytes: u64,
        cancel_rx: Option<&watch::Receiver<bool>>,
    ) -> Result<Option<String>, MaterializeError> {
        let Some(name) = registry_name(path) else {
            return Ok(None);
        };
        let Some(target) = self.resolve(&name) else {
            return Ok(None);
        };
        let cache_path = materialize_target(repo_root, target, max_bytes, cancel_rx, None).await?;
        Ok(Some(cache_path))
    }

    /// Materializes the declared entry one outer-document request addresses.
    ///
    /// The request names a mount point of the declaration rather than a cache
    /// path, so the harness resolves the location it derived for that entry and
    /// keeps the cache layout out of the model's hands. A root identifier answers
    /// the root file itself, while `{id}/{relative}` answers a document below the
    /// directory of that root.
    pub async fn materialize_declared(
        &self,
        repo_root: &Path,
        id: &str,
        max_bytes: u64,
        cancel_rx: Option<&watch::Receiver<bool>>,
    ) -> Result<String, MaterializeError> {
        self.materialize_declared_with(repo_root, id, max_bytes, cancel_rx, None)
            .await
    }

    /// Materializes one declared entry against a trust declaration.
    pub(crate) async fn materialize_declared_with(
        &self,
        repo_root: &Path,
        id: &str,
        max_bytes: u64,
        cancel_rx: Option<&watch::Receiver<bool>>,
        trust: Option<RefsTrust>,
    ) -> Result<String, MaterializeError> {
        let name = id.trim();
        if name.is_empty() {
            return Err(MaterializeError::Delivery(
                "an outer document request names no declared document".to_string(),
            ));
        }
        let Some(target) = self.resolve(name) else {
            return Err(MaterializeError::Delivery(format!(
                "the outer document `{name}` is not declared, so it cannot be requested"
            )));
        };
        materialize_target(repo_root, target, max_bytes, cancel_rx, trust).await
    }

    /// Resolves the identifier a request names to the document it addresses.
    ///
    /// An exact identifier answers its own root document. Otherwise the longest
    /// declared id that prefixes the request names the root, and the remainder is
    /// the relative document below it, so a declaration can never shadow a more
    /// specific declaration beside it.
    fn resolve(&self, name: &str) -> Option<DeclaredTarget<'_>> {
        if let Some(doc) = self.docs.iter().find(|doc| doc.id == name) {
            return Some(DeclaredTarget::Root(doc));
        }
        let mut chosen: Option<(&OuterDoc, &str)> = None;
        for doc in &self.docs {
            let Some(relative) = name
                .strip_prefix(doc.id.as_str())
                .and_then(|rest| rest.strip_prefix('/'))
            else {
                continue;
            };
            if relative.is_empty() {
                continue;
            }
            match chosen {
                Some((current, _)) if current.id.len() >= doc.id.len() => {}
                _ => chosen = Some((doc, relative)),
            }
        }
        chosen.map(|(doc, relative)| DeclaredTarget::Relative(doc, relative.to_string()))
    }
}

/// Refuses a relative document that would leave its declared root.
///
/// The jail is decided on the raw segments, before any transfer and before the
/// trust declaration is even read: a parent directory, an absolute path, a
/// backslash, an empty segment, and a foreign scheme all name material the
/// declaration did not publish.
fn ensure_inside_root(doc: &OuterDoc, relative: &str) -> Result<(), MaterializeError> {
    if is_clean_relative_path(relative) {
        return Ok(());
    }
    Err(MaterializeError::Delivery(format!(
        "the request `{id}/{relative}` leaves the declared root `{id}`: a documented request addresses the root document or a clean relative path below its directory, never a parent directory, an absolute path, a percent-encoded segment, or another scheme",
        id = doc.id
    )))
}

/// Materializes the document one resolved target names.
async fn materialize_target(
    repo_root: &Path,
    target: DeclaredTarget<'_>,
    max_bytes: u64,
    cancel_rx: Option<&watch::Receiver<bool>>,
    trust: Option<RefsTrust>,
) -> Result<String, MaterializeError> {
    match target {
        DeclaredTarget::Root(doc) => {
            materialize::materialize_document_with(repo_root, doc, max_bytes, cancel_rx, trust)
                .await
        }
        DeclaredTarget::Relative(doc, relative) => {
            ensure_inside_root(doc, &relative)?;
            materialize::materialize_relative_with(
                repo_root, doc, &relative, max_bytes, cancel_rx, trust,
            )
            .await
        }
    }
}

/// Reduces a cache path to the registry name it addresses.
///
/// A request outside the documentation cache addresses no declared document,
/// and the cache directory itself names no single document either: both answer
/// `None` and stay ordinary requests.
fn registry_name(path: &str) -> Option<String> {
    logical_path(path).map(|path| path.to_string())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use tempfile::tempdir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::{
        affected_documents, document_cache_path, document_dir, is_clean_relative_path,
        is_outer_docs_cache_path, list_cache_entries, logical_path, relative_cache_path,
        remove_cache_entry, CacheRemoveError, OuterDoc, OuterDocsRegistry, MAX_DOCUMENT_BYTES,
        OUTER_DOCS_CACHE_DIR,
    };
    use crate::config::{OuterDocEntry, OuterDocSource};
    use crate::outer_docs::trust::{RefsTrust, TrustRefusal};
    use tauqe_protocol::ContextLayer;

    /// A declaration with an empty source, which every fixture fills in.
    fn entry(id: &str, title: &str) -> OuterDocEntry {
        OuterDocEntry {
            id: id.to_string(),
            title: title.to_string(),
            description: None,
            source: String::new(),
        }
    }

    /// A resolved declaration.
    fn document(id: &str, source: OuterDocSource) -> OuterDoc {
        OuterDoc {
            id: id.to_string(),
            title: "Reference".to_string(),
            description: None,
            source,
        }
    }

    #[test]
    fn test_a_clean_relative_path_never_escapes_the_cache() {
        for path in ["vendor/api.md", "mylib/architecture.md", "api.md"] {
            assert!(is_clean_relative_path(path), "{path} must be accepted");
        }
        for path in [
            "",
            "/etc/passwd",
            "../secret.md",
            "vendor/../secret.md",
            "vendor\\api.md",
            "https://example.com/x.md",
            "vendor//api.md",
            "vendor/%2e%2e/secret.md",
            "%2e%2e/secret.md",
            "vendor/%2E/api.md",
            "vendor/%2fapi.md",
        ] {
            assert!(!is_clean_relative_path(path), "{path} must be rejected");
        }

        let doc = document(
            "vendor/api.md",
            OuterDocSource::Remote("https://example.com/llm.txt".to_string()),
        );
        assert_eq!(
            document_dir("vendor/api.md"),
            format!("{OUTER_DOCS_CACHE_DIR}/vendor/api.md")
        );
        assert_eq!(
            document_cache_path(&doc),
            format!("{OUTER_DOCS_CACHE_DIR}/vendor/api.md/llm.txt")
        );
        assert_eq!(
            relative_cache_path(&doc, "notes.md"),
            format!("{OUTER_DOCS_CACHE_DIR}/vendor/api.md/notes.md")
        );
        assert!(is_outer_docs_cache_path(&document_cache_path(&doc)));
        assert!(is_outer_docs_cache_path(&relative_cache_path(
            &doc, "notes.md"
        )));
        assert!(!is_outer_docs_cache_path("src/lib.rs"));
    }

    #[test]
    fn test_a_declared_entry_resolves_into_a_document() {
        let mut declared = entry("mylib", " MyLib Book ");
        declared.description = Some("  Published corpus.  ".to_string());
        declared.source = "https://example.com/book/README.md".to_string();

        let doc = OuterDoc::from_entry(&declared).expect("the entry resolves");
        assert_eq!(doc.id, "mylib");
        assert_eq!(doc.title, "MyLib Book");
        assert_eq!(doc.description.as_deref(), Some("Published corpus."));
        assert_eq!(
            doc.source,
            OuterDocSource::Remote("https://example.com/book/README.md".to_string())
        );

        let mut local = entry("vendor/api.md", "Upstream API");
        local.source = " /srv/docs/api.md ".to_string();
        let doc = OuterDoc::from_entry(&local).expect("the local entry resolves");
        assert_eq!(
            doc.source,
            OuterDocSource::Local(PathBuf::from("/srv/docs/api.md"))
        );
    }

    #[test]
    fn test_an_invalid_entry_is_reported_and_left_out() {
        let broken = entry("vendor/broken.md", "Broken");
        let registry = OuterDocsRegistry::new(&[broken]);
        assert!(registry.is_empty());
        assert_eq!(registry.diagnostics().len(), 1);
        assert!(registry.diagnostics()[0].contains("empty source"));
    }

    #[tokio::test]
    async fn test_a_relative_request_resolves_below_the_declared_root() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/book/index.md"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("# MyLib\n\n- [Architecture](architecture.md)\n"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/book/architecture.md"))
            .respond_with(ResponseTemplate::new(200).set_body_string("layouts and contracts"))
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let mut declared = entry("mylib", "MyLib Book");
        declared.source = format!("{}/book/index.md", server.uri());
        let registry = OuterDocsRegistry::new(&[declared]);
        let trust = Some(RefsTrust::for_workspace(dir.path(), Vec::new(), true));
        let doc = registry
            .document("mylib")
            .expect("the declaration resolves");

        let root = registry
            .materialize_declared_with(dir.path(), "mylib", MAX_DOCUMENT_BYTES, None, trust.clone())
            .await
            .expect("the root document materializes");
        assert_eq!(root, document_cache_path(doc));
        assert_eq!(
            std::fs::read_to_string(dir.path().join(&root)).unwrap(),
            "# MyLib\n\n- [Architecture](architecture.md)\n",
            "the root is answered as its original text, never as a parsed index"
        );

        let nested = registry
            .materialize_declared_with(
                dir.path(),
                "mylib/architecture.md",
                MAX_DOCUMENT_BYTES,
                None,
                trust,
            )
            .await
            .expect("the relative document materializes");
        assert_eq!(nested, relative_cache_path(doc, "architecture.md"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join(&nested)).unwrap(),
            "layouts and contracts"
        );
    }

    #[tokio::test]
    async fn test_a_request_that_leaves_the_declared_root_is_refused() {
        let dir = tempdir().unwrap();
        let mut declared = entry("mylib", "MyLib Book");
        declared.source = "https://example.com/book/index.md".to_string();
        let registry = OuterDocsRegistry::new(&[declared]);

        for request in [
            "mylib/../secret.md",
            "mylib/notes/../../secret.md",
            "mylib//etc/passwd.md",
            "/etc/passwd.md",
            "mylib/%2e%2e/secret.md",
        ] {
            let error = registry
                .materialize_declared(dir.path(), request, MAX_DOCUMENT_BYTES, None)
                .await
                .expect_err("a request that leaves the root is refused");
            assert!(
                error.reason().contains("leaves the declared root")
                    || error.reason().contains("is not declared"),
                "unexpected reason for {request}: {error}"
            );
        }

        let error = registry
            .materialize_request(
                dir.path(),
                &format!("{}/../secret.md", document_dir("mylib")),
                MAX_DOCUMENT_BYTES,
                None,
            )
            .await
            .expect_err("a cache path that leaves the root is refused");
        assert!(
            error.reason().contains("leaves the declared root"),
            "unexpected reason: {error}"
        );
        assert!(!dir.path().join(document_dir("mylib")).exists());
    }

    #[tokio::test]
    async fn test_an_undeclared_document_request_names_the_declaration() {
        let dir = tempdir().unwrap();
        let registry = OuterDocsRegistry::new(&[]);

        let error = registry
            .materialize_declared(dir.path(), "absent", MAX_DOCUMENT_BYTES, None)
            .await
            .expect_err("an undeclared document cannot be requested");
        assert!(
            error.reason().contains("is not declared"),
            "unexpected reason: {error}"
        );

        let error = registry
            .materialize_declared(dir.path(), "   ", MAX_DOCUMENT_BYTES, None)
            .await
            .expect_err("an empty request names no document");
        assert!(
            error.reason().contains("names no declared document"),
            "unexpected reason: {error}"
        );
    }

    #[tokio::test]
    async fn test_an_undeclared_cache_path_stays_an_ordinary_request() {
        let dir = tempdir().unwrap();
        let registry = OuterDocsRegistry::new(&[]);

        for path in [
            format!("{OUTER_DOCS_CACHE_DIR}/vendor/unknown.md"),
            "src/lib.rs".to_string(),
            OUTER_DOCS_CACHE_DIR.to_string(),
        ] {
            let resolved = registry
                .materialize_request(dir.path(), &path, MAX_DOCUMENT_BYTES, None)
                .await
                .expect("an undeclared path is not a failure");
            assert!(resolved.is_none(), "{path} names no declared document");
        }
    }

    #[tokio::test]
    async fn test_a_refused_source_reaches_the_caller_as_a_typed_decision() {
        let outside = tempdir().unwrap();
        let source = outside.path().join("api.md");
        std::fs::write(&source, "endpoint notes").unwrap();

        let dir = tempdir().unwrap();
        let mut declared = entry("vendor/api.md", "Upstream API");
        declared.source = source.to_string_lossy().into_owned();
        let registry = OuterDocsRegistry::new(&[declared]);
        let doc = registry
            .document("vendor/api.md")
            .expect("the declaration resolves");
        let cache_path = document_cache_path(doc);

        let error = registry
            .materialize_request(dir.path(), &cache_path, MAX_DOCUMENT_BYTES, None)
            .await
            .expect_err("a workspace no declaration authorizes reads nothing");

        assert!(
            matches!(
                error.trust(),
                Some(TrustRefusal::WorkspaceNotAuthorized { .. })
            ),
            "unexpected classification: {error}"
        );
        assert!(
            error
                .remedy()
                .is_some_and(|remedy| remedy.contains("[[workspace]]")),
            "the refusal must carry the declaration that authorizes the workspace"
        );
        assert!(!dir.path().join(&cache_path).exists());
    }

    /// Creates one cached document and returns nothing: the fixtures address a
    /// document by the logical path they pass in.
    fn cache_file(root: &Path, logical: &str, contents: &str) {
        let path = root.join(OUTER_DOCS_CACHE_DIR).join(logical);
        std::fs::create_dir_all(path.parent().expect("the cache path has a directory")).unwrap();
        std::fs::write(&path, contents).unwrap();
    }

    #[test]
    fn test_the_cache_inventory_reports_logical_paths() {
        let dir = tempdir().unwrap();
        cache_file(dir.path(), "openrouter/models.md", "models");
        cache_file(dir.path(), "openrouter/chat-completion/api.md", "endpoints");

        let entries = list_cache_entries(dir.path()).expect("the inventory reads");
        let paths: Vec<&str> = entries.iter().map(|entry| entry.path.as_str()).collect();
        assert_eq!(
            paths,
            vec!["openrouter/chat-completion/api.md", "openrouter/models.md"]
        );
        assert!(entries.iter().all(|entry| entry.size_bytes > 0));
        assert!(entries.iter().all(|entry| entry.estimated_tokens > 0));
        assert!(
            entries
                .iter()
                .all(|entry| !entry.path.starts_with(OUTER_DOCS_CACHE_DIR)),
            "the disk prefix must never reach the wire contract"
        );

        // A cache that was never written holds no document.
        let empty = tempdir().unwrap();
        assert!(list_cache_entries(empty.path()).unwrap().is_empty());
    }

    #[test]
    fn test_a_removal_takes_one_document_and_prunes_the_directories_it_emptied() {
        let dir = tempdir().unwrap();
        cache_file(dir.path(), "openrouter/api.md", "endpoints");
        cache_file(dir.path(), "openrouter/models.md", "models");

        let removed =
            remove_cache_entry(dir.path(), "openrouter/api.md").expect("the removal runs");

        assert_eq!(removed.removed_files, 1);
        assert_eq!(removed.removed_bytes, "endpoints".len() as u64);
        let cache = dir.path().join(OUTER_DOCS_CACHE_DIR);
        assert!(!cache.join("openrouter/api.md").exists());
        assert!(cache.join("openrouter/models.md").is_file());

        remove_cache_entry(dir.path(), "openrouter/models.md").expect("the second removal runs");
        assert!(
            !cache.join("openrouter").exists(),
            "a directory the removal emptied is pruned"
        );
        assert!(cache.is_dir(), "the cache directory itself survives");
    }

    #[test]
    fn test_a_removal_of_a_directory_takes_the_whole_subtree() {
        let dir = tempdir().unwrap();
        cache_file(dir.path(), "openrouter/api.md", "endpoints");
        cache_file(dir.path(), "openrouter/chat/completions.md", "chat");
        cache_file(dir.path(), "vendor/api.md", "vendor");

        let removed = remove_cache_entry(dir.path(), "openrouter").expect("the removal runs");

        assert_eq!(removed.removed_files, 2);
        assert_eq!(
            removed.removed_bytes,
            ("endpoints".len() + "chat".len()) as u64
        );
        let cache = dir.path().join(OUTER_DOCS_CACHE_DIR);
        assert!(!cache.join("openrouter").exists());
        assert!(cache.join("vendor/api.md").is_file());
    }

    #[test]
    fn test_a_removal_that_leaves_the_cache_is_refused() {
        let dir = tempdir().unwrap();
        cache_file(dir.path(), "openrouter/api.md", "endpoints");
        std::fs::write(dir.path().join("secret.md"), "outside").unwrap();

        for path in [
            "",
            "   ",
            "../secret.md",
            "/etc/passwd",
            "openrouter/../secret.md",
            "openrouter\\api.md",
        ] {
            let error = remove_cache_entry(dir.path(), path)
                .expect_err("a path outside the cache is refused");
            assert!(
                matches!(error, CacheRemoveError::InvalidPath(_)),
                "unexpected classification for {path}: {error}"
            );
        }

        assert!(dir.path().join("secret.md").is_file());
        assert!(dir
            .path()
            .join(OUTER_DOCS_CACHE_DIR)
            .join("openrouter/api.md")
            .is_file());
    }

    #[test]
    fn test_a_removal_of_an_absent_document_is_not_found() {
        let dir = tempdir().unwrap();
        let error = remove_cache_entry(dir.path(), "openrouter/api.md")
            .expect_err("an absent document is not found");
        assert!(
            matches!(error, CacheRemoveError::NotFound(_)),
            "unexpected classification: {error}"
        );
    }

    #[test]
    fn test_a_resident_document_is_matched_by_its_logical_path() {
        let resident = vec![
            format!("{OUTER_DOCS_CACHE_DIR}/openrouter/api.md"),
            format!("{OUTER_DOCS_CACHE_DIR}/vendor/api.md"),
        ];

        assert_eq!(
            affected_documents("openrouter", &resident),
            vec![resident[0].clone()]
        );
        assert_eq!(
            affected_documents("openrouter/api.md", &resident),
            vec![resident[0].clone()]
        );
        // A prefix that ends inside a name addresses no neighbour.
        assert!(affected_documents("openrouter/ap", &resident).is_empty());
        assert!(affected_documents("", &resident).is_empty());

        // A resident path stored without the cache directory is matched too.
        let logical = vec!["openrouter/api.md".to_string()];
        assert_eq!(affected_documents("openrouter", &logical), logical);
        assert_eq!(logical_path(&resident[0]), Some("openrouter/api.md"));
        assert_eq!(logical_path("src/lib.rs"), None);
    }

    /// A materialized document reaches every layer that names it under its
    /// logical path: the OuterDocs context layer and the rendered `<file>`
    /// element share the mount point, while the cache directory the snapshot
    /// occupies never leaves the storage substrate.
    #[tokio::test]
    async fn test_a_document_reaches_the_context_under_its_logical_path() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/book/index.md"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("# MyLib\n\n- [Architecture](architecture.md)\n"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/book/architecture.md"))
            .respond_with(ResponseTemplate::new(200).set_body_string("layouts and contracts"))
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let mut declared = entry("mylib", "MyLib Book");
        declared.source = format!("{}/book/index.md", server.uri());
        let registry = OuterDocsRegistry::new(&[declared]);
        let trust = Some(RefsTrust::for_workspace(dir.path(), Vec::new(), true));

        let mut cached = Vec::new();
        for request in ["mylib", "mylib/architecture.md"] {
            cached.push(
                registry
                    .materialize_declared_with(
                        dir.path(),
                        request,
                        MAX_DOCUMENT_BYTES,
                        None,
                        trust.clone(),
                    )
                    .await
                    .expect("the declared document materializes"),
            );
        }
        assert_eq!(
            cached,
            vec![
                format!("{OUTER_DOCS_CACHE_DIR}/mylib/index.md"),
                format!("{OUTER_DOCS_CACHE_DIR}/mylib/architecture.md"),
            ]
        );

        // The layer is filled with the cache paths the materialization reported,
        // and it answers the logical paths the rest of the harness addresses.
        let mut context = crate::context::ContextManager::new(dir.path().to_path_buf());
        for cache_path in &cached {
            context
                .add_outer_docs_file(cache_path)
                .expect("the snapshot enters the context");
        }

        let state = context.get_state();
        let documents: Vec<&str> = state
            .items
            .iter()
            .filter(|item| item.layer == ContextLayer::OuterDocs)
            .map(|item| item.path.as_str())
            .collect();
        assert_eq!(documents, vec!["mylib/index.md", "mylib/architecture.md"]);
        assert!(
            state
                .items
                .iter()
                .all(|item| !item.path.contains(OUTER_DOCS_CACHE_DIR)),
            "the cache directory must never reach the wire contract"
        );

        // The rendered element carries the logical path and the snapshot's bytes.
        let rendered: Vec<(String, String)> = context
            .read_context_files()
            .into_iter()
            .map(|file| (file.path, file.content))
            .collect();
        assert_eq!(
            rendered,
            vec![
                (
                    "mylib/index.md".to_string(),
                    "# MyLib\n\n- [Architecture](architecture.md)\n".to_string(),
                ),
                (
                    "mylib/architecture.md".to_string(),
                    "layouts and contracts".to_string(),
                ),
            ]
        );
    }
}
