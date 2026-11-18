//! Project lifetime usage persistence.

use crate::api::{UsageTotals, UsageTotalsSnapshot};
use std::path::{Path, PathBuf};

/// Directory, relative to the repository root, holding the project telemetry.
const TELEMETRY_DIR: &str = ".tauqe";

/// File inside [`TELEMETRY_DIR`] carrying the project lifetime totals.
const TELEMETRY_FILE: &str = "usage.json";

/// Resolves the telemetry file of a repository.
fn telemetry_path(repo_root: &Path) -> PathBuf {
    repo_root.join(TELEMETRY_DIR).join(TELEMETRY_FILE)
}

/// Loads the persisted project lifetime totals of a repository.
///
/// A missing file, an unreadable file, or a document written by an
/// incompatible version is a normal state, not an error: the totals start from
/// zero and the next flush rewrites the document. Only the lifetime scope is
/// persisted, because the session and per-operation scopes restart with the
/// daemon and restoring them would resurrect stale counters.
pub(crate) fn load_initial_snapshot(repo_root: &Path) -> UsageTotalsSnapshot {
    let path = telemetry_path(repo_root);
    // Bootstrap may run inside an async runtime. Keep the synchronous read
    // behind the blocking-aware helper instead of occupying a worker.
    let loaded = crate::blocking::run_blocking(move || read_document(&path));
    match loaded {
        Some((lifetime, lifetime_operations)) => UsageTotalsSnapshot {
            lifetime,
            lifetime_operations,
            ..UsageTotalsSnapshot::default()
        },
        None => UsageTotalsSnapshot::default(),
    }
}

/// Reads the telemetry document, returning its lifetime totals.
///
/// Returns `None` when no document exists yet, when it cannot be read, or when
/// it is not valid JSON; every one of those cases is reported and degraded to
/// zero totals rather than failing the daemon startup.
fn read_document(path: &Path) -> Option<(UsageTotals, u64)> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "Failed to read the project telemetry",
            );
            return None;
        }
    };
    let document: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(document) => document,
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "Discarding unreadable project telemetry",
            );
            return None;
        }
    };
    let lifetime = document
        .get("lifetime")
        .and_then(|value| serde_json::from_value::<UsageTotals>(value.clone()).ok())
        .unwrap_or_default();
    let lifetime_operations = document
        .get("lifetime_operations")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    Some((lifetime, lifetime_operations))
}

/// Writes the telemetry document atomically under `<repo_root>/.tauqe`.
///
/// The directory is created on demand, so a repository that has never recorded
/// a turn still gains its telemetry file on the first flush. The payload is
/// handed to the core atomic-write policy, which stages, syncs, and renames it,
/// so a concurrent reader never observes a partial document.
fn write_document(path: &Path, lifetime: &UsageTotals, operations: u64) -> Result<(), String> {
    let lifetime = serde_json::to_value(lifetime)
        .map_err(|err| format!("Failed to encode the project telemetry: {}", err))?;
    let document = serde_json::json!({
        "lifetime": lifetime,
        "lifetime_operations": operations,
    });
    let payload = serde_json::to_vec_pretty(&document)
        .map_err(|err| format!("Failed to encode the project telemetry: {}", err))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("Failed to create {}: {}", parent.display(), err))?;
    }
    tauqe_tools::atomic_write::write_atomic(path, &payload).map_err(|err| format!("{:#}", err))
}

/// Persists the project lifetime totals off the async worker pool.
pub(crate) async fn persist_lifetime(root: PathBuf, lifetime: &UsageTotals, operations: u64) {
    let path = telemetry_path(&root);
    let lifetime = lifetime.clone();
    let written =
        crate::blocking::run_blocking_async(move || write_document(&path, &lifetime, operations))
            .await;
    match written {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            tracing::warn!(error = %err, "Failed to persist the project telemetry");
        }
        Err(err) => {
            tracing::warn!(error = %err, "Project telemetry write task failed");
        }
    }
}

pub(crate) async fn load_lifetime(root: PathBuf) -> (UsageTotals, u64) {
    let path = telemetry_path(&root);
    match crate::blocking::run_blocking_async(move || read_document(&path)).await {
        Ok(Some(loaded)) => loaded,
        Ok(None) => (UsageTotals::default(), 0),
        Err(error) => {
            tracing::warn!(error = %error, "Project telemetry read task failed");
            (UsageTotals::default(), 0)
        }
    }
}
