//! Environment isolation of the Git subprocess layer.
//!
//! The transaction substrate resolves a repository from the directory it hands
//! the child, so an inherited redirection variable silently addresses a foreign
//! repository, index, or object store instead. Every spawn is sanitized, and
//! this suite proves it against a decoy repository.
//!
//! The redirection family is installed with `set_var`, which mutates the
//! environment of the whole process. The suite therefore holds a single test: a
//! second one would observe the mutation concurrently.

use std::path::Path;

use tauqe_tools::git::cmd::{
    commit_with_developer_author, run_git, run_git_check_ignore, stage_files,
};
use tauqe_tools::git::GitHooks;

/// Name of the file the probe stages in the target repository.
const PROBE_FILE: &str = "tauqe-env-isolation-probe.txt";

/// Creates a repository with a base commit and a developer identity.
fn init_repository(root: &Path) {
    run_git(Some(root), &["init", "-b", "master"]).unwrap();
    run_git(Some(root), &["config", "user.name", "Target Developer"]).unwrap();
    run_git(Some(root), &["config", "user.email", "target@example.com"]).unwrap();
    std::fs::write(root.join("base.txt"), "base content\n").unwrap();
    run_git(Some(root), &["add", "base.txt"]).unwrap();
    run_git(Some(root), &["commit", "-m", "chore: base commit"]).unwrap();
}

/// Reads the revision the repository currently points at.
fn head_of(root: &Path) -> String {
    run_git(Some(root), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string()
}

#[test]
fn a_redirected_environment_never_moves_an_operation_out_of_the_named_repository() {
    let target_dir = tempfile::tempdir().unwrap();
    let target = target_dir.path();
    init_repository(target);
    let target_head_before = head_of(target);

    // The decoy excludes the probe file and lends its paths to the redirection
    // variables below, so an inherited one would answer from here.
    let decoy_dir = tempfile::tempdir().unwrap();
    let decoy = decoy_dir.path();
    init_repository(decoy);
    std::fs::write(decoy.join(".gitignore"), format!("{PROBE_FILE}\n")).unwrap();
    run_git(Some(decoy), &["add", ".gitignore"]).unwrap();
    run_git(
        Some(decoy),
        &["commit", "-m", "chore: ignore the probe file"],
    )
    .unwrap();
    let decoy_head_before = head_of(decoy);
    let decoy_index_before = std::fs::read(decoy.join(".git/index")).unwrap();

    // The family a daemon launched from a Git hook or a scripted environment
    // inherits, pointed at the decoy.
    let injected_index = decoy.join(".git/injected-index");
    std::env::set_var("GIT_DIR", decoy.join(".git"));
    std::env::set_var("GIT_WORK_TREE", decoy);
    std::env::set_var("GIT_INDEX_FILE", &injected_index);
    std::env::set_var("GIT_COMMON_DIR", decoy.join(".git"));
    std::env::set_var("GIT_OBJECT_DIRECTORY", decoy.join(".git/objects"));
    std::env::set_var("GIT_CONFIG_COUNT", "1");
    std::env::set_var("GIT_CONFIG_KEY_0", "user.email");
    std::env::set_var("GIT_CONFIG_VALUE_0", "injected@example.com");
    std::env::set_var("GIT_CONFIG_PARAMETERS", "'user.name'='Injected'");

    // A repository-scoped read resolves the directory the caller named.
    let toplevel = run_git(Some(target), &["rev-parse", "--show-toplevel"]).unwrap();
    assert_eq!(
        std::fs::canonicalize(toplevel.trim()).unwrap(),
        std::fs::canonicalize(target).unwrap()
    );

    // The injected configuration never reaches the child, so the identity the
    // commit path probes stays the identity the target declares.
    let identity = run_git(Some(target), &["var", "GIT_AUTHOR_IDENT"]).unwrap();
    assert!(
        identity.contains("<target@example.com>"),
        "the target identity must win: {identity}"
    );
    assert!(
        !identity.contains("Injected"),
        "the injected identity must stay out: {identity}"
    );

    // The ignore probe answers from the target, which declares no rule, instead
    // of repeating the rule the decoy declares.
    let ignored = run_git_check_ignore(target, &[PROBE_FILE.to_string()]).unwrap();
    assert!(
        ignored.is_empty(),
        "the decoy ignore rule must stay invisible: {ignored:?}"
    );

    // A staging and commit sequence lands in the target repository.
    std::fs::write(target.join(PROBE_FILE), "probe\n").unwrap();
    stage_files(target, &[PROBE_FILE.to_string()]).unwrap();
    commit_with_developer_author(target, "feat: probe file", false, GitHooks::Bypass).unwrap();

    assert_ne!(head_of(target), target_head_before);
    let committed = run_git(Some(target), &["ls-tree", "--name-only", "HEAD"]).unwrap();
    assert!(committed.contains(PROBE_FILE), "tree: {committed}");

    // The decoy's history, index, and work tree are untouched.
    assert_eq!(head_of(decoy), decoy_head_before);
    assert_eq!(
        std::fs::read(decoy.join(".git/index")).unwrap(),
        decoy_index_before
    );
    assert!(!injected_index.exists());
    assert!(!decoy.join(PROBE_FILE).exists());
}
