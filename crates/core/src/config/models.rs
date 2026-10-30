use serde::Serialize;
use tauqe_protocol::{
    ArchitectTier, ModelCatalogEntry, ModelEntry, ModelRef, ModelRole, ModelRolesSummary,
    ModelSelection, ReasoningEffort,
};

mod raw;
#[cfg(test)]
mod tests;

pub(crate) use raw::ModelsConfigRaw;
use raw::RoleModelSpec;

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
                    // The configured marker travels with the entry, so the
                    // resolved catalog preserves an exclusion declared on disk.
                    Ok(ModelEntry {
                        model,
                        effort: spec.effort,
                        level_up: spec.level_up,
                    })
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

    /// Reports whether the role excludes `model` from automatic escalation.
    ///
    /// A model declared with `level_up = false` stays in the role catalog
    /// ([`ModelsConfig::role_entries`]) so the developer may run it directly, while
    /// [`ModelsConfig::coder_ladder`] and [`ModelsConfig::architect_ladder`] skip it.
    /// A run started on such a model is already saturated: the automatic ladder
    /// never enters it and holds no stronger rung above it.
    pub fn excluded_from_escalation(&self, role: ModelRole, model: &ModelRef) -> bool {
        self.role_slice(role)
            .unwrap_or(&[])
            .iter()
            .any(|entry| &entry.model == model && entry.level_up() == Some(false))
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
    /// `primary` list. A rung whose entry declares `level_up = false` is
    /// skipped: the model stays selectable in the role catalog
    /// ([`ModelsConfig::role_entries`]) but automatic escalation never enters
    /// it. When even that leaves nothing, the ladder degrades to a single rung
    /// resolved through the role chain, so escalation always has a base.
    ///
    /// Every rung preserves the level-up marker declared on its configuration
    /// entry, so an excluded model remains distinguishable from a participating
    /// rung without re-reading configuration.
    pub fn coder_ladder(&self) -> Vec<ModelEntry> {
        let ladder: Vec<ModelEntry> = self
            .role_slice(ModelRole::Coder)
            .unwrap_or(&[])
            .iter()
            .filter(|entry| entry.level_up() != Some(false))
            .cloned()
            .collect();
        if ladder.is_empty() {
            vec![self.fallback_coder_tier()]
        } else {
            ladder
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

    /// Returns the rung that follows ladder position `idx`.
    ///
    /// `None` means the ladder is saturated: the next position resolves to the same
    /// `(model, effort)` pair, so advancing would only repeat the configuration that
    /// already failed. A run started on a model the coder role excludes by
    /// `level_up = false` begins at the strongest eligible position
    /// ([`ModelsConfig::coder_tier_index`]), so it saturates here as well and no
    /// stronger tier is ever entered automatically.
    pub fn next_coder_tier(&self, idx: usize) -> Option<(usize, ModelEntry)> {
        let current = self.coder_tier(idx);
        let next_idx = idx + 1;
        let next = self.coder_tier(next_idx);
        if next.model == current.model && next.effort == current.effort {
            None
        } else {
            Some((next_idx, next))
        }
    }

    /// Returns the ladder position a coder selection starts execution from.
    ///
    /// The ladder is the very list [`ModelsConfig::coder_ladder`] exposes, so an
    /// index returned here is directly usable as the starting rung of a turn.
    /// The selection is resolved to its concrete model and reasoning depth
    /// first: an entry matching both wins, because the same model may occupy
    /// several rungs at different depths; a model-only match is the fallback.
    /// An absent selection, or a model the ladder does not carry at all, restarts
    /// from the base rung instead of guessing a position. A manual run on a model
    /// the coder role excludes from escalation by `level_up = false` is not a
    /// ladder position either, yet it is already at the top: it starts from the
    /// strongest eligible rung and leaves the ladder saturated
    /// ([`ModelsConfig::next_coder_tier`]).
    pub fn coder_tier_index(&self, selection: Option<&ModelSelection>) -> usize {
        let Some(selection) = selection else {
            return 0;
        };
        let model = self.resolve_model(selection);
        let effort = self.resolve_selection_effort(ModelRole::Coder, selection);
        let ladder = self.coder_ladder();
        let position = ladder
            .iter()
            .position(|entry| entry.model == model && entry.effort == effort)
            .or_else(|| ladder.iter().position(|entry| entry.model == model));
        match position {
            Some(index) => index,
            // A model the coder role excludes from escalation is a manual run: it
            // occupies no ladder position, so it starts on the strongest eligible
            // rung and retrying can never enter a stronger tier.
            None if self.excluded_from_escalation(ModelRole::Coder, &model) => {
                ladder.len().saturating_sub(1)
            }
            None => 0,
        }
    }

    /// Fallback coder rung derived from the deterministic role resolution chain.
    fn fallback_coder_tier(&self) -> ModelEntry {
        ModelEntry::from(self.resolve_role_with_fallback(ModelRole::Coder, None))
    }

    /// Returns the ordered architect ladder.
    ///
    /// The ladder is the architect list when present, otherwise the inherited
    /// `primary` list, so re-planning never loses access to the configured
    /// Architect models. A tier whose entry declares `level_up = false` is
    /// skipped: the model stays selectable in the role catalog but automatic
    /// escalation never enters it. When even that leaves nothing, the ladder
    /// degrades to a single rung resolved through the role chain, so escalation
    /// always has a base.
    ///
    /// Every rung preserves the level-up marker declared on its configuration
    /// entry, so an excluded Architect remains distinguishable from a
    /// participating rung without re-reading configuration.
    pub fn architect_ladder(&self) -> Vec<ArchitectTier> {
        let ladder: Vec<ArchitectTier> = self
            .role_slice(ModelRole::Architect)
            .unwrap_or(&[])
            .iter()
            .filter(|entry| entry.level_up() != Some(false))
            .map(|entry| ArchitectTier {
                model: entry.model.clone(),
                effort: entry.effort,
                level_up: entry.level_up,
            })
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
    /// base rung, so escalation never guesses a position. The last rung saturates,
    /// so an exhausted ladder keeps the strongest Architect. A recorded rung the
    /// ladder excludes by `level_up = false` is a manual run the developer pinned:
    /// restarting would silently downgrade it, so the configuration stays as it is
    /// and escalation is saturated.
    pub fn next_architect_tier(&self, recorded: Option<&ArchitectTier>) -> ArchitectTier {
        let Some(recorded) = recorded else {
            return self.architect_tier(0);
        };
        if self.excluded_from_escalation(ModelRole::Architect, &recorded.model) {
            return recorded.clone();
        }
        match self.architect_tier_index(Some(recorded)) {
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
