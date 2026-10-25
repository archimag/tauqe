//! Resolution of the project verification command line.
//!
//! The project declares its mandatory verification lifecycle in
//! `[project.verify]` of `tauqe.toml`, or exports it from the task runner named
//! in `[project.runner]`. The harness executes that single command line directly
//! as a process in the repository root: it carries no knowledge of a language, a
//! compiler, or a build system, and it never guesses a command a repository did
//! not declare.
//!
//! A repository that declares no verification command therefore resolves no step
//! at all, and the turn reports that the change set stayed unverified instead of
//! inventing a toolchain from a build manifest.

use tauqe_protocol::ProjectConfig;

use super::exec::CommandLine;
use super::DEFAULT_PROJECT_TIMEOUT_SECS;

/// One verification command together with its wall-clock budget.
pub(super) struct VerificationStep {
    pub(super) command: CommandLine,
    pub(super) timeout_secs: u64,
}

/// Resolves the verification lifecycle a project declares.
///
/// The declaration is the single authority on how the repository verifies, so
/// its command line is the whole pipeline. An entry-local `timeout_secs`
/// overrides the harness budget for that invocation alone, and a declaration
/// whose reference chain is broken is reported and skipped: the repository then
/// verifies nothing instead of running an unintended program.
pub(super) fn resolve_verification_plan(project: &ProjectConfig) -> Vec<VerificationStep> {
    let Some(spec) = project.verify.as_ref() else {
        return Vec::new();
    };

    let resolved = match project.resolve("verify", spec) {
        Ok(resolved) => resolved,
        Err(err) => {
            tracing::warn!(
                reason = %err,
                "project verification command ignored; the change set stays unverified"
            );
            return Vec::new();
        }
    };

    let timeout_secs = resolved
        .timeout_secs
        .filter(|secs| *secs > 0)
        .unwrap_or(DEFAULT_PROJECT_TIMEOUT_SECS);

    vec![VerificationStep {
        command: CommandLine::new(resolved.command, resolved.args),
        timeout_secs,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses a `[project]` declaration from its JSON form.
    ///
    /// The model is serde-generic, so the JSON form pins the same shape the TOML
    /// reader produces without depending on a TOML parser in this module.
    fn project(source: &str) -> ProjectConfig {
        serde_json::from_str(source).expect("the [project] section parses")
    }

    /// Renders resolved steps as printable command lines.
    fn displays(steps: &[VerificationStep]) -> Vec<String> {
        steps.iter().map(|step| step.command.display()).collect()
    }

    #[test]
    fn test_the_declared_lifecycle_is_the_whole_pipeline() {
        let config = project(r#"{"verify":{"command":"cargo","args":["test","--workspace"]}}"#);

        let steps = resolve_verification_plan(&config);

        assert_eq!(displays(&steps), vec!["cargo test --workspace"]);
        assert_eq!(steps[0].timeout_secs, DEFAULT_PROJECT_TIMEOUT_SECS);
    }

    #[test]
    fn test_an_entry_local_timeout_overrides_the_harness_budget() {
        let config =
            project(r#"{"verify":{"command":"make","args":["verify"],"timeout_secs":90}}"#);

        let steps = resolve_verification_plan(&config);

        assert_eq!(displays(&steps), vec!["make verify"]);
        assert_eq!(steps[0].timeout_secs, 90);
        assert_eq!(steps[0].command.program(), "make");
    }

    #[test]
    fn test_a_project_without_a_declaration_resolves_no_step() {
        assert!(resolve_verification_plan(&ProjectConfig::default()).is_empty());
    }

    #[test]
    fn test_a_broken_reference_resolves_no_step() {
        let config = project(r#"{"verify":{"command":"@missing"}}"#);

        assert!(
            resolve_verification_plan(&config).is_empty(),
            "a declaration that cannot resolve must never fall back to a guess"
        );
    }
}
