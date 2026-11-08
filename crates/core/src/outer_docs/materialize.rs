//! Materialization of declared external documentation into the isolated cache.
//!
//! The registry declares where a document comes from; this module turns that
//! declaration into the files the model reads. Every declared root lands in the
//! isolated, Git-ignored cache inside the directory [`super::document_dir`]
//! derives from its `id`, and every relative document lands below that same
//! directory, so the cache mirrors the addressing rule of the prefix jail: one
//! root file, and the parts beside it.
//!
//! The snapshot is immutable: once a cache file exists it is answered as is, so a
//! repeated request performs no network transfer and no copy, and the material the
//! model reasoned about stays stable for the whole task. The cache is consulted
//! before the trust declaration is even read: a snapshot the harness already holds
//! is local material, and no later decision revokes what an earlier turn
//! delivered.
//!
//! A remote source is fetched with the project wall-clock budget, bounded by the
//! caller's byte ceiling, and races the turn cancellation signal. A declared
//! `Content-Length` above that ceiling and a non-textual `Content-Type` are
//! refused before the body is read, and a local source has its size inspected
//! before it is read. Nothing enters the cache unless it is valid UTF-8 text.
//! Both write through [`tauqe_tools::atomic_write::write_atomic`], so a reader
//! observes either the previous complete snapshot or the new one, never a
//! truncated file, and every filesystem operation runs off the async executor
//! thread through [`crate::blocking`].
//!
//! # The developer's trust boundary
//! The declaration travels with the repository, so it records what the project
//! wants rather than what the developer allowed. Every source is therefore checked
//! against the workspace trust declaration (`RefsTrust`) before a byte moves: a
//! local source outside the repository is read only from a directory the developer
//! declared, a local source inside it is the project's own material, and a remote
//! source must resolve to a public address — or to a non-public one the developer
//! allowed — before a connection is opened.
//! Redirects are followed hop by hop through the same check, and a relative
//! document inherits the authorization of its root, because it is read below the
//! very directory that declaration names.
//!
//! Every failure keeps the audience it belongs to. A decision of the trust
//! declaration is a [`MaterializeError::Trust`] and addresses the developer,
//! because no retry inside a turn can widen a declaration the repository cannot
//! reach. Everything else — an HTTP status, a timeout, a cancelled turn, an
//! oversized body, an unreadable file — is a [`MaterializeError::Delivery`] and
//! addresses the model, which routes around the missing document instead of
//! retrying it.

use std::net::{SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use tokio::sync::watch;

use super::trust::{literal_address, load_trust, RefsTrust, TrustRefusal};
use super::{document_cache_path, relative_cache_path, OuterDoc};
use crate::config::OuterDocSource;
use crate::toolchain::DEFAULT_PROJECT_TIMEOUT_SECS;

/// Why one declared document could not be materialized.
///
/// The two kinds are addressed to different readers and are therefore never
/// merged: a trust refusal asks the developer to widen a declaration the
/// repository cannot reach, while a delivery failure asks the model to work with
/// the material it already holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MaterializeError {
    /// The developer's trust declaration refuses the declared source.
    Trust(TrustRefusal),
    /// The source could not be delivered to the isolated cache.
    Delivery(String),
}

impl MaterializeError {
    /// The refusal, when the failure is a trust decision rather than delivery.
    pub fn trust(&self) -> Option<&TrustRefusal> {
        match self {
            Self::Trust(refusal) => Some(refusal),
            Self::Delivery(_) => None,
        }
    }

    /// One-line cause of the failure.
    pub fn reason(&self) -> String {
        match self {
            Self::Trust(refusal) => refusal.reason(),
            Self::Delivery(reason) => reason.clone(),
        }
    }

    /// Ready-to-paste `[[workspace]]` declaration a trust refusal carries.
    pub fn remedy(&self) -> Option<String> {
        match self {
            Self::Trust(refusal) => refusal.remedy(),
            Self::Delivery(_) => None,
        }
    }
}

impl std::fmt::Display for MaterializeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason())
    }
}

impl From<TrustRefusal> for MaterializeError {
    fn from(refusal: TrustRefusal) -> Self {
        Self::Trust(refusal)
    }
}

impl From<String> for MaterializeError {
    fn from(reason: String) -> Self {
        Self::Delivery(reason)
    }
}

/// Maximum time to establish a connection to a documentation host.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum number of redirect hops one transfer follows.
///
/// The harness walks a redirect chain one hop at a time so every address it
/// reaches passes the trust check. The budget bounds a chain that never settles,
/// so a redirect loop is abandoned with a compact reason.
const MAX_REDIRECTS: usize = 5;

/// Materializes the declared root document against a trust declaration.
///
/// A snapshot the cache already holds is served without reading the declaration:
/// the material is local, and a request that never leaves the machine owes no
/// trust decision.
pub(crate) async fn materialize_document_with(
    repo_root: &Path,
    doc: &OuterDoc,
    max_bytes: u64,
    cancel_rx: Option<&watch::Receiver<bool>>,
    trust: Option<RefsTrust>,
) -> Result<String, MaterializeError> {
    let cache_path = document_cache_path(doc);
    let target = repo_root.join(&cache_path);
    ensure_inside_repository(&target, repo_root, &doc.id)?;
    if cache_file_exists(&target, &doc.id).await? {
        audit(&doc.id, "served the cached snapshot");
        return Ok(cache_path);
    }

    let declaration = resolve_trust(repo_root, trust).await;
    let bytes = match &doc.source {
        OuterDocSource::Remote(url) => fetch_bytes(url, max_bytes, &declaration, cancel_rx).await?,
        OuterDocSource::Local(path) => {
            read_local_source(&path.to_string_lossy(), max_bytes, declaration).await?
        }
    };
    store_snapshot(repo_root, &cache_path, &doc.id, bytes).await?;
    audit(&doc.id, "materialized");
    Ok(cache_path)
}

