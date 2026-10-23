//! The `just` task runner: parsing of `just --dump --dump-format json`.
//!
//! `just` publishes its entire justfile model as one JSON document, so the
//! harness reads structured data and never parses the justfile grammar. Two
//! invariants keep that surface safe:
//!
//! - **The allowlist group is the only door.** A recipe is reachable only when
//!   it carries `[group('tauqe')]`. Administrative, release, and destructive
//!   recipes stay invisible at parse time and can never be addressed by the
//!   model, which is the *no arbitrary shell* invariant applied to a task
//!   runner.
//! - **The mapping is declarative.** `[doc('...')]` (or the recipe's doc
//!   comment) becomes the tool description, `metadata` carries the wall-clock
//!   budget in seconds, and the recipe named `verify` fills the verification
//!   lifecycle while every other recipe becomes a typed custom tool invoked as
//!   `just <recipe>`.
//!
//! A recipe that cannot be invoked by name — a private one — is skipped, so the
//! published tool list never advertises a command the harness cannot run.

use std::collections::BTreeMap;

use serde_json::Value;
use tauqe_protocol::ProjectCommandSpec;

use super::{RunnerCommand, RunnerManifest, RunnerManifestError, TaskRunner};

/// Executable that resolves and runs the recipes.
const PROGRAM: &str = "just";

/// Recipe bound to the verification lifecycle.
const VERIFY_RECIPE: &str = "verify";

/// Group a recipe must declare to become reachable by the harness.
const ALLOWLIST_GROUP: &str = "tauqe";

/// Metadata keys that carry the wall-clock budget of one recipe.
const TIMEOUT_KEYS: [&str; 2] = ["timeout", "timeout_secs"];

/// The `just` task runner.
pub(super) struct JustRunner;

/// Shared instance handed out by [`super::task_runner`].
pub(super) static JUST: JustRunner = JustRunner;

impl TaskRunner for JustRunner {
    fn program(&self) -> &str {
        PROGRAM
    }

    fn dump_args(&self) -> Vec<String> {
        // `--dump` prints the model and `--dump-format json` selects the
        // structured shape, so nothing downstream depends on the grammar.
        vec![
            "--dump".to_string(),
            "--dump-format".to_string(),
            "json".to_string(),
        ]
    }

    fn parse_manifest(&self, dump: &str) -> Result<RunnerManifest, RunnerManifestError> {
        parse_dump(dump)
    }
}

/// Parses one `just --dump --dump-format json` document.
fn parse_dump(dump: &str) -> Result<RunnerManifest, RunnerManifestError> {
    let root: Value =
        serde_json::from_str(dump).map_err(|err| RunnerManifestError::InvalidDump {
            reason: format!("the dump is not valid JSON: {err}"),
        })?;
    let recipes = root
        .get("recipes")
        .and_then(Value::as_object)
        .ok_or_else(|| RunnerManifestError::InvalidDump {
            reason: "the dump carries no `recipes` table".to_string(),
        })?;

    // Recipes are read in name order, so the same justfile always produces the
    // same tool list and therefore the same prompt bytes.
    let mut names: Vec<&str> = recipes.keys().map(String::as_str).collect();
    names.sort_unstable();

    let mut manifest = RunnerManifest::default();
    for name in names {
        let Some(recipe) = recipes.get(name) else {
            continue;
        };
        let Some(command) = command_from_recipe(name, recipe) else {
            continue;
        };
        if name == VERIFY_RECIPE {
            manifest.verify = Some(command);
        } else {
            manifest.commands.push(command);
        }
    }
    Ok(manifest)
}

/// Builds the harness command of one recipe, or `None` when the recipe is not
/// addressable by the harness.
fn command_from_recipe(name: &str, recipe: &Value) -> Option<RunnerCommand> {
    if name.trim().is_empty() {
        return None;
    }
    let recipe = recipe.as_object()?;

    // A private recipe has no invocation of its own, so publishing it would
    // advertise a command the harness cannot run.
    if recipe
        .get("private")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return None;
    }

    let attributes = Attributes::parse(recipe.get("attributes"));
    if !attributes.is_allowed() {
        return None;
    }

    Some(RunnerCommand {
        name: name.to_string(),
        description: attributes.description(recipe.get("doc")),
        spec: ProjectCommandSpec {
            // The recipe is addressed by name, so `just` resolves its own
            // parameters, dependencies, and shebang exactly as on a terminal.
            command: PROGRAM.to_string(),
            args: vec![name.to_string()],
            timeout_secs: attributes.timeout_secs(),
        },
    })
}

