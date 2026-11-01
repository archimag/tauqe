//! Materialization of declared references into the isolated cache.
//!
//! The registry manifest declares where a reference comes from; this module
//! turns that declaration into the file the model actually reads. Every entry
//! lands in the isolated, Git-ignored cache at the path
//! [`super::cache_path_for`] derives from its registry name, so a relative link
//! inside a snapshot resolves inside the cache and never reaches the working
//! tree.
//!
//! The snapshot is immutable for the whole task. Once a cache file exists it is
//! answered as is: a repeated materialization performs no network request and no
//! copy, which keeps the material the model reasoned about stable and keeps
//! repeated Discovery rounds free of redundant downloads.
//!
//! A remote source is fetched over HTTP with the project wall-clock budget,
//! bounded by the configured reference budget, and races the turn cancellation
//! signal: a slow reference never outlives the turn that requested it, and an
//! oversized one is abandoned while it streams. A local source has its size
//! inspected before it is read. Both
//! write through [`tauqe_tools::atomic_write::write_atomic`], so a reader observes
//! either the previous complete snapshot or the new one, never a truncated file,
//! and every filesystem operation runs off the async executor thread through
//! [`crate::blocking`]. A reference that cannot be materialized returns a compact
//! reason and never panics: the failure degrades into a diagnostic of the
//! requesting round.
//!
//! A Discovery round reaches the cache lazily: [`materialize_requests`] turns a
//! request that names a declared entry into a request for the cached snapshot,
//! so the model asks for a reference by its registry name and reads the isolated
//! copy within the same round.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use tauqe_protocol::ContextAccess;
use tokio::sync::watch;

use super::{cache_path_for, RefsEntry, RefsSource};
use super::{is_refs_registry_path, load_registry, registry_name, RefsRegistry};
use crate::edits::protocol::ContextRequest;
use crate::toolchain::DEFAULT_PROJECT_TIMEOUT_SECS;

/// Maximum time to establish a connection to a reference host.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Returns the HTTP client every reference fetch shares.
///
/// The client carries the connection budget only. The per-request budget comes
/// from `DEFAULT_PROJECT_TIMEOUT_SECS`, because a vendored document may
/// legitimately take far longer to arrive than a small API answer.
fn http_client() -> Result<reqwest::Client, String> {
    static CLIENT: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .build()
                .map_err(|error| format!("the reference HTTP client could not be built: {error}"))
        })
        .clone()
}

/// Materializes `entry` into the isolated cache and returns its cache path.
///
/// The returned path is repository-relative and lives under
/// [`super::REFS_CACHE_DIR`]. It is returned immediately when the snapshot is
/// already present: the cache holds one immutable snapshot of the declaration,
/// so a repeated request never re-downloads and never re-copies it.
///
/// A remote source is fetched with the project wall-clock budget and abandoned
/// when the turn is cancelled or the document passes the reference budget,
/// which is applied while the body streams; a local source has its size
/// inspected before it is read. Both land through
/// [`tauqe_tools::atomic_write::write_atomic`], so a crash can never publish a
/// truncated snapshot, and an unreachable host degrades into a diagnostic.
pub async fn materialize_entry(
    repo_root: &Path,
    entry: &RefsEntry,
    refs_token_budget: usize,
    cancel_rx: Option<&watch::Receiver<bool>>,
) -> Result<String, String> {
    let cache_path = cache_path_for(&entry.name);
    let target = repo_root.join(&cache_path);

    // The registry name is validated as a clean repository-relative path, so the
    // snapshot always stays inside the repository. Verifying it here keeps the
    // invariant next to the write instead of trusting every caller.
    if !target.starts_with(repo_root) {
        return Err(format!(
            "the reference `{}` does not resolve inside the repository",
            entry.name
        ));
    }

    if cache_file_exists(&target, &entry.name).await? {
        return Ok(cache_path);
    }

    // The transfer is bounded by the same estimate `add_refs_file` applies on
    // admission, so a document that could never enter the Refs layer is
    // abandoned while it streams instead of after it was fully buffered.
    let max_bytes = byte_ceiling(refs_token_budget);

    match &entry.source {
        RefsSource::Url(url) => {
            let bytes = fetch_url(url, max_bytes, cancel_rx).await?;
            store_snapshot(repo_root, &cache_path, &entry.name, bytes).await?;
        }
        RefsSource::LocalPath(source) => {
            let bytes = read_local_source(source, max_bytes).await?;
            store_snapshot(repo_root, &cache_path, &entry.name, bytes).await?;
        }
    }

    Ok(cache_path)
}

