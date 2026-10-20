use serde::{Deserialize, Serialize};
use tauqe_protocol::{
    ArchitectTier, ModelEntry, ModelRef, ModelRole, ModelRolesSummary, ModelSelection,
    ReasoningEffort,
};

pub const BUILTIN_DEFAULT_SCRIBE_MODEL: &str = "~anthropic/claude-haiku-latest";
pub const BUILTIN_DEFAULT_CODER_MODEL: &str = "~google/gemini-flash-latest";
pub const BUILTIN_DEFAULT_ARCHITECT_MODEL: &str = "~anthropic/claude-sonnet-latest";
pub const BUILTIN_DEFAULT_PRIMARY_MODEL: &str = "~google/gemini-flash-latest";
pub const BUILTIN_DEFAULT_REVIEWER_MODEL: &str = "~anthropic/claude-sonnet-latest";
pub const BUILTIN_DEFAULT_MODEL: &str = BUILTIN_DEFAULT_PRIMARY_MODEL;

pub fn parse_model_ref(s: &str) -> Result<ModelRef, String> {
    s.parse()
}

pub fn deserialize_model_ref_opt<'de, D>(deserializer: D) -> Result<Option<ModelRef>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum ModelRefRaw {
        Str(String),
        Struct(ModelRef),
    }

    match Option::<ModelRefRaw>::deserialize(deserializer)? {
        None => Ok(None),
        Some(ModelRefRaw::Str(s)) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                parse_model_ref(trimmed)
                    .map(Some)
                    .map_err(serde::de::Error::custom)
            }
        }
        Some(ModelRefRaw::Struct(m)) => Ok(Some(m)),
    }
}

pub fn serialize_model_ref_opt<S>(opt: &Option<ModelRef>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match opt {
        Some(m) => serializer.serialize_str(&m.to_string()),
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
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelsConfig {
    pub primary: Vec<ModelEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coder: Option<Vec<ModelEntry>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub architect: Option<Vec<ModelEntry>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer: Option<Vec<ModelEntry>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_model_ref_opt",
        serialize_with = "serialize_model_ref_opt"
    )]
    pub scribe: Option<ModelRef>,
    #[serde(default)]
    pub auto_level_up: bool,
}

impl ModelsConfig {
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
    /// effort specified, returns `None`.
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
        }
    }

    /// Resolves a [`ModelSelection`] to a concrete [`ModelRef`].
    pub fn resolve_model(&self, selection: &ModelSelection) -> ModelRef {
        match selection {
            ModelSelection::Role(role) => self.resolve_role(*role),
            ModelSelection::Specific(model) => model.clone(),
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
        let config: ModelsConfig = toml::from_str(
            r#"
primary = ["openrouter:base/primary"]
coder = [
    { model = "openrouter:deepseek/deepseek-chat", effort = "medium" },
    { model = "openrouter:google/gemini-flash-latest", effort = "high" },
]
architect = [{ model = "openrouter:anthropic/claude-sonnet-latest", effort = "high" }]
reviewer = ["openrouter:google/gemini-flash-latest"]
"#,
        )
        .expect("parse models config");

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
        let config: ModelsConfig = toml::from_str(
            r#"
primary = ["openrouter:base/primary"]
coder = ["openrouter:anthropic/claude-3.7-sonnet"]
"#,
        )
        .expect("parse coder string");
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
        let config: ModelsConfig = toml::from_str(
            r#"
primary = ["openrouter:base/primary"]
coder = [{ model = "openrouter:deepseek/deepseek-chat", effort = "high" }]
"#,
        )
        .expect("parse coder table");
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
        let config: ModelsConfig = toml::from_str(
            r#"
primary = ["openrouter:base/primary"]
coder = [
    "openrouter:base/model",
    { model = "openrouter:strong/model", effort = "xhigh" },
]
"#,
        )
        .expect("parse coder array");
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
        let config: ModelsConfig = toml::from_str(
            r#"
primary = ["openrouter:only/primary", "openrouter:second/primary"]
architect = ["openrouter:only/architect"]
"#,
        )
        .expect("parse models config");
        let ladder = config.coder_ladder();
        assert_eq!(ladder.len(), 2);
        assert_eq!(ladder[0].model, ModelRef::openrouter("only/primary"));
        assert_eq!(ladder[1].model, ModelRef::openrouter("second/primary"));
        assert_eq!(config.coder_tier(0), ladder[0]);
        assert_eq!(config.coder_tier(5), ladder[1]);
    }

    #[test]
    fn test_architect_ladder_derives_from_role_list() {
        let config: ModelsConfig = toml::from_str(
            r#"
primary = ["openrouter:base/primary"]
architect = [
    { model = "openrouter:planner/one", effort = "high" },
    { model = "openrouter:planner/two", effort = "xhigh" },
]
"#,
        )
        .expect("parse models config");

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
        let config: ModelsConfig = toml::from_str(
            r#"
primary = ["openrouter:base/primary"]
architect = ["openrouter:planner/one"]
"#,
        )
        .expect("parse models config");

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
        let config: ModelsConfig = toml::from_str(
            r#"
primary = ["openrouter:base/primary"]
architect = [
    { model = "openrouter:deepseek/flash", effort = "max" },
    { model = "openrouter:google/gemini-flash", effort = "high" },
    "openrouter:google/gemini-argon",
]
"#,
        )
        .expect("parse models config");

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
    fn test_architect_tier_index_falls_back_to_model_matching_on_effort_discrepancy() {
        let config: ModelsConfig = toml::from_str(
            r#"
primary = ["openrouter:base/primary"]
architect = [
    { model = "openrouter:deepseek/flash", effort = "max" },
    { model = "openrouter:google/gemini-flash", effort = "high" },
    "openrouter:google/gemini-argon",
]
"#,
        )
        .expect("parse models config");

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
    fn test_model_entry_polymorphic_parsing_with_alias_and_effort() {
        let config: ModelsConfig = toml::from_str(
            r#"
primary = ["openrouter:base/primary"]
coder = [
    "openrouter:plain/model",
    { model = "openrouter:aliased/model", effort = "high", alias = "Fast Coder" },
]
"#,
        )
        .expect("parse models config");
        let ladder = config.coder_ladder();
        assert_eq!(ladder.len(), 2);
        assert_eq!(ladder[0].model, ModelRef::openrouter("plain/model"));
        assert_eq!(ladder[0].alias, None);
        assert_eq!(ladder[1].model, ModelRef::openrouter("aliased/model"));
        assert_eq!(ladder[1].effort, Some(ReasoningEffort::High));
        assert_eq!(ladder[1].alias, Some("Fast Coder".to_string()));
    }

    #[test]
    fn test_role_slice_mirrors_role_entries_for_stored_roles() {
        let config: ModelsConfig = toml::from_str(
            r#"
primary = ["openrouter:base/primary", "openrouter:second/primary"]
coder = ["openrouter:only/coder"]
architect = [{ model = "openrouter:only/architect", effort = "high" }]
"#,
        )
        .expect("parse models config");

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
}