/// Attributes declared on one recipe.
///
/// Only the three attributes the harness reads are collected; every other
/// attribute a justfile declares is irrelevant to the published command.
#[derive(Debug, Default)]
struct Attributes {
    /// Groups the recipe declares.
    groups: Vec<String>,
    /// Documentation strings the recipe declares.
    docs: Vec<String>,
    /// Metadata key/value pairs, in key order.
    metadata: BTreeMap<String, String>,
}

impl Attributes {
    /// Collects the attributes of one recipe.
    fn parse(value: Option<&Value>) -> Self {
        let mut attributes = Self::default();
        let Some(items) = value.and_then(Value::as_array) else {
            return attributes;
        };
        for item in items {
            let Some(declared) = item.as_object() else {
                continue;
            };
            for (name, payload) in declared {
                match name.as_str() {
                    "group" => attributes.groups.extend(text_values(payload)),
                    "doc" => attributes.docs.extend(text_values(payload)),
                    "metadata" => attributes.metadata.extend(metadata_pairs(payload)),
                    _ => {}
                }
            }
        }
        attributes
    }

    /// Reports whether the recipe declares the allowlist group.
    fn is_allowed(&self) -> bool {
        self.groups
            .iter()
            .any(|group| group.trim() == ALLOWLIST_GROUP)
    }

    /// Returns the description of the recipe, preferring the declared attribute
    /// over the doc comment.
    fn description(&self, doc: Option<&Value>) -> Option<String> {
        self.docs
            .iter()
            .map(|doc| doc.trim())
            .find(|doc| !doc.is_empty())
            .map(str::to_string)
            .or_else(|| {
                doc.and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|doc| !doc.is_empty())
                    .map(str::to_string)
            })
    }

    /// Returns the declared wall-clock budget, when it is a positive number.
    ///
    /// An absent, non-numeric, or zero value leaves the harness budget in place,
    /// so a malformed attribute can never disable the timeout.
    fn timeout_secs(&self) -> Option<u64> {
        for wanted in TIMEOUT_KEYS {
            let declared = self
                .metadata
                .iter()
                .find(|(name, _)| name.trim().eq_ignore_ascii_case(wanted))
                .and_then(|(_, value)| value.trim().parse::<u64>().ok())
                .filter(|secs| *secs > 0);
            if let Some(secs) = declared {
                return Some(secs);
            }
        }
        None
    }
}

