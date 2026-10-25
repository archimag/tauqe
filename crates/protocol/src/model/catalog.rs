use serde::{Deserialize, Serialize};

use super::reference::ModelRef;

/// A model registered in a provider catalog.
///
/// The catalog is the single source of truth for the models a project exposes.
/// An entry pairs the provider-native model reference with an optional
/// human-readable alias, so the alias describes the model itself instead of a
/// particular role slot that happens to select it. A role list therefore names
/// only models and their runtime parameters, and every display name is resolved
/// from the catalog.
///
/// Deserializes from either a bare model string or a `{ model, alias }` table,
/// so a catalog mixes terse and annotated entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelCatalogEntry {
    pub model: ModelRef,
    pub alias: Option<String>,
}

/// Normalizes an optional alias by trimming surrounding whitespace and
/// collapsing an empty result to `None`.
///
/// Routing every alias through this helper keeps a hand-built entry identical
/// to one decoded from configuration: a blank alias is never stored as
/// `Some("")` and therefore never renders as an empty label.
fn normalize_alias(alias: Option<String>) -> Option<String> {
    alias.and_then(|a| {
        let trimmed = a.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

impl<'de> Deserialize<'de> for ModelCatalogEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct CatalogEntryVisitor;

        impl<'de> serde::de::Visitor<'de> for CatalogEntryVisitor {
            type Value = ModelCatalogEntry;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a model string or a { model, alias } table")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                match value.parse::<ModelRef>() {
                    Ok(model) => Ok(ModelCatalogEntry { model, alias: None }),
                    Err(err) => Err(E::custom(err)),
                }
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut model: Option<String> = None;
                let mut alias: Option<String> = None;
                let mut alias_seen = false;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "model" => {
                            if model.is_some() {
                                return Err(serde::de::Error::duplicate_field("model"));
                            }
                            model = Some(map.next_value()?);
                        }
                        "alias" => {
                            if alias_seen {
                                return Err(serde::de::Error::duplicate_field("alias"));
                            }
                            alias_seen = true;
                            // Read as `Option<String>` so an explicit `null`
                            // means "no alias" instead of failing the entry.
                            alias = map.next_value()?;
                        }
                        other => {
                            return Err(serde::de::Error::unknown_field(other, &["model", "alias"]))
                        }
                    }
                }

                let raw_model = match model {
                    Some(model) => model,
                    None => return Err(serde::de::Error::missing_field("model")),
                };
                let model = match raw_model.parse::<ModelRef>() {
                    Ok(model) => model,
                    Err(err) => return Err(serde::de::Error::custom(err)),
                };

                Ok(ModelCatalogEntry {
                    model,
                    alias: normalize_alias(alias),
                })
            }
        }

        deserializer.deserialize_any(CatalogEntryVisitor)
    }
}

impl Serialize for ModelCatalogEntry {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match &self.alias {
            Some(alias) => {
                use serde::ser::SerializeStruct;
                let mut state = serializer.serialize_struct("ModelCatalogEntry", 2)?;
                state.serialize_field("model", &self.model.to_spec())?;
                state.serialize_field("alias", alias)?;
                state.end()
            }
            None => serializer.serialize_str(&self.model.to_spec()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_catalog_entry_accepts_a_bare_model_string() {
        let entry: ModelCatalogEntry =
            serde_json::from_str("\"openrouter:google/gemini-flash\"").unwrap();
        assert_eq!(entry.model, ModelRef::openrouter("google/gemini-flash"));
        assert_eq!(entry.alias, None);

        // An entry without an alias keeps the terse string form on the way out,
        // so a catalog never grows structural noise it does not need.
        assert_eq!(
            serde_json::to_string(&entry).unwrap(),
            "\"openrouter:google/gemini-flash\""
        );
    }

    #[test]
    fn test_catalog_entry_accepts_a_table_with_alias() {
        let entry: ModelCatalogEntry = serde_json::from_str(
            r#"{"model":"openrouter:anthropic/claude-sonnet","alias":"Sonnet"}"#,
        )
        .unwrap();
        assert_eq!(entry.model, ModelRef::openrouter("anthropic/claude-sonnet"));
        assert_eq!(entry.alias.as_deref(), Some("Sonnet"));

        let serialized = serde_json::to_string(&entry).unwrap();
        assert_eq!(
            serialized,
            r#"{"model":"openrouter:anthropic/claude-sonnet","alias":"Sonnet"}"#
        );
        assert_eq!(
            serde_json::from_str::<ModelCatalogEntry>(&serialized).unwrap(),
            entry
        );
    }

    #[test]
    fn test_catalog_entry_normalizes_blank_and_null_alias() {
        for raw in [
            r#"{"model":"openrouter:a/b","alias":"   "}"#,
            r#"{"model":"openrouter:a/b","alias":null}"#,
            r#"{"model":"openrouter:a/b"}"#,
        ] {
            let entry: ModelCatalogEntry = serde_json::from_str(raw).unwrap();
            assert_eq!(entry.alias, None, "raw: {raw}");
            assert_eq!(
                serde_json::to_string(&entry).unwrap(),
                "\"openrouter:a/b\"",
                "raw: {raw}"
            );
        }

        // A meaningful alias is trimmed but preserved.
        let entry: ModelCatalogEntry =
            serde_json::from_str(r#"{"model":"openrouter:a/b","alias":"  Fast  "}"#).unwrap();
        assert_eq!(entry.alias.as_deref(), Some("Fast"));
    }

    #[test]
    fn test_catalog_entry_reports_precise_field_errors() {
        // An unrecognized key is reported instead of silently dropped.
        let err =
            serde_json::from_str::<ModelCatalogEntry>(r#"{"model":"openrouter:a/b","note":"x"}"#)
                .unwrap_err()
                .to_string();
        assert!(err.contains("unknown field"), "got: {err}");
        assert!(err.contains("alias"), "got: {err}");

        let err = serde_json::from_str::<ModelCatalogEntry>(r#"{"alias":"Sonnet"}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("missing field"), "got: {err}");

        // A malformed model reference keeps its own precise diagnostic.
        let err = serde_json::from_str::<ModelCatalogEntry>(r#"{"model":"anthropc:claude"}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("Unknown model provider"), "got: {err}");

        // A repeated key is reported instead of letting the last value win.
        let err = serde_json::from_str::<ModelCatalogEntry>(
            r#"{"model":"openrouter:a/b","alias":"x","alias":"y"}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("duplicate field"), "got: {err}");

        let err = serde_json::from_str::<ModelCatalogEntry>(
            r#"{"model":"openrouter:a/b","model":"openrouter:c/d"}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("duplicate field"), "got: {err}");
    }
}