/// Materializes one relative document of a declared root.
///
/// The relative path is resolved beside the root document — below the directory
/// that holds it — so the reference the model followed in the root text names the
/// same file the harness caches. The jail itself is enforced by the caller before
/// this function runs; what remains here is the placement and the transfer.
pub(crate) async fn materialize_relative_with(
    repo_root: &Path,
    doc: &OuterDoc,
    relative: &str,
    max_bytes: u64,
    cancel_rx: Option<&watch::Receiver<bool>>,
    trust: Option<RefsTrust>,
) -> Result<String, MaterializeError> {
    let cache_path = relative_cache_path(doc, relative);
    let target = repo_root.join(&cache_path);
    ensure_inside_repository(&target, repo_root, &doc.id)?;
    if cache_file_exists(&target, relative).await? {
        audit(&doc.id, "served the cached relative document");
        return Ok(cache_path);
    }

    // Authorizing the root authorizes the subtree the jail keeps inside it, so a
    // relative document is read below the directory the declaration already
    // covers instead of demanding a second declaration of its own.
    let declaration = resolve_trust(repo_root, trust).await;
    let bytes = match &doc.source {
        OuterDocSource::Remote(base) => {
            let url = join_remote(base, relative)?;
            fetch_bytes(&url, max_bytes, &declaration, cancel_rx).await?
        }
        OuterDocSource::Local(root) => {
            let resolved = resolve_beside(root, relative, &doc.id)?;
            read_local_source(&resolved, max_bytes, declaration).await?
        }
    };
    store_snapshot(repo_root, &cache_path, relative, bytes).await?;
    audit(&doc.id, "materialized a relative document");
    Ok(cache_path)
}

/// Resolves `relative` beside the root document `root` on disk.
fn resolve_beside(root: &Path, relative: &str, id: &str) -> Result<String, MaterializeError> {
    let directory = root.parent().ok_or_else(|| {
        MaterializeError::Delivery(format!(
            "the root document `{}` of `{id}` names no directory to resolve `{relative}` against",
            root.display()
        ))
    })?;
    Ok(directory.join(relative).to_string_lossy().into_owned())
}

/// Resolves `relative` beside the root document `base` on the network.
///
/// The root names a file, so a relative document resolves beside it: the last
/// path segment is dropped before the relative path is joined, which places
/// `architecture.md` next to `index.md` instead of below it.
fn join_remote(base: &str, relative: &str) -> Result<String, String> {
    let mut url = reqwest::Url::parse(base).map_err(|error| {
        format!("the source `{base}` is not an address the harness can resolve: {error}")
    })?;
    let directory = match url.path().rsplit_once('/') {
        Some((prefix, _)) => format!("{prefix}/"),
        None => "/".to_string(),
    };
    url.set_path(&directory);
    url.set_query(None);
    url.set_fragment(None);
    let joined = url.join(relative).map_err(|error| {
        format!("the document `{relative}` does not resolve under `{base}`: {error}")
    })?;
    // The reference is resolved against the directory that holds the root
    // document, so the joined address stays inside that directory. A reference
    // the jail admitted cannot climb above it, and checking the joined path
    // makes that a property of the address a host receives rather than of the
    // spelling that produced it.
    if !joined.path().starts_with(directory.as_str()) {
        return Err(format!(
            "the document `{relative}` resolves to `{joined}`, which is not below the document directory `{directory}` of `{base}`"
        ));
    }
    Ok(joined.to_string())
}

/// Verifies that a cache path stays inside the repository.
fn ensure_inside_repository(target: &Path, repo_root: &Path, id: &str) -> Result<(), String> {
    if target.starts_with(repo_root) {
        return Ok(());
    }
    Err(format!(
        "the outer document `{id}` does not resolve inside the repository"
    ))
}

/// Records the origin of a snapshot in the semantic journal.
///
/// The record names the declaration and what the harness did with it, so a turn
/// can be reconstructed afterwards. The document itself is never logged.
fn audit(id: &str, outcome: &str) {
    tracing::info!(document = id, outcome, "outer documentation snapshot");
}

/// Returns the HTTP client every documentation fetch shares.
///
/// The client carries the connection budget only. The per-request budget comes
/// from `DEFAULT_PROJECT_TIMEOUT_SECS`, because a vendored corpus may legitimately
/// take far longer to arrive than a small API answer.
///
/// Redirects are never followed by the client: the harness follows them itself
/// so every hop passes the trust check, which a client that followed them behind
/// its back would defeat.
fn http_client() -> Result<reqwest::Client, String> {
    static CLIENT: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|error| {
                    format!("the documentation HTTP client could not be built: {error}")
                })
        })
        .clone()
}

