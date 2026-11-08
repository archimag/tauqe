//! The OpenRouter model catalog and its disk cache.
//!
//! The catalog is the authoritative source of model metadata: the context
//! window a model offers and the request parameters it declares support for.
//! Those facts belong to the model a turn runs, so the harness reads them
//! from the provider's `GET /models` response instead of maintaining them by
//! hand in configuration.
//!
//! The response is cached under `.tauqe/cache/models.json` with a
//! twenty-four-hour TTL: model metadata changes on the scale of days, so a
//! lookup reuses a cached snapshot instead of paying a catalog round-trip on
//! every turn. The cache loads tolerantly — a missing or damaged file reads
//! as an empty cache — and saves atomically through the shared staging
//! writer, so a crash publishes either the previous complete catalog or the
//! new one.

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauqe_tools::atomic_write::{create_dir_all_durable, write_atomic};

/// Repository-relative path of the cached model catalog.
const CATALOG_CACHE_PATH: &str = ".tauqe/cache/models.json";

/// How long a cached catalog snapshot stays fresh.
///
/// Model metadata changes on the scale of days, not turns: one day keeps the
/// cached facts current without paying a catalog round-trip on every lookup.
const CATALOG_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// One model row of the OpenRouter `GET /models` response.
///
/// The catalog keeps only the fields a turn derives facts from; every other
/// property of the wire row (pricing, modalities, provider limits) is
/// ignored by deserialization.
#[derive(Debug, Deserialize)]
pub struct ModelWire {
    /// Provider identifier of the model, e.g. `openai/gpt-4`.
    #[serde(default)]
    pub id: String,
    /// Maximum context length in tokens the model declares, if any.
    #[serde(default)]
    pub context_length: Option<u64>,
    /// Request parameters the model declares support for.
    #[serde(default)]
    pub supported_parameters: Vec<String>,
}

/// The OpenRouter `GET /models` response body.
#[derive(Debug, Deserialize)]
pub struct ModelsListWire {
    /// The model rows the response carries.
    #[serde(default)]
    pub data: Vec<ModelWire>,
}

/// The catalog-declared capabilities of one model.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelCapabilities {
    /// Provider identifier of the model, e.g. `openai/gpt-4`.
    pub id: String,
    /// Maximum context length in tokens the model declares, if any.
    pub context_length: Option<u64>,
    /// Request parameters the model declares support for.
    pub supported_parameters: Vec<String>,
}

impl ModelCapabilities {
    /// Reports whether the model declares structured output support.
    ///
    /// The catalog gates on the `structured_outputs` parameter: a model that
    /// only accepts `response_format` declares the JSON-object form, not the
    /// schema-constrained form the structured contour relies on.
    pub fn supports_structured_outputs(&self) -> bool {
        self.supported_parameters
            .iter()
            .any(|parameter| parameter == "structured_outputs")
    }

    /// Reports whether the model declares reasoning support.
    ///
    /// `reasoning` is the canonical gateway parameter: the effort spellings
    /// (`reasoning_effort`, `include_reasoning`) are accepted by individual
    /// gateways but are not the declaration the catalog standardizes on.
    pub fn supports_reasoning(&self) -> bool {
        self.supported_parameters
            .iter()
            .any(|parameter| parameter == "reasoning")
    }

    /// Builds a capabilities record from its wire row.
    ///
    /// A row without an identifier names no model and is dropped: the
    /// snapshot keeps only records a lookup can address.
    fn from_wire(wire: ModelWire) -> Option<Self> {
        if wire.id.trim().is_empty() {
            return None;
        }
        Some(Self {
            id: wire.id,
            context_length: wire.context_length,
            supported_parameters: wire.supported_parameters,
        })
    }
}

/// The model records one catalog snapshot holds.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CatalogSnapshot {
    /// The catalog records, in provider order.
    models: Vec<ModelCapabilities>,
}

impl CatalogSnapshot {
    /// Builds a snapshot from the rows of a `GET /models` response.
    ///
    /// Provider order is preserved, so identical responses produce identical
    /// snapshots and a repeated lookup answers the same record.
    pub fn from_wire(rows: Vec<ModelWire>) -> Self {
        Self {
            models: rows
                .into_iter()
                .filter_map(ModelCapabilities::from_wire)
                .collect(),
        }
    }

