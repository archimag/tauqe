//! Declarative project command configuration (`[project]`).
//!
//! `tauqe.toml` is the single configuration surface of the harness, so the
//! commands a repository knows how to run are declared there as well:
//!
//! - `verify` is the mandatory verification command. A change set the harness
//!   cannot verify is never accepted.
//! - `format` is the optional in-memory filter that rewrites staged content
//!   through standard input and output before any byte reaches disk.
//! - `post_hook` is the optional disk-side step that runs strictly after the
//!   change set is written and before verification.
//! - `commands` is the named allowlist of project commands the harness can
//!   publish to a model as typed custom tools.
//!
//! Every entry is a plain command line executed as a process with literal
//! arguments, never through a shell, so a declared command cannot widen into
//! arbitrary command execution. An entry may reuse another declared command by
//! writing `@<name>` as its executable. The reference chain is validated here,
//! so a missing target or a cycle is rejected before anything runs.
//!
//! An entry also declares the contract of the command it runs: a human-readable
//! `description` and the named `parameters` callers may supply. Both travel with
//! the resolved command line, so the harness renders the signature a command
//! actually accepts instead of letting a caller guess it.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};

/// Prefix that turns an executable into a reference to a declared command.
const REFERENCE_PREFIX: char = '@';

/// External task runner a project delegates its commands to.
///
/// The runner is declared explicitly and never detected: a project either opts
/// into one external runner or relies solely on the commands declared in this
/// section. Detection would let a stray manifest on disk change what the
/// harness executes, so the declaration is the single source of that decision.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskRunnerKind {
    /// No external runner; only the commands declared in this section run.
    #[default]
    None,
    /// Recipes exported by a Just manifest provide the project commands.
    Just,
}

impl TaskRunnerKind {
    /// Reports whether the project declares no external runner.
    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }
}

impl fmt::Display for TaskRunnerKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::None => "none",
            Self::Just => "just",
        })
    }
}

/// One named parameter a project command accepts.
///
/// Parameters are the typed contract of a command: a declared name lets a
/// caller supply a value without widening the command line into free-form
/// arguments, and the optional default removes the need to supply anything at
/// all. `doc` carries the author's description, so the harness can publish the
/// signature instead of leaving a caller to guess it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectParameterSpec {
    /// Name callers address the parameter by.
    pub name: String,
    /// Value used when a caller supplies nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// Human-readable description of the parameter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
}

/// One command the harness runs as a process with literal arguments.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ProjectCommandSpec {
    /// Executable to run, or `@<name>` referring to `[project.commands.<name>]`.
    pub command: String,
    /// Literal arguments passed to the executable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Wall-clock budget of a single invocation, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// Human-readable purpose of the command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Named parameters a caller may supply, in declaration order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parameters: Vec<ProjectParameterSpec>,
}

/// A fully resolved command line, ready to be spawned without a shell.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ResolvedProjectCommand {
    /// Executable to run.
    pub command: String,
    /// Literal arguments passed to the executable.
    #[serde(default)]
    pub args: Vec<String>,
    /// Wall-clock budget of a single invocation, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// Human-readable purpose of the command, carried from the declaration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Named parameters the command accepts, carried from the declaration.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parameters: Vec<ProjectParameterSpec>,
}

/// Rejection reason of an invalid `[project]` declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectConfigError {
    /// The mandatory `verify` command is absent.
    MissingVerify,
    /// An entry names neither an executable nor a declared command.
    BlankCommand { operation: String },
    /// A declared command name is empty or unusable.
    InvalidName { operation: String, name: String },
    /// An entry refers to a command that is not declared.
    UnknownCommand { operation: String, name: String },
    /// A reference chain returns to a command it already passed through.
    CyclicCommand { operation: String, name: String },
    /// An entry declares an argument that cannot reach a process.
    InvalidArgument { operation: String },
    /// A parameter carries a name a caller could not address.
    InvalidParameterName { operation: String, name: String },
    /// A command declares the same parameter more than once.
    DuplicateParameter { operation: String, name: String },
    /// An entry refers to a command while redeclaring what that command owns.
    InvalidReference { operation: String },
}

