use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Default wall-clock timeout for a single toolchain verification command.
///
/// Real projects routinely exceed one minute for `cargo test` or a full lint
/// pass, so the default budget is generous and overridable through
/// `[toolchain] timeout_secs`.
pub const DEFAULT_TOOLCHAIN_TIMEOUT_SECS: u64 = 600;

/// Resolves an optional command together with its literal arguments.
///
/// A missing or blank command resolves to `None`, so a declared but empty
/// section degrades to deterministic auto-detection instead of spawning an
/// empty program.
fn command_line(command: Option<&str>, args: &[String]) -> Option<(String, Vec<String>)> {
    command
        .map(str::trim)
        .filter(|program| !program.is_empty())
        .map(|program| (program.to_string(), args.to_vec()))
}

/// Declarative settings for one toolchain phase: `[toolchain.check]`,
/// `[toolchain.lint]`, or `[toolchain.test]`.
///
/// A phase either names an explicit `command` with literal `args`, or leaves
/// the command unset so the harness keeps deterministic auto-detection.
/// `enabled` switches a phase off without deleting its configuration, and
/// `timeout_secs` overrides the global `[toolchain] timeout_secs` budget for
/// that phase alone.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolchainPhaseConfig {
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

impl ToolchainPhaseConfig {
    /// Program and arguments to execute, when the phase names a command.
    pub fn command_line(&self) -> Option<(String, Vec<String>)> {
        command_line(self.command.as_deref(), &self.args)
    }

    /// Whether the phase may run; an unset flag means enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }

    /// Phase-local timeout override, ignoring a non-positive value.
    pub fn timeout_override_secs(&self) -> Option<u64> {
        self.timeout_secs.filter(|secs| *secs > 0)
    }
}

/// One named entry of the formatter registry, `[toolchain.fmt.<name>]`.
///
/// `extensions` is the mandatory selection filter: a formatter takes part only
/// for changed files whose extension it lists, so a monorepo registers one
/// formatter per language without touching unrelated files. An entry that names
/// no command or lists no extension therefore matches nothing.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FormatterConfig {
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub extensions: Vec<String>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

impl FormatterConfig {
    /// Program and arguments to execute, when the formatter names a command.
    pub fn command_line(&self) -> Option<(String, Vec<String>)> {
        command_line(self.command.as_deref(), &self.args)
    }

    /// Whether the entry can run: it names a command and at least one extension.
    pub fn is_usable(&self) -> bool {
        self.command_line().is_some() && !self.extensions.is_empty()
    }

    /// Whether the formatter applies to `path` by its file extension.
    ///
    /// Matching ignores case and a leading dot, so `rs`, `.rs`, and `RS` all
    /// select Rust sources.
    pub fn matches_path(&self, path: &Path) -> bool {
        if !self.is_usable() {
            return false;
        }
        let Some(extension) = path.extension().and_then(|value| value.to_str()) else {
            return false;
        };
        let extension = extension.to_ascii_lowercase();
        self.extensions.iter().any(|candidate| {
            candidate
                .trim()
                .trim_start_matches('.')
                .to_ascii_lowercase()
                == extension
        })
    }

    /// Effective timeout for one formatter invocation.
    pub fn effective_timeout_secs(&self) -> u64 {
        self.timeout_secs
            .filter(|secs| *secs > 0)
            .unwrap_or(DEFAULT_TOOLCHAIN_TIMEOUT_SECS)
    }
}

/// Declarative project toolchain configuration.
///
/// The flat legacy fields (`check_command`, `timeout_secs`, `target`) keep
/// existing files working; the typed phase sections and the formatter registry
/// extend them without changing their meaning.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolchainConfig {
    /// Legacy custom verification command, used when `[toolchain.check]`
    /// declares no command of its own.
    #[serde(default)]
    pub check_command: Option<String>,
    #[serde(default)]
    pub max_retries: Option<usize>,
    #[serde(default)]
    pub auto_heal: Option<bool>,
    /// Global wall-clock budget for one verification command.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub auto_verify: Option<bool>,
    #[serde(default, alias = "default_target")]
    pub target: Option<String>,
    /// Typed `[toolchain.check]` section.
    #[serde(default)]
    pub check: Option<ToolchainPhaseConfig>,
    /// Typed `[toolchain.lint]` section.
    #[serde(default)]
    pub lint: Option<ToolchainPhaseConfig>,
    /// Typed `[toolchain.test]` section.
    #[serde(default)]
    pub test: Option<ToolchainPhaseConfig>,
    /// Named formatters registered under `[toolchain.fmt.<name>]`.
    #[serde(default)]
    pub fmt: BTreeMap<String, FormatterConfig>,
}

