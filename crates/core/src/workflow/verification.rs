use std::path::Path;
use tauqe_protocol::ProjectConfig;
use tokio::sync::{mpsc, watch};

use crate::edits::protocol::{VerifyOnSuccess, VerifyRequest, VerifyTarget};
use crate::providers::StreamEvent;

pub struct VerificationOutcome {
    pub success: bool,
    pub is_cancelled: bool,
    pub report: String,
}

/// Records the outcome of a whole verification run as a single structured log record.
fn log_verification_outcome(target: &str, duration_ms: u64, outcome: &VerificationOutcome) {
    match (outcome.is_cancelled, outcome.success) {
        (true, _) => tracing::warn!(
            target = %target,
            duration_ms,
            success = false,
            cancelled = true,
            "verification cancelled"
        ),
        (false, true) => tracing::info!(
            target = %target,
            duration_ms,
            success = true,
            cancelled = false,
            "verification finished"
        ),
        (false, false) => tracing::warn!(
            target = %target,
            duration_ms,
            success = false,
            cancelled = false,
            "verification failed"
        ),
    }
}

/// Resolves the effective project configuration and runs the verification
/// pipeline of a `<verify .../>` request.
///
/// This facade is reserved for turn-free callers — a manual or CLI invocation
/// that owns no turn state. No call site inside a bounded turn may invoke it: a
/// turn holds the project configuration it froze on its first proposal round
/// and must call [`run_workflow_verification_with_config`], so no verification
/// round re-reads `tauqe.toml` or re-runs a declared task runner's dump.
// The facade has no caller inside the crate: every bounded turn verifies
// against the configuration it froze on its first proposal round, so the
// turn-free surface is exercised by its tests alone.
#[allow(dead_code)]
pub async fn run_workflow_verification(
    repo_root: &Path,
    changed_files: &[String],
    verify_req: &VerifyRequest,
    stream_tx: &mpsc::Sender<StreamEvent>,
    stream_output: bool,
    cancel_rx: Option<&watch::Receiver<bool>>,
) -> VerificationOutcome {
    let project = crate::toolchain::effective_project_config(repo_root, cancel_rx).await;
    run_workflow_verification_with_config(
        repo_root,
        &project,
        changed_files,
        verify_req,
        stream_tx,
        stream_output,
        cancel_rx,
    )
    .await
}

