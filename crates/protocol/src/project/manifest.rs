//! Declarative model of the project manifest (`tauqe-project.toml`).
//!
//! The harness knows nothing about formatters, compilers, or build systems: a
//! repository declares its engineering layout in `tauqe-project.toml`, and the
//! project driver reads that declaration autonomously. This module owns the
//! shape of the declaration and the pure routing rules that map a changed path
//! to the formatter, the disk hooks, and the check steps responsible for it.
//!
//! Routing is what lets one driver serve a polyglot repository. A Rust backend
//! and a TypeScript frontend share a single manifest, and every path is claimed
//! by the entries that declare it: formatters by file extension or path glob,
//! hooks and check steps by path glob. All routing here is deterministic,
//! performs no I/O, and depends only on the manifest and the paths it is given.
//!
//! ```toml
//! [formatters.rust]
//! command = "rustfmt"
//! args = ["--edition", "2021"]
//! extensions = ["rs"]
//!
//! [formatters.web]
//! command = "prettier"
//! args = ["--stdin-filepath"]
//! extensions = ["ts", "tsx"]
//!
//! [post_hooks.protobuf]
//! command = "protoc"
//! paths = ["proto/**"]
//! pass_files = true
//!
//! [checks.rust.steps.check]
//! command = "cargo"
//! args = ["check", "--all-targets", "--message-format=json"]
//! paths = ["crates/**"]
//! parser = "cargo_json"
//! ```

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize};

use crate::glob::GlobPattern;

/// Parser a project driver applies to the output of one check step.
///
/// A step declares how its toolchain reports failures, and the driver turns that
/// output into typed [`crate::Diagnostic`] values. The set is closed and
/// declaration-independent: an unknown spelling resolves to
/// [`DiagnosticParserKind::Plain`], so an unrecognized name degrades to
/// unstructured output instead of discarding the whole manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub enum DiagnosticParserKind {
    /// No structured parsing; the driver reports the output as one message.
    #[default]
    #[serde(rename = "plain")]
    Plain,
    /// `cargo check`/`cargo clippy --message-format=json` records.
    #[serde(rename = "cargo_json")]
    CargoJson,
    /// TypeScript compiler (`tsc`) output.
    #[serde(rename = "typescript")]
    TypeScript,
    /// Go toolchain (`go vet`, `go test`) output.
    #[serde(rename = "go")]
    Go,
}

impl DiagnosticParserKind {
    /// Canonical lower-case identifier used in the manifest.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Plain => "plain",
            Self::CargoJson => "cargo_json",
            Self::TypeScript => "typescript",
            Self::Go => "go",
        }
    }

    /// Tolerant constructor: trims the input, matches case-insensitively, treats
    /// `-` as `_`, and resolves any unknown or empty spelling to
    /// [`DiagnosticParserKind::Plain`].
    pub fn parse_lossy(raw: &str) -> Self {
        let normalized = raw.trim().to_ascii_lowercase().replace('-', "_");
        match normalized.as_str() {
            "cargo_json" | "cargo" => Self::CargoJson,
            "typescript" | "tsc" => Self::TypeScript,
            "go" => Self::Go,
            _ => Self::Plain,
        }
    }
}

// Tolerant decoding is confined to the manifest boundary: a `null`, an absent
// value, or an unknown spelling resolves to `Plain` instead of rejecting a
// hand-written project manifest.
impl<'de> Deserialize<'de> for DiagnosticParserKind {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = Option::<String>::deserialize(deserializer)?;
        Ok(raw.as_deref().map(Self::parse_lossy).unwrap_or_default())
    }
}

/// Declaration of one in-memory formatter entry.
///
/// The formatter is a pure streaming filter: the driver writes the staged file
/// content to its stdin and reads the rewritten content from its stdout. Nothing
/// touches the file system while the change set still lives in virtual staging.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ProjectFormatterSpec {
    /// Executable to run.
    pub command: String,
    /// Arguments passed to the executable before the file content.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// File extensions this entry claims, with an optional leading dot.
    ///
    /// Extension comparison is ASCII case-insensitive, so `rs` claims both
    /// `src/lib.rs` and `src/LIB.RS`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<String>,
    /// Glob patterns this entry claims, for files an extension cannot identify.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    /// Working directory relative to the repository root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<String>,
    /// Per-invocation timeout in seconds; the driver timeout applies when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

