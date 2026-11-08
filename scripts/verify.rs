#!/usr/bin/env rust-script

use std::env;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::{exit, Command};

fn find_repo_root() -> PathBuf {
    let mut dir = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    loop {
        if dir.join("tauqe.toml").is_file() && dir.join("Cargo.toml").is_file() {
            return dir;
        }
        if !dir.pop() {
            break;
        }
    }
    Path::new(file!())
        .canonicalize()
        .ok()
        .and_then(|p| p.parent().and_then(|p| p.parent().map(|p| p.to_path_buf())))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn run_step(label: &str, program: &str, args: &[String]) {
    let status = Command::new(program)
        .args(args)
        .status()
        .unwrap_or_else(|err| {
            eprintln!("verify: failed to execute {label}: {err}");
            exit(1);
        });
    if !status.success() {
        exit(status.code().unwrap_or(1));
    }
}

fn main() {
    let repo_root = find_repo_root();
    if let Err(err) = env::set_current_dir(&repo_root) {
        eprintln!(
            "verify: failed to set current dir to repo root ({}): {err}",
            repo_root.display()
        );
        exit(1);
    }

    if env::var_os("TMPDIR").is_none() {
        let dev_shm = Path::new("/dev/shm");
        let test_file = dev_shm.join(format!(".tauqe_tmpdir_test_{}", std::process::id()));
        if std::fs::write(&test_file, b"").is_ok() {
            let _ = std::fs::remove_file(&test_file);
            env::set_var("TMPDIR", "/dev/shm");
        }
    }

    let mut changed = Vec::new();
    if !io::stdin().is_terminal() {
        let stdin = io::stdin();
        for line in stdin.lines() {
            match line {
                Ok(path) => {
                    let trimmed = path.trim();
                    if !trimmed.is_empty() {
                        changed.push(trimmed.to_string());
                    }
                }
                Err(err) => {
                    eprintln!("verify: error reading changed paths from stdin: {err}");
                    exit(1);
                }
            }
        }
    }

    let mut has_tui = false;
    let mut has_server = false;
    let mut has_core = false;
    let mut has_tools = false;
    let mut has_protocol = false;
    let mut has_other_rs = false;
    let mut has_root = false;
    let mut unknown = false;

    for path in &changed {
        let p = Path::new(path);
        let base = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let is_rs = path.ends_with(".rs");
        let is_cargo_toml = base == "Cargo.toml";

        if path.starts_with("crates/tui/") {
            if is_rs || is_cargo_toml {
                has_tui = true;
            } else if !path.ends_with(".md") {
                unknown = true;
            }
        } else if path.starts_with("crates/server/") {
            if is_rs || is_cargo_toml {
                has_server = true;
            } else if !path.ends_with(".md") {
                unknown = true;
            }
        } else if path.starts_with("crates/core/") {
            if is_rs || is_cargo_toml {
                has_core = true;
            } else if !path.ends_with(".md") {
                unknown = true;
            }
        } else if path.starts_with("crates/tools/") {
            if is_rs || is_cargo_toml {
                has_tools = true;
            } else if !path.ends_with(".md") {
                unknown = true;
            }
        } else if path.starts_with("crates/protocol/") {
            if is_rs || is_cargo_toml {
                has_protocol = true;
            } else if !path.ends_with(".md") {
                unknown = true;
            }
        } else if path.ends_with(".md") {
            // Markdown files no longer trigger checks
        } else if is_rs {
            has_other_rs = true;
        } else {
            match base {
                "Cargo.toml" | "Cargo.lock" | "rustfmt.toml" | "Justfile" | "tauqe.toml" => {
                    has_root = true;
                }
                _ => {
                    if path.starts_with("scripts/") {
                        has_root = true;
                    } else {
                        unknown = true;
                    }
                }
            }
        }
    }

    let mut cargo_args: Vec<String> = Vec::new();
    let run_cargo: bool;

    if changed.is_empty() || unknown || has_protocol || has_root || has_other_rs {
        run_cargo = true;
        cargo_args.push("--workspace".to_string());
    } else {
        if has_tools {
            cargo_args.extend([
                "-p".to_string(),
                "tauqe-tools".to_string(),
                "-p".to_string(),
                "tauqe-core".to_string(),
                "-p".to_string(),
                "tauqe-server".to_string(),
            ]);
        } else if has_core {
            cargo_args.extend([
                "-p".to_string(),
                "tauqe-core".to_string(),
                "-p".to_string(),
                "tauqe-server".to_string(),
            ]);
        } else if has_server {
            cargo_args.extend(["-p".to_string(), "tauqe-server".to_string()]);
        }

        if has_tui {
            cargo_args.extend(["-p".to_string(), "tauqe-tui".to_string()]);
        }

        run_cargo = !cargo_args.is_empty();
    }

    let tmpdir = env::var("TMPDIR").unwrap_or_else(|_| "system-default".to_string());
    let cargo_desc = if cargo_args.is_empty() {
        "none".to_string()
    } else {
        cargo_args.join(" ")
    };

    eprintln!(
        "verify: {} changed path(s); cargo={} ({}) tmpdir={}",
        changed.len(),
        if run_cargo { 1 } else { 0 },
        cargo_desc,
        tmpdir
    );

    if run_cargo {
        let mut check_args = vec!["check".to_string()];
        check_args.extend(cargo_args.iter().cloned());
        check_args.push("--all-targets".to_string());
        run_step("`cargo check`", "cargo", &check_args);

        let mut clippy_args = vec!["clippy".to_string()];
        clippy_args.extend(cargo_args.iter().cloned());
        clippy_args.extend(["--all-targets", "--", "-D", "warnings"].map(String::from));
        run_step("`cargo clippy`", "cargo", &clippy_args);

        // The task runner owns the test invocation. Its `test` recipe decides
        // which runner executes the suite, how failures are reported, and that
        // the doctest pass follows, so the script never re-derives that policy.
        let mut test_args = vec!["test".to_string()];
        test_args.extend(cargo_args.iter().cloned());
        run_step("`just test`", "just", &test_args);
    }

    exit(0);
}
