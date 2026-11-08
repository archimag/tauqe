//! The `[[outer_docs]]` section: external documentation declared by a project.
//!
//! A project keeps authoritative material — an upstream API reference, a
//! protocol summary, a published book — outside the repository and declares it
//! once here. Every entry names a mount point (`id`), a label (`title`), an
//! optional `description`, and one mandatory `source`.
//!
//! `source` is a single string that the configuration resolver types: a value
//! carrying an `http://` or `https://` scheme becomes a remote address, and every
//! other value becomes a local file-system path. One key instead of three
//! mutually exclusive ones means an entry can never name two documents at once,
//! and the validator asks a single question — is the source present and of a shape
//! the harness can resolve — instead of reconciling competing keys.
//!
//! The `id` names the root document. A request for `{id}` answers that root file
//! as its original text, and a request for `{id}/{relative}` resolves below the
//! directory that holds the root, so one declaration covers a whole published
//! corpus without the harness ever parsing an index of it.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::outer_docs::is_clean_relative_path;

/// One external document declared by `[[outer_docs]]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OuterDocEntry {
    /// Mount point of the root document inside the outer documentation tree.
    ///
    /// The id names the root itself; a request for `{id}/{relative}` addresses a
    /// document below the directory that holds it.
    pub id: String,
    /// Human-readable label rendered in the catalog the model reads.
    pub title: String,
    /// Optional note describing what the document covers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Where the root document comes from: an `http(s)` address or a local path.
    pub source: String,
}

/// Where the content of one declared document comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OuterDocSource {
    /// A remote document addressed by an `http://` or `https://` URL.
    Remote(String),
    /// A local document named by a file-system path.
    Local(PathBuf),
}

impl OuterDocEntry {
    /// Resolves the source the entry declares into its typed form.
    ///
    /// The scheme decides the kind: an `http(s)` address is fetched over the
    /// network, and every other value names a local path. A value carrying a
    /// scheme the registry cannot fetch is rejected here, while the entry is still
    /// being resolved, instead of failing later at a turn boundary.
    pub fn source(&self) -> Result<OuterDocSource, String> {
        let id = self.id.trim();
        let source = self.source.trim();
        if source.is_empty() {
            return Err(format!("the entry `{id}` declares an empty source"));
        }
        match scheme_of(source) {
            Some(scheme) if is_http_scheme(scheme) => {
                let url = reqwest::Url::parse(source).map_err(|error| {
                    format!(
                        "the entry `{id}` declares the source `{source}`, which is not an address the harness can fetch: {error}"
                    )
                })?;
                if url.host_str().is_none() {
                    return Err(format!(
                        "the entry `{id}` declares the source `{source}`, which names no host"
                    ));
                }
                Ok(OuterDocSource::Remote(source.to_string()))
            }
            Some(scheme) => Err(format!(
                "the entry `{id}` declares the source `{source}` with the scheme `{scheme}`, which the registry does not fetch: name an http(s) address or a local path"
            )),
            None => Ok(OuterDocSource::Local(PathBuf::from(source))),
        }
    }
}

/// Validates every declared entry.
///
/// The section is optional, so an absent declaration is valid. A declared entry
/// must carry a clean repository-relative id, a non-empty title, a resolvable
/// source, and an id no other entry already mounts, because a duplicate would
/// address two documents with one name.
pub fn validate(entries: &[OuterDocEntry]) -> Result<(), String> {
    let mut seen: Vec<&str> = Vec::new();
    for entry in entries {
        let id = entry.id.trim();
        if id.is_empty() {
            return Err("an entry declares an empty id".to_string());
        }
        if !is_clean_relative_path(id) {
            return Err(format!(
                "the entry id `{id}` is not a clean repository-relative path"
            ));
        }
        if seen.contains(&id) {
            return Err(format!("duplicate outer document id `{id}`"));
        }
        seen.push(id);
        if entry.title.trim().is_empty() {
            return Err(format!("the outer document `{id}` declares an empty title"));
        }
        entry.source()?;
    }
    Ok(())
}

/// Returns the scheme `source` carries before its `://` separator.
///
/// A source without that separator names a local path, so a plain path is never
/// mistaken for an address. A separator preceded by an empty or malformed scheme
/// is not a scheme either, which keeps a path that happens to contain `://`
/// readable as a path.
fn scheme_of(source: &str) -> Option<&str> {
    let (scheme, _) = source.split_once("://")?;
    let shaped = !scheme.is_empty()
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    shaped.then_some(scheme)
}