impl ToolchainConfig {
    /// Effective per-command timeout in seconds.
    ///
    /// Falls back to [`DEFAULT_TOOLCHAIN_TIMEOUT_SECS`] when unset or set to a
    /// non-positive value, so malformed configuration degrades to a sane budget
    /// instead of aborting or blocking indefinitely.
    pub fn effective_timeout_secs(&self) -> u64 {
        self.phase_timeout_secs(None)
    }

    /// Effective timeout for `phase`.
    ///
    /// A phase-local override wins; otherwise the legacy global budget applies,
    /// and finally the built-in default.
    pub fn phase_timeout_secs(&self, phase: Option<&ToolchainPhaseConfig>) -> u64 {
        phase
            .and_then(ToolchainPhaseConfig::timeout_override_secs)
            .or_else(|| self.timeout_secs.filter(|secs| *secs > 0))
            .unwrap_or(DEFAULT_TOOLCHAIN_TIMEOUT_SECS)
    }

    /// Executable and arguments for the check phase.
    ///
    /// The typed `[toolchain.check]` section wins; when it declares no command
    /// the legacy `check_command` string is used verbatim as the executable.
    pub fn effective_check_command(&self) -> Option<(String, Vec<String>)> {
        self.check
            .as_ref()
            .and_then(ToolchainPhaseConfig::command_line)
            .or_else(|| command_line(self.check_command.as_deref(), &[]))
    }

    /// Whether toolchain verification is automatically run after edits.
    ///
    /// Defaults to `true` so invariant checks and compiler verifications execute
    /// deterministically even if the model omitted `<verify>`.
    pub fn effective_auto_verify(&self) -> bool {
        self.auto_verify.unwrap_or(true)
    }