/// Executes the project verification pipeline according to the model's
/// `<verify .../>` request, under an already resolved project configuration.
///
/// `changed_files` is the authoritative change set of the turn: the model's
/// edits, the post-hook artifacts, the files a repair round touched, and the
/// working-copy differential of the transaction. It reaches the project command
/// on standard input, one path per line, so a project dispatcher can narrow the
/// checks to the affected technologies while the mandatory gate stays in force.
///
/// `project` is the configuration a bounded turn froze on its first proposal
/// round, so a healing round never re-reads `tauqe.toml` or runs a declared task
/// runner's dump again.
pub async fn run_workflow_verification_with_config(
    repo_root: &Path,
    project: &ProjectConfig,
    changed_files: &[String],
    verify_req: &VerifyRequest,
    stream_tx: &mpsc::Sender<StreamEvent>,
    stream_output: bool,
    cancel_rx: Option<&watch::Receiver<bool>>,
) -> VerificationOutcome {
    let target_str = match verify_req.target {
        VerifyTarget::Check => "check",
        VerifyTarget::Clippy => "clippy",
        VerifyTarget::Test => "test",
        VerifyTarget::All => "all",
    };
    let started = std::time::Instant::now();
    tracing::info!(target = %target_str, "verification started");

    let _ = stream_tx
        .send(StreamEvent::ToolchainStarted {
            command: target_str.to_string(),
        })
        .await;

    if stream_output {
        let start_msg = format!("\n\n🔍 Running verification (`{}`)...\n", target_str);
        let _ = stream_tx.send(StreamEvent::TextDelta(start_msg)).await;
    }

    let (ok, results) = crate::toolchain::run_verification_pipeline_with_config(
        repo_root,
        project,
        changed_files,
        cancel_rx,
    )
    .await;

    if results.is_empty() {
        let note = "⚠️ The project declares no verification command, so the change set was not verified.\n"
            .to_string();
        let _ = stream_tx
            .send(StreamEvent::ToolchainFinished {
                command: target_str.to_string(),
                success: true,
                message: Some("No project verification command declared".to_string()),
            })
            .await;
        if stream_output || verify_req.on_success == VerifyOnSuccess::Report {
            let _ = stream_tx.send(StreamEvent::TextDelta(note.clone())).await;
        }
        let outcome = VerificationOutcome {
            success: true,
            is_cancelled: false,
            report: note,
        };
        log_verification_outcome(target_str, started.elapsed().as_millis() as u64, &outcome);
        return outcome;
    }

    let is_cancelled = results.iter().any(|r| r.is_cancelled);
    if is_cancelled {
        let cancel_msg = format!("Verification (`{}`) cancelled", target_str);
        let _ = stream_tx
            .send(StreamEvent::ToolchainFinished {
                command: target_str.to_string(),
                success: false,
                message: Some(cancel_msg),
            })
            .await;
        let _ = stream_tx.send(StreamEvent::Cancelled).await;
        let report = format!("Verification (`{}`) was cancelled.\n", target_str);
        if stream_output {
            let _ = stream_tx.send(StreamEvent::TextDelta(report.clone())).await;
        }
        let outcome = VerificationOutcome {
            success: false,
            is_cancelled: true,
            report,
        };
        log_verification_outcome(target_str, started.elapsed().as_millis() as u64, &outcome);
        return outcome;
    }

    let _ = stream_tx
        .send(StreamEvent::ToolchainFinished {
            command: target_str.to_string(),
            success: ok,
            message: if ok {
                Some(format!("Verification (`{}`) passed", target_str))
            } else {
                Some(format!("Verification (`{}`) failed", target_str))
            },
        })
        .await;

    let mut report = String::new();
    if ok {
        let success_msg = format!("✅ Code verification (`{}`) passed.\n", target_str);
        if stream_output || verify_req.on_success == VerifyOnSuccess::Report {
            let _ = stream_tx
                .send(StreamEvent::TextDelta(success_msg.clone()))
                .await;
        }

        if verify_req.on_success == VerifyOnSuccess::Report {
            for r in &results {
                if !r.combined_output.trim().is_empty() {
                    report.push_str(&format!(
                        "`{}` output:\n```\n{}\n```\n\n",
                        r.command,
                        r.combined_output.trim()
                    ));
                }
            }
            if report.is_empty() {
                report = success_msg;
            } else {
                report = format!("{}\n{}", success_msg, report);
            }
        }
    } else {
        report.push_str(&format!(
            "❌ Code verification (`{}`) failed:\n\n",
            target_str
        ));
        for r in results.iter().filter(|r| !r.success) {
            // A failing command contributes the typed findings its output could
            // be parsed into, so the healing round reasons about the offending
            // location instead of a transcript full of ANSI escapes.
            let body = if r.diagnostics.is_empty() {
                r.combined_output.trim().to_string()
            } else {
                tauqe_protocol::format_diagnostics(&r.diagnostics)
            };
            report.push_str(&format!(
                "Command `{}` failed:\n```\n{}\n```\n\n",
                r.command, body
            ));
        }
        if stream_output {
            let _ = stream_tx.send(StreamEvent::TextDelta(report.clone())).await;
        }
    }

    let outcome = VerificationOutcome {
        success: ok,
        is_cancelled: false,
        report,
    };
    log_verification_outcome(target_str, started.elapsed().as_millis() as u64, &outcome);
    outcome
}

#[cfg(test)]
mod verification_outcome_tests {
    use super::*;

