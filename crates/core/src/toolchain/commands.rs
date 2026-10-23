//! Resolution of the project verification command line.
//!
//! The project declares its mandatory verification lifecycle in
//! `[project.verify]` of `tauqe.toml`, and the harness executes that command line
//! directly as a process in the repository root. An entry may reuse a named
//! `[project.commands.<name>]` declaration, and its own `timeout_secs` overrides
//! the harness budget; `ProjectConfig::resolve` follows the reference chain and
//! rejects a broken one, so the harness never spawns an unintended program.
//!
//! A repository that declares no verification command keeps the legacy
//! deterministic resolution: the typed `[toolchain.check]`, `[toolchain.lint]`,
//! and `[toolchain.test]` sections first, then the legacy `check_command`, and
//! finally manifest auto-detection. A phase switched off with
//! `enabled = false` contributes no command at all, so a project can drop the
//! lint pass without deleting its configuration.

use std::path::Path;

use tauqe_protocol::ProjectConfig;

use crate::config::{ToolchainConfig, ToolchainPhaseConfig};
use crate::edits::protocol::xml::VerifyTarget;

use super::exec::CommandLine;
use super::ToolchainKind;

/// Packaged scripts declared by `package.json`, read once per resolution.
type PackageScripts = serde_json::Map<String, serde_json::Value>;

/// A single verification phase of the legacy toolchain pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VerificationPhase {
    Check,
    Lint,
    Test,
}

impl VerificationPhase {
    /// Phases selected by a verification target, in execution order.
    fn selected(target: &VerifyTarget) -> Vec<VerificationPhase> {
        match target {
            VerifyTarget::Check => vec![VerificationPhase::Check],
            VerifyTarget::Clippy => vec![VerificationPhase::Lint],
            VerifyTarget::Test => vec![VerificationPhase::Test],
            VerifyTarget::All => vec![
                VerificationPhase::Check,
                VerificationPhase::Lint,
                VerificationPhase::Test,
            ],
        }
    }

    /// Typed configuration section that governs the phase.
    fn section<'a>(&self, config: &'a ToolchainConfig) -> Option<&'a ToolchainPhaseConfig> {
        match self {
            VerificationPhase::Check => config.check.as_ref(),
            VerificationPhase::Lint => config.lint.as_ref(),
            VerificationPhase::Test => config.test.as_ref(),
        }
    }
}

/// One verification command together with its wall-clock budget.
pub(super) struct VerificationStep {
    pub command: CommandLine,
    pub timeout_secs: u64,
}

/// Resolves the verification pipeline for `target`.
#[cfg(test)]
pub(super) fn resolve_verification_plan(
    repo_root: &Path,
    target: &VerifyTarget,
) -> Vec<VerificationStep> {
    let config = crate::config::load_toolchain_config(repo_root);
    let project = crate::config::load_section::<ProjectConfig>(repo_root, "project");

    resolve_verification_plan_with_config(repo_root, target, &project, &config)
}

/// Resolves the verification pipeline using pre-loaded configurations.
pub(super) fn resolve_verification_plan_with_config(
    repo_root: &Path,
    target: &VerifyTarget,
    project: &ProjectConfig,
    config: &ToolchainConfig,
) -> Vec<VerificationStep> {
    if let Some(step) = declared_verification(project, config) {
        return vec![step];
    }

    legacy_plan(repo_root, target, config)
}

/// Resolves the project-declared verification lifecycle.
///
/// The declaration is the single authority on how the repository verifies, so
/// its command line runs for every target. A declaration whose reference chain
/// is broken is reported and ignored: the repository then keeps verifying
/// through deterministic resolution instead of silently skipping verification.
fn declared_verification(
    project: &ProjectConfig,
    config: &ToolchainConfig,
) -> Option<VerificationStep> {
    let spec = project.verify.as_ref()?;
    let resolved = match project.resolve("verify", spec) {
        Ok(resolved) => resolved,
        Err(err) => {
            tracing::warn!(
                reason = %err,
                "project verification command ignored; using deterministic resolution"
            );
            return None;
        }
    };

    let timeout_secs = resolved
        .timeout_secs
        .filter(|secs| *secs > 0)
        .unwrap_or_else(|| config.effective_timeout_secs());

    Some(VerificationStep {
        command: CommandLine::new(resolved.command, resolved.args),
        timeout_secs,
    })
}

