use std::path::{Path, PathBuf};

pub const DEFAULT_CONFIG_TEMPLATE: &str = include_str!("../../../../tauqe.toml");

/// Parses a command line option value supporting both `--flag value` and `--flag=value`.
///
/// A pure helper over an explicit argument iterator: the library never reads
/// the process command line itself. Binary crates parse their own arguments
/// and pass the resolved paths explicitly.
pub fn cli_arg_value_from<I, S>(args: I, flag: &str) -> Option<PathBuf>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let prefix = format!("{}=", flag);
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        let arg_ref = arg.as_ref();
        if arg_ref == flag {
            if let Some(val) = iter.next() {
                let trimmed = val.as_ref().trim();
                if !trimmed.is_empty() {
                    return Some(PathBuf::from(trimmed));
                }
            }
        } else if let Some(val) = arg_ref.strip_prefix(&prefix) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return Some(PathBuf::from(trimmed));
            }
        }
    }
    None
}

pub(crate) fn dirs_config() -> Option<PathBuf> {
    std::env::var("XDG_CONFIG_HOME")
        .or_else(|_| std::env::var("APPDATA"))
        .ok()
        .map(PathBuf::from)
}

pub(crate) fn dirs_home() -> Option<PathBuf> {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()
        .map(PathBuf::from)
}

pub fn config_candidates(repo_root: Option<&Path>, explicit_config: Option<&Path>) -> Vec<PathBuf> {
    config_candidates_internal(
        repo_root,
        dirs_home().as_deref(),
        dirs_config().as_deref(),
        explicit_config,
    )
}

pub(crate) fn config_candidates_internal(
    repo_root: Option<&Path>,
    home: Option<&Path>,
    config_dir: Option<&Path>,
    cli_config: Option<&Path>,
) -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    if let Some(explicit) = cli_config {
        if !explicit.as_os_str().is_empty() {
            candidates.push(explicit.to_path_buf());
            return candidates;
        }
    }

    let mut add_candidate = |p: PathBuf| {
        if !candidates.contains(&p) {
            candidates.push(p);
        }
    };

    if let Some(root) = repo_root {
        add_candidate(root.join("tauqe.toml"));
        add_candidate(root.join(".tauqe.toml"));
    } else if let Ok(cwd) = std::env::current_dir() {
        add_candidate(cwd.join("tauqe.toml"));
        add_candidate(cwd.join(".tauqe.toml"));
    }

    if let Some(cfg) = config_dir {
        add_candidate(cfg.join("tauqe").join("tauqe.toml"));
    }

    if let Some(h) = home {
        add_candidate(h.join(".config").join("tauqe").join("tauqe.toml"));
        add_candidate(h.join(".tauqe.toml"));
    }

    candidates
}

pub fn find_config_file(
    repo_root: Option<&Path>,
    explicit_config: Option<&Path>,
) -> Option<PathBuf> {
    find_config_file_internal(
        repo_root,
        dirs_home().as_deref(),
        dirs_config().as_deref(),
        explicit_config,
    )
}

pub(crate) fn find_config_file_internal(
    repo_root: Option<&Path>,
    home: Option<&Path>,
    config_dir: Option<&Path>,
    cli_config: Option<&Path>,
) -> Option<PathBuf> {
    config_candidates_internal(repo_root, home, config_dir, cli_config)
        .into_iter()
        .find(|p| p.is_file())
}

pub fn default_config_path(repo_root: Option<&Path>, explicit_config: Option<&Path>) -> PathBuf {
    if let Some(cli_cfg) = explicit_config {
        if !cli_cfg.as_os_str().is_empty() {
            return cli_cfg.to_path_buf();
        }
    }
    if let Some(root) = repo_root {
        if !root.as_os_str().is_empty() {
            return root.join("tauqe.toml");
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        return cwd.join("tauqe.toml");
    }
    if let Some(cfg) = dirs_config() {
        return cfg.join("tauqe").join("tauqe.toml");
    }
    PathBuf::from("tauqe.toml")
}

pub fn write_default_config(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
        let conventions_path = if parent.as_os_str().is_empty() {
            PathBuf::from("Conventions.md")
        } else {
            parent.join("Conventions.md")
        };
        let _ = write_default_conventions(&conventions_path);
    } else {
        let _ = write_default_conventions(Path::new("Conventions.md"));
    }

    std::fs::write(path, DEFAULT_CONFIG_TEMPLATE)
}