/// Builds the client of one transfer whose connection is pinned to `address`.
///
/// The harness resolves and approves the address itself, so the connection is
/// bound to exactly that address instead of resolving the host a second time.
fn pinned_client(host: &str, address: std::net::SocketAddr) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .resolve(host, address)
        .build()
        .map_err(|error| {
            format!("the documentation HTTP client for `{host}` could not be built: {error}")
        })
}

/// Downloads `url` into memory, honouring the size ceiling, the trust
/// declaration, and the turn cancellation signal.
async fn fetch_bytes(
    url: &str,
    max_bytes: u64,
    trust: &RefsTrust,
    cancel_rx: Option<&watch::Receiver<bool>>,
) -> Result<Vec<u8>, MaterializeError> {
    let parsed = reqwest::Url::parse(url).map_err(|error| {
        MaterializeError::Delivery(format!(
            "the document `{url}` is not an address the harness can fetch: {error}"
        ))
    })?;
    trust.check_remote_url(&parsed)?;
    if cancel_requested(cancel_rx) {
        return Err(cancelled(url));
    }

    let client = http_client()?;
    let display = parsed.to_string();
    // Cancellation covers the whole exchange, every redirect hop and the body,
    // so an abandoned turn stops waiting for a slow host immediately.
    let transfer = fetch_hops(&client, &parsed, max_bytes, trust);

    tokio::select! {
        () = wait_for_cancellation(cancel_rx) => Err(cancelled(&display)),
        bytes = transfer => bytes,
    }
}

/// Follows the transfer through at most `MAX_REDIRECTS` redirect hops.
///
/// Every hop is validated before a connection is opened: the scheme and the
/// credentials of the address, the network of every address it resolves to, and
/// the address the client is pinned to.
async fn fetch_hops(
    client: &reqwest::Client,
    url: &reqwest::Url,
    max_bytes: u64,
    trust: &RefsTrust,
) -> Result<Vec<u8>, MaterializeError> {
    let mut current = url.clone();
    for _ in 0..=MAX_REDIRECTS {
        trust.check_remote_url(&current)?;
        let pin = approve_remote_address(trust, &current).await?;
        let hop_client = match pin {
            Some((host, address)) => pinned_client(&host, address)?,
            None => client.clone(),
        };
        let response = hop_client
            .get(current.clone())
            .timeout(Duration::from_secs(DEFAULT_PROJECT_TIMEOUT_SECS))
            .send()
            .await
            .map_err(|error| {
                MaterializeError::Delivery(format!(
                    "the document `{current}` could not be fetched: {error}"
                ))
            })?;
        let status = response.status();
        if !status.is_redirection() {
            if !status.is_success() {
                return Err(MaterializeError::Delivery(format!(
                    "the document `{current}` answered with the HTTP status {}",
                    status.as_u16()
                )));
            }
            guard_declared_response(&response, current.as_str(), max_bytes)?;
            return read_body(response, current.as_str(), max_bytes)
                .await
                .map_err(MaterializeError::from);
        }
        let target = response
            .headers()
            .get(reqwest::header::LOCATION)
            .ok_or_else(|| {
                MaterializeError::Delivery(format!(
                    "the document `{current}` answered with the HTTP status {} and named no redirect target",
                    status.as_u16()
                ))
            })?;
        let target = target.to_str().map_err(|_| {
            MaterializeError::Delivery(format!(
                "the document `{current}` answered with a redirect target that is not valid text"
            ))
        })?;
        current = current.join(target).map_err(|error| {
            MaterializeError::Delivery(format!(
                "the document `{current}` answered with the redirect target `{target}`, which is not a valid address: {error}"
            ))
        })?;
    }
    Err(MaterializeError::Delivery(format!(
        "the document `{url}` redirects more than {MAX_REDIRECTS} times, so the transfer was abandoned"
    )))
}

/// Resolves every address `url` reaches and approves it through `trust`.
///
/// The answer is `None` for a literal address: the declaration named an address
/// and [`RefsTrust::check_remote_url`] classified it, so no second resolution can
/// move it. A hostname is resolved here, every address is classified, and the
/// first validated address is returned so the HTTP client connects to exactly
/// what was approved instead of resolving the name a second time.
async fn approve_remote_address(
    trust: &RefsTrust,
    url: &reqwest::Url,
) -> Result<Option<(String, SocketAddr)>, MaterializeError> {
    let Some(host) = url.host_str() else {
        return Err(MaterializeError::Delivery(format!(
            "the document `{url}` names no host"
        )));
    };
    if literal_address(url).is_some() {
        return Ok(None);
    }

    let display = url.to_string();
    let owner = host.to_string();
    let port = url.port_or_known_default().unwrap_or(80);
    let resolved = crate::blocking::run_blocking_async(move || {
        (owner.as_str(), port)
            .to_socket_addrs()
            .map(|addresses| addresses.collect::<Vec<SocketAddr>>())
    })
    .await
    .map_err(|error| {
        crate::blocking::log_join_error(&error, "documentation host resolution");
        MaterializeError::Delivery(format!("the document `{display}` could not be resolved"))
    })?;
    let addresses = resolved.map_err(|error| {
        MaterializeError::Delivery(format!(
            "the document `{display}` could not be resolved: {error}"
        ))
    })?;
    let Some(first) = addresses.first().copied() else {
        return Err(MaterializeError::Delivery(format!(
            "the document `{display}` resolved to no address"
        )));
    };
    for address in &addresses {
        trust.check_remote_address(url, address.ip())?;
    }
    Ok(Some((host.to_string(), first)))
}

