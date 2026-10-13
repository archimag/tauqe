use serde::{Deserialize, Serialize};
use tauqe_protocol::{
    CoderTier, ModelRef, ModelRole, ModelRolesEffort, ModelRolesSummary, ModelSelection,
    ReasoningEffort,
};

pub const BUILTIN_DEFAULT_SCRIBE_MODEL: &str = "~anthropic/claude-haiku-latest";
pub const BUILTIN_DEFAULT_CODER_MODEL: &str = "~google/gemini-flash-latest";
pub const BUILTIN_DEFAULT_ARCHITECT_MODEL: &str = "~anthropic/claude-sonnet-latest";
pub const BUILTIN_DEFAULT_ADVISOR_MODEL: &str = "~google/gemini-flash-latest";
pub const BUILTIN_DEFAULT_REVIEWER_MODEL: &str = "~anthropic/claude-sonnet-latest";
pub const BUILTIN_DEFAULT_MODEL: &str = BUILTIN_DEFAULT_ADVISOR_MODEL;

/// Optional per-role reasoning depth configuration (`[models.effort]`).
///
/// Reuses the protocol-level `ModelRolesEffort` shape so effort levels stay
/// strictly typed (`ReasoningEffort`) and orthogonal to model identity.
pub type ModelsEffortConfig = ModelRolesEffort;

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
                parse_model_ref(trimmed).map(Some).map_err(serde::de::Error::custom)
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

/// Polymorphic configuration of the coder escalation ladder.
///
/// Accepts a single tier (a bare model string or a `{ model, effort }` table)
/// or an ordered array of tiers. The array form expresses an explicit
/// escalation ladder; the single form yields a one-rung ladder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CoderConfig {
    Single(CoderTier),
    Ladder(Vec<CoderTier>),
}

impl From<ModelRef> for CoderConfig {
    fn from(model: ModelRef) -> Self {
        CoderConfig::Single(CoderTier::from(model))
    }
}

/// Deserializes the optional coder configuration, treating blank strings as absence.
///
/// Mirrors the tolerant model-reference parsing: a blank `coder = ""` disables
/// the override instead of failing configuration load.
fn deserialize_coder_config_opt<'de, D>(
    deserializer: D,
) -> Result<Option<CoderConfig>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum CoderConfigRaw {
        Str(String),
        Config(CoderConfig),
    }

    match Option::<CoderConfigRaw>::deserialize(deserializer)? {
        None => Ok(None),
        Some(CoderConfigRaw::Str(s)) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                let model = parse_model_ref(trimmed).map_err(serde::de::Error::custom)?;
                Ok(Some(CoderConfig::from(model)))
            }
        }
        Some(CoderConfigRaw::Config(config)) => Ok(Some(config)),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelsConfig {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_model_ref_opt",
        serialize_with = "serialize_model_ref_opt"
    )]
    pub scribe: Option<ModelRef>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_coder_config_opt"
    )]
    pub coder: Option<CoderConfig>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_model_ref_opt",
        serialize_with = "serialize_model_ref_opt"
    )]
    pub architect: Option<ModelRef>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_model_ref_opt",
        serialize_with = "serialize_model_ref_opt"
    )]
    pub advisor: Option<ModelRef>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_model_ref_opt",
        serialize_with = "serialize_model_ref_opt"
    )]
    pub reviewer: Option<ModelRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<ModelsEffortConfig>,
    #[serde(default)]
    pub auto_level_up: bool,
}

impl ModelsConfig {
    /// Validates that at least one semantic role model is configured.
    pub fn validate(&self) -> Result<(), String> {
        if self.scribe.is_none()
            && self.coder.is_none()
            && self.architect.is_none()
            && self.advisor.is_none()
            && self.reviewer.is_none()
        {
            return Err(
                "Invalid models configuration: at least one model role (scribe, coder, architect, advisor, reviewer) must be configured"
                    .to_string(),
            );
        }
        Ok(())
    }

    /// Resolves a ModelRole using the deterministic proximity fallback chain within the models configuration.
    pub fn resolve_role(&self, role: ModelRole) -> ModelRef {
        self.resolve_role_with_fallback(role, None)
    }