impl fmt::Display for ProjectConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingVerify => f.write_str("the section must declare a `verify` command"),
            Self::BlankCommand { operation } => {
                write!(f, "`{operation}` names neither an executable nor a command")
            }
            Self::InvalidName { operation, name } => {
                write!(
                    f,
                    "`{operation}` declares an unusable command name `{name}`"
                )
            }
            Self::UnknownCommand { operation, name } => {
                write!(f, "`{operation}` refers to an undeclared command `{name}`")
            }
            Self::CyclicCommand { operation, name } => {
                write!(
                    f,
                    "`{operation}` reaches a reference cycle through `{name}`"
                )
            }
            Self::InvalidArgument { operation } => {
                write!(
                    f,
                    "`{operation}` declares an argument that cannot reach a process"
                )
            }
            Self::InvalidParameterName { operation, name } => {
                write!(
                    f,
                    "`{operation}` declares an unusable parameter name `{name}`"
                )
            }
            Self::DuplicateParameter { operation, name } => {
                write!(
                    f,
                    "`{operation}` declares the parameter `{name}` more than once"
                )
            }
            Self::InvalidReference { operation } => {
                write!(
                    f,
                    "`{operation}` refers to a declared command and must not redeclare \
                     its arguments, description, or parameters"
                )
            }
        }
    }
}

impl std::error::Error for ProjectConfigError {}

/// The `[project]` section of `tauqe.toml`.
///
/// Every field is optional on the wire so a partially authored section still
/// decodes, but [`ProjectConfig::validate`] rejects a declaration that leaves
/// verification undefined.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ProjectConfig {
    /// External task runner providing project commands, when one is declared.
    #[serde(default, skip_serializing_if = "TaskRunnerKind::is_none")]
    pub runner: TaskRunnerKind,
    /// Mandatory verification command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify: Option<ProjectCommandSpec>,
    /// Optional in-memory formatting filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<ProjectCommandSpec>,
    /// Optional disk-side post-hook.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_hook: Option<ProjectCommandSpec>,
    /// Named project commands published as typed custom tools.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub commands: BTreeMap<String, ProjectCommandSpec>,
}

impl ProjectConfig {
    /// Reports whether the section declares nothing at all.
    pub fn is_empty(&self) -> bool {
        self.runner.is_none()
            && self.verify.is_none()
            && self.format.is_none()
            && self.post_hook.is_none()
            && self.commands.is_empty()
    }

    /// Resolves the mandatory verification command.
    pub fn verify_command(&self) -> Result<ResolvedProjectCommand, ProjectConfigError> {
        let spec = self
            .verify
            .as_ref()
            .ok_or(ProjectConfigError::MissingVerify)?;
        self.resolve("verify", spec)
    }

    /// Resolves `spec` to the concrete command line that will be executed.
    ///
    /// An entry that names an executable is its own resolution. An entry that
    /// refers to a declared command is followed to the end of the chain, so the
    /// caller always receives a concrete command line. A blank executable, an
    /// unusable reference, an undeclared target, and a cycle are rejected
    /// instead of being run or silently ignored. The description and the
    /// parameter contract travel with the command line, so a caller learns what
    /// runs and which values it accepts from one place. A reference may also
    /// declare `timeout_secs`, which bounds one invocation rather than defining
    /// the command line it names: the outermost declared budget overrides the
    /// target's, and the target's budget is the fallback.
    pub fn resolve(
        &self,
        operation: &str,
        spec: &ProjectCommandSpec,
    ) -> Result<ResolvedProjectCommand, ProjectConfigError> {
        let mut visited: BTreeSet<String> = BTreeSet::new();
        let mut current = spec;
        // The budget bounds one invocation, so it is not part of the command line
        // the referred command owns. The outermost declaration wins, and the
        // terminal target's budget stays the fallback.
        let mut timeout_override: Option<u64> = None;

        loop {
            let command = current.command.trim();
            if command.is_empty() {
                return Err(ProjectConfigError::BlankCommand {
                    operation: operation.to_string(),
                });
            }

            let Some(target) = command.strip_prefix(REFERENCE_PREFIX) else {
                let mut resolved = check_executable(operation, current)?;
                if let Some(timeout) = timeout_override {
                    resolved.timeout_secs = Some(timeout);
                }
                return Ok(resolved);
            };

            let target = target.trim();
            if target.is_empty() {
                return Err(ProjectConfigError::InvalidName {
                    operation: operation.to_string(),
                    name: command.to_string(),
                });
            }
            if !current.args.is_empty()
                || current.description.is_some()
                || !current.parameters.is_empty()
            {
                // The referenced command owns its command line and its contract.
                // A reference only names it, so redeclaring either here would
                // silently drop or contradict what actually runs.
                return Err(ProjectConfigError::InvalidReference {
                    operation: operation.to_string(),
                });
            }
            // The entry now names a command, so a budget declared here is the
            // outermost one the chain carries.
            timeout_override = timeout_override.or(current.timeout_secs);
            if !visited.insert(target.to_string()) {
                return Err(ProjectConfigError::CyclicCommand {
                    operation: operation.to_string(),
                    name: target.to_string(),
                });
            }
            current =
                self.commands
                    .get(target)
                    .ok_or_else(|| ProjectConfigError::UnknownCommand {
                        operation: operation.to_string(),
                        name: target.to_string(),
                    })?;
        }
    }

