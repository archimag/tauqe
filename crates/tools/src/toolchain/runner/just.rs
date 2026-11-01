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
//!   comment) becomes the description, the parameters the recipe declares
//!   become its published contract, `metadata` carries the wall-clock budget in
//!   seconds, and the recipe name binds the role: `verify` fills the
//!   verification lifecycle, `format` fills the in-memory filter, `post-hook`
//!   (or `post_hook`) fills the disk-side hook, and every other recipe becomes
//!   a typed custom tool invoked as `just <recipe>`.
//!
//! A recipe that cannot be invoked by name — a private one — is skipped, so the
//! published tool list never advertises a command the harness cannot run.

use std::collections::BTreeMap;

use serde_json::Value;
use tauqe_protocol::{ProjectCommandSpec, ProjectParameterSpec};

use super::{RunnerCommand, RunnerManifest, RunnerManifestError, TaskRunner};

/// Executable that resolves and runs the recipes.
const PROGRAM: &str = "just";

/// Recipe bound to the verification lifecycle.
const VERIFY_RECIPE: &str = "verify";

/// Recipe bound to the in-memory formatting filter.
const FORMAT_RECIPE: &str = "format";

/// Recipe names that may bind the disk-side post-hook, in precedence order.
const POST_HOOK_RECIPES: [&str; 2] = ["post-hook", "post_hook"];

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
        } else if name == FORMAT_RECIPE {
            // The filter reads staged source on stdin and writes the rewritten
            // text on stdout, so the invocation stays non-quiet:
            // `just --quiet` silences the recipe's own output, not just the
            // echoed command line, and would discard the formatted text
            // before the filter ever received it. Echo suppression belongs to
            // the recipe's own `@`-prefixed lines.
            manifest.format = Some(command);
        } else if POST_HOOK_RECIPES.contains(&name) {
            // Both spellings name the same lifecycle stage: the first one read
            // in name order wins, so the outcome never depends on iteration.
            manifest.post_hook.get_or_insert(command);
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
        spec: ProjectCommandSpec {
            // The recipe is addressed by name, so `just` resolves its own
            // dependencies and shebang exactly as on a terminal, while the
            // declared parameter values are supplied by the caller.
            command: PROGRAM.to_string(),
            args: vec![name.to_string()],
            timeout_secs: attributes.timeout_secs(),
            // The purpose and the parameter contract live with the command
            // line, so one declaration answers what runs and what it accepts.
            description: attributes.description(recipe.get("doc")),
            parameters: parameters(recipe.get("parameters")),
        },
    })
}

/// Reads the parameter contract a recipe declares, in declaration order.
///
/// `just` binds positional arguments to parameters in the order they are
/// declared, so the published order is the declaration order. Only a parameter
/// that binds exactly one value joins the contract: a variadic parameter (`*`
/// or `+`) consumes a whole argument tail, so no single named attribute could
/// ever fill its slot.
fn parameters(items: Option<&Value>) -> Vec<ProjectParameterSpec> {
    items
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(parameter_spec).collect())
        .unwrap_or_default()
}

/// Reads one declared parameter, or `None` when it is not addressable by name.
fn parameter_spec(item: &Value) -> Option<ProjectParameterSpec> {
    let parameter = item.as_object()?;
    if binds_argument_tail(parameter.get("kind")) {
        return None;
    }
    let name = parameter.get("name").map(scalar)?;
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    Some(ProjectParameterSpec {
        name: name.to_string(),
        default: parameter
            .get("default")
            .map(scalar)
            .and_then(|value| default_value(&value)),
        // The dump publishes no per-parameter documentation, so a parameter
        // explains itself by its name and its default alone.
        doc: None,
    })
}

/// Reports whether a parameter consumes a variable number of arguments.
///
/// `just` spells the kind as `singular`, `star`, or `plus`, and a dump that
/// omits the field describes a singular parameter, so an absent kind stays
/// addressable.
fn binds_argument_tail(kind: Option<&Value>) -> bool {
    let kind = kind.map(scalar).unwrap_or_default();
    matches!(
        kind.trim().to_ascii_lowercase().as_str(),
        "star" | "plus" | "*" | "+"
    )
}