/// Reads a string attribute payload.
///
/// `just` renders a single string attribute as a plain string; a list-shaped
/// value is accepted as well, so grouping repeated attributes cannot change the
/// meaning of a declaration.
fn text_values(payload: &Value) -> Vec<String> {
    match payload {
        Value::String(text) => vec![text.clone()],
        Value::Array(items) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

/// Reads a `metadata(...)` payload into key/value pairs.
fn metadata_pairs(payload: &Value) -> Vec<(String, String)> {
    match payload {
        Value::Array(items) => items.iter().filter_map(metadata_pair).collect(),
        Value::Object(pairs) => pairs
            .iter()
            .map(|(key, value)| (key.clone(), scalar(value)))
            .collect(),
        _ => Vec::new(),
    }
}

/// Reads one metadata pair, accepting every shape a dump may render it in.
fn metadata_pair(item: &Value) -> Option<(String, String)> {
    match item {
        Value::Array(pair) => match pair.as_slice() {
            [key, value] => {
                let key = scalar(key);
                let value = scalar(value);
                if !value.trim().is_empty() {
                    return Some((key, value));
                }
                // `metadata('timeout=300')` may keep the pair inside one string.
                let (key, value) = key.split_once('=')?;
                Some((key.to_string(), value.to_string()))
            }
            // A pair declared as a single `key=value` string renders as a
            // one-element array, so that shape carries the whole pair.
            [pair] => {
                let text = scalar(pair);
                let (key, value) = text.split_once('=')?;
                Some((key.to_string(), value.to_string()))
            }
            _ => None,
        },
        Value::Object(pair) => Some((scalar(pair.get("key")?), scalar(pair.get("value")?))),
        Value::String(pair) => {
            let (key, value) = pair.split_once('=')?;
            Some((key.to_string(), value.to_string()))
        }
        _ => None,
    }
}

/// Renders a JSON scalar as text, dropping structured values that cannot be a
/// metadata key or value.
fn scalar(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        Value::Bool(flag) => flag.to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// Builds a dump document from the given recipe table.
    fn dump(recipes: Value) -> String {
        json!({ "recipes": recipes }).to_string()
    }

    /// Parses a dump that must be accepted.
    fn manifest(recipes: Value) -> RunnerManifest {
        parse_dump(&dump(recipes)).expect("the dump parses")
    }

    /// Collects the published command names in the order they were read.
    fn names(manifest: &RunnerManifest) -> Vec<&str> {
        manifest
            .commands
            .iter()
            .map(|command| command.name.as_str())
            .collect()
    }

    #[test]
    fn test_group_allowlist_admits_only_tauqe_recipes() {
        let manifest = manifest(json!({
            "check": { "attributes": [{ "group": "tauqe" }] },
            "release": { "attributes": [] },
            "deploy": { "attributes": [{ "group": "admin" }] },
            "publish": {}
        }));

        // A recipe without the allowlist group never reaches the model.
        assert_eq!(names(&manifest), ["check"]);
        assert!(manifest.verify.is_none());
    }

    #[test]
    fn test_doc_and_metadata_map_to_the_published_command() {
        let manifest = manifest(json!({
            "check": {
                "attributes": [
                    { "group": "tauqe" },
                    { "doc": "Type-check the workspace" },
                    { "metadata": [["timeout", "600"]] }
                ]
            },
            "test": {
                "attributes": [{ "group": "tauqe" }],
                "doc": "Run the test suite"
            }
        }));

        let check = &manifest.commands[0];
        assert_eq!(check.name, "check");
        assert_eq!(
            check.description.as_deref(),
            Some("Type-check the workspace")
        );
        assert_eq!(check.spec.command, "just");
        assert_eq!(check.spec.args, ["check"]);
        assert_eq!(check.spec.timeout_secs, Some(600));

        let test = &manifest.commands[1];
        assert_eq!(test.name, "test");
        assert_eq!(test.description.as_deref(), Some("Run the test suite"));
        assert_eq!(test.spec.args, ["test"]);
        assert_eq!(test.spec.timeout_secs, None);
    }

    #[test]
    fn test_verify_recipe_fills_the_lifecycle() {
        let manifest = manifest(json!({
            "verify": {
                "attributes": [
                    { "group": "tauqe" },
                    { "metadata": [["timeout", "900"]] }
                ]
            },
            "check": { "attributes": [{ "group": "tauqe" }] }
        }));

        let verify = manifest
            .verify
            .as_ref()
            .expect("the verify recipe is published");
        assert_eq!(verify.name, "verify");
        assert_eq!(verify.spec.command, "just");
        assert_eq!(verify.spec.args, ["verify"]);
        assert_eq!(verify.spec.timeout_secs, Some(900));

        assert_eq!(names(&manifest), ["check"]);
    }

    #[test]
    fn test_unaddressable_recipes_are_skipped() {
        let manifest = manifest(json!({
            "hidden": { "attributes": [{ "group": "tauqe" }], "private": true },
            "broken": "not a recipe",
            "  ": { "attributes": [{ "group": "tauqe" }] }
        }));

        assert!(manifest.is_empty());
    }

    #[test]
    fn test_metadata_is_read_tolerantly() {
        let manifest = manifest(json!({
            "objects": {
                "attributes": [
                    { "group": "tauqe" },
                    { "metadata": { "timeout_secs": 120 } }
                ]
            },
            "strings": {
                "attributes": [
                    { "group": "tauqe" },
                    { "metadata": [["timeout=45"]] }
                ]
            },
            "useless": {
                "attributes": [
                    { "group": "tauqe" },
                    { "metadata": [["timeout", "soon"], ["timeout_secs", "0"]] }
                ]
            }
        }));

        let timeouts: Vec<Option<u64>> = manifest
            .commands
            .iter()
            .map(|command| command.spec.timeout_secs)
            .collect();
        assert_eq!(timeouts, [Some(120), Some(45), None]);
    }

    #[test]
    fn test_invalid_dumps_are_rejected() {
        for dump in ["", "not json", "{}", r#"{"recipes":[]}"#] {
            assert!(
                matches!(
                    parse_dump(dump),
                    Err(RunnerManifestError::InvalidDump { .. })
                ),
                "dump {dump:?} was accepted"
            );
        }
    }

    #[test]
    fn test_command_order_is_deterministic() {
        let manifest = manifest(json!({
            "zeta": { "attributes": [{ "group": "tauqe" }] },
            "alpha": { "attributes": [{ "group": "tauqe" }] }
        }));

        assert_eq!(names(&manifest), ["alpha", "zeta"]);
    }

    #[test]
    fn test_the_runner_declares_its_own_dump() {
        assert_eq!(JUST.program(), PROGRAM);
        assert_eq!(JUST.dump_args(), ["--dump", "--dump-format", "json"]);
        assert!(matches!(
            JUST.parse_manifest("{}"),
            Err(RunnerManifestError::InvalidDump { .. })
        ));
    }
}
