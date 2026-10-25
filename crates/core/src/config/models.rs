use serde::{Deserialize, Serialize};
use tauqe_protocol::{
    ArchitectTier, ModelCatalogEntry, ModelEntry, ModelRef, ModelRole, ModelRolesSummary,
    ModelSelection, ReasoningEffort,
};

pub const BUILTIN_DEFAULT_SCRIBE_MODEL: &str = "~anthropic/claude-haiku-latest";
pub const BUILTIN_DEFAULT_CODER_MODEL: &str = "~google/gemini-flash-latest";
pub const BUILTIN_DEFAULT_ARCHITECT_MODEL: &str = "~anthropic/claude-sonnet-latest";
pub const BUILTIN_DEFAULT_PRIMARY_MODEL: &str = "~google/gemini-flash-latest";
pub const BUILTIN_DEFAULT_REVIEWER_MODEL: &str = "~anthropic/claude-sonnet-latest";
pub const BUILTIN_DEFAULT_MODEL: &str = BUILTIN_DEFAULT_PRIMARY_MODEL;

/// Serializes an optional model reference in its canonical `provider:name` form.
///
/// A [`ModelRef`] serializes as a structured `{ provider, name }` table by
/// default, while configuration writes the terse specification string. Keeping
/// the terse form on the way out lets a written configuration round-trip
/// through the same reader that produced it.
pub fn serialize_model_ref_opt<S>(opt: &Option<ModelRef>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match opt {
        Some(model) => serializer.serialize_str(&model.to_spec()),
        None => serializer.serialize_none(),
    }
}

/// A model reference exactly as written in configuration.
///
/// Holds the raw text the developer wrote: either a provider model
/// specification or an alias registered in the provider catalog. Resolution
/// happens only after the whole configuration file has been decoded, because an
/// alias is declared in `[providers]` and is still unknown while `[models]` is
/// being read.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ModelReference(String);

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
    fn resolve(&self, catalog: &[ModelCatalogEntry]) -> Result<ModelRef, String> {
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
/// Pairs the written model reference with the reasoning depth of that role
/// slot. A display alias is not a role parameter: it describes the model itself
/// and is declared once in the provider catalog, so a role entry that carries
/// one is rejected instead of quietly reinstating a second source of truth for
/// the same label.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RoleModelSpec {
    reference: ModelReference,
    effort: Option<ReasoningEffort>,
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
                f.write_str("a model string or a { model, effort } table")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(RoleModelSpec {
                    reference: ModelReference::parse_text(value)?,
                    effort: None,
                })
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut reference: Option<ModelReference> = None;
                let mut effort: Option<ReasoningEffort> = None;
                let mut effort_seen = false;

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
                        other => {
                            return Err(serde::de::Error::unknown_field(
                                other,
                                &["model", "effort"],
                            ))
                        }
                    }
                }

                let reference = match reference {
                    Some(reference) => reference,
                    None => return Err(serde::de::Error::missing_field("model")),
                };

                Ok(RoleModelSpec { reference, effort })
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
    primary: Vec<RoleModelSpec>,
    #[serde(default)]
    coder: Option<Vec<RoleModelSpec>>,
    #[serde(default)]
    architect: Option<Vec<RoleModelSpec>>,
    #[serde(default)]
    reviewer: Option<Vec<RoleModelSpec>>,
    #[serde(default)]
    scribe: Option<ModelReference>,
    #[serde(default)]
    auto_level_up: bool,
}

/// Role model lists and the coder escalation ladder (`[models]`).
///
/// `primary` is mandatory and must list at least one [`ModelEntry`]; its first
/// entry is the idle active model and the deterministic default for every role
/// that omits its own list. `coder`, `architect`, and `reviewer` are optional
/// ordered lists: when omitted they inherit the entire `primary` list, so a
/// specialized role never silently loses access to the configured models. The
/// `coder` list doubles as the escalation ladder: the FSM advances one entry at
/// a time and saturates on the last. `scribe` is a single utility model used for
/// history compaction and commit message synthesis; it defaults to the first
/// `primary` entry.
///
/// Every reference is already resolved against the provider catalog, so a role
/// never stores an alias and every consumer reads a concrete model.
#[derive(Debug, Clone, Serialize, Default)]
pub struct ModelsConfig {
    pub primary: Vec<ModelEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coder: Option<Vec<ModelEntry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub architect: Option<Vec<ModelEntry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reviewer: Option<Vec<ModelEntry>>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_model_ref_opt"
    )]
    pub scribe: Option<ModelRef>,
    pub auto_level_up: bool,
}

impl ModelsConfig {
    /// Resolves a model reference written in configuration.
    ///
    /// A reference is either an alias registered in the provider catalog or a
    /// provider model specification. A registered alias wins, so a project may
    /// name a model the way the catalog declares it without restating the
    /// provider identifier.
    pub fn resolve_model_spec(
        catalog: &[ModelCatalogEntry],
        spec: &str,
    ) -> Result<ModelRef, String> {
        let trimmed = spec.trim();
        if trimmed.is_empty() {
            return Err("Model specification cannot be empty".to_string());
        }
        if let Some(entry) = catalog
            .iter()
            .find(|entry| entry.alias.as_deref() == Some(trimmed))
        {
            return Ok(entry.model.clone());
        }
        trimmed.parse::<ModelRef>()
    }