    /// Finds one model by its exact provider identifier.
    ///
    /// The lookup is exact: a family sibling or a prefix of another
    /// identifier never substitutes for the requested model.
    pub fn get(&self, id: &str) -> Option<&ModelCapabilities> {
        self.models.iter().find(|model| model.id == id)
    }

    /// Reports whether the snapshot holds no model.
    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }
}

/// A catalog snapshot on disk, with the moment it was fetched.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CatalogCache {
    /// Unix seconds at which `snapshot` was fetched.
    fetched_at: u64,
    /// The catalog records the cache holds.
    snapshot: CatalogSnapshot,
}

impl CatalogCache {
    /// An empty cache that has fetched nothing.
    pub fn new() -> Self {
        Self::default()
    }

    /// Wraps a snapshot fetched at Unix second `fetched_at`.
    pub fn from_snapshot(snapshot: CatalogSnapshot, fetched_at: u64) -> Self {
        Self {
            fetched_at,
            snapshot,
        }
    }

    /// The catalog records the cache holds.
    pub fn snapshot(&self) -> &CatalogSnapshot {
        &self.snapshot
    }

    /// The Unix second at which the snapshot was fetched.
    pub fn fetched_at(&self) -> u64 {
        self.fetched_at
    }

    /// Reports whether the snapshot is younger than the catalog TTL.
    ///
    /// A snapshot ages out on its twenty-fourth hour: an age strictly below
    /// the TTL is fresh, the boundary itself and everything past it are
    /// expired. The subtraction saturates, so a cache fetched after `now` (a
    /// skewed clock) reads as fresh instead of wrapping around.
    pub fn is_fresh(&self, now: u64) -> bool {
        now.saturating_sub(self.fetched_at) < CATALOG_TTL.as_secs()
    }

    /// Atomically saves the cache under `repo_root`.
    ///
    /// The `.tauqe/cache` directory is created when missing, and the payload
    /// is staged and renamed into place through the shared atomic writer, so
    /// a crash leaves either the previous complete catalog or the new one.
    pub fn save(&self, repo_root: &Path) -> Result<(), ModelCatalogError> {
        let target = repo_root.join(CATALOG_CACHE_PATH);
        let payload = serde_json::to_vec(self).map_err(|error| {
            ModelCatalogError::CacheWrite(format!(
                "the model catalog could not be serialized: {error}"
            ))
        })?;
        let directory = target.parent().map(Path::to_path_buf).ok_or_else(|| {
            ModelCatalogError::CacheWrite(format!(
                "the model catalog cache path `{}` has no parent directory",
                target.display()
            ))
        })?;

        crate::blocking::run_blocking(move || {
            create_dir_all_durable(&directory).map_err(|error| {
                ModelCatalogError::CacheWrite(format!(
                    "the model catalog cache directory `{}` could not be created: {error:#}",
                    directory.display()
                ))
            })?;
            write_atomic(&target, &payload).map_err(|error| {
                ModelCatalogError::CacheWrite(format!(
                    "the model catalog cache `{}` could not be written: {error:#}",
                    target.display()
                ))
            })
        })
    }

    /// Loads the cache under `repo_root`, tolerating a missing or damaged file.
    ///
    /// A cache that cannot be read — absent, unreadable, or not the JSON the
    /// harness wrote — reads as an empty cache: a damaged file never fails a
    /// turn, it only costs the refetch that repopulates it.
    pub fn load(repo_root: &Path) -> Self {
        let target = repo_root.join(CATALOG_CACHE_PATH);

        crate::blocking::run_blocking(move || match std::fs::read(&target) {
            Ok(bytes) => parse_cached_catalog(&target, &bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(error) => {
                tracing::warn!(
                    path = %target.display(),
                    error = %error,
                    "model catalog cache could not be read and reads as empty"
                );
                Self::default()
            }
        })
    }
}

/// Parses a cached catalog, reading a damaged file as an empty cache.
///
/// The file exists but is not the JSON the harness wrote, so it is damaged
/// by definition: the caller refetches instead of failing the turn.
fn parse_cached_catalog(path: &Path, bytes: &[u8]) -> CatalogCache {
    serde_json::from_slice(bytes).unwrap_or_else(|error| {
        tracing::warn!(
            path = %path.display(),
            error = %error,
            "model catalog cache is damaged and reads as empty"
        );
        CatalogCache::default()
    })
}

/// Why a model catalog operation failed.
#[derive(Debug)]
pub enum ModelCatalogError {
    /// No usable metadata exists for the requested model.
    MetadataUnavailable {
        /// The model whose metadata was requested.
        model: String,
        /// Why no metadata could be served.
        reason: String,
    },
    /// The cache snapshot could not be persisted.
    CacheWrite(String),
}

impl std::fmt::Display for ModelCatalogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MetadataUnavailable { model, reason } => {
                write!(f, "model metadata unavailable for `{model}`: {reason}")
            }
            Self::CacheWrite(reason) => {
                write!(f, "the model catalog cache could not be written: {reason}")
            }
        }
    }
}