/// Resolves the declared registry requests of one Discovery round.
///
/// A request that names a declared entry under the tracked registry directory
/// has no file on disk until the harness materializes it. The entry is looked up
/// by name in the manifest, its content lands in the isolated cache, and the
/// request is rewritten to the cache path with read-only access, so the round
/// that asked for a reference reads the snapshot the registry declares.
///
/// Every other request travels unchanged: a registry file that already exists on
/// disk is read where it is — the manifest above all, which declares the entries
/// instead of being one — a name the manifest does not declare stays an ordinary
/// request, and a path outside the registry directory is not a reference at all.
///
/// A refused materialization keeps the original request, which the round then
/// reports as missing, and yields one diagnostic naming the reason, so the model
/// learns why the declared document could not be loaded instead of waiting for a
/// file that will never appear. The answers are aligned with the requests: one
/// resolved request per input, in the order the round declared them.
///
/// Every transfer is bounded by `refs_token_budget`, the budget the Refs layer
/// applies on admission, so an oversized document is abandoned while it
/// streams instead of after it was fully buffered.
pub async fn materialize_requests(
    repo_root: &Path,
    requests: &[ContextRequest],
    refs_token_budget: usize,
    cancel_rx: Option<&watch::Receiver<bool>>,
) -> (Vec<ContextRequest>, Vec<String>) {
    let mut resolved: Vec<ContextRequest> = Vec::with_capacity(requests.len());
    let mut feedback: Vec<String> = Vec::new();
    // The manifest is read at most once per round, and only when a request
    // actually names a declared entry.
    let mut registry: Option<RefsRegistry> = None;

    for request in requests {
        let name = if is_refs_registry_path(&request.path) {
            registry_name(&request.path)
        } else {
            None
        };
        let Some(name) = name else {
            resolved.push(request.clone());
            continue;
        };
        // A registry file that already exists on disk is read as it is: the
        // tracked manifest and any document committed beside it were never
        // meant to be copied into the cache.
        if registry_file_exists(repo_root, &request.path).await {
            resolved.push(request.clone());
            continue;
        }
        if registry.is_none() {
            registry = Some(read_registry(repo_root).await);
        }
        let entry = registry
            .as_ref()
            .and_then(|registry| registry.entries.iter().find(|entry| entry.name == name))
            .cloned();
        let Some(entry) = entry else {
            resolved.push(request.clone());
            continue;
        };
        match materialize_entry(repo_root, &entry, refs_token_budget, cancel_rx).await {
            Ok(cache_path) => resolved.push(ContextRequest {
                path: cache_path,
                access: ContextAccess::ReadOnly,
            }),
            Err(reason) => {
                feedback.push(refs_failure_block(&request.path, &reason));
                resolved.push(request.clone());
            }
        }
    }

    (resolved, feedback)
}

/// Reads the registry manifest of `repo_root` off the async executor thread.
async fn read_registry(repo_root: &Path) -> RefsRegistry {
    let root = repo_root.to_path_buf();
    match crate::blocking::run_blocking_async(move || load_registry(&root)).await {
        Ok(registry) => registry,
        Err(error) => {
            crate::blocking::log_join_error(&error, "reference registry read");
            RefsRegistry::default()
        }
    }
}

/// Reports whether the registry path `path` already exists in the working tree.
async fn registry_file_exists(repo_root: &Path, path: &str) -> bool {
    let target = repo_root.join(path);
    match crate::blocking::run_blocking_async(move || target.is_file()).await {
        Ok(exists) => exists,
        Err(error) => {
            crate::blocking::log_join_error(&error, "reference registry path inspection");
            false
        }
    }
}