impl ProjectFormatterSpec {
    /// Reports whether this entry claims `path`.
    ///
    /// A path is claimed when its extension is declared in
    /// [`ProjectFormatterSpec::extensions`] or when it matches one of
    /// [`ProjectFormatterSpec::paths`] glob patterns. An entry that declares
    /// neither claims no path, so an incomplete entry never silently rewrites
    /// every file of the repository.
    pub fn matches_path(&self, path: &str) -> bool {
        if path.trim().is_empty() {
            return false;
        }
        self.extensions
            .iter()
            .any(|extension| extension_matches(extension, path))
            || self
                .paths
                .iter()
                .any(|pattern| GlobPattern::new(pattern).matches(path))
    }
}

/// Declaration of one disk post-hook entry.
///
/// The hook runs strictly after the change set was written to disk and before
/// verification, so generated sources, refreshed lock files, and migrations are
/// present when the check suite runs.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ProjectPostHookSpec {
    /// Executable to run.
    pub command: String,
    /// Arguments passed to the executable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Glob patterns this hook is scoped to.
    ///
    /// An empty scope makes the hook global: it runs for every change set, which
    /// is what repository-wide side effects such as lock-file refreshes need.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    /// Whether the changed paths are appended to the command line, so the hook
    /// can process only the files the turn actually touched.
    #[serde(default)]
    pub pass_files: bool,
    /// Working directory relative to the repository root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<String>,
    /// Per-invocation timeout in seconds; the driver timeout applies when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

impl ProjectPostHookSpec {
    /// Reports whether this hook is responsible for the given change set.
    ///
    /// A hook without a path scope always applies; a scoped hook applies only
    /// when at least one changed file matches one of its patterns.
    pub fn applies_to(&self, changed_files: &[String]) -> bool {
        self.paths.is_empty() || glob_matches_any(&self.paths, changed_files)
    }
}

/// Declaration of one step of a verification suite.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ProjectCheckStepSpec {
    /// Executable to run.
    pub command: String,
    /// Arguments passed to the executable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Glob patterns this step is scoped to.
    ///
    /// An empty scope makes the step unconditional: it runs for every change
    /// set, which is what a whole-project build needs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    /// Working directory relative to the repository root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<String>,
    /// Per-invocation timeout in seconds; the driver timeout applies when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// Explicit enablement flag; an absent flag means the step is enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Parser the driver applies to this step's output.
    #[serde(default)]
    pub parser: DiagnosticParserKind,
}

impl ProjectCheckStepSpec {
    /// Reports whether the step runs; a step is enabled unless it declares
    /// `enabled = false`.
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }

    /// Reports whether this step is in scope for the given change set.
    pub fn applies_to(&self, changed_files: &[String]) -> bool {
        self.paths.is_empty() || glob_matches_any(&self.paths, changed_files)
    }
}

/// Declaration of one named verification suite.
///
/// A suite groups the steps a driver runs together, so the harness can request
/// `check` before a turn and `all` after it without knowing what either contains.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ProjectCheckSuiteSpec {
    /// Steps of the suite keyed by name and ordered by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub steps: BTreeMap<String, ProjectCheckStepSpec>,
}

/// The complete `tauqe-project.toml` manifest.
///
/// Every section is optional, and every entry is keyed by name, so the whole
/// declaration is a set of named formatters, named post-hooks, and named check
/// suites. Selection across entries is deterministic: entries are examined in
/// lexicographic name order, so a manifest author controls precedence by naming.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ProjectManifest {
    /// In-memory formatters keyed by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub formatters: BTreeMap<String, ProjectFormatterSpec>,
    /// Disk post-hooks keyed by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub post_hooks: BTreeMap<String, ProjectPostHookSpec>,
    /// Verification suites keyed by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub checks: BTreeMap<String, ProjectCheckSuiteSpec>,
}

impl ProjectManifest {
    /// Returns the formatter entry that claims `path`.
    ///
    /// Entries are examined in lexicographic name order, so the first claim in
    /// that order wins. `None` means the file is formatted as authored, which is
    /// a normal outcome for documentation and configuration files.
    pub fn select_formatter_for_path(&self, path: &str) -> Option<(&str, &ProjectFormatterSpec)> {
        self.formatters
            .iter()
            .find(|(_, spec)| spec.matches_path(path))
            .map(|(name, spec)| (name.as_str(), spec))
    }

