//! Single source of truth for how a model is printed in the interface.
//!
//! Every surface that names a model — the header, the model steppers and their
//! embedded lists, the selection dialog, the provenance badges of Plans and
//! Review — renders its text through this module, so one model can never be
//! named two different ways (c-2).
//!
//! The label is the alias declared once in the provider catalog, else the
//! provider-native name with the redundant transport prefix dropped, plus the
//! configured reasoning depth in brackets when one is set. A semantic role is
//! never part of the text: a role is how a model is *selected*, not a property
//! of its name, so a surface that used to print a role badge prints the concrete
//! entry the role resolved to through [`role_entry`] instead.
//!
//! The catalog travels with the role summary the daemon already reports, and is
//! published into this module once per frame through [`install_snapshot`]. Every
//! renderer then reads that snapshot (c-3): threading it through every
//! intermediate render signature would widen each module's interface for a value
//! that has exactly one update point.
//!
//! Every renderer prints through this module, so no frontend module assembles a
//! model label from the provider string on its own (c-2).

use std::collections::HashMap;

use tauqe_protocol::{
    ModelEntry, ModelRef, ModelRole, ModelRolesEntries, ModelRolesSummary, ReasoningEffort,
};

/// Display state published once per frame: the catalog aliases and the concrete
/// entry every semantic role currently resolves to.
#[derive(Debug)]
struct Snapshot {
    /// Declared display alias per model.
    ///
    /// Keyed by the exact model reference the catalog declares, so a role entry
    /// and the catalog entry describing the same model compare equal and no
    /// second, string-derived naming rule can appear (c-1).
    aliases: HashMap<ModelRef, String>,
    /// Concrete entry each semantic role resolves to.
    roles: HashMap<ModelRole, ModelEntry>,
}

impl Snapshot {
    /// Builds the snapshot from the role summary the daemon reported.
    ///
    /// An absent or partial summary is not an error: it publishes an empty
    /// snapshot, and every lookup then degrades to the provider name.
    fn new(summary: Option<&ModelRolesSummary>) -> Self {
        Self {
            aliases: collect_aliases(summary.and_then(|summary| summary.entries.as_ref())),
            roles: collect_roles(summary),
        }
    }
}

/// Publishes the catalog and the role summary every renderer reads.
///
/// This is the single update point of the display state. It must run before the
/// body of a frame is drawn, so the labels of the frame always reflect the
/// configuration reported with that frame and never the previous one (c-3).
/// A missing summary publishes an empty snapshot instead of failing, so the
/// interface degrades to the provider name (c-2).
pub fn install_snapshot(summary: Option<&ModelRolesSummary>) {
    publish(Snapshot::new(summary));
}

/// Renders the display label of a concrete model.
///
/// A model the catalog does not describe is not an error: the provider-native
/// name is used, and an empty or partial reference yields an empty base instead
/// of panicking.
pub fn model_label(model: &ModelRef, effort: Option<ReasoningEffort>) -> String {
    let name = alias_for(model).unwrap_or_else(|| model.name.clone());
    match effort {
        Some(effort) => format!("{} [{}]", name, effort),
        None => name,
    }
}

/// Renders the display label of a configured role entry.
pub fn entry_label(entry: &ModelEntry) -> String {
    model_label(&entry.model, entry.effort)
}

/// Renders a numbered list row as `[N] <label>`.
///
/// The row reuses [`entry_label`], so the index and the name can never be
/// produced by two different rules and drift apart.
pub fn numbered_entry_label(index: usize, entry: &ModelEntry) -> String {
    format!("[{}] {}", index + 1, entry_label(entry))
}

/// Resolves a semantic role to the concrete entry it currently runs on.
///
/// A surface that used to print a role badge prints this entry instead, so the
/// role stays a selection mechanism and never becomes part of the name (c-2).
/// Returns `None` while no role summary has been published.
pub fn role_entry(role: ModelRole) -> Option<ModelEntry> {
    with_snapshot(|snapshot| snapshot.and_then(|snapshot| snapshot.roles.get(&role).cloned()))
}

/// Declared alias of a model, if the published catalog carries one.
fn alias_for(model: &ModelRef) -> Option<String> {
    with_snapshot(|snapshot| snapshot.and_then(|snapshot| snapshot.aliases.get(model).cloned()))
}