    #[tokio::test]
    async fn test_run_workflow_verification_without_a_declared_command() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, mut rx) = mpsc::channel(10);
        let req = VerifyRequest {
            target: VerifyTarget::All,
            on_success: VerifyOnSuccess::Silent,
        };
        let outcome = run_workflow_verification(dir.path(), &[], &req, &tx, false, None).await;
        assert!(outcome.success);
        assert!(!outcome.is_cancelled);
        assert!(outcome
            .report
            .contains("The project declares no verification command"));

        let mut saw_finished = false;
        while let Ok(event) = rx.try_recv() {
            if let StreamEvent::ToolchainFinished { message, .. } = event {
                assert_eq!(
                    message.as_deref(),
                    Some("No project verification command declared")
                );
                saw_finished = true;
            }
        }
        assert!(saw_finished);
    }

    #[tokio::test]
    async fn test_run_workflow_verification_reports_a_corrupted_project_configuration() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("tauqe.toml"),
            "[models]\nprimary = [\"openrouter:a/b\"]\n",
        )
        .unwrap();

        let (tx, _rx) = mpsc::channel(10);
        let req = VerifyRequest {
            target: VerifyTarget::All,
            on_success: VerifyOnSuccess::Silent,
        };
        let outcome = run_workflow_verification(dir.path(), &[], &req, &tx, false, None).await;

        assert!(!outcome.success);
        assert!(!outcome.is_cancelled);
        assert!(
            outcome.report.contains("tauqe.toml"),
            "the healing input must name the corrupted file: {}",
            outcome.report
        );
        assert!(
            outcome.report.contains("verify"),
            "the healing input must carry the semantic reason: {}",
            outcome.report
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_run_workflow_verification_renders_structured_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("tauqe.toml"),
            "[models]\nprimary = [\"openrouter:a/b\"]\n[project.verify]\ncommand = \"sh\"\nargs = ['-c', 'printf \"src/lib.rs:3:5: error: mismatched types\\n\" >&2; exit 1']\n",
        )
        .unwrap();

        let (tx, _rx) = mpsc::channel(10);
        let req = VerifyRequest {
            target: VerifyTarget::All,
            on_success: VerifyOnSuccess::Silent,
        };
        let outcome = run_workflow_verification(dir.path(), &[], &req, &tx, false, None).await;

        assert!(!outcome.success);
        assert!(!outcome.is_cancelled);
        assert!(
            outcome
                .report
                .contains("src/lib.rs:3:5: error: mismatched types"),
            "the healing input must carry the parsed location: {}",
            outcome.report
        );
        assert!(!outcome.report.contains('\u{1b}'));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_run_workflow_verification_delivers_the_change_set_on_stdin() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("tauqe.toml"),
            "[models]\nprimary = [\"openrouter:a/b\"]\n[project.verify]\ncommand = \"sh\"\nargs = ['-c', 'cat']\n",
        )
        .unwrap();

        let (tx, _rx) = mpsc::channel(10);
        let req = VerifyRequest {
            target: VerifyTarget::All,
            on_success: VerifyOnSuccess::Report,
        };
        let changed = vec!["src/lib.rs".to_string(), "generated.rs".to_string()];
        let outcome = run_workflow_verification(dir.path(), &changed, &req, &tx, false, None).await;

        assert!(outcome.success);
        assert!(
            outcome.report.contains("src/lib.rs\ngenerated.rs"),
            "the command must read the change set on its standard input: {}",
            outcome.report
        );
    }
}

/// Extracts repository file paths mentioned in compilation, linter, or test failure output.
pub fn extract_error_paths(report: &str, available_files: &[String]) -> Vec<String> {
    let available_set: std::collections::HashSet<&str> =
        available_files.iter().map(|s| s.as_str()).collect();
    let mut found = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for line in report.lines() {
        for word in line.split_whitespace() {
            let trimmed = word.trim_matches(|c: char| {
                matches!(
                    c,
                    '`' | '\'' | '"' | '(' | ')' | '[' | ']' | ',' | ';' | '<' | '>'
                )
            });
            let candidate = trimmed.strip_prefix("./").unwrap_or(trimmed);
            let path_part = candidate.split(':').next().unwrap_or("");
            if !path_part.is_empty()
                && available_set.contains(path_part)
                && seen.insert(path_part.to_string())
            {
                found.push(path_part.to_string());
            }
        }
    }
    found
}