    /// Validates the whole declaration.
    ///
    /// Verification is mandatory, and every declared entry must resolve to a
    /// runnable command line with an addressable parameter contract, so an
    /// invalid declaration is rejected as a whole instead of degrading into an
    /// unverified turn.
    pub fn validate(&self) -> Result<(), ProjectConfigError> {
        if self.runner.is_none() {
            self.verify_command()?;
        } else if let Some(verify) = &self.verify {
            self.resolve("verify", verify)?;
        }

        if let Some(format) = &self.format {
            self.resolve("format", format)?;
        }
        if let Some(post_hook) = &self.post_hook {
            self.resolve("post_hook", post_hook)?;
        }

        for (name, spec) in &self.commands {
            if name.trim().is_empty() {
                return Err(ProjectConfigError::InvalidName {
                    operation: "commands".to_string(),
                    name: name.clone(),
                });
            }
            self.resolve(&format!("commands.{name}"), spec)?;
        }

        Ok(())
    }
}

/// Rejects an executable, an argument, or a parameter contract that cannot
/// reach a process intact.
fn check_executable(
    operation: &str,
    spec: &ProjectCommandSpec,
) -> Result<ResolvedProjectCommand, ProjectConfigError> {
    if spec.command.contains('\0') || spec.args.iter().any(|arg| arg.contains('\0')) {
        return Err(ProjectConfigError::InvalidArgument {
            operation: operation.to_string(),
        });
    }
    validate_parameters(operation, &spec.parameters)?;

    Ok(ResolvedProjectCommand {
        command: spec.command.trim().to_string(),
        args: spec.args.clone(),
        timeout_secs: spec.timeout_secs,
        description: spec
            .description
            .as_deref()
            .map(str::trim)
            .filter(|description| !description.is_empty())
            .map(str::to_string),
        parameters: spec.parameters.clone(),
    })
}

/// Rejects a parameter contract a caller could not address unambiguously.
///
/// A name must be usable as an XML attribute name, because that is exactly how a
/// caller supplies a value, so it starts with a letter or an underscore and
/// continues with letters, digits, dashes, or underscores. A default value must
/// survive the trip to a process, and a duplicated name would bind one value to
/// two parameters, so both are rejected instead of being repaired silently.
fn validate_parameters(
    operation: &str,
    parameters: &[ProjectParameterSpec],
) -> Result<(), ProjectConfigError> {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for parameter in parameters {
        let name = parameter.name.trim();
        if !is_valid_parameter_name(name) {
            return Err(ProjectConfigError::InvalidParameterName {
                operation: operation.to_string(),
                name: parameter.name.clone(),
            });
        }
        if parameter
            .default
            .as_deref()
            .is_some_and(|default| default.contains('\0'))
        {
            return Err(ProjectConfigError::InvalidArgument {
                operation: operation.to_string(),
            });
        }
        if !seen.insert(name) {
            return Err(ProjectConfigError::DuplicateParameter {
                operation: operation.to_string(),
                name: name.to_string(),
            });
        }
    }
    Ok(())
}