/// Reports whether `scheme` names the HTTP transport the registry fetches.
fn is_http_scheme(scheme: &str) -> bool {
    scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::config::AppConfig;

    /// A declaration with an empty source, which every fixture fills in.
    fn entry(id: &str) -> OuterDocEntry {
        OuterDocEntry {
            id: id.to_string(),
            title: "Reference".to_string(),
            description: None,
            source: String::new(),
        }
    }

    #[test]
    fn test_the_source_is_typed_by_its_scheme() {
        let mut remote = entry("vendor/api.md");
        remote.source = " https://example.com/llm.txt ".to_string();
        assert_eq!(
            remote.source(),
            Ok(OuterDocSource::Remote(
                "https://example.com/llm.txt".to_string()
            ))
        );

        let mut secure = entry("vendor/secure.md");
        secure.source = "HTTPS://example.com/llm.txt".to_string();
        assert!(matches!(secure.source(), Ok(OuterDocSource::Remote(_))));

        let mut local = entry("vendor/protocol.md");
        local.source = " /srv/docs/protocol.md ".to_string();
        assert_eq!(
            local.source(),
            Ok(OuterDocSource::Local(PathBuf::from(
                "/srv/docs/protocol.md"
            )))
        );

        // An entry that names nothing, an address that names no host, and a
        // scheme the registry cannot fetch are all rejected instead of guessed at.
        let error = entry("vendor/none.md")
            .source()
            .expect_err("an empty source is rejected");
        assert!(error.contains("empty source"), "unexpected reason: {error}");

        let mut hostless = entry("vendor/hostless.md");
        hostless.source = "https://".to_string();
        let error = hostless
            .source()
            .expect_err("an address without a host is rejected");
        assert!(error.contains("can fetch"), "unexpected reason: {error}");

        let mut scheme = entry("vendor/file.md");
        scheme.source = "file:///etc/passwd".to_string();
        let error = scheme.source().expect_err("a foreign scheme is rejected");
        assert!(error.contains("scheme"), "unexpected reason: {error}");
        assert!(error.contains("local path"), "unexpected reason: {error}");
    }

    #[test]
    fn test_validation_rejects_duplicate_ids_and_blank_titles() {
        let mut first = entry("mylib");
        first.source = "/srv/docs/book/README.md".to_string();
        let mut second = entry("mylib");
        second.source = "https://example.com/llm.txt".to_string();
        let error = validate(&[first, second]).expect_err("a duplicate id is rejected");
        assert!(error.contains("mylib"), "unexpected reason: {error}");

        let mut untitled = entry("vendor/api.md");
        untitled.title = "   ".to_string();
        untitled.source = "https://example.com/llm.txt".to_string();
        let error = validate(&[untitled]).expect_err("an empty title is rejected");
        assert!(error.contains("title"), "unexpected reason: {error}");

        let mut escaping = entry("../secret.md");
        escaping.source = "https://example.com/llm.txt".to_string();
        assert!(validate(&[escaping]).is_err());

        let unsourced = entry("vendor/none.md");
        let error = validate(&[unsourced]).expect_err("an empty source invalidates the entry");
        assert!(error.contains("empty source"), "unexpected reason: {error}");

        let mut valid = entry("vendor/api.md");
        valid.source = "https://example.com/llm.txt".to_string();
        assert!(validate(&[valid]).is_ok());
    }

    #[test]
    fn test_the_section_decodes_from_the_configuration_document() {
        let config: AppConfig = toml::from_str(
            r#"
[models]
repl = ["openrouter:a/b"]

[[outer_docs]]
id = "vendor/api.md"
title = "Upstream API notes"
description = "Endpoint contracts."
source = "https://example.com/llm.txt"

[[outer_docs]]
id = "mylib"
title = "MyLib Book"
source = "/srv/docs/mylib/README.md"
"#,
        )
        .expect("parse config");

        assert_eq!(config.outer_docs.len(), 2);
        assert_eq!(config.outer_docs[0].id, "vendor/api.md");
        assert_eq!(
            config.outer_docs[0].description.as_deref(),
            Some("Endpoint contracts.")
        );
        assert_eq!(config.outer_docs[1].source, "/srv/docs/mylib/README.md");
        assert_eq!(
            config.outer_docs[0].source(),
            Ok(OuterDocSource::Remote(
                "https://example.com/llm.txt".to_string()
            ))
        );
        assert_eq!(
            config.outer_docs[1].source(),
            Ok(OuterDocSource::Local(PathBuf::from(
                "/srv/docs/mylib/README.md"
            )))
        );
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_an_invalid_section_invalidates_the_configuration() {
        let config: AppConfig = toml::from_str(
            r#"
[models]
repl = ["openrouter:a/b"]

[[outer_docs]]
id = "vendor/api.md"
title = "Upstream API notes"
source = "https://example.com/llm.txt"

[[outer_docs]]
id = "vendor/api.md"
title = "Duplicate"
source = "https://example.com/other.txt"
"#,
        )
        .expect("parse config");

        let error = config
            .validate()
            .expect_err("a duplicate id invalidates the configuration");
        assert!(error.contains("[outer_docs]"), "got: {error}");
        assert!(error.contains("vendor/api.md"), "got: {error}");
    }
}