/// Resolves the deterministic pipeline an unmigrated repository keeps.
fn legacy_plan(
    repo_root: &Path,
    target: &VerifyTarget,
    config: &ToolchainConfig,
) -> Vec<VerificationStep> {
    let scripts = read_package_scripts(repo_root);
    let (kind, _) = detect_toolchain(repo_root, None);

    let mut steps = Vec::new();
    for phase in VerificationPhase::selected(target) {
        if let Some(step) = phase_step(phase, repo_root, config, &kind, scripts.as_ref()) {
            steps.push(step);
        }
    }

    // Without a detected language nothing else can be verified, so an npm project
    // whose scripts claim none of the selected phases still runs its test suite.
    if steps.is_empty() && kind == ToolchainKind::Npm && repo_root.join("package.json").is_file() {
        steps.push(VerificationStep {
            command: npm_run("test"),
            timeout_secs: config.effective_timeout_secs(),
        });
    }

    steps
}

/// Resolves one phase, or `None` when it is disabled or contributes no command.
fn phase_step(
    phase: VerificationPhase,
    repo_root: &Path,
    config: &ToolchainConfig,
    kind: &ToolchainKind,
    scripts: Option<&PackageScripts>,
) -> Option<VerificationStep> {
    let section = phase.section(config);
    if section.is_some_and(|section| !section.is_enabled()) {
        return None;
    }
    let timeout_secs = config.phase_timeout_secs(section);

    let explicit = section
        .and_then(ToolchainPhaseConfig::command_line)
        .map(|(program, args)| CommandLine::new(program, args));

    let command = match phase {
        // The legacy field is a check command by name, so it precedes detection.
        VerificationPhase::Check => explicit.or_else(|| legacy_check_command(config)),
        VerificationPhase::Lint | VerificationPhase::Test => explicit,
    }
    .or_else(|| auto_detected_command(phase, repo_root, kind, scripts))
    // A project that only declares a custom command keeps verifying every phase
    // with it, matching the behavior of the flat configuration.
    .or_else(|| legacy_check_command(config))?;

    Some(VerificationStep {
        command,
        timeout_secs,
    })
}

/// Returns the legacy `check_command` line as a command.
fn legacy_check_command(config: &ToolchainConfig) -> Option<CommandLine> {
    let command = config.check_command.as_deref()?.trim();
    if command.is_empty() {
        return None;
    }
    Some(CommandLine::tokenized(command))
}

/// Returns the command deterministic auto-detection assigns to a phase.
fn auto_detected_command(
    phase: VerificationPhase,
    repo_root: &Path,
    kind: &ToolchainKind,
    scripts: Option<&PackageScripts>,
) -> Option<CommandLine> {
    match kind {
        ToolchainKind::Cargo => Some(match phase {
            VerificationPhase::Check => CommandLine::new(
                "cargo",
                vec!["check".to_string(), "--all-targets".to_string()],
            ),
            VerificationPhase::Lint => CommandLine::new(
                "cargo",
                vec![
                    "clippy".to_string(),
                    "--all-targets".to_string(),
                    "--".to_string(),
                    "-D".to_string(),
                    "warnings".to_string(),
                ],
            ),
            VerificationPhase::Test => {
                CommandLine::new("cargo", vec!["test".to_string(), "--quiet".to_string()])
            }
        }),
        ToolchainKind::Npm => npm_phase_command(phase, repo_root, scripts),
        ToolchainKind::Go => Some(match phase {
            VerificationPhase::Check | VerificationPhase::Lint => {
                CommandLine::new("go", vec!["vet".to_string(), "./...".to_string()])
            }
            VerificationPhase::Test => {
                CommandLine::new("go", vec!["test".to_string(), "./...".to_string()])
            }
        }),
        ToolchainKind::Custom(command) => {
            (phase == VerificationPhase::Check).then(|| CommandLine::tokenized(command))
        }
        ToolchainKind::None => None,
    }
}

/// Resolves a phase command from the accumulated `package.json` scripts.
fn npm_phase_command(
    phase: VerificationPhase,
    repo_root: &Path,
    scripts: Option<&PackageScripts>,
) -> Option<CommandLine> {
    let has_script = |name: &str| scripts.is_some_and(|scripts| scripts.contains_key(name));
    match phase {
        VerificationPhase::Check if has_script("check") => Some(npm_run("check")),
        VerificationPhase::Check if repo_root.join("tsconfig.json").is_file() => Some(
            CommandLine::new("npx", vec!["tsc".to_string(), "--noEmit".to_string()]),
        ),
        VerificationPhase::Lint if has_script("lint") => Some(npm_run("lint")),
        VerificationPhase::Test if has_script("test") => Some(npm_run("test")),
        _ => None,
    }
}