    /// Resolves the optional reasoning effort configured for a semantic role.
    ///
    /// Returns `None` when no effort overrides are configured or when the
    /// requested role has no explicit effort, preserving the provider default.
    pub fn resolve_effort(&self, role: ModelRole) -> Option<ReasoningEffort> {
        let effort = self.effort.as_ref()?;
        match role {
            ModelRole::Scribe => effort.scribe,
            ModelRole::Coder => effort.coder,
            ModelRole::Architect => effort.architect,
            ModelRole::Advisor => effort.advisor,
            ModelRole::Reviewer => effort.reviewer,
        }
    }

    /// Resolves a ModelRole with an optional external fallback model (e.g. from configured provider models).
    pub fn resolve_role_with_fallback(
        &self,
        role: ModelRole,
        fallback: Option<ModelRef>,
    ) -> ModelRef {
        match role {
            ModelRole::Scribe => self
                .scribe
                .clone()
                .or_else(|| self.coder_base_model())
                .or_else(|| self.advisor.clone())
                .or_else(|| self.architect.clone())
                .or_else(|| self.reviewer.clone())
                .or(fallback)
                .unwrap_or_else(|| ModelRef::openrouter(BUILTIN_DEFAULT_SCRIBE_MODEL)),
            ModelRole::Coder => self
                .coder_base_model()
                .or_else(|| self.architect.clone())
                .or_else(|| self.advisor.clone())
                .or_else(|| self.scribe.clone())
                .or_else(|| self.reviewer.clone())
                .or(fallback)
                .unwrap_or_else(|| ModelRef::openrouter(BUILTIN_DEFAULT_CODER_MODEL)),
            ModelRole::Architect => self
                .architect
                .clone()
                .or_else(|| self.advisor.clone())
                .or_else(|| self.reviewer.clone())
                .or_else(|| self.coder_base_model())
                .or_else(|| self.scribe.clone())
                .or(fallback)
                .unwrap_or_else(|| ModelRef::openrouter(BUILTIN_DEFAULT_ARCHITECT_MODEL)),
            ModelRole::Advisor => self
                .advisor
                .clone()
                .or_else(|| self.architect.clone())
                .or_else(|| self.coder_base_model())
                .or_else(|| self.reviewer.clone())
                .or_else(|| self.scribe.clone())
                .or(fallback)
                .unwrap_or_else(|| ModelRef::openrouter(BUILTIN_DEFAULT_ADVISOR_MODEL)),
            ModelRole::Reviewer => self
                .reviewer
                .clone()
                .or_else(|| self.architect.clone())
                .or_else(|| self.advisor.clone())
                .or_else(|| self.coder_base_model())
                .or_else(|| self.scribe.clone())
                .or(fallback)
                .unwrap_or_else(|| ModelRef::openrouter(BUILTIN_DEFAULT_REVIEWER_MODEL)),
        }
    }

    /// Base coder model: the first rung of the configured ladder, if any.
    ///
    /// The deterministic role fallback chains treat this as the coder's model,
    /// so a multi-rung ladder still exposes a single base for cross-role
    /// resolution.
    fn coder_base_model(&self) -> Option<ModelRef> {
        match &self.coder {
            Some(CoderConfig::Single(tier)) => Some(tier.model.clone()),
            Some(CoderConfig::Ladder(tiers)) => tiers.first().map(|tier| tier.model.clone()),
            None => None,
        }
    }

    /// Returns the ordered coder escalation ladder.
    ///
    /// A single-tier configuration yields a one-rung ladder; when no coder is
    /// configured the ladder degrades to one fallback rung resolved through the
    /// deterministic role chain, so escalation always has a base.
    pub fn coder_ladder(&self) -> Vec<CoderTier> {
        match &self.coder {
            Some(CoderConfig::Single(tier)) => vec![tier.clone()],
            Some(CoderConfig::Ladder(tiers)) if !tiers.is_empty() => tiers.clone(),
            _ => vec![self.fallback_coder_tier()],
        }
    }