/// Constructs a prompt asking the model to fix compiler, clippy, or test failures.
pub fn build_verification_retry_prompt(report: &str, target_language: Option<&str>) -> String {
    let mut prompt = String::new();
    prompt.push_str("Code verification failed with the following errors:\n\n```\n");
    prompt.push_str(report.trim());
    prompt.push_str("\n```\n\nPlease fix the errors by proposing corrected edits that follow the edit protocol defined in your system instructions. If you need additional files to understand or fix the issue, request them through the discovery protocol defined in your system instructions.");
    if let Some(lang) = target_language {
        prompt.push_str(&format!(
            "\n\nNote: Formulate your explanations in {} (the language of the user's request), while conducting all reasoning strictly in English.",
            lang
        ));
    }
    prompt
}

/// Constructs a prompt reminding the model that it must provide code edits when fixing verification failures.
pub fn build_verification_missing_edits_prompt(
    report: &str,
    target_language: Option<&str>,
) -> String {
    let mut prompt = String::new();
    prompt.push_str("Your previous response contained explanations but did not include any code edits that follow the edit protocol defined in your system instructions.\n\nCode verification is still failing with the following errors:\n\n```\n");
    prompt.push_str(report.trim());
    prompt.push_str("\n```\n\nYou MUST propose concrete code edits to resolve the compilation or verification errors, following the edit protocol defined in your system instructions. If you need additional files, request them through the discovery protocol defined in your system instructions.");
    if let Some(lang) = target_language {
        prompt.push_str(&format!(
            "\n\nNote: Formulate your explanations in {} (the language of the user's request), while conducting all reasoning strictly in English.",
            lang
        ));
    }
    prompt
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_build_verification_missing_edits_prompt() {
        let prompt =
            build_verification_missing_edits_prompt("error: mismatched types", Some("Russian"));
        assert!(prompt.contains("did not include any code edits"));
        assert!(prompt.contains("error: mismatched types"));
        assert!(prompt.contains("Russian"));
        assert!(!prompt.contains("<tauqe_edits"));
        assert!(!prompt.contains("<context_request"));
    }

    #[test]
    fn test_build_verification_retry_prompt_is_protocol_neutral() {
        let prompt = build_verification_retry_prompt("error[E0308]: mismatched types", None);
        assert!(prompt.contains("error[E0308]: mismatched types"));
        assert!(prompt.contains("edit protocol defined in your system instructions"));
        assert!(prompt.contains("discovery protocol defined in your system instructions"));
        assert!(!prompt.contains("<tauqe_edits"));
        assert!(!prompt.contains("<context_request"));
    }
    use super::*;

    #[test]
    fn test_extract_error_paths_from_clippy_report() {
        let report = r#"
error: used `unwrap()` on an `Option` value
  --> crates/protocol/src/context.rs:88:20
   |
88 |         let file = self.files.get(path).unwrap();
   |                    ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^

error: aborting due to 1 previous error
"#;
        let available = vec![
            "crates/protocol/src/context.rs".to_string(),
            "crates/core/src/lib.rs".to_string(),
        ];
        let paths = extract_error_paths(report, &available);
        assert_eq!(paths, vec!["crates/protocol/src/context.rs"]);
    }

    #[test]
    fn test_extract_error_paths_ignores_unknown_files() {
        let report = "--> unknown/file.rs:10:5\n  crates/core/src/lib.rs:20:1";
        let available = vec!["crates/core/src/lib.rs".to_string()];
        let paths = extract_error_paths(report, &available);
        assert_eq!(paths, vec!["crates/core/src/lib.rs"]);
    }
}