/// Collects the declared display aliases, keyed by the model they describe.
///
/// A catalog entry without an alias, or with a blank one, contributes nothing:
/// it describes a model that the provider name already names.
fn collect_aliases(entries: Option<&ModelRolesEntries>) -> HashMap<ModelRef, String> {
    entries
        .and_then(|entries| entries.catalog.as_deref())
        .unwrap_or_default()
        .iter()
        .filter_map(|entry| {
            let alias = entry.alias.as_deref()?.trim();
            if alias.is_empty() {
                None
            } else {
                Some((entry.model.clone(), alias.to_string()))
            }
        })
        .collect()
}

/// Resolves every semantic role to the concrete entry it runs on.
fn collect_roles(summary: Option<&ModelRolesSummary>) -> HashMap<ModelRole, ModelEntry> {
    let Some(summary) = summary else {
        return HashMap::new();
    };
    [
        (ModelRole::Scribe, &summary.scribe),
        (ModelRole::Coder, &summary.coder),
        (ModelRole::Architect, &summary.architect),
        (ModelRole::Primary, &summary.primary),
        (ModelRole::Reviewer, &summary.reviewer),
    ]
    .into_iter()
    .map(|(role, model)| (role, ModelEntry::new(model.clone(), role_effort(summary, role))))
    .collect()
}

/// Reasoning depth configured for a role override, if any.
fn role_effort(summary: &ModelRolesSummary, role: ModelRole) -> Option<ReasoningEffort> {
    let efforts = summary.efforts.as_ref()?;
    match role {
        ModelRole::Scribe => efforts.scribe,
        ModelRole::Coder => efforts.coder,
        ModelRole::Architect => efforts.architect,
        ModelRole::Primary => efforts.primary,
        ModelRole::Reviewer => efforts.reviewer,
    }
}

/// Process-wide snapshot, mirroring the theme and locale projection.
#[cfg(not(test))]
static SNAPSHOT: std::sync::RwLock<Option<Snapshot>> = std::sync::RwLock::new(None);

#[cfg(test)]
thread_local! {
    /// Per-thread replacement of the process-wide snapshot, active only in the
    /// crate's own test build.
    ///
    /// The test harness runs every `#[test]` in its own thread inside one shared
    /// process, so a single global snapshot lets a concurrently running test
    /// overwrite the one another test has just installed. Scoping the test
    /// snapshot to the thread keeps each test's installation private, while the
    /// production build keeps the single process-wide projection.
    static SNAPSHOT: std::cell::RefCell<Option<Snapshot>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(not(test))]
fn publish(snapshot: Snapshot) {
    if let Ok(mut guard) = SNAPSHOT.write() {
        *guard = Some(snapshot);
    }
}

#[cfg(test)]
fn publish(snapshot: Snapshot) {
    let _ = SNAPSHOT.try_with(move |cell| {
        if let Ok(mut guard) = cell.try_borrow_mut() {
            *guard = Some(snapshot);
        }
    });
}

#[cfg(not(test))]
fn with_snapshot<T>(f: impl FnOnce(Option<&Snapshot>) -> T) -> T {
    match SNAPSHOT.read() {
        Ok(guard) => f(guard.as_ref()),
        Err(_) => f(None),
    }
}