/// Builds `npm run <script>`.
fn npm_run(script: &str) -> CommandLine {
    CommandLine::new("npm", vec!["run".to_string(), script.to_string()])
}

/// Reads the `scripts` table from `package.json` once so toolchain detection and
/// command resolution share a single manifest read.
fn read_package_scripts(repo_root: &Path) -> Option<PackageScripts> {
    let content = std::fs::read_to_string(repo_root.join("package.json")).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;
    json.get("scripts")
        .and_then(|scripts| scripts.as_object())
        .cloned()
}

/// Deterministically detects the build/validation toolchain from project files in repo root.
///
/// A non-empty custom command short-circuits detection and wins over every
/// manifest, so an explicit project-wide command is never silently ignored.
pub fn detect_toolchain(
    repo_root: &Path,
    custom_command: Option<&str>,
) -> (ToolchainKind, Option<String>) {
    if let Some(cmd) = custom_command {
        if !cmd.trim().is_empty() {
            let cmd = cmd.trim().to_string();
            return (ToolchainKind::Custom(cmd.clone()), Some(cmd));
        }
    }

    if repo_root.join("Cargo.toml").is_file() {
        return (ToolchainKind::Cargo, Some("cargo check".to_string()));
    }

    if repo_root.join("package.json").is_file() {
        if let Some(scripts) = read_package_scripts(repo_root) {
            for script in ["check", "typecheck", "test"] {
                if scripts.contains_key(script) {
                    return (ToolchainKind::Npm, Some(format!("npm run {}", script)));
                }
            }
        }
        if repo_root.join("tsconfig.json").is_file() {
            return (ToolchainKind::Npm, Some("npx tsc --noEmit".to_string()));
        }
        return (ToolchainKind::Npm, Some("npm test".to_string()));
    }

    if repo_root.join("go.mod").is_file() {
        return (ToolchainKind::Go, Some("go vet ./...".to_string()));
    }

    (ToolchainKind::None, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Creates a temporary repository with an optional `tauqe.toml`.
    fn repo(config: Option<&str>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        if let Some(config) = config {
            std::fs::write(dir.path().join("tauqe.toml"), config).unwrap();
        }
        dir
    }

    /// Renders resolved steps as printable command lines.
    fn displays(steps: &[VerificationStep]) -> Vec<String> {
        steps.iter().map(|step| step.command.display()).collect()
    }

    #[test]
    fn test_cargo_phases_are_auto_detected() {
        let dir = repo(None);
        std::fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();

        let steps = resolve_verification_plan(dir.path(), &VerifyTarget::All);
        assert_eq!(
            displays(&steps),
            vec![
                "cargo check --all-targets",
                "cargo clippy --all-targets -- -D warnings",
                "cargo test --quiet",
            ]
        );
        assert_eq!(
            steps[0].timeout_secs,
            crate::config::DEFAULT_TOOLCHAIN_TIMEOUT_SECS
        );

        let check_only = resolve_verification_plan(dir.path(), &VerifyTarget::Check);
        assert_eq!(displays(&check_only), vec!["cargo check --all-targets"]);
    }

    #[test]
    fn test_project_verify_is_the_authoritative_lifecycle() {
        let dir = repo(Some(
            r#"
[toolchain.check]
command = "cargo"
args = ["check"]

[project.verify]
command = "cargo"
args = ["test", "--workspace"]
"#,
        ));
        std::fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();

        // The declared lifecycle replaces every legacy phase for every target.
        for target in [
            VerifyTarget::All,
            VerifyTarget::Check,
            VerifyTarget::Clippy,
            VerifyTarget::Test,
        ] {
            let steps = resolve_verification_plan(dir.path(), &target);
            assert_eq!(displays(&steps), vec!["cargo test --workspace"]);
        }
    }

    #[test]
    fn test_project_verify_resolves_references_and_timeout() {
        let dir = repo(Some(
            r#"
[project.verify]
command = "@ci"

[project.commands.ci]
command = "make"
args = ["verify"]
timeout_secs = 90
"#,
        ));

        let steps = resolve_verification_plan(dir.path(), &VerifyTarget::Check);
        assert_eq!(displays(&steps), vec!["make verify"]);
        assert_eq!(steps[0].timeout_secs, 90);
        assert_eq!(steps[0].command.program(), "make");
        assert_eq!(steps[0].command.args(), ["verify".to_string()]);
    }

    #[test]
    fn test_broken_project_verify_falls_back_to_deterministic_resolution() {
        let dir = repo(Some("[project.verify]\ncommand = \"@missing\"\n"));
        std::fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();

        let steps = resolve_verification_plan(dir.path(), &VerifyTarget::Check);
        assert_eq!(displays(&steps), vec!["cargo check --all-targets"]);
    }

    #[test]
    fn test_typed_sections_override_detection_and_honor_disable() {
        let dir = repo(Some(
            r#"
[toolchain.check]
command = "cargo"
args = ["check", "--workspace"]
timeout_secs = 90

[toolchain.lint]
enabled = false
"#,
        ));
        std::fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();

        let steps = resolve_verification_plan(dir.path(), &VerifyTarget::All);
        assert_eq!(
            displays(&steps),
            vec!["cargo check --workspace", "cargo test --quiet"]
        );
        assert_eq!(steps[0].timeout_secs, 90);
        assert_eq!(
            steps[1].timeout_secs,
            crate::config::DEFAULT_TOOLCHAIN_TIMEOUT_SECS
        );
    }

    #[test]
    fn test_phase_section_without_command_falls_back_to_detection() {
        let dir = repo(Some(
            r#"
[toolchain.check]
args = ["--fast"]
"#,
        ));
        std::fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();

        let steps = resolve_verification_plan(dir.path(), &VerifyTarget::Check);
        assert_eq!(displays(&steps), vec!["cargo check --all-targets"]);
    }

    #[test]
    fn test_legacy_check_command_applies_to_every_phase() {
        let dir = repo(Some("[toolchain]\ncheck_command = \"make verify\""));

        let steps = resolve_verification_plan(dir.path(), &VerifyTarget::Test);
        assert_eq!(displays(&steps), vec!["make verify"]);
        assert_eq!(steps[0].command.program(), "make");
        assert_eq!(steps[0].command.args(), ["verify".to_string()]);
    }

    #[test]
    fn test_legacy_check_command_precedes_detection_for_check_phase() {
        let dir = repo(Some("[toolchain]\ncheck_command = \"make verify\""));
        std::fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();

        let steps = resolve_verification_plan(dir.path(), &VerifyTarget::All);
        assert_eq!(
            displays(&steps),
            vec![
                "make verify",
                "cargo clippy --all-targets -- -D warnings",
                "cargo test --quiet",
            ]
        );
    }

    #[test]
    fn test_npm_scripts_select_phase_commands() {
        let dir = repo(None);
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"check":"tsc --noEmit","lint":"eslint ."}}"#,
        )
        .unwrap();

        let steps = resolve_verification_plan(dir.path(), &VerifyTarget::All);
        assert_eq!(displays(&steps), vec!["npm run check", "npm run lint"]);
    }

    #[test]
    fn test_npm_project_without_selected_script_runs_tests() {
        let dir = repo(None);
        std::fs::write(dir.path().join("package.json"), r#"{"name":"app"}"#).unwrap();

        let steps = resolve_verification_plan(dir.path(), &VerifyTarget::Check);
        assert_eq!(displays(&steps), vec!["npm run test"]);
    }

    #[test]
    fn test_disabled_phase_with_no_alternative_yields_no_command() {
        let dir = repo(Some(
            r#"
[toolchain.lint]
enabled = false
"#,
        ));
        std::fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();

        let steps = resolve_verification_plan(dir.path(), &VerifyTarget::Clippy);
        assert!(steps.is_empty());
    }

    #[test]
    fn test_detect_toolchain_kinds() {
        let dir = repo(None);
        assert_eq!(detect_toolchain(dir.path(), None).0, ToolchainKind::None);

        std::fs::write(dir.path().join("go.mod"), "module example").unwrap();
        assert_eq!(detect_toolchain(dir.path(), None).0, ToolchainKind::Go);

        assert_eq!(
            detect_toolchain(dir.path(), Some("make verify")),
            (
                ToolchainKind::Custom("make verify".to_string()),
                Some("make verify".to_string())
            )
        );
    }
}