    /// Returns the coder rung at `idx`, saturating at the strongest tier.
    ///
    /// Out-of-range indices clamp to the last rung, so escalation never panics
    /// once the ladder is exhausted.
    pub fn coder_tier(&self, idx: usize) -> CoderTier {
        let ladder = self.coder_ladder();
        let last = ladder.len().saturating_sub(1);
        ladder
            .get(idx.min(last))
            .cloned()
            .unwrap_or_else(|| self.fallback_coder_tier())
    }

    /// Fallback coder rung derived from the deterministic role resolution chain.
    fn fallback_coder_tier(&self) -> CoderTier {
        CoderTier::new(
            self.resolve_role_with_fallback(ModelRole::Coder, None),
            self.resolve_effort(ModelRole::Coder),
        )
    }

    /// Resolves a ModelSelection to a concrete ModelRef.
    pub fn resolve_model(&self, selection: &ModelSelection) -> ModelRef {
        match selection {
            ModelSelection::Role(role) => self.resolve_role(*role),
            ModelSelection::Specific(model) => model.clone(),
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
            advisor: self.resolve_role_with_fallback(ModelRole::Advisor, fallback.clone()),
            reviewer: self.resolve_role_with_fallback(ModelRole::Reviewer, fallback),
            efforts: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_effort_defaults_to_none() {
        let config = ModelsConfig::default();
        assert!(config.effort.is_none());
        for role in [
            ModelRole::Scribe,
            ModelRole::Coder,
            ModelRole::Architect,
            ModelRole::Advisor,
            ModelRole::Reviewer,
        ] {
            assert_eq!(config.resolve_effort(role), None);
        }
    }

    #[test]
    fn test_resolve_effort_per_role_override() {
        let toml_str = r#"
architect = "xhigh"
coder = "low"
"#;
        let effort: ModelsEffortConfig = toml::from_str(toml_str).expect("parse effort");
        let config = ModelsConfig {
            effort: Some(effort),
            ..Default::default()
        };
        assert_eq!(
            config.resolve_effort(ModelRole::Architect),
            Some(ReasoningEffort::XHigh)
        );
        assert_eq!(
            config.resolve_effort(ModelRole::Coder),
            Some(ReasoningEffort::Low)
        );
        assert_eq!(config.resolve_effort(ModelRole::Scribe), None);
        assert_eq!(config.resolve_effort(ModelRole::Advisor), None);
        assert_eq!(config.resolve_effort(ModelRole::Reviewer), None);
    }

    #[test]
    fn test_coder_single_string_yields_one_rung() {
        let config: ModelsConfig =
            toml::from_str("coder = \"openrouter:anthropic/claude-3.7-sonnet\"")
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
            "coder = { model = \"openrouter:deepseek/deepseek-chat\", effort = \"high\" }",
        )
        .expect("parse coder table");
        let ladder = config.coder_ladder();
        assert_eq!(ladder.len(), 1);
        assert_eq!(ladder[0].model, ModelRef::openrouter("deepseek/deepseek-chat"));
        assert_eq!(ladder[0].effort, Some(ReasoningEffort::High));
        assert_eq!(config.resolve_role(ModelRole::Coder), ladder[0].model);
    }

    #[test]
    fn test_coder_array_builds_ordered_ladder() {
        let config: ModelsConfig = toml::from_str(
            r#"
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
    fn test_coder_blank_string_is_absent() {
        let config: ModelsConfig =
            toml::from_str("coder = \"\"").expect("parse blank coder");
        assert!(config.coder.is_none());
    }

    #[test]
    fn test_coder_ladder_falls_back_through_role_chain() {
        let config = ModelsConfig {
            architect: Some(ModelRef::openrouter("only/architect")),
            ..Default::default()
        };
        let ladder = config.coder_ladder();
        assert_eq!(ladder.len(), 1);
        assert_eq!(ladder[0].model, ModelRef::openrouter("only/architect"));
        assert_eq!(ladder[0].effort, None);
        assert_eq!(config.coder_tier(0), ladder[0]);
        assert_eq!(config.coder_tier(3), ladder[0]);
    }
}