    /// Returns the alias registered for a model in the provider catalog.
    pub fn model_alias<'a>(catalog: &'a [ModelCatalogEntry], model: &ModelRef) -> Option<&'a str> {
        catalog
            .iter()
            .find(|entry| &entry.model == model)
            .and_then(|entry| entry.alias.as_deref())
    }

    /// Resolves the configured role lists against the provider catalog.
    ///
    /// Every role entry names a model either by its provider specification or by
    /// an alias registered in the catalog; both forms normalize to a concrete
    /// [`ModelRef`] here, so nothing downstream ever handles an unresolved
    /// reference.
    pub(crate) fn resolve(
        raw: ModelsConfigRaw,
        catalog: &[ModelCatalogEntry],
    ) -> Result<Self, String> {
        fn resolve_role(
            role: &str,
            entries: Vec<RoleModelSpec>,
            catalog: &[ModelCatalogEntry],
        ) -> Result<Vec<ModelEntry>, String> {
            entries
                .into_iter()
                .map(|spec| {
                    let model = spec
                        .reference
                        .resolve(catalog)
                        .map_err(|err| format!("[models] {}: {}", role, err))?;
                    Ok(ModelEntry::new(model, spec.effort))
                })
                .collect()
        }

        Ok(ModelsConfig {
            primary: resolve_role("primary", raw.primary, catalog)?,
            coder: raw
                .coder
                .map(|entries| resolve_role("coder", entries, catalog))
                .transpose()?,
            architect: raw
                .architect
                .map(|entries| resolve_role("architect", entries, catalog))
                .transpose()?,
            reviewer: raw
                .reviewer
                .map(|entries| resolve_role("reviewer", entries, catalog))
                .transpose()?,
            scribe: raw
                .scribe
                .map(|reference| {
                    reference
                        .resolve(catalog)
                        .map_err(|err| format!("[models] scribe: {}", err))
                })
                .transpose()?,
            auto_level_up: raw.auto_level_up,
        })
    }

    /// Validates that `primary` is present and non-empty and that no populated
    /// role list is empty.
    ///
    /// `primary` is mandatory because it is the deterministic default source for
    /// every other role; an empty list would leave the harness unable to resolve
    /// a single model.
    pub fn validate(&self) -> Result<(), String> {
        if self.primary.is_empty() {
            return Err(
                "Invalid models configuration: models.primary must list at least one model"
                    .to_string(),
            );
        }

        for (name, list) in [
            ("coder", &self.coder),
            ("architect", &self.architect),
            ("reviewer", &self.reviewer),
        ] {
            if matches!(list, Some(entries) if entries.is_empty()) {
                return Err(format!(
                    "Invalid models configuration: models.{} cannot be an empty list",
                    name
                ));
            }
        }

        Ok(())
    }

    /// Borrowed model list backing a functional role.
    ///
    /// A specialized role without its own list borrows the entire `primary`
    /// list, so read-only callers never clone the configured entries. `scribe` is
    /// the single exception: it is a derived one-element role rather than a stored
    /// list, so it yields `None` and its entry is resolved on demand instead.
    fn role_slice(&self, role: ModelRole) -> Option<&[ModelEntry]> {
        match role {
            ModelRole::Primary => Some(&self.primary),
            ModelRole::Coder => Some(self.coder.as_deref().unwrap_or(&self.primary)),
            ModelRole::Architect => Some(self.architect.as_deref().unwrap_or(&self.primary)),
            ModelRole::Reviewer => Some(self.reviewer.as_deref().unwrap_or(&self.primary)),
            ModelRole::Scribe => None,
        }
    }

    /// Base (first) entry of a role list, without cloning the list.
    fn base_entry(&self, role: ModelRole) -> Option<&ModelEntry> {
        self.role_slice(role)?.first()
    }

    /// Entry of a role list matching a concrete model, without cloning the list.
    fn entry_for_model(&self, role: ModelRole, model: &ModelRef) -> Option<&ModelEntry> {
        self.role_slice(role)?
            .iter()
            .find(|entry| &entry.model == model)
    }

    /// Ordered model list backing a functional role.
    ///
    /// A specialized role without its own list inherits the entire `primary`
    /// list. `scribe` is a single utility model that defaults to the first
    /// `primary` entry; it is reported as a one-element list so callers can treat
    /// every role uniformly.
    pub fn role_entries(&self, role: ModelRole) -> Vec<ModelEntry> {
        if let Some(entries) = self.role_slice(role) {
            return entries.to_vec();
        }

        match &self.scribe {
            Some(model) => vec![ModelEntry::from(model.clone())],
            None => self.primary.clone(),
        }
    }

    /// Resolves a [`ModelRole`] to a concrete [`ModelRef`] with no external fallback.
    pub fn resolve_role(&self, role: ModelRole) -> ModelRef {
        self.resolve_role_with_fallback(role, None)
    }

    /// Resolves a [`ModelRole`] to its base (first) model, consulting an optional
    /// external fallback and finally the built-in default for the role.
    pub fn resolve_role_with_fallback(
        &self,
        role: ModelRole,
        fallback: Option<ModelRef>,
    ) -> ModelRef {
        if role == ModelRole::Scribe {
            return self
                .scribe
                .clone()
                .or_else(|| self.primary.first().map(|entry| entry.model.clone()))
                .or(fallback)
                .unwrap_or_else(|| ModelRef::openrouter(BUILTIN_DEFAULT_SCRIBE_MODEL));
        }

        self.base_entry(role)
            .map(|entry| entry.model.clone())
            .or(fallback)
            .unwrap_or_else(|| ModelRef::openrouter(default_model_for_role(role)))
    }

    /// Resolves the reasoning effort declared on the base (first) entry of a role list.
    ///
    /// Effort is read only from the base entry because the base entry is the model
    /// the role starts on; deeper ladder rungs may carry their own effort and are
    /// applied during escalation instead.
    pub fn resolve_effort(&self, role: ModelRole) -> Option<ReasoningEffort> {
        self.base_entry(role).and_then(|entry| entry.effort)
    }

    /// Resolves the reasoning effort for a model selection within the context of a specific role.
    ///
    /// If the selection specifies a role, returns the base effort of that role.
    /// If the selection specifies a concrete model, looks for that model among the
    /// entries configured for `role` first, then falls back to `primary`, returning
    /// that entry's configured effort. If the model is not in either list or has no
    /// effort specified, returns `None`. A tier selection names the exact
    /// role-list entry the developer picked, so its effort is read from the
    /// DTO itself and is never re-derived by matching the model against the
    /// configured entries.
    pub fn resolve_selection_effort(
        &self,
        role: ModelRole,
        selection: &ModelSelection,
    ) -> Option<ReasoningEffort> {
        match selection {
            ModelSelection::Role(r) => self.resolve_effort(*r),
            ModelSelection::Specific(model) => self
                .entry_for_model(role, model)
                .or_else(|| {
                    if role != ModelRole::Primary {
                        self.entry_for_model(ModelRole::Primary, model)
                    } else {
                        None
                    }
                })
                .and_then(|entry| entry.effort),
            // A tier names the exact role-list entry the developer picked, so
            // its reasoning depth travels inside the DTO and is never
            // re-derived by matching the model against the configured entries.
            ModelSelection::Tier { effort, .. } => *effort,
        }
    }

    /// Resolves a [`ModelSelection`] to a concrete [`ModelRef`].
    pub fn resolve_model(&self, selection: &ModelSelection) -> ModelRef {
        match selection {
            ModelSelection::Role(role) => self.resolve_role(*role),
            ModelSelection::Specific(model) => model.clone(),
            ModelSelection::Tier { model, .. } => model.clone(),
        }
    }

    /// Returns the ordered coder escalation ladder.
    ///
    /// The ladder is the coder list when present, otherwise the inherited
    /// `primary` list. When even that is empty it degrades to a single rung
    /// resolved through the role chain, so escalation always has a base.
    pub fn coder_ladder(&self) -> Vec<ModelEntry> {
        match self.role_slice(ModelRole::Coder) {
            Some(entries) if !entries.is_empty() => entries.to_vec(),
            _ => vec![self.fallback_coder_tier()],
        }
    }

    /// Returns the coder rung at `idx`, saturating at the strongest entry.
    ///
    /// Out-of-range indices clamp to the last rung, so escalation never panics
    /// once the ladder is exhausted.
    pub fn coder_tier(&self, idx: usize) -> ModelEntry {
        let ladder = self.coder_ladder();
        let last = ladder.len().saturating_sub(1);
        ladder
            .get(idx.min(last))
            .cloned()
            .unwrap_or_else(|| self.fallback_coder_tier())
    }

    /// Returns the ladder position a coder selection starts execution from.
    ///
    /// The ladder is the very list [`ModelsConfig::coder_ladder`] exposes, so an
    /// index returned here is directly usable as the starting rung of a turn.
    /// The selection is resolved to its concrete model and reasoning depth
    /// first: an entry matching both wins, because the same model may occupy
    /// several rungs at different depths; a model-only match is the fallback.
    /// An absent selection, or a model the ladder does not carry, restarts from
    /// the base rung instead of guessing a position.
    pub fn coder_tier_index(&self, selection: Option<&ModelSelection>) -> usize {
        let Some(selection) = selection else {
            return 0;
        };
        let model = self.resolve_model(selection);
        let effort = self.resolve_selection_effort(ModelRole::Coder, selection);
        let ladder = self.coder_ladder();
        ladder
            .iter()
            .position(|entry| entry.model == model && entry.effort == effort)
            .or_else(|| ladder.iter().position(|entry| entry.model == model))
            .unwrap_or(0)
    }

    /// Fallback coder rung derived from the deterministic role resolution chain.
    fn fallback_coder_tier(&self) -> ModelEntry {
        ModelEntry::from(self.resolve_role_with_fallback(ModelRole::Coder, None))
    }

    /// Returns the ordered architect ladder.
    ///
    /// The ladder is the architect list when present, otherwise the inherited
    /// `primary` list, so re-planning never loses access to the configured
    /// Architect models. When even that is empty it degrades to a single rung
    /// resolved through the role chain, so escalation always has a base.
    pub fn architect_ladder(&self) -> Vec<ArchitectTier> {
        let ladder: Vec<ArchitectTier> = self
            .role_slice(ModelRole::Architect)
            .unwrap_or(&[])
            .iter()
            .map(|entry| ArchitectTier::new(entry.model.clone(), entry.effort))
            .collect();
        if ladder.is_empty() {
            vec![self.fallback_architect_tier()]
        } else {
            ladder
        }
    }

    /// Returns the architect rung at `idx`, saturating at the strongest entry.
    ///
    /// Out-of-range indices clamp to the last rung, so escalation never panics
    /// once the ladder is exhausted.
    pub fn architect_tier(&self, idx: usize) -> ArchitectTier {
        let ladder = self.architect_ladder();
        let last = ladder.len().saturating_sub(1);
        ladder
            .get(idx.min(last))
            .cloned()
            .unwrap_or_else(|| self.fallback_architect_tier())
    }

    /// Fallback architect rung derived from the deterministic role resolution chain.
    fn fallback_architect_tier(&self) -> ArchitectTier {
        ArchitectTier::from(self.resolve_role_with_fallback(ModelRole::Architect, None))
    }

    /// Returns the ladder position of a recorded Architect configuration.
    ///
    /// `None` means the record does not belong to the current ladder: the
    /// configuration changed between waves, so its position cannot be trusted.
    pub fn architect_tier_index(&self, recorded: Option<&ArchitectTier>) -> Option<usize> {
        let recorded = recorded?;
        let ladder = self.architect_ladder();
        ladder
            .iter()
            .position(|tier| tier == recorded)
            .or_else(|| ladder.iter().position(|tier| tier.model == recorded.model))
    }

    /// Returns the rung that follows a recorded Architect configuration.
    ///
    /// A recorded rung that is absent from the current ladder restarts from the
    /// base rung, so escalation never guesses a position. The last rung
    /// saturates, so an exhausted ladder keeps the strongest Architect.
    pub fn next_architect_tier(&self, recorded: Option<&ArchitectTier>) -> ArchitectTier {
        match self.architect_tier_index(recorded) {
            Some(idx) => self.architect_tier(idx + 1),
            None => self.architect_tier(0),
        }
    }

    /// Summary of effective concrete models assigned to all semantic roles.
    pub fn roles_summary(&self) -> ModelRolesSummary {
        self.roles_summary_with_fallback(None)
    }

    /// Summary of effective concrete models assigned to all semantic roles with fallback.
    pub fn roles_summary_with_fallback(&self, fallback: Option<ModelRef>) -> ModelRolesSummary {
        ModelRolesSummary {
            scribe: self.resolve_role_with_fallback(ModelRole::Scribe, fallback.clone()),
            coder: self.resolve_role_with_fallback(ModelRole::Coder, fallback.clone()),
            architect: self.resolve_role_with_fallback(ModelRole::Architect, fallback.clone()),
            primary: self.resolve_role_with_fallback(ModelRole::Primary, fallback.clone()),
            reviewer: self.resolve_role_with_fallback(ModelRole::Reviewer, fallback),
            efforts: None,
            entries: None,
        }
    }
}