    /// Returns every post-hook entry responsible for the change set, in name
    /// order.
    pub fn matching_post_hooks(
        &self,
        changed_files: &[String],
    ) -> Vec<(&str, &ProjectPostHookSpec)> {
        self.post_hooks
            .iter()
            .filter(|(_, hook)| hook.applies_to(changed_files))
            .map(|(name, hook)| (name.as_str(), hook))
            .collect()
    }

    /// Returns the enabled, in-scope steps of `suite` for the change set, in name
    /// order.
    ///
    /// An unknown suite yields an empty list rather than an error, so a caller
    /// validates the suite name against [`ProjectManifest::checks`] to
    /// distinguish an unknown suite from one with nothing to run.
    pub fn select_check_steps_for_suite(
        &self,
        suite: &str,
        changed_files: &[String],
    ) -> Vec<(&str, &ProjectCheckStepSpec)> {
        let Some(suite) = self.checks.get(suite) else {
            return Vec::new();
        };
        suite
            .steps
            .iter()
            .filter(|(_, step)| step.is_enabled() && step.applies_to(changed_files))
            .map(|(name, step)| (name.as_str(), step))
            .collect()
    }
}

/// Reports whether any glob pattern claims at least one of the changed files.
///
/// Each pattern is compiled once, so the manifest is not re-parsed per file.
fn glob_matches_any(patterns: &[String], changed_files: &[String]) -> bool {
    patterns.iter().any(|pattern| {
        let compiled = GlobPattern::new(pattern);
        changed_files.iter().any(|file| compiled.matches(file))
    })
}

/// Reports whether a declared extension claims `path`.
fn extension_matches(declared: &str, path: &str) -> bool {
    let declared = declared.trim().trim_start_matches('.');
    if declared.is_empty() {
        return false;
    }
    file_extension(path).is_some_and(|extension| extension.eq_ignore_ascii_case(declared))
}