/// Refuses a response whose declared length or media type rules out text.
///
/// Both checks run on the headers alone, so an oversized or binary answer is
/// abandoned before a single body byte is buffered. A response that declares
/// neither is read under the streaming ceiling of [`read_body`], where the
/// UTF-8 gate still refuses material that is not documentation text.
fn guard_declared_response(
    response: &reqwest::Response,
    url: &str,
    max_bytes: u64,
) -> Result<(), MaterializeError> {
    if let Some(declared) = declared_length(response) {
        if declared > max_bytes {
            return Err(declared_oversized(url, declared, max_bytes));
        }
    }
    if let Some(media_type) = declared_media_type(response) {
        if !is_textual_media_type(&media_type) {
            return Err(non_textual_media_type(url, &media_type));
        }
    }
    Ok(())
}

/// Reads the `Content-Length` a response declares, when it declares one.
fn declared_length(response: &reqwest::Response) -> Option<u64> {
    let value = response.headers().get(reqwest::header::CONTENT_LENGTH)?;
    value.to_str().ok()?.trim().parse::<u64>().ok()
}

/// Reads the media type a response declares, when it declares one.
fn declared_media_type(response: &reqwest::Response) -> Option<String> {
    let value = response.headers().get(reqwest::header::CONTENT_TYPE)?;
    let value = value.to_str().ok()?.trim();
    if value.is_empty() {
        return None;
    }
    Some(value.to_string())
}

/// Reports whether `value` names a media type a documentation page uses.
///
/// The registry accepts the `text/*` family together with the structured
/// application types a documentation corpus is published as. A binary type —
/// an archive, an image, a PDF, or an opaque byte stream — names material the
/// harness cannot read, so the transfer is refused instead of buffering it.
fn is_textual_media_type(value: &str) -> bool {
    let essence = value
        .split(';')
        .next()
        .unwrap_or(value)
        .trim()
        .to_ascii_lowercase();
    if essence.starts_with("text/") {
        return true;
    }
    matches!(
        essence.as_str(),
        "application/json" | "application/xml" | "application/yaml"
    )
}

/// The reason a response whose declared length exceeds the ceiling reports.
fn declared_oversized(url: &str, declared: u64, max_bytes: u64) -> MaterializeError {
    MaterializeError::Delivery(format!(
        "the document `{url}` declares a Content-Length of {declared} bytes, which exceeds the configured documentation budget of {max_bytes} bytes, so the transfer was abandoned before the body was read"
    ))
}