fn default_model_for_role(role: ModelRole) -> &'static str {
    match role {
        ModelRole::Scribe => BUILTIN_DEFAULT_SCRIBE_MODEL,
        ModelRole::Coder => BUILTIN_DEFAULT_CODER_MODEL,
        ModelRole::Architect => BUILTIN_DEFAULT_ARCHITECT_MODEL,
        ModelRole::Primary => BUILTIN_DEFAULT_PRIMARY_MODEL,
        ModelRole::Reviewer => BUILTIN_DEFAULT_REVIEWER_MODEL,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_models(raw: &str) -> ModelsConfig {
        parse_models_with_catalog(raw, &[])
    }

    fn parse_models_with_catalog(raw: &str, catalog: &[ModelCatalogEntry]) -> ModelsConfig {
        let raw: ModelsConfigRaw = toml::from_str(raw).expect("parse models config");
        ModelsConfig::resolve(raw, catalog).expect("resolve models config")
    }

    #[test]
    fn test_resolve_effort_defaults_to_none() {
        let config = ModelsConfig::default();
        for role in [
            ModelRole::Scribe,
            ModelRole::Coder,
            ModelRole::Architect,
            ModelRole::Primary,
            ModelRole::Reviewer,
        ] {
            assert_eq!(config.resolve_effort(role), None);
        }
    }

    #[test]
    fn test_resolve_effort_reads_base_entry() {
        let config = parse_models(
            r#"
primary = ["openrouter:base/primary"]
coder = [
    { model = "openrouter:deepseek/deepseek-chat", effort = "medium" },
    { model = "openrouter:google/gemini-flash-latest", effort = "high" },
]
architect = [{ model = "openrouter:anthropic/claude-sonnet-latest", effort = "high" }]
reviewer = ["openrouter:google/gemini-flash-latest"]
"#,
        );

        // Effort is read from the base (first) entry of the role list.
        assert_eq!(
            config.resolve_effort(ModelRole::Coder),
            Some(ReasoningEffort::Medium)
        );
        assert_eq!(
            config.resolve_effort(ModelRole::Architect),
            Some(ReasoningEffort::High)
        );
        assert_eq!(config.resolve_effort(ModelRole::Reviewer), None);
        assert_eq!(config.resolve_effort(ModelRole::Primary), None);
        // Scribe is a single utility model and never carries effort.
        assert_eq!(config.resolve_effort(ModelRole::Scribe), None);
    }

    #[test]
    fn test_coder_single_string_yields_one_rung() {
        let config = parse_models(
            r#"
primary = ["openrouter:base/primary"]
coder = ["openrouter:anthropic/claude-3.7-sonnet"]
"#,
        );
        let ladder = config.coder_ladder();
        assert_eq!(ladder.len(), 1);
        assert_eq!(
            ladder[0].model,
            ModelRef::openrouter("anthropic/claude-3.7-sonnet")
        );
        assert_eq!(ladder[0].effort, None);
        assert_eq!(config.resolve_role(ModelRole::Coder), ladder[0].model);
        assert_eq!(config.coder_tier(0), ladder[0]);
        assert_eq!(config.coder_tier(7), ladder[0]);
    }

    #[test]
    fn test_coder_single_table_carries_effort() {
        let config = parse_models(
            r#"
primary = ["openrouter:base/primary"]
coder = [{ model = "openrouter:deepseek/deepseek-chat", effort = "high" }]
"#,
        );
        let ladder = config.coder_ladder();
        assert_eq!(ladder.len(), 1);
        assert_eq!(
            ladder[0].model,
            ModelRef::openrouter("deepseek/deepseek-chat")
        );
        assert_eq!(ladder[0].effort, Some(ReasoningEffort::High));
        assert_eq!(config.resolve_role(ModelRole::Coder), ladder[0].model);
        assert_eq!(
            config.resolve_effort(ModelRole::Coder),
            Some(ReasoningEffort::High)
        );
    }

    #[test]
    fn test_coder_array_builds_ordered_ladder() {
        let config = parse_models(
            r#"
primary = ["openrouter:base/primary"]
coder = [
    "openrouter:base/model",
    { model = "openrouter:strong/model", effort = "xhigh" },
]
"#,
        );
        let ladder = config.coder_ladder();
        assert_eq!(ladder.len(), 2);
        assert_eq!(ladder[0].model, ModelRef::openrouter("base/model"));
        assert_eq!(ladder[0].effort, None);
        assert_eq!(ladder[1].model, ModelRef::openrouter("strong/model"));
        assert_eq!(ladder[1].effort, Some(ReasoningEffort::XHigh));
        assert_eq!(config.resolve_role(ModelRole::Coder), ladder[0].model);
        assert_eq!(config.coder_tier(1), ladder[1]);
        assert_eq!(config.coder_tier(9), ladder[1]);
    }

    #[test]
    fn test_coder_empty_ladder_rejected_by_validate() {
        let config = ModelsConfig {
            primary: vec![ModelRef::openrouter("base/primary").into()],
            coder: Some(Vec::new()),
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_coder_absent_inherits_primary_ladder() {
        let config = parse_models(
            r#"
primary = ["openrouter:only/primary", "openrouter:second/primary"]
architect = ["openrouter:only/architect"]
"#,
        );
        let ladder = config.coder_ladder();
        assert_eq!(ladder.len(), 2);
        assert_eq!(ladder[0].model, ModelRef::openrouter("only/primary"));
        assert_eq!(ladder[1].model, ModelRef::openrouter("second/primary"));
        assert_eq!(config.coder_tier(0), ladder[0]);
        assert_eq!(config.coder_tier(5), ladder[1]);
    }

    #[test]
    fn test_architect_ladder_derives_from_role_list() {
        let config = parse_models(
            r#"
primary = ["openrouter:base/primary"]
architect = [
    { model = "openrouter:planner/one", effort = "high" },
    { model = "openrouter:planner/two", effort = "xhigh" },
]
"#,
        );

        let ladder = config.architect_ladder();
        assert_eq!(ladder.len(), 2);
        assert_eq!(
            ladder[0],
            ArchitectTier::new(
                ModelRef::openrouter("planner/one"),
                Some(ReasoningEffort::High)
            )
        );
        assert_eq!(
            ladder[1],
            ArchitectTier::new(
                ModelRef::openrouter("planner/two"),
                Some(ReasoningEffort::XHigh)
            )
        );

        // Escalation advances one rung at a time and saturates on the strongest.
        assert_eq!(config.next_architect_tier(None), ladder[0]);
        assert_eq!(config.next_architect_tier(Some(&ladder[0])), ladder[1]);
        assert_eq!(config.next_architect_tier(Some(&ladder[1])), ladder[1]);
        assert_eq!(config.architect_tier(7), ladder[1]);
    }

    #[test]
    fn test_architect_ladder_restarts_when_recorded_rung_is_unknown() {
        let config = parse_models(
            r#"
primary = ["openrouter:base/primary"]
architect = ["openrouter:planner/one"]
"#,
        );

        let unknown = ArchitectTier::new(ModelRef::openrouter("planner/retired"), None);
        assert_eq!(config.architect_tier_index(Some(&unknown)), None);
        // A configuration that no longer exists restarts from the base rung
        // instead of guessing a position on the current ladder.
        assert_eq!(
            config.next_architect_tier(Some(&unknown)),
            ArchitectTier::new(ModelRef::openrouter("planner/one"), None)
        );
    }

    #[test]
    fn test_resolve_selection_effort_picks_matching_model_entry() {
        let config = parse_models(
            r#"
primary = ["openrouter:base/primary"]
architect = [
    { model = "openrouter:deepseek/flash", effort = "max" },
    { model = "openrouter:google/gemini-flash", effort = "high" },
    "openrouter:google/gemini-argon",
]
"#,
        );

        // Role selection returns base effort (max)
        assert_eq!(
            config.resolve_selection_effort(
                ModelRole::Architect,
                &ModelSelection::Role(ModelRole::Architect)
            ),
            Some(ReasoningEffort::Max)
        );

        // Specific gemini-flash returns high, NOT max from deepseek
        assert_eq!(
            config.resolve_selection_effort(
                ModelRole::Architect,
                &ModelSelection::Specific(ModelRef::openrouter("google/gemini-flash"))
            ),
            Some(ReasoningEffort::High)
        );

        // Specific gemini-argon without effort returns None
        assert_eq!(
            config.resolve_selection_effort(
                ModelRole::Architect,
                &ModelSelection::Specific(ModelRef::openrouter("google/gemini-argon"))
            ),
            None
        );

        // Model not in list returns None
        assert_eq!(
            config.resolve_selection_effort(
                ModelRole::Architect,
                &ModelSelection::Specific(ModelRef::openrouter("unknown/model"))
            ),
            None
        );
    }

    #[test]
    fn test_resolve_selection_effort_tier_reads_effort_from_the_dto() {
        let config = parse_models(
            r#"
primary = ["openrouter:base/primary"]
architect = [
    { model = "openrouter:deepseek/flash", effort = "max" },
    { model = "openrouter:google/gemini-flash", effort = "high" },
]
"#,
        );

        // A tier selection carries the full role-list coordinate, so the
        // resolver reads its depth straight from the DTO: the configured
        // `high` effort of the same model never leaks into the answer.
        let tier = ModelSelection::Tier {
            model: ModelRef::openrouter("google/gemini-flash"),
            effort: Some(ReasoningEffort::XHigh),
        };
        assert_eq!(
            config.resolve_selection_effort(ModelRole::Architect, &tier),
            Some(ReasoningEffort::XHigh)
        );
        assert_eq!(
            config.resolve_model(&tier),
            ModelRef::openrouter("google/gemini-flash")
        );

        // A tier without a depth resolves to no effort instead of inheriting
        // the first entry's configured one.
        let bare = ModelSelection::Tier {
            model: ModelRef::openrouter("google/gemini-flash"),
            effort: None,
        };
        assert_eq!(
            config.resolve_selection_effort(ModelRole::Architect, &bare),
            None
        );
    }

    #[test]
    fn test_architect_tier_index_falls_back_to_model_matching_on_effort_discrepancy() {
        let config = parse_models(
            r#"
primary = ["openrouter:base/primary"]
architect = [
    { model = "openrouter:deepseek/flash", effort = "max" },
    { model = "openrouter:google/gemini-flash", effort = "high" },
    "openrouter:google/gemini-argon",
]
"#,
        );

        // Recorded tier has mismatched effort (Max instead of High)
        let mismatched = ArchitectTier::new(
            ModelRef::openrouter("google/gemini-flash"),
            Some(ReasoningEffort::Max),
        );

        // Falls back to model match and finds index 1 instead of returning None
        assert_eq!(config.architect_tier_index(Some(&mismatched)), Some(1));
        // Escalation proceeds to index 2 (gemini-argon) instead of resetting to tier 0
        assert_eq!(
            config.next_architect_tier(Some(&mismatched)),
            ArchitectTier::new(ModelRef::openrouter("google/gemini-argon"), None)
        );
    }

    #[test]
    fn test_model_entry_polymorphic_parsing_with_effort() {
        let config = parse_models(
            r#"
primary = ["openrouter:base/primary"]
coder = [
    "openrouter:plain/model",
    { model = "openrouter:detailed/model", effort = "high" },
]
"#,
        );
        let ladder = config.coder_ladder();
        assert_eq!(ladder.len(), 2);
        assert_eq!(ladder[0].model, ModelRef::openrouter("plain/model"));
        assert_eq!(ladder[0].effort, None);
        assert_eq!(ladder[1].model, ModelRef::openrouter("detailed/model"));
        assert_eq!(ladder[1].effort, Some(ReasoningEffort::High));
    }

    #[test]
    fn test_role_entry_rejects_a_display_alias() {
        // A display alias describes the model itself and is declared in the
        // provider catalog, so a role slot that carries one is rejected instead
        // of quietly reinstating a second source of truth for the same label.
        let err = toml::from_str::<ModelsConfigRaw>(
            r#"
primary = [{ model = "openrouter:base/primary", alias = "Fast" }]
"#,
        )
        .expect_err("a role entry carrying an alias must be rejected")
        .to_string();
        assert!(err.contains("alias"), "got: {err}");
    }

    #[test]
    fn test_role_entry_reports_precise_field_errors() {
        // An unrecognized effort surfaces the reasoning-effort diagnostic
        // instead of an opaque "data did not match any variant".
        let err = toml::from_str::<ModelsConfigRaw>(
            r#"
primary = [{ model = "openrouter:base/primary", effort = "extreme" }]
"#,
        )
        .expect_err("an unknown reasoning effort must be rejected")
        .to_string();
        assert!(err.contains("Unknown reasoning effort"), "got: {err}");

        // A repeated key is rejected instead of letting the last value win.
        // TOML refuses a duplicated key inside an inline table while parsing,
        // before the role entry visitor runs, so the diagnostic is the parser's
        // `duplicate key` report and it still names the offending key.
        let err = toml::from_str::<ModelsConfigRaw>(
            r#"
primary = [{ model = "openrouter:a/b", model = "openrouter:c/d" }]
"#,
        )
        .expect_err("a duplicated model key must be rejected")
        .to_string();
        assert!(err.contains("duplicate key"), "got: {err}");
        assert!(err.contains("model"), "got: {err}");

        // A table without a model reference is rejected.
        let err = toml::from_str::<ModelsConfigRaw>(
            r#"
primary = [{ effort = "high" }]
"#,
        )
        .expect_err("a table without a model must be rejected")
        .to_string();
        assert!(err.contains("missing field"), "got: {err}");
    }

    #[test]
    fn test_role_slice_mirrors_role_entries_for_stored_roles() {
        let config = parse_models(
            r#"
primary = ["openrouter:base/primary", "openrouter:second/primary"]
coder = ["openrouter:only/coder"]
architect = [{ model = "openrouter:only/architect", effort = "high" }]
"#,
        );

        for role in [
            ModelRole::Primary,
            ModelRole::Coder,
            ModelRole::Architect,
            ModelRole::Reviewer,
        ] {
            let slice = config
                .role_slice(role)
                .expect("stored role exposes a borrowed slice");
            assert_eq!(slice, config.role_entries(role).as_slice());
        }

        // Scribe is a derived single-entry role and never exposes a stored list.
        assert!(config.role_slice(ModelRole::Scribe).is_none());
    }

    #[test]
    fn test_role_reference_resolves_catalog_alias() {
        let catalog = [
            ModelCatalogEntry {
                model: ModelRef::openrouter("deepseek/deepseek-chat"),
                alias: Some("Deep".to_string()),
            },
            ModelCatalogEntry {
                model: ModelRef::openrouter("google/gemini-flash-latest"),
                alias: None,
            },
        ];
        let config = parse_models_with_catalog(
            r#"
primary = ["Deep"]
coder = [{ model = "Deep", effort = "max" }]
scribe = "Deep"
"#,
            &catalog,
        );

        let deep = ModelRef::openrouter("deepseek/deepseek-chat");
        assert_eq!(config.resolve_role(ModelRole::Primary), deep);
        assert_eq!(config.scribe, Some(deep.clone()));
        assert_eq!(config.coder_tier(0).model, deep);
        assert_eq!(config.coder_tier(0).effort, Some(ReasoningEffort::Max));

        // A provider specification still resolves without an alias.
        assert_eq!(
            ModelsConfig::resolve_model_spec(&catalog, "google/gemini-flash-latest").unwrap(),
            ModelRef::openrouter("google/gemini-flash-latest")
        );
        assert_eq!(ModelsConfig::model_alias(&catalog, &deep), Some("Deep"));
        assert_eq!(
            ModelsConfig::model_alias(
                &catalog,
                &ModelRef::openrouter("google/gemini-flash-latest")
            ),
            None
        );
    }

    #[test]
    fn test_unresolvable_role_reference_reports_the_role() {
        let raw: ModelsConfigRaw =
            toml::from_str(r#"primary = ["anthropc:claude"]"#).expect("parse models config");
        let err = ModelsConfig::resolve(raw, &[])
            .expect_err("a mistyped provider prefix must fail resolution");
        assert!(err.contains("primary"), "got: {err}");
        assert!(err.contains("anthropc:claude"), "got: {err}");
    }

    #[test]
    fn test_resolve_selection_effort_distinguishes_tiers_of_the_same_model() {
        let config = parse_models(
            r#"
primary = ["openrouter:base/primary"]
architect = [
    { model = "openrouter:google/gemini-flash", effort = "low" },
    { model = "openrouter:google/gemini-flash", effort = "high" },
    { model = "openrouter:google/gemini-flash", effort = "max" },
]
"#,
        );

        let model = ModelRef::openrouter("google/gemini-flash");

        // The same model may occupy several rungs of a role list, each with its
        // own reasoning depth. A tier names exactly one of them, so its depth
        // is read from the DTO and never from the first matching entry.
        for effort in [
            ReasoningEffort::Low,
            ReasoningEffort::High,
            ReasoningEffort::Max,
        ] {
            let tier = ModelSelection::Tier {
                model: model.clone(),
                effort: Some(effort),
            };
            let resolved = config.resolve_selection_effort(ModelRole::Architect, &tier);
            assert_eq!(resolved, Some(effort));
            assert_eq!(config.resolve_model(&tier), model);
        }

        // A tier without a recorded depth resolves to no effort instead of
        // borrowing the depth of the first configured entry of the same model.
        let bare = ModelSelection::Tier {
            model: model.clone(),
            effort: None,
        };
        assert_eq!(
            config.resolve_selection_effort(ModelRole::Architect, &bare),
            None
        );

        // A bare model reference names no entry, so it keeps the established
        // behavior and reads the depth of the first matching entry.
        let specific = ModelSelection::Specific(model.clone());
        assert_eq!(
            config.resolve_selection_effort(ModelRole::Architect, &specific),
            Some(ReasoningEffort::Low)
        );
    }

    /// Three rows of one model stay distinguishable end to end: the picker keeps
    /// every rung, the confirmed row travels as a `Tier`, and the resolver
    /// applies exactly the depth printed on that row (c-1).
    #[test]
    fn test_picker_rows_of_one_model_reach_the_resolver_with_their_own_effort() {
        let config = parse_models(
            r#"
primary = [
    { model = "openrouter:google/gemini-flash", effort = "low" },
    { model = "openrouter:google/gemini-flash", effort = "high" },
    { model = "openrouter:google/gemini-flash", effort = "max" },
]
"#,
        );
        let model = ModelRef::openrouter("google/gemini-flash");

        // The role list keeps all three rungs of the same model, so the picker
        // presents rows that differ only by their reasoning depth.
        let rows = config.role_entries(ModelRole::Primary);
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows.iter().map(|row| row.effort).collect::<Vec<_>>(),
            vec![
                Some(ReasoningEffort::Low),
                Some(ReasoningEffort::High),
                Some(ReasoningEffort::Max),
            ]
        );
        for row in &rows {
            assert_eq!(row.model, model);
        }

        // Reopening the picker reproduces the confirmed rung: matching by both
        // coordinates distinguishes the three rows, while matching only the
        // model always lands on the first rung.
        for (index, row) in rows.iter().enumerate() {
            let exact = |candidate: &ModelEntry| {
                candidate.model == row.model && candidate.effort == row.effort
            };
            let same_model = |candidate: &ModelEntry| candidate.model == row.model;
            assert_eq!(rows.iter().position(exact), Some(index));
            // Every rung shares the model, so a search that compares only the
            // model collapses all of them onto the first entry: the depth of
            // the confirmed rung survives only in the `(model, effort)` pair.
            assert_eq!(
                rows.iter().position(same_model),
                Some(0),
                "a model-only search must land on the first rung"
            );
        }

        // Activating a row sends the rung whole, so the depth on screen is the
        // depth the resolver applies.
        for row in &rows {
            let tier = ModelSelection::Tier {
                model: row.model.clone(),
                effort: row.effort,
            };
            assert_eq!(
                config.resolve_selection_effort(ModelRole::Primary, &tier),
                row.effort
            );
            assert_eq!(config.resolve_model(&tier), model);
        }

        // Collapsing a row to `Specific` drops the depth and falls back to the
        // first rung of the same model, which is the defect the tier form
        // removes.
        let collapsed = ModelSelection::Specific(model.clone());
        assert_eq!(
            config.resolve_selection_effort(ModelRole::Primary, &collapsed),
            Some(ReasoningEffort::Low)
        );
    }

    /// Every role-driven RPC carries the confirmed rung whole, so the server
    /// never re-derives the reasoning depth by matching the model against the
    /// configured role list (c-3).
    #[test]
    fn test_role_rpc_tier_reaches_the_server_resolver() {
        let config = parse_models(
            r#"
primary = ["openrouter:base/primary"]

reviewer = [
    { model = "openrouter:shared/reviewer", effort = "low" },
    { model = "openrouter:shared/reviewer", effort = "xhigh" },
]

architect = [
    { model = "openrouter:shared/architect", effort = "medium" },
    { model = "openrouter:shared/architect", effort = "max" },
]

coder = [
    { model = "openrouter:shared/coder", effort = "low" },
    { model = "openrouter:shared/coder", effort = "high" },
]
"#,
        );

        for (role, model, first, second) in [
            (
                ModelRole::Reviewer,
                ModelRef::openrouter("shared/reviewer"),
                Some(ReasoningEffort::Low),
                Some(ReasoningEffort::XHigh),
            ),
            (
                ModelRole::Architect,
                ModelRef::openrouter("shared/architect"),
                Some(ReasoningEffort::Medium),
                Some(ReasoningEffort::Max),
            ),
            (
                ModelRole::Coder,
                ModelRef::openrouter("shared/coder"),
                Some(ReasoningEffort::Low),
                Some(ReasoningEffort::High),
            ),
        ] {
            // Both rungs of the same model stay reachable in the role dialog.
            let rows = config.role_entries(role);
            assert_eq!(rows.len(), 2, "{role}: every rung stays reachable");
            assert_eq!(rows[0].effort, first, "{role}");
            assert_eq!(rows[1].effort, second, "{role}");
            assert_eq!(rows[1].model, model, "{role}");

            // The second rung is the one the developer confirmed, and it
            // reaches the resolver with its own depth instead of the depth of
            // the first entry of the same model.
            let tier = ModelSelection::Tier {
                model: rows[1].model.clone(),
                effort: rows[1].effort,
            };
            assert_eq!(
                config.resolve_selection_effort(role, &tier),
                second,
                "{role}: the confirmed rung keeps its depth"
            );
            assert_eq!(config.resolve_model(&tier), model, "{role}");

            // A legacy client that names only the model reference keeps the
            // established decoding: the first matching entry supplies the depth.
            let legacy = ModelSelection::Specific(model.clone());
            assert_eq!(
                config.resolve_selection_effort(role, &legacy),
                first,
                "{role}: the legacy form decodes unchanged"
            );
        }
    }

    #[test]
    fn test_coder_tier_index_defaults_to_the_base_rung() {
        let config = parse_models(
            r#"
primary = ["openrouter:base/primary"]
coder = [
    { model = "openrouter:start/model", effort = "low" },
    { model = "openrouter:strong/model", effort = "max" },
]
"#,
        );

        // No explicit selection starts execution at the base of the ladder.
        assert_eq!(config.coder_tier_index(None), 0);

        // A role selection names the model the coder role resolves to, which is
        // the base rung of the very same ladder.
        assert_eq!(
            config.coder_tier_index(Some(&ModelSelection::Role(ModelRole::Coder))),
            0
        );

        // A model the ladder does not carry restarts from the base rung instead
        // of guessing a position.
        assert_eq!(
            config.coder_tier_index(Some(&ModelSelection::Specific(ModelRef::openrouter(
                "unknown/model"
            )))),
            0
        );
    }

    #[test]
    fn test_coder_tier_index_matches_effort_then_model() {
        let config = parse_models(
            r#"
primary = ["openrouter:base/primary"]
coder = [
    { model = "openrouter:shared/model", effort = "low" },
    { model = "openrouter:shared/model", effort = "high" },
    { model = "openrouter:other/model", effort = "max" },
]
"#,
        );
        let model = ModelRef::openrouter("shared/model");

        // The same model occupies two rungs, so the reasoning depth
        // distinguishes them instead of collapsing onto the first one.
        for (effort, index) in [(ReasoningEffort::Low, 0), (ReasoningEffort::High, 1)] {
            let selection = ModelSelection::Tier {
                model: model.clone(),
                effort: Some(effort),
            };
            assert_eq!(config.coder_tier_index(Some(&selection)), index, "{effort}");
            // The returned position is a usable starting rung: the ladder entry
            // it names is exactly the configured model at the requested depth.
            let rung = config.coder_tier(index);
            assert_eq!(rung.model, model, "{effort}");
            assert_eq!(rung.effort, Some(effort), "{effort}");
        }

        // A rung naming the model at a depth the ladder does not configure
        // falls back to the model match, which lands on its first rung.
        assert_eq!(
            config.coder_tier_index(Some(&ModelSelection::Tier {
                model: model.clone(),
                effort: Some(ReasoningEffort::Max),
            })),
            0
        );

        // A bare model reference carries no depth of its own, so the configured
        // entry supplies it and the ladder position follows.
        assert_eq!(
            config.coder_tier_index(Some(&ModelSelection::Specific(model.clone()))),
            0
        );
        assert_eq!(
            config.coder_tier_index(Some(&ModelSelection::Specific(ModelRef::openrouter(
                "other/model"
            )))),
            2
        );
    }
}
