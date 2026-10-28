use serde::{Deserialize, Serialize};
use tauqe_protocol::review::{PostAnalysisMode, ReviewGroup};

use crate::review::uuid::generate_uuid_v7;

/// The `[review]` configuration section: named presets for a multi-level audit.
///
/// A preset decomposes one audit into isolated groups and declares when the
/// post-analysis layer is produced. The section is optional: a project that
/// declares no preset still runs a single audit with one implicit group and no
/// classification, so the feature never forces a migration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReviewConfig {
    /// Named presets the interface offers, in declaration order.
    #[serde(default)]
    pub presets: Vec<ReviewPreset>,
}

impl ReviewConfig {
    /// Validates every declared preset.
    ///
    /// An absent section is valid, because it declares no preset. A declared
    /// preset must carry a name that is unique across the section and at least
    /// one group, and every group must name at least one repository path: a
    /// session that audits nothing would spend a full run and report no
    /// finding.
    pub fn validate(&self) -> Result<(), String> {
        let mut seen: Vec<&str> = Vec::new();
        for preset in &self.presets {
            let name = preset.name.trim();
            if name.is_empty() {
                return Err("a review preset declares an empty name".to_string());
            }
            if seen.contains(&name) {
                return Err(format!("duplicate review preset name '{name}'"));
            }
            seen.push(name);

            if preset.groups.is_empty() {
                return Err(format!("review preset '{name}' declares no groups"));
            }
            for group in &preset.groups {
                if group.paths.iter().all(|path| path.trim().is_empty()) {
                    return Err(format!(
                        "review preset '{name}' group '{}' declares no paths",
                        group.name.trim()
                    ));
                }
            }
        }
        Ok(())
    }
}

/// One named review preset declared in `[[review.presets]]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewPreset {
    /// Name that identifies the preset, unique across the section.
    pub name: String,
    /// Human-readable title the interface renders instead of `name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Groups the preset decomposes the audit into, in execution order.
    #[serde(default)]
    pub groups: Vec<ReviewGroupConfig>,
    /// Moment at which the post-analysis layer is produced for a session
    /// launched from this preset.
    #[serde(default)]
    pub post_analysis: PostAnalysisMode,
}

impl ReviewPreset {
    /// Materializes the groups a session launched from this preset audits.
    ///
    /// Every launch mints fresh group identifiers, so launching one preset
    /// twice yields two sessions whose groups never collide. A group is titled
    /// by its configured name.
    pub fn to_launch_groups(&self) -> Vec<ReviewGroup> {
        self.groups
            .iter()
            .map(|group| ReviewGroup {
                id: generate_uuid_v7(),
                title: group.name.clone(),
                paths: group.paths.clone(),
                prompt: group.prompt.clone(),
                // A launch declares the slice to audit, never a root cause or a
                // remediation horizon: both are produced later, by the
                // post-analysis and decomposition waves.
                ..Default::default()
            })
            .collect()
    }
}

/// One isolated slice of a preset: the paths it audits and its local prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewGroupConfig {
    /// Name of the group, rendered as its title.
    pub name: String,
    /// Repository-relative paths this group audits.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Instructions that apply to this group only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}