/// The reason a response carrying a non-textual media type reports.
fn non_textual_media_type(url: &str, media_type: &str) -> MaterializeError {
    MaterializeError::Delivery(format!(
        "the document `{url}` answered with the media type `{media_type}`, which is not documentation text, so the transfer was abandoned before the body was read"
    ))
}
/// Streams the body of an accepted response, bounded by `max_bytes`.
async fn read_body(
    mut response: reqwest::Response,
    url: &str,
    max_bytes: u64,
) -> Result<Vec<u8>, String> {
    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("the document `{url}` could not be read: {error}"))?
    {
        let accumulated = (body.len() as u64).saturating_add(chunk.len() as u64);
        if accumulated > max_bytes {
            return Err(oversized(url, max_bytes));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Reads a local source from its absolute path.
///
/// The trust declaration is checked inside the blocking phase and before the
/// file is opened, so a source outside every declared root never leaves the
/// disk and a document that could never enter the cache is rejected without
/// being loaded into memory.
async fn read_local_source(
    source: &str,
    max_bytes: u64,
    trust: RefsTrust,
) -> Result<Vec<u8>, MaterializeError> {
    let path = PathBuf::from(source);
    let display = source.to_string();
    let failed = format!("the local document `{display}` could not be read");
    let owner = source.to_string();
    crate::blocking::run_blocking_async(move || -> Result<Vec<u8>, MaterializeError> {
        // The trust check runs inside the blocking phase and before the file is
        // opened, so a source outside every declared root never leaves the disk
        // and a document that could never enter the cache is rejected without
        // being loaded into memory.
        trust.permits_local_source(&owner)?;
        let metadata = std::fs::metadata(&path).map_err(|error| {
            MaterializeError::Delivery(format!(
                "the local document `{display}` could not be read: {error}"
            ))
        })?;
        if metadata.len() > max_bytes {
            return Err(MaterializeError::Delivery(oversized(&display, max_bytes)));
        }
        std::fs::read(&path).map_err(|error| {
            MaterializeError::Delivery(format!(
                "the local document `{display}` could not be read: {error}"
            ))
        })
    })
    .await
    .map_err(|error| {
        crate::blocking::log_join_error(&error, "outer documentation source read");
        MaterializeError::Delivery(failed)
    })?
}

/// Writes one snapshot into the isolated cache through the atomic replacement.
///
/// The bytes are validated as UTF-8 text before the write, so a document the
/// model could never read is refused instead of entering the cache. The gate
/// covers both contours, because a local source and a remote transfer meet at
/// this single write.
async fn store_snapshot(
    repo_root: &Path,
    cache_path: &str,
    name: &str,
    bytes: Vec<u8>,
) -> Result<(), String> {
    if let Err(error) = std::str::from_utf8(&bytes) {
        return Err(format!(
            "the document `{name}` is not valid UTF-8 text ({error}), so it was not written to `{cache_path}`"
        ));
    }
    let target = repo_root.join(cache_path);
    crate::blocking::run_blocking_async(move || -> anyhow::Result<()> {
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        tauqe_tools::atomic_write::write_atomic(&target, &bytes)
    })
    .await
    .map_err(|error| {
        crate::blocking::log_join_error(&error, "outer documentation snapshot write");
        format!("the snapshot of `{name}` could not be written to `{cache_path}`")
    })?
    .map_err(|error| {
        format!("the snapshot of `{name}` could not be written to `{cache_path}`: {error}")
    })
}

/// Reports whether the snapshot of `name` already occupies `target`.
async fn cache_file_exists(target: &Path, name: &str) -> Result<bool, String> {
    let target = target.to_path_buf();
    let name = name.to_string();
    crate::blocking::run_blocking_async(move || target.is_file())
        .await
        .map_err(|error| {
            crate::blocking::log_join_error(&error, "outer documentation cache inspection");
            format!("the cached snapshot of `{name}` could not be inspected")
        })
}

/// Reads the trust declaration of `repo_root` off the async executor thread.
async fn read_trust(repo_root: &Path) -> RefsTrust {
    let root = repo_root.to_path_buf();
    let owner = root.clone();
    match crate::blocking::run_blocking_async(move || load_trust(&owner)).await {
        Ok(trust) => trust,
        Err(error) => {
            crate::blocking::log_join_error(&error, "outer documentation trust declaration read");
            RefsTrust::unauthorized(&root)
        }
    }
}

/// Resolves the trust declaration one materialization runs against.
async fn resolve_trust(repo_root: &Path, trust: Option<RefsTrust>) -> RefsTrust {
    match trust {
        Some(trust) => trust,
        None => read_trust(repo_root).await,
    }
}

/// Reports whether the turn cancellation signal already fired.
fn cancel_requested(cancel_rx: Option<&watch::Receiver<bool>>) -> bool {
    cancel_rx.is_some_and(|receiver| *receiver.borrow())
}

/// Resolves once the turn cancellation signal fires.
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
fn cancelled(url: &str) -> MaterializeError {
    MaterializeError::Delivery(format!(
        "the document `{url}` was not materialized because the turn was cancelled"
    ))
}

/// Renders the diagnostic of a document abandoned for exceeding the ceiling.
fn oversized(url: &str, max_bytes: u64) -> String {
    format!(
        "the document `{url}` exceeds the configured documentation budget of {max_bytes} bytes, so the transfer was abandoned instead of buffering the document"
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tempfile::tempdir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::{is_textual_media_type, materialize_document_with, materialize_relative_with};
    use crate::config::OuterDocSource;
    use crate::outer_docs::trust::{RefsTrust, TrustRefusal};
    use crate::outer_docs::{
        document_cache_path, is_outer_docs_cache_path, relative_cache_path, OuterDoc,
        OUTER_DOCS_CACHE_DIR,
    };

    /// Byte ceiling the fixtures that do not exercise the bound fit into.
    const TEST_MAX_BYTES: u64 = 1_000_000;

    /// The workspace every fixture declares itself to be.
    const WORKSPACE: &str = "/workspace";

    /// The trust a fixture grants when it reads one directory outside the
    /// repository.
    fn local_trust(root: &Path) -> RefsTrust {
        RefsTrust::for_workspace(Path::new(WORKSPACE), vec![root.to_path_buf()], false)
    }

    /// The trust a fixture grants when it addresses the loopback mock server.
    fn loopback_trust() -> RefsTrust {
        RefsTrust::for_workspace(Path::new(WORKSPACE), Vec::new(), true)
    }

    /// A declared document.
    fn document(id: &str, source: OuterDocSource) -> OuterDoc {
        OuterDoc {
            id: id.to_string(),
            title: "Reference".to_string(),
            description: None,
            source,
        }
    }

    #[tokio::test]
    async fn test_a_local_document_lands_in_the_isolated_cache() {
        let outside = tempdir().unwrap();
        let source = outside.path().join("api.md");
        std::fs::write(&source, "endpoint notes").unwrap();

        let dir = tempdir().unwrap();
        let doc = document(
            "vendor/api.md",
            OuterDocSource::Local(source.to_string_lossy().into_owned().into()),
        );

        let cache_path = materialize_document_with(
            dir.path(),
            &doc,
            TEST_MAX_BYTES,
            None,
            Some(local_trust(outside.path())),
        )
        .await
        .expect("the declared document materializes");

        assert_eq!(cache_path, document_cache_path(&doc));
        assert_eq!(
            cache_path,
            format!("{OUTER_DOCS_CACHE_DIR}/vendor/api.md/api.md")
        );
        assert!(is_outer_docs_cache_path(&cache_path));
        let stored = std::fs::read_to_string(dir.path().join(&cache_path)).unwrap();
        assert_eq!(stored, "endpoint notes");
    }

    #[tokio::test]
    async fn test_a_materialized_snapshot_is_never_read_again() {
        let outside = tempdir().unwrap();
        let source = outside.path().join("api.md");
        std::fs::write(&source, "first snapshot").unwrap();

        let dir = tempdir().unwrap();
        let doc = document(
            "vendor/api.md",
            OuterDocSource::Local(source.to_string_lossy().into_owned().into()),
        );
        let trust = local_trust(outside.path());

        let first =
            materialize_document_with(dir.path(), &doc, TEST_MAX_BYTES, None, Some(trust.clone()))
                .await
                .unwrap();

        // Changing the source proves the second materialization answers the
        // cached snapshot instead of reading the declaration again.
        std::fs::write(&source, "changed").unwrap();
        let second = materialize_document_with(dir.path(), &doc, TEST_MAX_BYTES, None, Some(trust))
            .await
            .unwrap();

        assert_eq!(second, first);
        let stored = std::fs::read_to_string(dir.path().join(&second)).unwrap();
        assert_eq!(stored, "first snapshot");
    }

    #[tokio::test]
    async fn test_a_cached_snapshot_is_served_without_consulting_the_trust_declaration() {
        let outside = tempdir().unwrap();
        let source = outside.path().join("api.md");
        std::fs::write(&source, "endpoint notes").unwrap();

        let dir = tempdir().unwrap();
        let doc = document(
            "vendor/api.md",
            OuterDocSource::Local(source.to_string_lossy().into_owned().into()),
        );
        materialize_document_with(
            dir.path(),
            &doc,
            TEST_MAX_BYTES,
            None,
            Some(local_trust(outside.path())),
        )
        .await
        .expect("the declared document materializes");

        // The snapshot is already local material, so a workspace whose trust
        // declaration authorizes nothing still reads what the cache holds.
        let served = materialize_document_with(
            dir.path(),
            &doc,
            TEST_MAX_BYTES,
            None,
            Some(RefsTrust::unauthorized(dir.path())),
        )
        .await
        .expect("the cached snapshot is served without a trust decision");

        assert_eq!(served, document_cache_path(&doc));
        assert_eq!(
            std::fs::read_to_string(dir.path().join(&served)).unwrap(),
            "endpoint notes"
        );
    }

    #[tokio::test]
    async fn test_an_oversized_local_document_is_refused_before_it_is_read() {
        let outside = tempdir().unwrap();
        let source = outside.path().join("huge.md");
        std::fs::write(&source, "x".repeat(8192)).unwrap();

        let dir = tempdir().unwrap();
        let doc = document(
            "vendor/huge.md",
            OuterDocSource::Local(source.to_string_lossy().into_owned().into()),
        );

        let error = materialize_document_with(
            dir.path(),
            &doc,
            16,
            None,
            Some(local_trust(outside.path())),
        )
        .await
        .expect_err("an oversized document surfaces a diagnostic");

        assert!(
            error.reason().contains("budget"),
            "unexpected reason: {error}"
        );
        assert!(!dir.path().join(document_cache_path(&doc)).exists());
    }

    #[tokio::test]
    async fn test_a_relative_document_lands_beside_the_root_document() {
        let outside = tempdir().unwrap();
        let corpus = outside.path().join("mylib");
        std::fs::create_dir_all(&corpus).unwrap();
        std::fs::write(
            corpus.join("README.md"),
            "# MyLib\n\n- [Architecture](architecture.md)\n",
        )
        .unwrap();
        std::fs::write(corpus.join("architecture.md"), "layouts and contracts").unwrap();

        let dir = tempdir().unwrap();
        let doc = document("mylib", OuterDocSource::Local(corpus.join("README.md")));
        let trust = local_trust(outside.path());

        let root =
            materialize_document_with(dir.path(), &doc, TEST_MAX_BYTES, None, Some(trust.clone()))
                .await
                .expect("the root document materializes");
        assert_eq!(root, document_cache_path(&doc));
        assert_eq!(
            std::fs::read_to_string(dir.path().join(&root)).unwrap(),
            "# MyLib\n\n- [Architecture](architecture.md)\n"
        );

        let relative = materialize_relative_with(
            dir.path(),
            &doc,
            "architecture.md",
            TEST_MAX_BYTES,
            None,
            Some(trust),
        )
        .await
        .expect("the relative document materializes");
        assert_eq!(relative, relative_cache_path(&doc, "architecture.md"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join(&relative)).unwrap(),
            "layouts and contracts"
        );
    }

    #[tokio::test]
    async fn test_a_remote_document_lands_in_the_isolated_cache() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/llm.txt"))
            .respond_with(ResponseTemplate::new(200).set_body_string("upstream notes"))
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let doc = document(
            "vendor/api.md",
            OuterDocSource::Remote(format!("{}/llm.txt", server.uri())),
        );

        let cache_path = materialize_document_with(
            dir.path(),
            &doc,
            TEST_MAX_BYTES,
            None,
            Some(loopback_trust()),
        )
        .await
        .expect("the remote document materializes");

        assert_eq!(cache_path, document_cache_path(&doc));
        let stored = std::fs::read_to_string(dir.path().join(&cache_path)).unwrap();
        assert_eq!(stored, "upstream notes");
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_a_remote_relative_document_joins_below_the_root() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/book/index.md"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("- [Architecture](architecture.md)\n"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/book/architecture.md"))
            .respond_with(ResponseTemplate::new(200).set_body_string("layers"))
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let doc = document(
            "mylib",
            OuterDocSource::Remote(format!("{}/book/index.md", server.uri())),
        );

        let cache_path = materialize_relative_with(
            dir.path(),
            &doc,
            "architecture.md",
            TEST_MAX_BYTES,
            None,
            Some(loopback_trust()),
        )
        .await
        .expect("the relative document materializes");

        assert_eq!(cache_path, relative_cache_path(&doc, "architecture.md"));
        let stored = std::fs::read_to_string(dir.path().join(&cache_path)).unwrap();
        assert_eq!(stored, "layers");
    }

    #[tokio::test]
    async fn test_a_non_public_target_is_refused_without_a_declaration() {
        let server = MockServer::start().await;
        let dir = tempdir().unwrap();
        let doc = document(
            "vendor/api.md",
            OuterDocSource::Remote(format!("{}/llm.txt", server.uri())),
        );

        let error = materialize_document_with(
            dir.path(),
            &doc,
            TEST_MAX_BYTES,
            None,
            Some(RefsTrust::for_workspace(dir.path(), Vec::new(), false)),
        )
        .await
        .expect_err("a non-public target needs the declaration");

        assert!(
            error.trust().is_some(),
            "a trust decision must be typed, not a sentence: {error}"
        );
        assert!(
            error.reason().contains("non-public address"),
            "unexpected reason: {error}"
        );
        assert!(
            error
                .remedy()
                .is_some_and(|remedy| remedy.contains("allow_private_network = true")),
            "the refusal must carry the declaration that authorizes the network"
        );
        assert!(server.received_requests().await.unwrap().is_empty());
        assert!(!dir.path().join(document_cache_path(&doc)).exists());
    }

    #[tokio::test]
    async fn test_a_failed_transfer_is_a_delivery_failure_not_a_trust_refusal() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/llm.txt"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let doc = document(
            "vendor/api.md",
            OuterDocSource::Remote(format!("{}/llm.txt", server.uri())),
        );

        let error = materialize_document_with(
            dir.path(),
            &doc,
            TEST_MAX_BYTES,
            None,
            Some(loopback_trust()),
        )
        .await
        .expect_err("a failing host surfaces a diagnostic");

        assert!(
            error.trust().is_none(),
            "a failed transfer must not be classified as a trust decision: {error}"
        );
        assert!(
            error.reason().contains("HTTP status 500"),
            "unexpected reason: {error}"
        );
        assert!(error.remedy().is_none());
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_an_unauthorized_workspace_refuses_host_material() {
        let dir = tempdir().unwrap();
        // The source lives outside the repository: a file inside it is the
        // project's own material, which no entry gates.
        let outside = tempdir().unwrap();
        let source = outside.path().join("notes.md");
        std::fs::write(&source, "notes").unwrap();
        let trust = RefsTrust::unauthorized(dir.path());

        let local = document(
            "vendor/notes.md",
            OuterDocSource::Local(source.to_string_lossy().into_owned().into()),
        );
        let error = materialize_document_with(
            dir.path(),
            &local,
            TEST_MAX_BYTES,
            None,
            Some(trust.clone()),
        )
        .await
        .expect_err("an unauthorized workspace reads nothing");
        assert!(
            matches!(
                error.trust(),
                Some(TrustRefusal::WorkspaceNotAuthorized { .. })
            ),
            "unexpected classification: {error}"
        );

        let server = MockServer::start().await;
        let remote = document(
            "vendor/api.md",
            OuterDocSource::Remote(format!("{}/llm.txt", server.uri())),
        );
        let error =
            materialize_document_with(dir.path(), &remote, TEST_MAX_BYTES, None, Some(trust))
                .await
                .expect_err("an unauthorized workspace fetches nothing");
        assert!(
            matches!(
                error.trust(),
                Some(TrustRefusal::WorkspaceNotAuthorized { .. })
            ),
            "unexpected classification: {error}"
        );
        assert!(
            server.received_requests().await.unwrap().is_empty(),
            "an unauthorized workspace must never reach the host"
        );
        assert!(!dir.path().join(document_cache_path(&remote)).exists());
    }

    #[tokio::test]
    async fn test_a_local_source_outside_every_trust_root_is_a_trust_refusal() {
        let outside = tempdir().unwrap();
        let source = outside.path().join("api.md");
        std::fs::write(&source, "endpoint notes").unwrap();

        let dir = tempdir().unwrap();
        let doc = document(
            "vendor/api.md",
            OuterDocSource::Local(source.to_string_lossy().into_owned().into()),
        );

        // The workspace is authorized, but only for a directory the source does
        // not live in.
        let trust = RefsTrust::for_workspace(dir.path(), vec![dir.path().to_path_buf()], false);
        let error = materialize_document_with(dir.path(), &doc, TEST_MAX_BYTES, None, Some(trust))
            .await
            .expect_err("a source outside every declared root is refused");

        assert!(
            matches!(
                error.trust(),
                Some(TrustRefusal::LocalSourceOutsideRoots { .. })
            ),
            "unexpected classification: {error}"
        );
        let remedy = error.remedy().expect("the refusal carries its declaration");
        assert!(
            remedy.contains("[[workspace]]"),
            "unexpected remedy: {remedy}"
        );
        let directory = std::fs::canonicalize(outside.path()).unwrap();
        assert!(
            remedy.contains(directory.to_string_lossy().as_ref()),
            "the remedy must name the directory the source lives in: {remedy}"
        );
    }

    #[tokio::test]
    async fn test_a_url_carrying_credentials_is_a_trust_refusal() {
        let dir = tempdir().unwrap();
        let doc = document(
            "vendor/api.md",
            OuterDocSource::Remote("https://user:secret@example.com/llm.txt".to_string()),
        );

        let error = materialize_document_with(
            dir.path(),
            &doc,
            TEST_MAX_BYTES,
            None,
            Some(loopback_trust()),
        )
        .await
        .expect_err("credentials in the URL are refused");

        assert!(
            matches!(error.trust(), Some(TrustRefusal::CredentialsInUrl { .. })),
            "unexpected classification: {error}"
        );
        assert!(
            error.reason().contains("credentials"),
            "unexpected reason: {error}"
        );
        assert!(!dir.path().join(document_cache_path(&doc)).exists());
    }
    #[test]
    fn test_the_textual_media_type_gate_reads_the_essence() {
        let textual = [
            "text/plain",
            "text/html; charset=utf-8",
            "application/json",
            "Application/XML",
        ];
        for value in textual {
            assert!(is_textual_media_type(value), "{value} must count as text");
        }
        let binary = ["application/pdf", "application/octet-stream", "image/png"];
        for value in binary {
            assert!(
                !is_textual_media_type(value),
                "{value} must not count as text"
            );
        }
    }

    #[tokio::test]
    async fn test_a_textual_media_type_is_accepted() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/notes.md"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(b"# Notes\n".to_vec(), "text/markdown"),
            )
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let doc = document(
            "vendor/notes.md",
            OuterDocSource::Remote(format!("{}/notes.md", server.uri())),
        );

        let cache_path = materialize_document_with(
            dir.path(),
            &doc,
            TEST_MAX_BYTES,
            None,
            Some(loopback_trust()),
        )
        .await
        .expect("a textual document materializes");

        let stored = std::fs::read_to_string(dir.path().join(&cache_path)).unwrap();
        assert_eq!(stored, "# Notes\n");
    }

    #[tokio::test]
    async fn test_a_binary_media_type_is_refused_before_the_body_is_read() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/manual.pdf"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(b"%PDF-1.7".to_vec(), "application/pdf"),
            )
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let doc = document(
            "vendor/manual.md",
            OuterDocSource::Remote(format!("{}/manual.pdf", server.uri())),
        );

        let error = materialize_document_with(
            dir.path(),
            &doc,
            TEST_MAX_BYTES,
            None,
            Some(loopback_trust()),
        )
        .await
        .expect_err("a binary document is refused");

        assert!(
            error.trust().is_none(),
            "a rejected media type is a delivery failure: {error}"
        );
        assert!(
            error.reason().contains("application/pdf"),
            "unexpected reason: {error}"
        );
        assert!(!dir.path().join(document_cache_path(&doc)).exists());
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_a_declared_length_above_the_budget_is_refused_before_the_body() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/llm.txt"))
            .respond_with(ResponseTemplate::new(200).set_body_string("x".repeat(8192)))
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let doc = document(
            "vendor/api.md",
            OuterDocSource::Remote(format!("{}/llm.txt", server.uri())),
        );

        let error = materialize_document_with(dir.path(), &doc, 16, None, Some(loopback_trust()))
            .await
            .expect_err("a declared length above the budget is refused");

        assert!(
            error.reason().contains("Content-Length"),
            "unexpected reason: {error}"
        );
        assert!(
            error.reason().contains("before the body was read"),
            "unexpected reason: {error}"
        );
        assert!(!dir.path().join(document_cache_path(&doc)).exists());
    }

    #[tokio::test]
    async fn test_a_document_that_is_not_valid_utf8_is_refused() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/llm.txt"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(vec![0xff, 0xfe, 0x00], "text/plain"),
            )
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let doc = document(
            "vendor/api.md",
            OuterDocSource::Remote(format!("{}/llm.txt", server.uri())),
        );

        let error = materialize_document_with(
            dir.path(),
            &doc,
            TEST_MAX_BYTES,
            None,
            Some(loopback_trust()),
        )
        .await
        .expect_err("invalid UTF-8 is refused");

        assert!(
            error.reason().contains("UTF-8"),
            "unexpected reason: {error}"
        );
        assert!(!dir.path().join(document_cache_path(&doc)).exists());
    }

    /// A reference the jail admitted can never resolve above the directory that
    /// holds the root document, whatever the joined address looks like.
    #[test]
    fn test_a_relative_reference_never_resolves_above_the_root_directory() {
        let base = "https://example.com/book/index.md";
        assert_eq!(
            super::join_remote(base, "architecture.md").expect("the reference resolves"),
            "https://example.com/book/architecture.md"
        );
        assert_eq!(
            super::join_remote(base, "nested/api.md").expect("the reference resolves"),
            "https://example.com/book/nested/api.md"
        );
        for escaping in ["../secret.md", "./../secret.md"] {
            let error = super::join_remote(base, escaping)
                .expect_err("a reference above the root directory is refused");
            assert!(
                error.contains("not below"),
                "unexpected reason for {escaping}: {error}"
            );
        }
    }
}
