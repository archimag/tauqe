//! Role entries and role lists exactly as written in configuration, before
//! catalog resolution.
//!
//! A role entry may name a model through a catalog alias, and an alias is only
//! resolvable against a catalog that has already been read. The wire-decoding
//! proxies in this file are therefore kept separate from the resolved
//! [`ModelsConfig`](super::ModelsConfig) surface: configuration decodes into
//! them first and resolves afterwards, so no role resolves against a half-read
//! catalog.

use serde::Deserialize;
use tauqe_protocol::{ModelCatalogEntry, ModelRef, ReasoningEffort};

use super::ModelsConfig;

/// A model reference exactly as written in configuration.
///
/// Holds the raw text the developer wrote: either a provider model
/// specification or an alias registered in the provider catalog. Resolution
/// happens only after the whole configuration file has been decoded, because an
/// alias is declared in `[providers]` and is still unknown while `[models]` is
/// being read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ModelReference(String);

impl ModelReference {
    /// Trims and validates a written model reference.
    fn parse_text<E>(value: &str) -> Result<Self, E>
    where
        E: serde::de::Error,
    {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err(E::custom("Model specification cannot be empty"));
        }
        Ok(ModelReference(trimmed.to_string()))
    }

    /// Resolves the written reference against the provider catalog.
    pub(super) fn resolve(&self, catalog: &[ModelCatalogEntry]) -> Result<ModelRef, String> {
        ModelsConfig::resolve_model_spec(catalog, &self.0)
    }
}

impl<'de> Deserialize<'de> for ModelReference {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct ReferenceVisitor;

        impl<'de> serde::de::Visitor<'de> for ReferenceVisitor {
            type Value = ModelReference;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a model specification or a { provider, name } table")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                ModelReference::parse_text(value)
            }

            fn visit_map<A>(self, map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                // The structured form is delegated to `ModelRef`, so its
                // provider and name fields keep their own precise diagnostics.
                let model =
                    ModelRef::deserialize(serde::de::value::MapAccessDeserializer::new(map))?;
                Ok(ModelReference(model.to_spec()))
            }
        }

        deserializer.deserialize_any(ReferenceVisitor)
    }
}

/// A role entry as written in configuration.
///
/// Pairs the written model reference with the runtime parameters of that role
/// slot: the reasoning depth and the level-up marker. A display alias is not a
/// role parameter: it describes the model itself and is declared once in the
/// provider catalog, so a role entry that carries one is rejected instead of
/// quietly reinstating a second source of truth for the same label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RoleModelSpec {
    pub(super) reference: ModelReference,
    pub(super) effort: Option<ReasoningEffort>,
    /// Explicit level-up marker, absent by default.
    ///
    /// An absent marker means the entry participates in ladder escalation, so
    /// `level_up = false` is the only spelling that excludes a model from it.
    pub(super) level_up: Option<bool>,
}

impl<'de> Deserialize<'de> for RoleModelSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct SpecVisitor;

        impl<'de> serde::de::Visitor<'de> for SpecVisitor {
            type Value = RoleModelSpec;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a model string or a { model, effort, level_up } table")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(RoleModelSpec {
                    reference: ModelReference::parse_text(value)?,
                    effort: None,
                    level_up: None,
                })
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut reference: Option<ModelReference> = None;
                let mut effort: Option<ReasoningEffort> = None;
                let mut effort_seen = false;
                let mut level_up: Option<bool> = None;
                let mut level_up_seen = false;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "model" => {
                            if reference.is_some() {
                                return Err(serde::de::Error::duplicate_field("model"));
                            }
                            reference = Some(map.next_value()?);
                        }
                        "effort" => {
                            if effort_seen {
                                return Err(serde::de::Error::duplicate_field("effort"));
                            }
                            effort_seen = true;
                            // Read as a string so a blank value means "no
                            // effort" and a malformed one keeps the precise
                            // reasoning-effort diagnostic.
                            effort = match map.next_value::<Option<String>>()? {
                                Some(raw) if !raw.trim().is_empty() => Some(
                                    raw.trim()
                                        .parse::<ReasoningEffort>()
                                        .map_err(serde::de::Error::custom)?,
                                ),
                                _ => None,
                            };
                        }
                        "level_up" => {
                            if level_up_seen {
                                return Err(serde::de::Error::duplicate_field("level_up"));
                            }
                            level_up_seen = true;
                            // The marker is a boolean literal or absent; any
                            // other type is a configuration error instead of a
                            // silently ignored annotation.
                            level_up = map.next_value::<Option<bool>>()?;
                        }
                        other => {
                            return Err(serde::de::Error::unknown_field(
                                other,
                                &["model", "effort", "level_up"],
                            ))
                        }
                    }
                }

                let reference = match reference {
                    Some(reference) => reference,
                    None => return Err(serde::de::Error::missing_field("model")),
                };

                Ok(RoleModelSpec {
                    reference,
                    effort,
                    level_up,
                })
            }
        }

        deserializer.deserialize_any(SpecVisitor)
    }
}

/// Role model lists as written in configuration, before catalog resolution.
///
/// A role entry may name a model by a catalog alias, and an alias is only
/// resolvable against a catalog that has already been read. Configuration is
/// therefore decoded into this proxy first and resolved through
/// [`ModelsConfig::resolve`], so no role can resolve against a half-read
/// catalog.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct ModelsConfigRaw {
    #[serde(default)]
    pub(super) primary: Vec<RoleModelSpec>,
    #[serde(default)]
    pub(super) coder: Option<Vec<RoleModelSpec>>,
    #[serde(default)]
    pub(super) architect: Option<Vec<RoleModelSpec>>,
    #[serde(default)]
    pub(super) reviewer: Option<Vec<RoleModelSpec>>,
    #[serde(default)]
    pub(super) scribe: Option<ModelReference>,
    #[serde(default)]
    pub(super) auto_level_up: bool,
}