/// Reads the default value of a parameter.
///
/// `just` publishes a default as the source expression that produced it, so a
/// string literal arrives quoted while a numeric or boolean literal arrives as
/// written. The value the recipe would receive is the literal content, so the
/// surrounding quotes are removed; every other expression keeps its source
/// form, and a default that declares nothing stays absent.
fn default_value(expression: &str) -> Option<String> {
    let trimmed = expression.trim();
    let unquoted = trimmed
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .or_else(|| {
            trimmed
                .strip_prefix('\'')
                .and_then(|rest| rest.strip_suffix('\''))
        })
        .unwrap_or(trimmed)
        .trim();
    if unquoted.is_empty() {
        None
    } else {
        Some(unquoted.to_string())
    }
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
    fn description(&self, doc_comment: Option<&Value>) -> Option<String> {
        self.docs
            .iter()
            .map(|doc| doc.trim())
            .find(|doc| !doc.is_empty())
            .map(str::to_string)
            .or_else(|| {
                doc_comment
                    .and_then(Value::as_str)
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
        Value::Array(items) => match items.as_slice() {
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
            check.spec.description.as_deref(),
            Some("Type-check the workspace")
        );
        assert_eq!(check.spec.command, "just");
        assert_eq!(check.spec.args, ["check"]);
        assert_eq!(check.spec.timeout_secs, Some(600));
        assert!(check.spec.parameters.is_empty());

        let test = &manifest.commands[1];
        assert_eq!(test.name, "test");
        assert_eq!(test.spec.description.as_deref(), Some("Run the test suite"));
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
    fn test_format_recipe_fills_the_filter() {
        let manifest = manifest(json!({
            "format": {
                "attributes": [
                    { "group": "tauqe" },
                    { "doc": "Rewrite staged source" }
                ]
            },
            "check": { "attributes": [{ "group": "tauqe" }] }
        }));

        let format = manifest
            .format
            .as_ref()
            .expect("the format recipe is published");
        assert_eq!(format.name, "format");
        assert_eq!(format.spec.command, "just");
        // The invocation stays non-quiet: `just --quiet` silences the
        // recipe's own output, so the formatted text would never reach the
        // filter. The recipe suppresses its own echo through `@`-prefixed
        // lines.
        assert_eq!(format.spec.args, ["format"]);
        assert_eq!(
            format.spec.description.as_deref(),
            Some("Rewrite staged source")
        );

        // A lifecycle recipe is never published as a custom tool.
        assert_eq!(names(&manifest), ["check"]);
    }

    #[test]
    fn test_post_hook_recipe_fills_the_hook() {
        for recipe in POST_HOOK_RECIPES {
            let mut recipes = serde_json::Map::new();
            recipes.insert(
                recipe.to_string(),
                json!({ "attributes": [{ "group": "tauqe" }] }),
            );
            let manifest = manifest(serde_json::Value::Object(recipes));

            let hook = manifest
                .post_hook
                .as_ref()
                .expect("the post-hook recipe is published");
            assert_eq!(hook.name, recipe);
            assert_eq!(hook.spec.command, "just");
            assert_eq!(hook.spec.args, [recipe]);
            assert!(manifest.commands.is_empty());
        }
    }

    #[test]
    fn test_the_preferred_post_hook_name_wins() {
        let manifest = manifest(json!({
            "post-hook": { "attributes": [{ "group": "tauqe" }] },
            "post_hook": { "attributes": [{ "group": "tauqe" }] }
        }));

        let hook = manifest
            .post_hook
            .expect("one post-hook recipe is published");
        assert_eq!(hook.name, "post-hook");
        assert!(manifest.commands.is_empty());
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
    fn test_recipe_parameters_become_the_published_contract() {
        let manifest = manifest(json!({
            "log-tail": {
                "attributes": [
                    { "group": "tauqe" },
                    { "doc": "Print the last N lines of the server journal" }
                ],
                "parameters": [
                    { "name": "n", "kind": "singular", "default": "\"50\"" },
                    { "name": "follow", "kind": "singular" }
                ]
            }
        }));

        let command = &manifest.commands[0];
        assert_eq!(command.name, "log-tail");
        assert_eq!(command.spec.args, ["log-tail"]);
        assert_eq!(
            command.spec.description.as_deref(),
            Some("Print the last N lines of the server journal")
        );

        // Parameters keep their declaration order, because `just` binds
        // positional arguments in exactly that order.
        let published: Vec<(&str, Option<&str>)> = command
            .spec
            .parameters
            .iter()
            .map(|parameter| (parameter.name.as_str(), parameter.default.as_deref()))
            .collect();
        assert_eq!(published, [("n", Some("50")), ("follow", None)]);
        assert!(command
            .spec
            .parameters
            .iter()
            .all(|parameter| parameter.doc.is_none()));
    }

    #[test]
    fn test_variadic_parameters_are_not_published() {
        let manifest = manifest(json!({
            "docs": {
                "attributes": [{ "group": "tauqe" }],
                "parameters": [
                    { "name": "page", "kind": "singular" },
                    { "name": "rest", "kind": "star", "default": "\"\"" },
                    { "name": "more", "kind": "plus" }
                ]
            }
        }));

        let names: Vec<&str> = manifest.commands[0]
            .spec
            .parameters
            .iter()
            .map(|parameter| parameter.name.as_str())
            .collect();
        assert_eq!(names, ["page"]);
    }

    #[test]
    fn test_parameter_defaults_are_read_as_values() {
        let manifest = manifest(json!({
            "run": {
                "attributes": [{ "group": "tauqe" }],
                "parameters": [
                    { "name": "quoted", "kind": "singular", "default": "\"a b\"" },
                    { "name": "single", "kind": "singular", "default": "'x'" },
                    { "name": "number", "kind": "singular", "default": 10 },
                    { "name": "flag", "kind": "singular", "default": true },
                    { "name": "empty", "kind": "singular", "default": "\"\"" },
                    { "name": "absent", "kind": "singular" }
                ]
            }
        }));

        let defaults: Vec<Option<&str>> = manifest.commands[0]
            .spec
            .parameters
            .iter()
            .map(|parameter| parameter.default.as_deref())
            .collect();
        assert_eq!(
            defaults,
            [Some("a b"), Some("x"), Some("10"), Some("true"), None, None]
        );
    }

    #[test]
    fn test_unusable_parameter_declarations_are_skipped() {
        let manifest = manifest(json!({
            "hidden": {
                "attributes": [{ "group": "tauqe" }],
                "parameters": [{ "kind": "singular" }, "n", 7]
            }
        }));

        assert!(manifest.commands[0].spec.parameters.is_empty());
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