pub fn write_default_conventions(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    if path.exists() {
        return Ok(());
    }
    let content = r#"# Project Conventions

## 0. Language & Tone
- All source code, identifiers, comments, documentation, and Git commit messages MUST be strictly in English.

## 1. Architectural Philosophy & Minimalism
- Strictly adhere to YAGNI (You Aren't Gonna Need It). Implement only what is required by current user goals.
- Prefer small, cohesive, deterministic modules over speculative abstractions.
- Do not invent unused configuration options, speculative features, or dead code.

## 2. Git Safety & Commits
- Formulate Git commit summaries in the imperative mood using Conventional Commits (`feat:`, `fix:`, `refactor:`, `docs:`, `test:`).
- Keep commits atomic and self-contained.

## 3. Verification & Quality Gates
- Ensure all changes compile cleanly and pass project build, test, and lint toolchains without warnings or regressions.
"#;
    std::fs::write(path, content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cli_arg_parsing() {
        let args = vec![
            "tauqe-server".to_string(),
            "--config".to_string(),
            "/custom/tauqe.toml".to_string(),
            "--credentials=/custom/creds.toml".to_string(),
        ];

        let cfg = cli_arg_value_from(&args, "--config");
        assert_eq!(cfg, Some(PathBuf::from("/custom/tauqe.toml")));

        let creds = cli_arg_value_from(&args, "--credentials");
        assert_eq!(creds, Some(PathBuf::from("/custom/creds.toml")));

        let missing = cli_arg_value_from(&args, "--unknown");
        assert_eq!(missing, None);
    }

    #[test]
    fn test_cli_config_precedence() {
        let explicit = Path::new("/explicit/path/tauqe.toml");
        let candidates = config_candidates_internal(
            Some(Path::new("/repo")),
            None,
            None,
            Some(explicit),
        );
        assert_eq!(candidates, vec![explicit.to_path_buf()]);
    }

    #[test]
    fn test_config_candidates_isolated_from_cwd_when_repo_root_specified() {
        let temp_dir = tempfile::tempdir().unwrap();
        let candidates = config_candidates_internal(Some(temp_dir.path()), None, None, None);
        assert_eq!(
            candidates,
            vec![
                temp_dir.path().join("tauqe.toml"),
                temp_dir.path().join(".tauqe.toml"),
            ]
        );
    }

    #[test]
    fn test_write_and_find_default_config() {
        use crate::config::{load_config, AppConfig};

        let temp_dir = tempfile::tempdir().unwrap();
        let cfg_path = default_config_path(Some(temp_dir.path()), None);
        assert_eq!(cfg_path, temp_dir.path().join("tauqe.toml"));

        assert!(find_config_file_internal(Some(temp_dir.path()), None, None, None).is_none());
        write_default_config(&cfg_path).unwrap();

        let found = find_config_file_internal(Some(temp_dir.path()), None, None, None);
        assert_eq!(found, Some(cfg_path.clone()));

        let loaded = load_config(Some(temp_dir.path()));
        let template_cfg: AppConfig =
            toml::from_str(DEFAULT_CONFIG_TEMPLATE).expect("Template config must be valid TOML");
        assert!(template_cfg.validate().is_ok());
        assert_eq!(loaded.active_model(), template_cfg.active_model());
        assert_eq!(loaded.available_models(), template_cfg.available_models());
        assert_eq!(loaded.context.pinned, template_cfg.context.pinned);
        assert!(temp_dir.path().join("Conventions.md").is_file());
    }

    #[test]
    fn test_write_default_conventions_does_not_overwrite_existing() {
        let temp_dir = tempfile::tempdir().unwrap();
        let conv_path = temp_dir.path().join("Conventions.md");
        std::fs::write(&conv_path, "custom conventions").unwrap();
        write_default_conventions(&conv_path).unwrap();
        let content = std::fs::read_to_string(&conv_path).unwrap();
        assert_eq!(content, "custom conventions");
    }
}