#[cfg(test)]
fn with_snapshot<T>(f: impl FnOnce(Option<&Snapshot>) -> T) -> T {
    SNAPSHOT.with(|cell| match cell.try_borrow() {
        Ok(guard) => f(guard.as_ref()),
        Err(_) => f(None),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::{ModelCatalogEntry, ModelRolesEffort};

    fn flash() -> ModelRef {
        ModelRef::openrouter("~google/gemini-flash-latest")
    }

    fn deepseek() -> ModelRef {
        ModelRef::openrouter("~deepseek/deepseek-flash-latest")
    }

    /// A reported summary with two aliased catalog entries, distinct models per
    /// role, and one reasoning-depth override on the Coder role.
    fn fixture() -> ModelRolesSummary {
        ModelRolesSummary {
            scribe: flash(),
            coder: deepseek(),
            architect: flash(),
            primary: flash(),
            reviewer: deepseek(),
            efforts: Some(ModelRolesEffort {
                coder: Some(ReasoningEffort::High),
                ..Default::default()
            }),
            entries: Some(ModelRolesEntries {
                catalog: Some(vec![
                    ModelCatalogEntry {
                        model: flash(),
                        alias: Some("Gemini Flash".to_string()),
                    },
                    ModelCatalogEntry {
                        model: deepseek(),
                        alias: Some("DeepSeek Flash".to_string()),
                    },
                ]),
                ..Default::default()
            }),
        }
    }

    #[test]
    fn label_prefers_the_declared_alias_with_and_without_effort() {
        install_snapshot(Some(&fixture()));

        assert_eq!(model_label(&flash(), None), "Gemini Flash");
        assert_eq!(
            model_label(&flash(), Some(ReasoningEffort::High)),
            "Gemini Flash [high]"
        );
        assert_eq!(
            entry_label(&ModelEntry::new(flash(), Some(ReasoningEffort::Max))),
            "Gemini Flash [max]"
        );
    }

    #[test]
    fn label_falls_back_to_the_provider_name_without_the_transport_prefix() {
        install_snapshot(Some(&fixture()));
        let unlisted = ModelRef::openrouter("~anthropic/claude-sonnet-latest");

        assert_eq!(model_label(&unlisted, None), "~anthropic/claude-sonnet-latest");
        assert_eq!(
            model_label(&unlisted, Some(ReasoningEffort::Max)),
            "~anthropic/claude-sonnet-latest [max]"
        );
        assert!(!model_label(&unlisted, None).contains("openrouter:"));
    }

    #[test]
    fn role_resolves_to_the_concrete_model_and_never_prints_the_role() {
        install_snapshot(Some(&fixture()));

        let coder = role_entry(ModelRole::Coder).expect("coder entry");
        assert_eq!(coder.model, deepseek());
        assert_eq!(coder.effort, Some(ReasoningEffort::High));
        assert_eq!(entry_label(&coder), "DeepSeek Flash [high]");

        let primary = role_entry(ModelRole::Primary).expect("primary entry");
        assert_eq!(primary.model, flash());
        assert_eq!(primary.effort, None);
        assert_eq!(entry_label(&primary), "Gemini Flash");

        // The role is a selection mechanism, never part of the printed name.
        for label in [entry_label(&coder), entry_label(&primary)] {
            assert!(!label.contains("Coder"), "got: {label}");
            assert!(!label.contains("[Cod]"), "got: {label}");
            assert!(!label.contains("Primary"), "got: {label}");
        }
    }

    #[test]
    fn numbered_rows_reuse_the_single_label_rule() {
        install_snapshot(Some(&fixture()));

        let coder = ModelEntry::new(deepseek(), Some(ReasoningEffort::High));
        assert_eq!(numbered_entry_label(0, &coder), "[1] DeepSeek Flash [high]");

        let unlisted = ModelEntry::new(
            ModelRef::openrouter("~anthropic/claude-sonnet-latest"),
            None,
        );
        assert_eq!(
            numbered_entry_label(1, &unlisted),
            "[2] ~anthropic/claude-sonnet-latest"
        );
    }

    #[test]
    fn a_missing_summary_degrades_to_the_provider_name() {
        install_snapshot(None);

        let model = ModelRef::openrouter("a/b");
        assert_eq!(model_label(&model, None), "a/b");
        assert!(role_entry(ModelRole::Coder).is_none());
    }

    #[test]
    fn partial_model_descriptions_never_panic() {
        install_snapshot(None);

        let empty = ModelRef::default();
        assert_eq!(model_label(&empty, None), "");
        assert!(model_label(&empty, Some(ReasoningEffort::Low)).ends_with("[low]"));

        // A catalog entry with an explicit `null` or blank alias declares nothing,
        // so the provider name wins instead of an empty label.
        install_snapshot(Some(&ModelRolesSummary {
            scribe: ModelRef::openrouter("a/b"),
            coder: ModelRef::openrouter("a/b"),
            architect: ModelRef::openrouter("a/b"),
            primary: ModelRef::openrouter("a/b"),
            reviewer: ModelRef::openrouter("a/b"),
            efforts: None,
            entries: Some(ModelRolesEntries {
                catalog: Some(vec![
                    ModelCatalogEntry {
                        model: ModelRef::openrouter("a/b"),
                        alias: None,
                    },
                    ModelCatalogEntry {
                        model: ModelRef::openrouter("c/d"),
                        alias: Some("   ".to_string()),
                    },
                ]),
                ..Default::default()
            }),
        }));
        assert_eq!(model_label(&ModelRef::openrouter("a/b"), None), "a/b");
        assert_eq!(model_label(&ModelRef::openrouter("c/d"), None), "c/d");
    }
}