    /// Default verification target to run when the model does not specify one.
    ///
    /// Defaults to [`tauqe_protocol::VerifyTarget::All`].
    pub fn effective_target(&self) -> tauqe_protocol::VerifyTarget {
        match self
            .target
            .as_deref()
            .map(str::trim)
            .map(str::to_lowercase)
            .as_deref()
        {
            Some("check") => tauqe_protocol::VerifyTarget::Check,
            Some("clippy") => tauqe_protocol::VerifyTarget::Clippy,
            Some("test") => tauqe_protocol::VerifyTarget::Test,
            _ => tauqe_protocol::VerifyTarget::All,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn parse(source: &str) -> ToolchainConfig {
        toml::from_str(source).expect("toolchain configuration parses")
    }

    #[test]
    fn test_toolchain_config_defaults() {
        let config = ToolchainConfig::default();
        assert!(config.effective_auto_verify());
        assert_eq!(config.effective_target(), tauqe_protocol::VerifyTarget::All);
        assert_eq!(
            config.effective_timeout_secs(),
            DEFAULT_TOOLCHAIN_TIMEOUT_SECS
        );
        assert!(config.check.is_none());
        assert!(config.lint.is_none());
        assert!(config.test.is_none());
        assert!(config.fmt.is_empty());
        assert!(config.effective_check_command().is_none());
    }

    #[test]
    fn test_toolchain_config_custom_values() {
        let config: ToolchainConfig = toml::from_str(
            r#"
            auto_verify = false
            target = "check"
            timeout_secs = 30
        "#,
        )
        .unwrap();
        assert!(!config.effective_auto_verify());
        assert_eq!(
            config.effective_target(),
            tauqe_protocol::VerifyTarget::Check
        );
        assert_eq!(config.effective_timeout_secs(), 30);
    }

    #[test]
    fn test_toolchain_config_target_aliases_and_case() {
        let config: ToolchainConfig = toml::from_str(
            r#"
            default_target = "Clippy"
        "#,
        )
        .unwrap();
        assert_eq!(
            config.effective_target(),
            tauqe_protocol::VerifyTarget::Clippy
        );
    }

    #[test]
    fn test_typed_phase_sections_parse() {
        let config = parse(
            r#"
            [check]
            command = "cargo"
            args = ["check", "--all-targets"]
            timeout_secs = 120
            enabled = true

            [lint]
            command = "cargo"
            args = ["clippy", "--", "-D", "warnings"]
            enabled = false

            [test]
            command = "cargo"
            args = ["test", "--workspace"]
            "#,
        );

        let check = config.check.as_ref().expect("check section is declared");
        assert_eq!(check.command.as_deref(), Some("cargo"));
        assert_eq!(check.args, vec!["check", "--all-targets"]);
        assert_eq!(check.timeout_override_secs(), Some(120));
        assert!(check.is_enabled());
        assert_eq!(
            check.command_line(),
            Some((
                "cargo".to_string(),
                vec!["check".to_string(), "--all-targets".to_string()]
            ))
        );

        let lint = config.lint.as_ref().expect("lint section is declared");
        assert!(!lint.is_enabled());
        assert_eq!(lint.timeout_override_secs(), None);

        let test = config.test.as_ref().expect("test section is declared");
        assert_eq!(test.args, vec!["test", "--workspace"]);
        assert_eq!(
            config.phase_timeout_secs(Some(test)),
            DEFAULT_TOOLCHAIN_TIMEOUT_SECS
        );
    }

    #[test]
    fn test_phase_timeout_falls_back_to_global_budget() {
        let config = parse(
            r#"
            timeout_secs = 900

            [lint]
            command = "ruff"
            "#,
        );
        let lint = config.lint.as_ref().expect("lint section is declared");
        assert_eq!(config.phase_timeout_secs(Some(lint)), 900);
        assert_eq!(config.effective_timeout_secs(), 900);
    }

    #[test]
    fn test_legacy_check_command_still_applies() {
        let legacy = parse(r#"check_command = "make verify""#);
        assert_eq!(
            legacy.effective_check_command(),
            Some(("make verify".to_string(), Vec::new()))
        );

        let typed = parse(
            r#"
            check_command = "make verify"

            [check]
            command = "cargo"
            args = ["check"]
            "#,
        );
        assert_eq!(
            typed.effective_check_command(),
            Some(("cargo".to_string(), vec!["check".to_string()]))
        );

        let chapter_without_command = parse(
            r#"
            check_command = "make verify"

            [check]
            args = ["--fast"]
            "#,
        );
        assert_eq!(
            chapter_without_command.effective_check_command(),
            Some(("make verify".to_string(), Vec::new()))
        );
    }

    #[test]
    fn test_formatter_registry_parses_named_entries() {
        let config = parse(
            r#"
            [fmt.rustfmt]
            command = "cargo"
            args = ["fmt", "--"]
            extensions = ["rs"]
            timeout_secs = 45

            [fmt.prettier]
            command = "npx"
            args = ["prettier", "--write"]
            extensions = ["ts", "tsx", ".js"]
            "#,
        );

        assert_eq!(config.fmt.len(), 2);

        let rustfmt = config.fmt.get("rustfmt").expect("rustfmt entry exists");
        assert_eq!(rustfmt.command.as_deref(), Some("cargo"));
        assert_eq!(rustfmt.args, vec!["fmt", "--"]);
        assert_eq!(rustfmt.effective_timeout_secs(), 45);
        assert!(rustfmt.is_usable());
        assert!(rustfmt.matches_path(Path::new("crates/core/src/lib.rs")));
        assert!(!rustfmt.matches_path(Path::new("crates/tui/src/app.tsx")));

        let prettier = config.fmt.get("prettier").expect("prettier entry exists");
        assert!(prettier.matches_path(Path::new("crates/tui/src/app.tsx")));
        assert!(prettier.matches_path(Path::new("web/App.JS")));
        assert!(!prettier.matches_path(Path::new("crates/core/src/lib.rs")));
        assert!(!prettier.matches_path(Path::new("Makefile")));
    }

    #[test]
    fn test_formatter_without_extensions_never_matches() {
        let config = parse(
            r#"
            [fmt.unscoped]
            command = "true"
            "#,
        );
        let unscoped = config.fmt.get("unscoped").expect("unscoped entry exists");
        assert!(!unscoped.is_usable());
        assert!(!unscoped.matches_path(Path::new("crates/core/src/lib.rs")));
    }

    #[test]
    fn test_formatter_without_command_is_not_usable() {
        let config = parse(
            r#"
            [fmt.rustfmt]
            extensions = ["rs"]
            "#,
        );
        let rustfmt = config.fmt.get("rustfmt").expect("rustfmt entry exists");
        assert!(!rustfmt.is_usable());
        assert!(!rustfmt.matches_path(Path::new("crates/core/src/lib.rs")));
        assert_eq!(
            rustfmt.effective_timeout_secs(),
            DEFAULT_TOOLCHAIN_TIMEOUT_SECS
        );
    }
}