/// Returns the extension of `path` without its leading dot.
///
/// A bare dotfile such as `.gitignore` has no extension, so it is claimed by
/// path patterns only.
fn file_extension(path: &str) -> Option<&str> {
    let name = path.rsplit(['/', '\\']).next()?;
    let (stem, extension) = name.rsplit_once('.')?;
    if stem.is_empty() || extension.is_empty() {
        return None;
    }
    Some(extension)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A manifest describing a Rust backend and a TypeScript frontend, which is
    /// the polyglot layout path routing exists for.
    const POLYGLOT_MANIFEST: &str = r#"{
      "formatters": {
        "rust": {
          "command": "rustfmt",
          "args": ["--edition", "2021"],
          "extensions": ["rs"]
        },
        "styles": {
          "command": "prettier",
          "extensions": [".css", ".scss"]
        },
        "web": {
          "command": "prettier",
          "args": ["--stdin-filepath"],
          "extensions": [".ts", "tsx"],
          "paths": ["web/**"]
        }
      },
      "post_hooks": {
        "lock": { "command": "cargo", "args": ["generate-lockfile"] },
        "protobuf": {
          "command": "protoc",
          "paths": ["proto/**"],
          "pass_files": true,
          "workdir": "proto",
          "timeout_secs": 120
        }
      },
      "checks": {
        "rust": {
          "steps": {
            "check": {
              "command": "cargo",
              "args": ["check", "--all-targets", "--message-format=json"],
              "paths": ["crates/**"],
              "parser": "cargo-json"
            },
            "lint": {
              "command": "cargo",
              "args": ["clippy", "--all-targets"],
              "parser": "cargo_json",
              "enabled": false
            }
          }
        },
        "web": {
          "steps": {
            "typecheck": {
              "command": "npx",
              "args": ["tsc", "--noEmit"],
              "paths": ["web/**"],
              "parser": "typescript",
              "timeout_secs": 600
            }
          }
        }
      }
    }"#;

    /// Parses the polyglot manifest used by the routing tests.
    fn polyglot_manifest() -> ProjectManifest {
        serde_json::from_str(POLYGLOT_MANIFEST).unwrap()
    }

    /// Builds a change set from path literals.
    fn changed(files: &[&str]) -> Vec<String> {
        files.iter().map(|file| file.to_string()).collect()
    }

    /// Collects the names of the selected post-hooks.
    fn post_hook_names<'a>(hooks: &[(&'a str, &'a ProjectPostHookSpec)]) -> Vec<&'a str> {
        hooks.iter().map(|(name, _)| *name).collect()
    }

    /// Collects the names of the selected check steps.
    fn step_names<'a>(steps: &[(&'a str, &'a ProjectCheckStepSpec)]) -> Vec<&'a str> {
        steps.iter().map(|(name, _)| *name).collect()
    }

    #[test]
    fn test_manifest_deserializes_declared_sections() {
        let manifest = polyglot_manifest();

        assert_eq!(manifest.formatters.len(), 3);
        let rust = manifest.formatters.get("rust").unwrap();
        assert_eq!(rust.command, "rustfmt");
        assert_eq!(rust.args, ["--edition", "2021"]);
        assert_eq!(rust.extensions, ["rs"]);
        assert!(rust.paths.is_empty());
        assert!(rust.workdir.is_none());
        assert!(rust.timeout_secs.is_none());

        let lock = manifest.post_hooks.get("lock").unwrap();
        assert!(lock.paths.is_empty());
        assert!(!lock.pass_files);

        let protobuf = manifest.post_hooks.get("protobuf").unwrap();
        assert!(protobuf.pass_files);
        assert_eq!(protobuf.workdir.as_deref(), Some("proto"));
        assert_eq!(protobuf.timeout_secs, Some(120));

        let check = manifest
            .checks
            .get("rust")
            .unwrap()
            .steps
            .get("check")
            .unwrap();
        assert_eq!(check.parser, DiagnosticParserKind::CargoJson);
        assert!(check.is_enabled());
        assert_eq!(check.paths, ["crates/**"]);

        let lint = manifest
            .checks
            .get("rust")
            .unwrap()
            .steps
            .get("lint")
            .unwrap();
        assert!(!lint.is_enabled());
    }

    #[test]
    fn test_empty_manifest_selects_nothing() {
        let manifest = ProjectManifest::default();
        assert!(manifest.formatters.is_empty());
        assert!(manifest.post_hooks.is_empty());
        assert!(manifest.checks.is_empty());

        let files = changed(&["src/lib.rs"]);
        assert!(manifest.select_formatter_for_path("src/lib.rs").is_none());
        assert!(manifest.matching_post_hooks(&files).is_empty());
        assert!(manifest
            .select_check_steps_for_suite("rust", &files)
            .is_empty());
    }

    #[test]
    fn test_manifest_tolerates_unknown_keys() {
        // Postel's law: an unknown section or entry key must not discard the
        // parts of the manifest that are readable.
        let manifest: ProjectManifest = serde_json::from_str(
            r#"{
                "future_facet": true,
                "formatters": { "rust": { "command": "rustfmt", "mystery": 1 } }
            }"#,
        )
        .unwrap();

        assert_eq!(manifest.formatters.len(), 1);
        assert!(manifest.post_hooks.is_empty());
        assert!(manifest.checks.is_empty());
    }

    #[test]
    fn test_manifest_serde_roundtrip() {
        let manifest = polyglot_manifest();
        let encoded = serde_json::to_string(&manifest).unwrap();
        assert_eq!(
            serde_json::from_str::<ProjectManifest>(&encoded).unwrap(),
            manifest
        );
    }

    #[test]
    fn test_formatter_routing_covers_polyglot_paths() {
        let manifest = polyglot_manifest();

        let (name, spec) = manifest
            .select_formatter_for_path("crates/core/src/lib.rs")
            .unwrap();
        assert_eq!(name, "rust");
        assert_eq!(spec.command, "rustfmt");

        let (name, _) = manifest
            .select_formatter_for_path("web/src/app.tsx")
            .unwrap();
        assert_eq!(name, "web");

        // A declared extension keeps its leading dot stripped, and name order
        // decides between the extension claim and the `web/**` path claim.
        let (name, _) = manifest
            .select_formatter_for_path("web/src/theme.css")
            .unwrap();
        assert_eq!(name, "styles");

        // A path-scoped formatter claims files its extensions do not cover.
        let (name, _) = manifest
            .select_formatter_for_path("web/src/index.html")
            .unwrap();
        assert_eq!(name, "web");

        // Extension matching ignores ASCII case.
        let (name, _) = manifest
            .select_formatter_for_path("crates/core/src/LIB.RS")
            .unwrap();
        assert_eq!(name, "rust");

        assert!(manifest
            .select_formatter_for_path("docs/guide.md")
            .is_none());
        assert!(manifest.select_formatter_for_path("   ").is_none());
    }

    #[test]
    fn test_formatter_routing_prefers_the_first_declared_name() {
        let manifest: ProjectManifest = serde_json::from_str(
            r#"{
                "formatters": {
                    "beta": { "command": "beta-fmt", "extensions": ["rs"] },
                    "alpha": { "command": "alpha-fmt", "extensions": ["rs"] },
                    "gamma": { "command": "gamma-fmt" }
                }
            }"#,
        )
        .unwrap();

        let (name, _) = manifest.select_formatter_for_path("src/lib.rs").unwrap();
        assert_eq!(name, "alpha");

        // An entry that declares no scope claims no path at all.
        assert!(manifest.select_formatter_for_path("src/lib.toml").is_none());
    }

    #[test]
    fn test_post_hook_routing_selects_scoped_and_global_hooks() {
        let manifest = polyglot_manifest();

        let hooks = manifest.matching_post_hooks(&changed(&["proto/api.proto"]));
        assert_eq!(post_hook_names(&hooks), ["lock", "protobuf"]);

        // A scoped hook ignores unrelated paths, while the global hook runs for
        // every change set, including an empty one.
        let hooks = manifest.matching_post_hooks(&changed(&["crates/core/src/lib.rs"]));
        assert_eq!(post_hook_names(&hooks), ["lock"]);
        let hooks = manifest.matching_post_hooks(&[]);
        assert_eq!(post_hook_names(&hooks), ["lock"]);
    }

    #[test]
    fn test_check_step_routing_filters_scope_and_disabled_steps() {
        let manifest = polyglot_manifest();

        let steps =
            manifest.select_check_steps_for_suite("rust", &changed(&["crates/core/src/lib.rs"]));
        assert_eq!(step_names(&steps), ["check"]);

        let web_change = changed(&["web/src/app.tsx"]);
        assert!(manifest
            .select_check_steps_for_suite("rust", &web_change)
            .is_empty());

        let steps = manifest.select_check_steps_for_suite("web", &web_change);
        assert_eq!(step_names(&steps), ["typecheck"]);

        // An unknown suite selects nothing instead of failing.
        assert!(manifest
            .select_check_steps_for_suite("python", &web_change)
            .is_empty());
    }

    #[test]
    fn test_check_step_without_scope_always_applies() {
        let manifest: ProjectManifest = serde_json::from_str(
            r#"{"checks": {"all": {"steps": {"build": {"command": "make"}}}}}"#,
        )
        .unwrap();

        let steps = manifest.select_check_steps_for_suite("all", &[]);
        assert_eq!(step_names(&steps), ["build"]);

        let step = manifest
            .checks
            .get("all")
            .and_then(|suite| suite.steps.get("build"))
            .unwrap();
        assert_eq!(step.parser, DiagnosticParserKind::Plain);
        assert!(step.is_enabled());
        assert!(step.applies_to(&[]));
        assert!(step.args.is_empty());
    }

    #[test]
    fn test_diagnostic_parser_kind_tolerant_parsing() {
        let cases = [
            ("\"plain\"", DiagnosticParserKind::Plain),
            ("\"cargo_json\"", DiagnosticParserKind::CargoJson),
            ("\"cargo-json\"", DiagnosticParserKind::CargoJson),
            ("\"  CARGO \"", DiagnosticParserKind::CargoJson),
            ("\"typescript\"", DiagnosticParserKind::TypeScript),
            ("\"tsc\"", DiagnosticParserKind::TypeScript),
            ("\"go\"", DiagnosticParserKind::Go),
            ("\"javac\"", DiagnosticParserKind::Plain),
            ("null", DiagnosticParserKind::Plain),
        ];
        for (raw, expected) in cases {
            assert_eq!(
                serde_json::from_str::<DiagnosticParserKind>(raw).unwrap(),
                expected
            );
        }

        assert_eq!(
            serde_json::to_value(DiagnosticParserKind::CargoJson).unwrap(),
            serde_json::json!("cargo_json")
        );
        assert_eq!(
            serde_json::to_value(DiagnosticParserKind::TypeScript).unwrap(),
            serde_json::json!("typescript")
        );
        assert_eq!(DiagnosticParserKind::TypeScript.as_str(), "typescript");
        assert_eq!(DiagnosticParserKind::default(), DiagnosticParserKind::Plain);
    }
}
