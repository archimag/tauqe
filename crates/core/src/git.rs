use std::path::{Path, PathBuf};
use std::process::Command;
use workbench_protocol::RepositoryState;

pub fn get_repository_state(dir: Option<&Path>) -> RepositoryState {
    let root = match run_git(dir, &["rev-parse", "--show-toplevel"]) {
        Ok(out) => out.trim().to_string(),
        Err(_) => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            return RepositoryState {
                root: cwd.display().to_string(),
                branch: "unknown".to_string(),
                head: "unknown".to_string(),
                dirty: false,
            };
        }
    };

    let repo_dir = Path::new(&root);

    let branch = run_git(Some(repo_dir), &["rev-parse", "--abbrev-ref", "HEAD"])
        .unwrap_or_else(|_| "HEAD".to_string())
        .trim()
        .to_string();

    let head = run_git(Some(repo_dir), &["rev-parse", "--short", "HEAD"])
        .unwrap_or_else(|_| "unknown".to_string())
        .trim()
        .to_string();

    let status = run_git(Some(repo_dir), &["status", "--porcelain"]).unwrap_or_default();
    let dirty = !status.trim().is_empty();

    RepositoryState {
        root,
        branch,
        head,
        dirty,
    }
}

pub fn list_repository_files(dir: Option<&Path>) -> anyhow::Result<Vec<String>> {
    let output = run_git(dir, &["ls-files"])?;
    let files = output
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    Ok(files)
}

fn run_git(dir: Option<&Path>, args: &[&str]) -> anyhow::Result<String> {
    let mut command = Command::new("git");
    if let Some(d) = dir {
        command.current_dir(d);
    }
    let output = command.args(args).output()?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        anyhow::bail!(
            "git command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_repository_state_no_panic() {
        let state = get_repository_state(None);
        
        assert!(!state.root.is_empty(), "Repository root should not be empty");
        assert!(!state.branch.is_empty(), "Branch name should not be empty");
        assert!(!state.head.is_empty(), "HEAD hash should not be empty");
    }

    #[test]
    fn test_list_repository_files() {
        let files = list_repository_files(None).unwrap();
        assert!(!files.is_empty(), "Git repository should contain tracked files");
    }
}
