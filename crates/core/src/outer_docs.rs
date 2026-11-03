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

use std::path::Path;

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

/// Byte ceiling of one materialized snapshot.
///
/// A declared document is materialized before it competes for the single
/// `[context].token_budget` ceiling, so this ceiling bounds only what one transfer
/// may buffer: a source larger than it is refused with a diagnostic instead of
/// being fetched and rejected by the token budget afterwards.
pub const MAX_DOCUMENT_BYTES: u64 = 4 * 1024 * 1024;

/// Reports whether `path` is a clean repository-relative path written with `/`.
///
/// The declaration is portable, so the rule is stated on the raw segments: an
/// empty path, an empty segment (which covers an absolute path, a doubled
/// separator, and a trailing separator), a `.` or `..` segment, a backslash, a
/// NUL, and a `:` (which would smuggle a URL scheme or a drive letter) are all
/// rejected. The platform path then answers the remaining question, whether the
/// result could still name an absolute location.
pub fn is_clean_relative_path(path: &str) -> bool {
    if path.is_empty() || path.contains('\\') || path.contains('\0') || path.contains(':') {
        return false;
    }
    let portable = path
        .split('/')
        .all(|segment| !segment.is_empty() && segment != "." && segment != "..");
    portable && !Path::new(path).is_absolute()
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
        "the request `{id}/{relative}` leaves the declared root `{id}`: a documented request addresses the root document or a clean relative path below its directory, never a parent directory, an absolute path, or another scheme",
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
    let relative = path.strip_prefix(OUTER_DOCS_CACHE_DIR)?.strip_prefix('/')?;
    if relative.is_empty() {
        return None;
    }
    Some(relative.to_string())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use tempfile::tempdir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::{
        document_cache_path, document_dir, is_clean_relative_path, is_outer_docs_cache_path,
        relative_cache_path, OuterDoc, OuterDocsRegistry, MAX_DOCUMENT_BYTES, OUTER_DOCS_CACHE_DIR,
    };
    use crate::config::{OuterDocEntry, OuterDocSource};
    use crate::outer_docs::trust::{RefsTrust, TrustRefusal};

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
}