/// Renders the diagnostic of a reference that could not be materialized.
fn refs_failure_block(path: &str, reason: &str) -> String {
    format!(
        "<refs_feedback>\nThe declared reference `{path}` could not be materialized: {reason}\nThe request stays unsatisfied for this round: answer from the material you already hold, and ask the developer to fix the declared source when the document is required.\n</refs_feedback>"
    )
}

/// Reports whether the snapshot of `name` already occupies `target`.
async fn cache_file_exists(target: &Path, name: &str) -> Result<bool, String> {
    let target = target.to_path_buf();
    let name = name.to_string();
    crate::blocking::run_blocking_async(move || target.is_file())
        .await
        .map_err(|error| {
            crate::blocking::log_join_error(&error, "reference cache inspection");
            format!("the cached snapshot of `{name}` could not be inspected")
        })
}

/// Byte length of one estimated token, matching `estimate_tokens_from_bytes`.
///
/// The Refs layer prices a document at `ceil(bytes / 4)` tokens, so the same
/// ratio inverts into the largest snapshot one token budget admits.
const BYTES_PER_TOKEN: u64 = 4;

/// Converts a reference token budget into the byte ceiling of one snapshot.
///
/// A snapshot at or below the result is priced at no more than
/// `refs_token_budget` tokens by the estimate the Refs layer applies, so the
/// transfer-side bound and the admission check agree on the same document.
fn byte_ceiling(refs_token_budget: usize) -> u64 {
    (refs_token_budget as u64).saturating_mul(BYTES_PER_TOKEN)
}

/// Renders the diagnostic of a document abandoned for exceeding the budget.
fn oversized(url: &str, max_bytes: u64) -> String {
    let refs_token_budget = max_bytes / BYTES_PER_TOKEN;
    format!(
        "the reference `{url}` exceeds the configured refs budget of {refs_token_budget} tokens, so the transfer was abandoned instead of buffering the document"
    )
}