impl std::error::Error for ModelCatalogError {}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tempfile::tempdir;

    use super::*;

    /// A `GET /models` response decodes into a snapshot with exact lookups
    /// and capability predicates.
    #[test]
    fn test_a_models_response_decodes_into_a_snapshot() {
        let body = json!({
            "data": [
                {
                    "id": "openai/gpt-4",
                    "context_length": 8192,
                    "supported_parameters": ["temperature", "structured_outputs", "reasoning"]
                },
                {
                    "id": "openai/gpt-4o",
                    "context_length": null,
                    "supported_parameters": ["response_format", "reasoning_effort"]
                },
                {
                    "id": "",
                    "context_length": 4096,
                    "supported_parameters": ["structured_outputs"]
                },
                {
                    "context_length": 1024,
                    "supported_parameters": ["temperature"]
                }
            ],
            "total_count": 4
        });

        let wire: ModelsListWire = serde_json::from_value(body).expect("the response decodes");
        assert_eq!(wire.data.len(), 4, "unknown fields are ignored by decoding");

        let snapshot = CatalogSnapshot::from_wire(wire.data);
        assert_eq!(
            snapshot.models.len(),
            2,
            "a row without an identifier names no model and is dropped"
        );
        assert!(!snapshot.is_empty());

        let structured = snapshot
            .get("openai/gpt-4")
            .expect("the exact row is present");
        assert_eq!(structured.context_length, Some(8192));
        assert!(structured.supports_structured_outputs());
        assert!(structured.supports_reasoning());

        let gpt4o = snapshot
            .get("openai/gpt-4o")
            .expect("the exact row is present");
        assert_eq!(gpt4o.context_length, None);
        // `response_format` declares the JSON-object form, not the
        // schema-constrained structured output the catalog gates on.
        assert!(!gpt4o.supports_structured_outputs());
        // `reasoning_effort` is a gateway spelling, not the canonical
        // `reasoning` declaration the catalog standardizes on.
        assert!(!gpt4o.supports_reasoning());

        // The lookup is exact: family siblings and prefixes never substitute.
        assert!(snapshot.get("openai/gpt").is_none());
        assert!(snapshot.get("openai/gpt-4o-mini").is_none());
        assert!(snapshot.get("OPENAI/GPT-4").is_none());
        // The identifier-less row was dropped, not indexed under "".
        assert!(snapshot.get("").is_none());
    }

    /// A snapshot ages out on its twenty-fourth hour: the second before the
    /// TTL is fresh, the boundary itself and everything past it are expired.
    #[test]
    fn test_freshness_expires_on_the_ttl_boundary() {
        let fetched_at: u64 = 1_700_000_000;
        let ttl = CATALOG_TTL.as_secs();
        let cache = CatalogCache::from_snapshot(catalog_snapshot_fixture(), fetched_at);

        assert!(cache.is_fresh(fetched_at));
        assert!(cache.is_fresh(fetched_at + ttl - 1));
        assert!(!cache.is_fresh(fetched_at + ttl), "the boundary is expired");
        assert!(!cache.is_fresh(fetched_at + ttl + 1));

        // A cache that has fetched nothing is expired at any real clock.
        assert_eq!(CatalogCache::new(), CatalogCache::default());
        assert!(!CatalogCache::default().is_fresh(fetched_at));
        assert!(!CatalogCache::default().is_fresh(fetched_at + ttl));

        // A cache fetched after `now` (a skewed clock) saturates instead of
        // wrapping around.
        let future = CatalogCache::from_snapshot(catalog_snapshot_fixture(), fetched_at + 10_000);
        assert!(future.is_fresh(fetched_at));
    }

    /// A saved cache loads back identically and keeps its lookups addressable.
    #[test]
    fn test_cache_roundtrips_through_disk() {
        let dir = tempdir().unwrap();
        let fetched_at: u64 = 1_700_000_000;
        let cache = CatalogCache::from_snapshot(catalog_snapshot_fixture(), fetched_at);

        cache.save(dir.path()).expect("the cache saves");

        assert!(dir.path().join(CATALOG_CACHE_PATH).is_file());
        assert!(dir.path().join(".tauqe/cache").is_dir());

        let loaded = CatalogCache::load(dir.path());
        assert_eq!(loaded, cache);
        assert_eq!(loaded.fetched_at(), fetched_at);
        assert_eq!(
            loaded.snapshot().get("openai/gpt-4"),
            cache.snapshot().get("openai/gpt-4")
        );
        assert!(loaded.is_fresh(fetched_at));
    }

    /// A missing, damaged, wrong-shaped, or unreadable cache reads as empty.
    #[test]
    fn test_load_tolerates_missing_and_damaged_caches() {
        let dir = tempdir().unwrap();
        let cache_path = dir.path().join(CATALOG_CACHE_PATH);
        let cache_dir = cache_path.parent().unwrap().to_path_buf();

        // Missing: no cache was ever written.
        assert_eq!(CatalogCache::load(dir.path()), CatalogCache::default());

        // Truncated JSON.
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::write(&cache_path, r#"{"fetched_at": 123, "snapshot""#).unwrap();
        assert_eq!(CatalogCache::load(dir.path()), CatalogCache::default());

        // Structurally valid JSON that is not a catalog cache.
        std::fs::write(&cache_path, r#"{"data": []}"#).unwrap();
        assert_eq!(CatalogCache::load(dir.path()), CatalogCache::default());

        // An empty file.
        std::fs::write(&cache_path, "").unwrap();
        assert_eq!(CatalogCache::load(dir.path()), CatalogCache::default());

        // A `.tauqe` path occupied by a regular file blocks the read.
        let blocked = tempdir().unwrap();
        std::fs::write(blocked.path().join(".tauqe"), "a file, not a directory").unwrap();
        assert_eq!(CatalogCache::load(blocked.path()), CatalogCache::default());
    }

    /// The catalog error names the model it could not serve and the cache it
    /// could not write.
    #[test]
    fn test_catalog_errors_render_their_variants() {
        let unavailable = ModelCatalogError::MetadataUnavailable {
            model: "openai/gpt-4".to_string(),
            reason: "no fresh cache and no connection".to_string(),
        };
        let rendered = unavailable.to_string();
        assert!(rendered.contains("model metadata unavailable"));
        assert!(rendered.contains("openai/gpt-4"));
        assert!(rendered.contains("no fresh cache and no connection"));

        let write = ModelCatalogError::CacheWrite("disk full".to_string());
        assert!(write.to_string().contains("model catalog cache"));
        assert!(write.to_string().contains("disk full"));
    }

    /// A save into a blocked cache directory reports the write failure.
    #[test]
    fn test_save_reports_a_blocked_cache_directory() {
        let dir = tempdir().unwrap();
        // `.tauqe` exists as a regular file, so the cache directory under it
        // cannot be created.
        std::fs::write(dir.path().join(".tauqe"), "a file, not a directory").unwrap();

        let cache = CatalogCache::from_snapshot(catalog_snapshot_fixture(), 1_700_000_000);
        let error = cache.save(dir.path()).expect_err("the save must fail");

        assert!(matches!(error, ModelCatalogError::CacheWrite(_)));
        assert!(error.to_string().contains("could not be created"));
    }

    /// A snapshot with one structured, reasoning model and one plain model.
    fn catalog_snapshot_fixture() -> CatalogSnapshot {
        CatalogSnapshot::from_wire(vec![
            ModelWire {
                id: "openai/gpt-4".to_string(),
                context_length: Some(8192),
                supported_parameters: vec![
                    "temperature".to_string(),
                    "structured_outputs".to_string(),
                    "reasoning".to_string(),
                ],
            },
            ModelWire {
                id: "anthropic/claude-3.5-sonnet".to_string(),
                context_length: Some(200_000),
                supported_parameters: vec!["temperature".to_string()],
            },
        ])
    }
}