/// Reports whether `name` is an addressable parameter name.
fn is_valid_parameter_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses a `[project]` declaration from its JSON form.
    ///
    /// The model is serde-generic, so the JSON form pins the same shape the TOML
    /// reader produces without depending on a TOML parser in this leaf crate.
    fn parse(source: &str) -> ProjectConfig {
        serde_json::from_str(source).expect("the [project] section parses")
    }

    #[test]
    fn test_project_config_parses_declared_entries() {
        let config = parse(
            r#"{
                "verify": { "command": "cargo", "args": ["test"], "timeout_secs": 600 },
                "format": { "command": "rustfmt" },
                "commands": { "docs": { "command": "cargo", "args": ["doc"] } }
            }"#,
        );

        let verify = config.verify.as_ref().expect("verify is declared");
        assert_eq!(verify.command, "cargo");
        assert_eq!(verify.args, ["test"]);
        assert_eq!(verify.timeout_secs, Some(600));
        assert!(!config.is_empty());
        assert!(config.post_hook.is_none());
        assert_eq!(config.commands.len(), 1);
        assert!(config.validate().is_ok());

        let encoded = serde_json::to_string(&config).expect("the section encodes");
        assert_eq!(parse(&encoded), config);
    }

    #[test]
    fn test_project_config_parses_description_and_parameters() {
        let config = parse(
            r#"{
                "commands": {
                    "log-tail": {
                        "command": "just",
                        "args": ["log-tail"],
                        "description": "Print the last N lines of the server journal.",
                        "parameters": [
                            { "name": "n", "default": "50", "doc": "Line count" },
                            { "name": "follow" }
                        ]
                    }
                }
            }"#,
        );

        let declared = config
            .commands
            .get("log-tail")
            .expect("the command is declared");
        assert_eq!(
            declared.description.as_deref(),
            Some("Print the last N lines of the server journal.")
        );
        assert_eq!(declared.parameters.len(), 2);
        assert_eq!(declared.parameters[0].name, "n");
        assert_eq!(declared.parameters[0].default.as_deref(), Some("50"));
        assert_eq!(declared.parameters[0].doc.as_deref(), Some("Line count"));
        assert!(declared.parameters[1].default.is_none());
        assert!(declared.parameters[1].doc.is_none());

        // The resolved command line carries the same contract to its caller.
        let resolved = config
            .resolve("commands.log-tail", declared)
            .expect("the command resolves");
        assert_eq!(resolved.command, "just");
        assert_eq!(resolved.args, ["log-tail"]);
        assert_eq!(resolved.description, declared.description);
        assert_eq!(resolved.parameters, declared.parameters);

        // Absent optional fields stay absent on the wire.
        let encoded = serde_json::to_string(&config).expect("the section encodes");
        assert!(!encoded.contains("null"), "unexpected null: {encoded}");
        assert_eq!(parse(&encoded), config);
    }

    #[test]
    fn test_project_config_requires_verification() {
        let config = ProjectConfig::default();
        assert!(config.is_empty());
        assert_eq!(config.validate(), Err(ProjectConfigError::MissingVerify));
        assert_eq!(
            config.verify_command(),
            Err(ProjectConfigError::MissingVerify)
        );

        // An empty executable is not a verification command either.
        let blank = parse(r#"{"verify":{"command":"   "}}"#);
        assert!(matches!(
            blank.validate(),
            Err(ProjectConfigError::BlankCommand { .. })
        ));
    }

    #[test]
    fn test_project_config_resolves_reference_chain() {
        let config = parse(
            r#"{
                "verify": { "command": "@ci" },
                "commands": {
                    "ci": { "command": "@check" },
                    "check": { "command": "cargo", "args": ["check", "--all-targets"] }
                }
            }"#,
        );

        assert!(config.validate().is_ok());
        let resolved = config.verify_command().expect("verify resolves");
        assert_eq!(resolved.command, "cargo");
        assert_eq!(resolved.args, ["check", "--all-targets"]);
    }

    #[test]
    fn test_project_config_rejects_reference_cycles() {
        let indirect = parse(
            r#"{
                "verify": { "command": "@a" },
                "commands": { "a": { "command": "@b" }, "b": { "command": "@a" } }
            }"#,
        );
        assert!(matches!(
            indirect.validate(),
            Err(ProjectConfigError::CyclicCommand { .. })
        ));

        let direct = parse(
            r#"{
                "verify": { "command": "cargo", "args": ["test"] },
                "commands": { "self": { "command": "@self" } }
            }"#,
        );
        assert!(matches!(
            direct.validate(),
            Err(ProjectConfigError::CyclicCommand { .. })
        ));
    }

    #[test]
    fn test_project_config_rejects_broken_entries() {
        let unknown = parse(r#"{"verify":{"command":"@missing"}}"#);
        assert!(matches!(
            unknown.validate(),
            Err(ProjectConfigError::UnknownCommand { .. })
        ));

        let nameless = parse(r#"{"verify":{"command":"@"}}"#);
        assert!(matches!(
            nameless.validate(),
            Err(ProjectConfigError::InvalidName { .. })
        ));

        let nul_argument = parse(r#"{"verify":{"command":"cargo","args":["te\u0000st"]}}"#);
        assert!(matches!(
            nul_argument.validate(),
            Err(ProjectConfigError::InvalidArgument { .. })
        ));

        let unnamed_command =
            parse(r#"{"verify":{"command":"cargo"},"commands":{"  ":{"command":"make"}}}"#);
        assert!(matches!(
            unnamed_command.validate(),
            Err(ProjectConfigError::InvalidName { .. })
        ));
    }

    #[test]
    fn test_a_reference_does_not_redeclare_the_target() {
        // A reference naming a command with a parameter contract is valid: the
        // target owns both the command line and the contract.
        let chained = parse(
            r#"{
                "verify": { "command": "@ci" },
                "commands": {
                    "ci": {
                        "command": "just",
                        "args": ["check"],
                        "description": "Type-check the workspace",
                        "parameters": [{ "name": "n" }]
                    }
                }
            }"#,
        );
        assert!(chained.validate().is_ok());
        let resolved = chained.verify_command().expect("verify resolves");
        assert_eq!(resolved.command, "just");
        assert_eq!(
            resolved.description.as_deref(),
            Some("Type-check the workspace")
        );
        assert_eq!(resolved.parameters.len(), 1);

        // Redeclaring what the target owns would silently drop it.
        for redeclaration in [
            r#"{"command":"@ci","args":["--fast"]}"#,
            r#"{"command":"@ci","description":"alias"}"#,
            r#"{"command":"@ci","parameters":[{"name":"n"}]}"#,
        ] {
            let config = parse(&format!(
                r#"{{"verify":{redeclaration},"commands":{{"ci":{{"command":"cargo"}}}}}}"#
            ));
            assert!(
                matches!(
                    config.validate(),
                    Err(ProjectConfigError::InvalidReference { .. })
                ),
                "redeclaration {redeclaration} was accepted"
            );
        }
    }

    #[test]
    fn test_a_reference_timeout_bounds_the_invocation() {
        // A reference declares only the budget of one invocation, so it overrides
        // the budget of the command it names.
        let overriding = parse(
            r#"{
                "verify": { "command": "@ci", "timeout_secs": 30 },
                "commands": {
                    "ci": {
                        "command": "cargo",
                        "args": ["test"],
                        "timeout_secs": 600
                    }
                }
            }"#,
        );
        assert!(overriding.validate().is_ok());
        let resolved = overriding.verify_command().expect("verify resolves");
        assert_eq!(resolved.command, "cargo");
        assert_eq!(resolved.args, ["test"]);
        assert_eq!(resolved.timeout_secs, Some(30));

        // A reference that declares no budget inherits the target's budget.
        let inherited = parse(
            r#"{
                "verify": { "command": "@ci" },
                "commands": { "ci": { "command": "cargo", "timeout_secs": 600 } }
            }"#,
        );
        assert!(inherited.validate().is_ok());
        let resolved = inherited.verify_command().expect("verify resolves");
        assert_eq!(resolved.timeout_secs, Some(600));

        // The outermost declaration wins along a chain of references.
        let outermost = parse(
            r#"{
                "verify": { "command": "@outer", "timeout_secs": 30 },
                "commands": {
                    "outer": { "command": "@inner" },
                    "inner": { "command": "cargo", "timeout_secs": 600 }
                }
            }"#,
        );
        assert!(outermost.validate().is_ok());
        let resolved = outermost.verify_command().expect("verify resolves");
        assert_eq!(resolved.command, "cargo");
        assert_eq!(resolved.timeout_secs, Some(30));

        // A budget declared in the middle of a chain still applies.
        let middle = parse(
            r#"{
                "verify": { "command": "@outer" },
                "commands": {
                    "outer": { "command": "@inner", "timeout_secs": 45 },
                    "inner": { "command": "cargo", "timeout_secs": 600 }
                }
            }"#,
        );
        assert!(middle.validate().is_ok());
        let resolved = middle.verify_command().expect("verify resolves");
        assert_eq!(resolved.timeout_secs, Some(45));
    }

    #[test]
    fn test_parameter_names_are_validated() {
        let accepted = parse(
            r#"{
                "verify": { "command": "cargo" },
                "commands": {
                    "docs": {
                        "command": "just",
                        "args": ["docs"],
                        "parameters": [
                            { "name": "_n" },
                            { "name": "dry-run" },
                            { "name": "level2" }
                        ]
                    }
                }
            }"#,
        );
        assert!(accepted.validate().is_ok());

        for name in ["", "  ", "1n", "with space", "n=1", "dotted.name"] {
            let config = parse(&format!(
                r#"{{"verify":{{"command":"cargo"}},"commands":{{"docs":{{"command":"just","parameters":[{{"name":"{name}"}}]}}}}}}"#
            ));
            assert!(
                matches!(
                    config.validate(),
                    Err(ProjectConfigError::InvalidParameterName { .. })
                ),
                "parameter name {name:?} was accepted"
            );
        }
    }

    #[test]
    fn test_duplicate_and_unusable_parameter_values_are_rejected() {
        let duplicated = parse(
            r#"{
                "verify": { "command": "cargo" },
                "commands": {
                    "docs": {
                        "command": "just",
                        "parameters": [{ "name": "n" }, { "name": " n " }]
                    }
                }
            }"#,
        );
        assert!(matches!(
            duplicated.validate(),
            Err(ProjectConfigError::DuplicateParameter { .. })
        ));

        let nul_default = parse(
            r#"{
                "verify": { "command": "cargo" },
                "commands": {
                    "docs": {
                        "command": "just",
                        "parameters": [{ "name": "n", "default": "a\u0000b" }]
                    }
                }
            }"#,
        );
        assert!(matches!(
            nul_default.validate(),
            Err(ProjectConfigError::InvalidArgument { .. })
        ));
    }

    #[test]
    fn test_task_runner_kind_is_declared_explicitly() {
        assert_eq!(TaskRunnerKind::default(), TaskRunnerKind::None);
        assert!(TaskRunnerKind::default().is_none());

        // An omitted runner means no external runner at all.
        let absent = parse(r#"{"verify":{"command":"cargo"}}"#);
        assert_eq!(absent.runner, TaskRunnerKind::None);

        let just = parse(r#"{"verify":{"command":"cargo"},"runner":"just"}"#);
        assert_eq!(just.runner, TaskRunnerKind::Just);
        assert!(!just.runner.is_none());
        assert!(just.validate().is_ok());

        // Only the two declared runners are accepted: there is no `auto`.
        assert!(serde_json::from_str::<ProjectConfig>(r#"{"runner":"auto"}"#).is_err());

        // A declared runner can provide the verification lifecycle.
        let runner_only = parse(r#"{"runner":"just"}"#);
        assert!(!runner_only.is_empty());
        assert!(runner_only.validate().is_ok());

        // An absent runner stays absent in the encoding.
        let encoded = serde_json::to_string(&absent).expect("the section encodes");
        assert!(!encoded.contains("runner"));
        assert_eq!(parse(&encoded), absent);
        assert_eq!(
            serde_json::to_string(&TaskRunnerKind::Just).expect("the runner encodes"),
            "\"just\""
        );
        assert_eq!(TaskRunnerKind::Just.to_string(), "just");
        assert_eq!(TaskRunnerKind::None.to_string(), "none");
    }

    #[test]
    fn test_project_config_error_display_names_the_operation() {
        assert!(ProjectConfigError::MissingVerify
            .to_string()
            .contains("verify"));

        let undefined = ProjectConfigError::UnknownCommand {
            operation: "verify".to_string(),
            name: "ci".to_string(),
        };
        let rendered = undefined.to_string();
        assert!(rendered.contains("verify"));
        assert!(rendered.contains("ci"));

        let unusable = ProjectConfigError::InvalidParameterName {
            operation: "commands.log-tail".to_string(),
            name: "1n".to_string(),
        };
        let rendered = unusable.to_string();
        assert!(rendered.contains("commands.log-tail"));
        assert!(rendered.contains("1n"));

        let duplicated = ProjectConfigError::DuplicateParameter {
            operation: "commands.docs".to_string(),
            name: "n".to_string(),
        };
        assert!(duplicated.to_string().contains("commands.docs"));

        let reference = ProjectConfigError::InvalidReference {
            operation: "verify".to_string(),
        };
        assert!(reference.to_string().contains("verify"));
    }
}