/// Downloads `url` into memory, honouring the size ceiling and the turn
/// cancellation signal.
///
/// The body is streamed instead of buffered in one call: the transfer stops as
/// soon as the accumulated bytes pass `max_bytes`, so a host that serves an
/// oversized document is abandoned while it streams instead of after it has
/// exhausted process memory. The request timeout bounds duration only, which is
/// why the size is bounded here as well.
async fn fetch_url(
    url: &str,
    max_bytes: u64,
    cancel_rx: Option<&watch::Receiver<bool>>,
) -> Result<Vec<u8>, String> {
    if cancel_requested(cancel_rx) {
        return Err(cancelled(url));
    }

    let client = http_client()?;
    // Cancellation covers the whole exchange, the response headers and the body,
    // so an abandoned turn stops waiting for a slow host immediately.
    let request = async {
        let mut response = client
            .get(url)
            .timeout(Duration::from_secs(DEFAULT_PROJECT_TIMEOUT_SECS))
            .send()
            .await
            .map_err(|error| format!("the reference `{url}` could not be fetched: {error}"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!(
                "the reference `{url}` answered with the HTTP status {}",
                status.as_u16()
            ));
        }
        let mut body: Vec<u8> = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| format!("the reference `{url}` could not be read: {error}"))?
        {
            let accumulated = (body.len() as u64).saturating_add(chunk.len() as u64);
            if accumulated > max_bytes {
                return Err(oversized(url, max_bytes));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    };

    tokio::select! {
        () = wait_for_cancellation(cancel_rx) => Err(cancelled(url)),
        bytes = request => bytes,
    }
}

/// Reads a local reference source from its absolute path.
///
/// The metadata is inspected inside the blocking phase, before the file is
/// read, so a document that could never enter the Refs layer is rejected
/// without being loaded into memory.
async fn read_local_source(source: &str, max_bytes: u64) -> Result<Vec<u8>, String> {
    let path = PathBuf::from(source);
    let display = source.to_string();
    crate::blocking::run_blocking_async(move || -> anyhow::Result<Vec<u8>> {
        let metadata = std::fs::metadata(&path)?;
        if metadata.len() > max_bytes {
            anyhow::bail!("refusing to load a document larger than the configured refs budget");
        }
        Ok(std::fs::read(&path)?)
    })
    .await
    .map_err(|error| {
        crate::blocking::log_join_error(&error, "reference source read");
        format!("the local reference source `{display}` could not be read")
    })?
    .map_err(|error| format!("the local reference source `{display}` could not be read: {error}"))
}

/// Writes one snapshot into the isolated cache through the atomic replacement.
async fn store_snapshot(
    repo_root: &Path,
    cache_path: &str,
    name: &str,
    bytes: Vec<u8>,
) -> Result<(), String> {
    let target = repo_root.join(cache_path);
    crate::blocking::run_blocking_async(move || -> anyhow::Result<()> {
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        tauqe_tools::atomic_write::write_atomic(&target, &bytes)
    })
    .await
    .map_err(|error| {
        crate::blocking::log_join_error(&error, "reference snapshot write");
        format!("the snapshot of `{name}` could not be written to `{cache_path}`")
    })?
    .map_err(|error| {
        format!("the snapshot of `{name}` could not be written to `{cache_path}`: {error}")
    })
}

/// Reports whether the turn cancellation signal already fired.
fn cancel_requested(cancel_rx: Option<&watch::Receiver<bool>>) -> bool {
    cancel_rx.is_some_and(|receiver| *receiver.borrow())
}

/// Resolves once the turn cancellation signal fires.
///
/// The signal is optional: a caller outside a turn carries none, so the future
/// never resolves and the `select!` branch never wins. A dropped sender parks
/// the branch for the same reason: no signal can arrive again, so waiting must
/// not turn into an immediate cancellation.
async fn wait_for_cancellation(cancel_rx: Option<&watch::Receiver<bool>>) {
    let Some(receiver) = cancel_rx else {
        std::future::pending::<()>().await;
        return;
    };
    let mut receiver = receiver.clone();
    // `wait_for` yields a `watch::Ref` guard, which is not `Send`. The guard is
    // dropped before this future returns, so no caller future is forced to hold
    // it across an await.
    if receiver.wait_for(|cancelled| *cancelled).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// The reason a cancelled materialization reports.
fn cancelled(url: &str) -> String {
    format!("the reference `{url}` was not materialized because the turn was cancelled")
}

#[cfg(test)]
mod tests {
    use super::{byte_ceiling, materialize_entry, materialize_requests};
    use crate::edits::protocol::ContextRequest;
    use crate::refs::{
        cache_path_for, is_refs_cache_path, RefsEntry, RefsSource, REFS_CACHE_DIR, REFS_DIR,
        REFS_MANIFEST,
    };
    use std::path::Path;
    use std::time::Duration;
    use tauqe_protocol::ContextAccess;
    use tempfile::tempdir;
    use tokio::sync::watch;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Reference budget the fixtures that do not exercise the bound fit into.
    const TEST_REFS_BUDGET: usize = 50_000;

    /// A declared remote reference.
    fn url_entry(name: &str, url: &str) -> RefsEntry {
        RefsEntry {
            title: "reference".to_string(),
            name: name.to_string(),
            source: RefsSource::Url(url.to_string()),
        }
    }

    /// A declared local reference.
    fn local_entry(name: &str, source: &Path) -> RefsEntry {
        RefsEntry {
            title: "reference".to_string(),
            name: name.to_string(),
            source: RefsSource::LocalPath(source.to_string_lossy().into_owned()),
        }
    }

    #[tokio::test]
    async fn test_a_url_reference_lands_in_the_isolated_cache() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/llm.txt"))
            .respond_with(ResponseTemplate::new(200).set_body_string("upstream notes"))
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let entry = url_entry("vendor/api.md", &format!("{}/llm.txt", server.uri()));

        let cache_path = materialize_entry(dir.path(), &entry, TEST_REFS_BUDGET, None)
            .await
            .expect("the declared reference materializes");

        assert_eq!(cache_path, cache_path_for("vendor/api.md"));
        assert!(is_refs_cache_path(&cache_path));

        let stored = std::fs::read_to_string(dir.path().join(&cache_path)).unwrap();
        assert_eq!(stored, "upstream notes");

        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_a_materialized_snapshot_is_never_downloaded_again() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/llm.txt"))
            .respond_with(ResponseTemplate::new(200).set_body_string("first snapshot"))
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let entry = url_entry("vendor/api.md", &format!("{}/llm.txt", server.uri()));

        let first = materialize_entry(dir.path(), &entry, TEST_REFS_BUDGET, None)
            .await
            .unwrap();

        // Rewriting the cached snapshot proves a second materialization reads it
        // instead of the network: the file keeps bytes the origin never served.
        std::fs::write(dir.path().join(&first), "kept snapshot").unwrap();

        let second = materialize_entry(dir.path(), &entry, TEST_REFS_BUDGET, None)
            .await
            .unwrap();

        assert_eq!(second, first);
        let stored = std::fs::read_to_string(dir.path().join(&second)).unwrap();
        assert_eq!(stored, "kept snapshot");
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "a materialized snapshot must not be downloaded again"
        );
    }

    #[tokio::test]
    async fn test_a_local_reference_is_copied_from_its_absolute_path() {
        let outside = tempdir().unwrap();
        let source = outside.path().join("protocol.md");
        std::fs::write(&source, "protocol summary").unwrap();

        let dir = tempdir().unwrap();
        let entry = local_entry("vendor/protocol.md", &source);

        let cache_path = materialize_entry(dir.path(), &entry, TEST_REFS_BUDGET, None)
            .await
            .unwrap();

        assert_eq!(cache_path, cache_path_for("vendor/protocol.md"));
        let stored = std::fs::read_to_string(dir.path().join(&cache_path)).unwrap();
        assert_eq!(stored, "protocol summary");
    }

    #[tokio::test]
    async fn test_a_failing_url_reports_a_compact_reason() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/missing.txt"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let entry = url_entry(
            "vendor/missing.md",
            &format!("{}/missing.txt", server.uri()),
        );

        let error = materialize_entry(dir.path(), &entry, TEST_REFS_BUDGET, None)
            .await
            .expect_err("a failing download surfaces a diagnostic");

        assert!(error.contains("404"), "unexpected reason: {error}");
        assert!(!dir
            .path()
            .join(cache_path_for("vendor/missing.md"))
            .exists());
    }

    #[tokio::test]
    async fn test_a_missing_local_source_reports_a_compact_reason() {
        let outside = tempdir().unwrap();
        let missing = outside.path().join("absent.md");

        let dir = tempdir().unwrap();
        let entry = local_entry("vendor/absent.md", &missing);

        let error = materialize_entry(dir.path(), &entry, TEST_REFS_BUDGET, None)
            .await
            .expect_err("an unreadable source surfaces a diagnostic");

        assert!(error.contains("absent.md"), "unexpected reason: {error}");
        assert!(!dir.path().join(cache_path_for("vendor/absent.md")).exists());
    }

    #[tokio::test]
    async fn test_a_cancelled_turn_abandons_the_download() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/slow.txt"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_secs(60))
                    .set_body_string("late"),
            )
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let url = format!("{}/slow.txt", server.uri());
        let root = dir.path().to_path_buf();
        let (cancel_tx, cancel_rx) = watch::channel(false);

        let materialization = tokio::spawn(async move {
            let entry = url_entry("vendor/slow.md", &url);
            materialize_entry(&root, &entry, TEST_REFS_BUDGET, Some(&cancel_rx)).await
        });

        // Let the request reach the server before the turn is cancelled.
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel_tx.send(true).unwrap();

        let error = tokio::time::timeout(Duration::from_secs(5), materialization)
            .await
            .expect("cancellation must resolve promptly")
            .expect("the materialization task must not panic")
            .expect_err("a cancelled turn materializes nothing");

        assert!(error.contains("cancelled"), "unexpected reason: {error}");
        assert!(!dir.path().join(cache_path_for("vendor/slow.md")).exists());
    }

    #[tokio::test]
    async fn test_a_pre_cancelled_turn_never_reaches_the_network() {
        let server = MockServer::start().await;
        let dir = tempdir().unwrap();
        let entry = url_entry("vendor/api.md", &format!("{}/llm.txt", server.uri()));
        let (_cancel_tx, cancel_rx) = watch::channel(true);

        let error = materialize_entry(dir.path(), &entry, TEST_REFS_BUDGET, Some(&cancel_rx))
            .await
            .expect_err("a cancelled turn materializes nothing");

        assert!(error.contains("cancelled"), "unexpected reason: {error}");
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_the_snapshot_stays_inside_the_isolated_cache() {
        let outside = tempdir().unwrap();
        let source = outside.path().join("api.md");
        std::fs::write(&source, "external notes").unwrap();

        let dir = tempdir().unwrap();
        let entry = local_entry("vendor/nested/api.md", &source);

        let cache_path = materialize_entry(dir.path(), &entry, TEST_REFS_BUDGET, None)
            .await
            .unwrap();

        let target = dir.path().join(&cache_path);
        assert!(
            target.starts_with(dir.path()),
            "the snapshot must stay inside the repository"
        );
        assert!(target.starts_with(dir.path().join(REFS_CACHE_DIR)));
        assert!(
            !dir.path().join(REFS_DIR).exists(),
            "the tracked registry must stay untouched"
        );
    }

    /// Writes a registry manifest declaring one remote entry.
    fn write_manifest(root: &Path, name: &str, url: &str) -> String {
        let declared = format!("- [reference]({name}) = {url}\n");
        let manifest = root.join(REFS_MANIFEST);
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        std::fs::write(&manifest, &declared).unwrap();
        declared
    }

    /// A read-only request for a registry path.
    fn registry_request(path: &str) -> ContextRequest {
        ContextRequest {
            path: path.to_string(),
            access: ContextAccess::ReadOnly,
        }
    }

    /// A request for a declared name resolves to the cached snapshot, so the
    /// round that asked for the reference reads it without a second round trip.
    #[tokio::test]
    async fn test_a_declared_registry_request_resolves_to_its_snapshot() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/llm.txt"))
            .respond_with(ResponseTemplate::new(200).set_body_string("upstream notes"))
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let url = format!("{}/llm.txt", server.uri());
        write_manifest(dir.path(), "vendor/api.md", &url);
        let request = registry_request(".tauqe/refs/vendor/api.md");

        let (resolved, feedback) =
            materialize_requests(dir.path(), &[request], TEST_REFS_BUDGET, None).await;

        assert!(feedback.is_empty(), "unexpected diagnostics: {feedback:?}");
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].path, cache_path_for("vendor/api.md"));
        assert_eq!(resolved[0].access, ContextAccess::ReadOnly);
        let stored = std::fs::read_to_string(dir.path().join(&resolved[0].path)).unwrap();
        assert_eq!(stored, "upstream notes");
    }

    #[tokio::test]
    async fn test_a_resolved_registry_request_is_never_downloaded_again() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/llm.txt"))
            .respond_with(ResponseTemplate::new(200).set_body_string("upstream notes"))
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let url = format!("{}/llm.txt", server.uri());
        write_manifest(dir.path(), "vendor/api.md", &url);
        let request = registry_request(".tauqe/refs/vendor/api.md");

        let requests = std::slice::from_ref(&request);
        let (first, feedback) =
            materialize_requests(dir.path(), requests, TEST_REFS_BUDGET, None).await;
        assert!(feedback.is_empty());
        let (second, feedback) =
            materialize_requests(dir.path(), &[request], TEST_REFS_BUDGET, None).await;
        assert!(feedback.is_empty());

        assert_eq!(first[0].path, second[0].path);
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "a materialized snapshot must not be downloaded again"
        );
    }

    #[tokio::test]
    async fn test_a_failing_registry_source_is_reported_and_the_request_is_kept() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/missing.txt"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let url = format!("{}/missing.txt", server.uri());
        write_manifest(dir.path(), "vendor/missing.md", &url);
        let request = registry_request(".tauqe/refs/vendor/missing.md");

        let requests = std::slice::from_ref(&request);
        let (resolved, feedback) =
            materialize_requests(dir.path(), requests, TEST_REFS_BUDGET, None).await;

        assert_eq!(resolved, vec![request]);
        assert_eq!(feedback.len(), 1);
        assert!(feedback[0].contains(".tauqe/refs/vendor/missing.md"));
        assert!(feedback[0].contains("404"));
        assert!(!dir
            .path()
            .join(cache_path_for("vendor/missing.md"))
            .exists());
    }

    #[tokio::test]
    async fn test_an_undeclared_registry_name_stays_an_ordinary_request() {
        let server = MockServer::start().await;
        let dir = tempdir().unwrap();
        let url = format!("{}/llm.txt", server.uri());
        write_manifest(dir.path(), "vendor/api.md", &url);
        let request = registry_request(".tauqe/refs/vendor/other.md");

        let requests = std::slice::from_ref(&request);
        let (resolved, feedback) =
            materialize_requests(dir.path(), requests, TEST_REFS_BUDGET, None).await;

        assert_eq!(resolved, vec![request]);
        assert!(feedback.is_empty());
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_the_registry_manifest_is_never_rewritten_by_the_pre_step() {
        let server = MockServer::start().await;
        let dir = tempdir().unwrap();
        let url = format!("{}/llm.txt", server.uri());
        let declared = write_manifest(dir.path(), "vendor/api.md", &url);
        let request = registry_request(".tauqe/refs/refs.md");

        let requests = std::slice::from_ref(&request);
        let (resolved, feedback) =
            materialize_requests(dir.path(), requests, TEST_REFS_BUDGET, None).await;

        assert_eq!(resolved, vec![request]);
        assert!(feedback.is_empty());
        assert!(server.received_requests().await.unwrap().is_empty());
        assert_eq!(
            std::fs::read_to_string(dir.path().join(REFS_MANIFEST)).unwrap(),
            declared
        );
    }

    /// A declared document whose body passes the reference budget is abandoned
    /// while it streams: the materialization reports a compact reason instead of
    /// buffering the document, and no snapshot is left in the isolated cache.
    #[tokio::test]
    async fn test_an_oversized_remote_reference_aborts_before_it_is_buffered() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/huge.txt"))
            .respond_with(ResponseTemplate::new(200).set_body_string("x".repeat(8192)))
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let entry = url_entry("vendor/huge.md", &format!("{}/huge.txt", server.uri()));

        // One token admits four bytes, so the transfer is abandoned long before
        // the whole body could be accumulated in memory.
        let error = materialize_entry(dir.path(), &entry, 1, None)
            .await
            .expect_err("an oversized body surfaces a diagnostic");

        assert!(error.contains("refs budget"), "unexpected reason: {error}");
        assert!(!dir.path().join(cache_path_for("vendor/huge.md")).exists());
    }

    /// A local source is inspected before it is read, so a document larger than
    /// the budget is refused without ever being loaded into memory.
    #[tokio::test]
    async fn test_an_oversized_local_reference_is_refused_before_it_is_read() {
        let outside = tempdir().unwrap();
        let source = outside.path().join("huge.md");
        std::fs::write(&source, "x".repeat(8192)).unwrap();

        let dir = tempdir().unwrap();
        let entry = local_entry("vendor/huge.md", &source);

        let error = materialize_entry(dir.path(), &entry, 1, None)
            .await
            .expect_err("an oversized source surfaces a diagnostic");

        assert!(error.contains("refs budget"), "unexpected reason: {error}");
        assert!(!dir.path().join(cache_path_for("vendor/huge.md")).exists());
    }

    /// The transfer ceiling and the admission estimate agree on the same
    /// document, so the streaming bound never admits a snapshot the Refs layer
    /// would refuse immediately afterwards.
    #[test]
    fn test_the_transfer_ceiling_matches_the_admission_estimate() {
        for refs_token_budget in [0usize, 1, 7, 50_000] {
            let ceiling = byte_ceiling(refs_token_budget);
            assert_eq!(
                crate::prompt::estimate_tokens_from_bytes(ceiling) as usize,
                refs_token_budget
            );
            assert!(
                crate::prompt::estimate_tokens_from_bytes(ceiling + 1) as usize > refs_token_budget
            );
        }
    }
}
